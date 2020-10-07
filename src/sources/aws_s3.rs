use crate::{
    config::{DataType, GlobalOptions, SourceConfig, SourceDescription},
    dns::Resolver,
    event::Event,
    region::{self, RegionOrEndpoint},
    shutdown::ShutdownSignal,
    sinks::util::rusoto,
    Pipeline,
};
use codec::BytesDelimitedCodec;
use futures::{
    compat::{Compat, Future01CompatExt},
    future::{join_all, FutureExt, TryFutureExt},
    stream::{self, StreamExt},
};
use futures01::Sink;
use rusoto_core::{Region, RusotoError};
use rusoto_s3::{GetObjectRequest, S3Client, S3};
use rusoto_sqs::{
    DeleteMessageError, DeleteMessageRequest, GetQueueUrlError, GetQueueUrlRequest, Message,
    ReceiveMessageError, ReceiveMessageRequest, Sqs, SqsClient,
};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use std::{convert::TryInto, time::Duration};
use tokio::{select, time};
use tokio_util::codec::FramedRead;

// TODO:
// * Handle decompression
// * Revisit configuration of queue. Should we take the URL instead?
//   * At the least, support setting a differente queue owner
// * Move AWS utils from sink to general
// * Use multiline config
// * Consider / decide on multi-region S3 support (handling messages referring to buckets in
//   multiple regions)
// * Consider / decide on custom endpoint support
//   * How would we handle this for multi-region S3 support?
// * Max line bytes
// * Make sure we are handling shutdown well
// * Consider any special handling of FIFO SQS queues
// * Consider having helper methods stream data and have top-level forward to pipeline
//
// Future work:
// * Additional codecs. Just treating like `file` source with newlines for now

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Compression {
    Auto,
    None,
    Gzip,
    Lz4,
    Snappy,
    Zstd,
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Auto
    }
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Strategy {
    Sqs,
}

impl Default for Strategy {
    fn default() -> Self {
        Strategy::Sqs
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AwsS3Config {
    #[serde(default)]
    compression: Compression,

    #[serde(default)]
    strategy: Strategy,

    #[serde(default)]
    sqs: Option<SqsConfig>,

    #[serde(default)]
    assume_role: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SqsConfig {
    region: Region,
    queue_name: String,
    #[serde(default = "default_poll_interval_secs")]
    poll_secs: u64,
    #[serde(default = "default_visibility_timeout_secs")]
    visibility_timeout_secs: u64,
    #[serde(default = "default_true")]
    delete_message: bool,
}

const fn default_poll_interval_secs() -> u64 {
    15
}

const fn default_visibility_timeout_secs() -> u64 {
    300
}
const fn default_true() -> bool {
    true
}

inventory::submit! {
    SourceDescription::new::<AwsS3Config>("aws_s3")
}

#[async_trait::async_trait]
#[typetag::serde(name = "aws_s3")]
impl SourceConfig for AwsS3Config {
    async fn build(
        &self,
        _name: &str,
        _globals: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        match self.strategy {
            Strategy::Sqs => Ok(Box::new(
                self.create_sqs_ingestor()
                    .await?
                    .run(out, shutdown)
                    .boxed()
                    .compat(),
            )),
        }
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "aws_s3"
    }
}

#[derive(Debug, Snafu)]
enum CreateSqsIngestorError {
    #[snafu(display("Unable to initialize: {}", source))]
    Initialize { source: SqsIngestorNewError },
    #[snafu(display("Unable to create AWS client: {}", source))]
    Client { source: crate::Error },
    #[snafu(display("Unable to create AWS credentials provider: {}", source))]
    Credentials { source: crate::Error },
    #[snafu(display("sqs configuration required when strategy=sqs"))]
    ConfigMissing,
}

impl AwsS3Config {
    async fn create_sqs_ingestor(&self) -> Result<SqsIngestor, CreateSqsIngestorError> {
        match self.sqs {
            Some(ref sqs) => {
                // TODO:
                // * move resolver?
                // * try cloning credentials provider again?
                let resolver = Resolver;
                let client = rusoto::client(resolver).with_context(|| Client {})?;
                let creds =
                    rusoto::AwsCredentialsProvider::new(&sqs.region, self.assume_role.clone())
                        .with_context(|| Credentials {})?;
                let sqs_client = SqsClient::new_with(client.clone(), creds, sqs.region.clone());
                let creds =
                    rusoto::AwsCredentialsProvider::new(&sqs.region, self.assume_role.clone())
                        .with_context(|| Credentials {})?;
                let s3_client = S3Client::new_with(client.clone(), creds, sqs.region.clone());

                SqsIngestor::new(sqs_client, s3_client, sqs.clone(), self.compression)
                    .await
                    .with_context(|| Initialize {})
            }
            None => Err(CreateSqsIngestorError::ConfigMissing {}),
        }
    }
}

#[derive(Debug, Snafu)]
enum SqsIngestorNewError {
    #[snafu(display("Unable to fetch queue URL for {}: {}", name, source))]
    FetchQueueUrl {
        source: RusotoError<GetQueueUrlError>,
        name: String,
    },
    #[snafu(display("Got an empty queue URL for {}", name))]
    MissingQueueUrl { name: String },
}

struct SqsIngestor {
    s3_client: S3Client,
    sqs_client: SqsClient,

    compression: Compression,

    queue_url: String,
    poll_interval: Duration,
    visibility_timeout: Duration,
    delete_message: bool,
}

impl SqsIngestor {
    async fn new(
        sqs_client: SqsClient,
        s3_client: S3Client,
        config: SqsConfig,
        compression: Compression,
    ) -> Result<SqsIngestor, SqsIngestorNewError> {
        // TODO error handling
        let queue_url_result = sqs_client
            .get_queue_url(GetQueueUrlRequest {
                queue_name: config.queue_name.clone(),
                ..Default::default()
            })
            .await
            .with_context(|| FetchQueueUrl {
                name: config.queue_name.clone(),
            })?;

        let queue_url = queue_url_result
            .queue_url
            .ok_or(SqsIngestorNewError::MissingQueueUrl {
                name: config.queue_name.clone(),
            })?;

        Ok(SqsIngestor {
            s3_client,
            sqs_client,

            compression,

            queue_url,
            poll_interval: Duration::from_secs(config.poll_secs),
            visibility_timeout: Duration::from_secs(config.visibility_timeout_secs),
            delete_message: config.delete_message,
        })
    }

    async fn run(self, out: Pipeline, shutdown: ShutdownSignal) -> Result<(), ()> {
        let mut interval = time::interval(self.poll_interval).map(|_| ());
        let mut shutdown = shutdown.compat();

        loop {
            select! {
                Some(()) = interval.next() => (),
                _ = &mut shutdown => break Ok(()),
                else => break Ok(()),
            };

            let messages = self.receive_messages().await.unwrap_or_default();

            for message in messages {
                let receipt_handle = message.receipt_handle.clone();

                match self.handle_sqs_message(message, out.clone()).await {
                    Ok(()) => {
                        if self.delete_message {
                            //self.delete_message(receipt_handle).await.unwrap();
                        }
                    }
                    Err(()) => {} // TODO emit error
                }
            }
        }
    }

    async fn handle_sqs_message(&self, message: Message, out: Pipeline) -> Result<(), ()> {
        let s3_event: S3Event =
            serde_json::from_str(message.body.unwrap_or_default().as_ref()).unwrap();

        self.handle_s3_event(s3_event, out).map_ok(|_| ()).await
    }

    async fn handle_s3_event(&self, s3_event: S3Event, out: Pipeline) -> Result<(), ()> {
        for record in s3_event.records {
            self.handle_s3_event_record(record, out.clone()).await?
        }
        Ok(())
    }

    async fn handle_s3_event_record(
        &self,
        s3_event: S3EventRecord,
        out: Pipeline,
    ) -> Result<(), ()> {
        let object = self
            .s3_client
            .get_object(GetObjectRequest {
                bucket: s3_event.s3.bucket.name.clone(),
                key: s3_event.s3.object.key.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        // TODO assert event type

        let metadata = object.metadata.unwrap_or_default().clone(); // TODO can we avoid cloning the hashmap?

        match object.body {
            Some(body) => {
                let stream = FramedRead::new(
                    body.into_async_read(),
                    BytesDelimitedCodec::new_with_max_length(b'\n', 100000),
                )
                .filter_map(|line| {
                    let bucket_name = s3_event.s3.bucket.name.clone();
                    let object_key = s3_event.s3.object.key.clone();
                    let aws_region = s3_event.aws_region.clone();
                    let metadata = metadata.clone();

                    async move {
                        match line {
                            Ok(line) => {
                                let mut event = Event::from(line);

                                let log = event.as_mut_log();
                                log.insert("bucket", bucket_name);
                                log.insert("object", object_key);
                                log.insert("region", aws_region);

                                for (key, value) in &metadata {
                                    log.insert(key, value.clone());
                                }

                                Some(Ok(event))
                            }
                            Err(err) => {
                                // TODO handling IO errors here?
                                dbg!(err);
                                None
                            }
                        }
                    }
                });

                out.send_all(Compat::new(Box::pin(stream)))
                    .compat()
                    .await
                    .map_err(|error| {
                        error!(message = "Error sending S3 Logs", %error);
                        ()
                    })
                    .map(|_| ())
            }
            None => Ok(()),
        }
    }

    async fn receive_messages(&self) -> Result<Vec<Message>, RusotoError<ReceiveMessageError>> {
        self.sqs_client
            .receive_message(ReceiveMessageRequest {
                // TODO additional parameters?
                // TODO evaluate wait_time_seconds
                queue_url: self.queue_url.clone(),
                max_number_of_messages: Some(10),
                wait_time_seconds: Some(10),
                // TODO handle timeouts > i64
                visibility_timeout: Some(self.visibility_timeout.as_secs().try_into().unwrap()),
                ..Default::default()
            })
            .map_ok(|res| res.messages.unwrap_or_default()) // TODO
            .await
    }

    async fn delete_message(
        &self,
        receipt_handle: Option<String>,
    ) -> Result<(), RusotoError<DeleteMessageError>> {
        let receipt_handle = receipt_handle.unwrap_or_default(); // TODO
        self.sqs_client
            .delete_message(DeleteMessageRequest {
                queue_url: self.queue_url.clone(),
                receipt_handle,
                ..Default::default()
            })
            .await
    }
}

// https://docs.aws.amazon.com/AmazonS3/latest/dev/notification-content-structure.html
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct S3Event {
    records: Vec<S3EventRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3EventRecord {
    event_version: String, // TODO compare >=
    event_source: String,
    aws_region: String, // TODO validate?
    event_name: String, // TODO break up?

    s3: S3Message,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3Message {
    bucket: S3Bucket,
    object: S3Object,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3Bucket {
    name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3Object {
    key: String,
}

//{
//"Records": [
//{
//"eventVersion": "2.1",
//"eventSource": "aws:s3",
//"awsRegion": "us-east-1",
//"eventTime": "2020-10-05T19:03:28.642Z",
//"eventName": "ObjectCreated:Put",
//"userIdentity": {
//"principalId": "AWS:AROARBQI54DEYUD75WSD4:jesse"
//},
//"requestParameters": {
//"sourceIPAddress": "72.231.203.7"
//},
//"responseElements": {
//"x-amz-request-id": "B20DFAA51C377B25",
//"x-amz-id-2": "nD0lquqsFmMLK/JxelzvywPuU3Ro1KWnyS8mKnm1x87icEK7bms2wKqdej+YnGZjAshXYE29ky7GYUSjND26/glWinK9GpSk8+A+NlmVm/U="
//},
//"s3": {
//"s3SchemaVersion": "1.0",
//"configurationId": "Test Bucket Notifications",
//"bucket": {
//"name": "timberio-jesse-test",
//"ownerIdentity": {
//"principalId": "A3PEG170DF9VNQ"
//},
//"arn": "arn:aws:s3:::timberio-jesse-test"
//},
//"object": {
//"key": "out.log",
//"size": 103994,
//"eTag": "458dd6ac5ad1b3483621688cbc80cfc4",
//"sequencer": "005F7B6E0310DFB5F2"
//}
//}
//}
//]
//}

////func getRegionFromQueueURL(queueURL string) (string, error) {
////// get region from queueURL
/////// Example: https://sqs.us-east-1.amazonaws.com/627959692251/test-s3-logs
/////queueURLSplit := strings.Split(queueURL, ".")
/////if queueURLSplit[0] == "https://sqs" && queueURLSplit[2] == "amazonaws" {
/////return queueURLSplit[1], nil
/////}
/////return "", fmt.Errorf("queueURL is not in format:
/////https://sqs.{REGION_ENDPOINT}.amazonaws.com/{ACCOUNT_NUMBER}/{QUEUE_NAME}")
/////}

///// parse region out of SQS URL
/////
///// https://docs.aws.amazon.com/AWSSimpleQueueService/latest/SQSDeveloperGuide/sqs-queue-message-identifiers.html
///// https://docs.aws.amazon.com/general/latest/gr/sqs-service.html
//fn region_from_sqs_queue_url(url: String) -> Result<Region, ()> {
//// TODO error handling

//let uri = url.parse::<Uri>().unwrap();

//let parts = &uri.authority().unwrap().host().splitn(3, ".");

//let (service, region,
//}

// TODO remove?
// https://github.com/rusoto/rusoto/pull/803 makes it seem like the
// deserialization / serialization should just work, but it doesn't seem to
mod string {
    use serde::{de, Deserialize, Deserializer, Serializer};
    use std::fmt::Display;
    use std::str::FromStr;

    pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Display,
        S: Serializer,
    {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
    where
        T: FromStr,
        T::Err: Display,
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
}
