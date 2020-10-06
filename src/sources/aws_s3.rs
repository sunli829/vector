use crate::{
    config::{DataType, GlobalOptions, SourceConfig, SourceDescription},
    dns::Resolver,
    event::Event,
    region::RegionOrEndpoint,
    shutdown::ShutdownSignal,
    sinks::util::rusoto,
    Pipeline,
};
use codec::BytesDelimitedCodec;
use futures::{
    compat::{Compat, Future01CompatExt},
    future::{FutureExt, TryFutureExt},
    stream::StreamExt,
};
use futures01::Sink;
use rusoto_core::Region;
use rusoto_s3::{GetObjectRequest, S3Client, S3};
use rusoto_sqs::{GetQueueUrlRequest, ReceiveMessageRequest, Sqs, SqsClient};
use serde::{Deserialize, Serialize};
use std::{convert::TryInto, time::Duration};
use tokio::{select, time};
use tokio_util::codec::FramedRead;

// TODO:
// * Revisit configuration of queue. Should we take the URL instead?
//   * At the least, support setting a differente queue owner
// * Move AWS utils from sink to general
// * Use multiline config
// * Share provider config?
//
// * Look at Tower service
//
// Questions:
// * Handling shutdown gracefully
//   * Don't poll for new messages; finish processing current set
// * Concurrency. Should we process objects / messages concurrently?
// * Why does send_all/forward return the sink?

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
    #[serde(flatten)]
    region: RegionOrEndpoint,
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
                    .await
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

impl AwsS3Config {
    async fn create_sqs_ingestor(&self) -> SqsIngestor {
        match self.sqs {
            Some(ref sqs) => {
                let region: Region = (&sqs.region).try_into().unwrap();
                // TODO:
                // * move resolver?
                // * try cloning credentials provider again?
                let resolver = Resolver;
                let client = rusoto::client(resolver).unwrap();
                let creds =
                    rusoto::AwsCredentialsProvider::new(&region, self.assume_role.clone()).unwrap();
                let sqs_client = SqsClient::new_with(client.clone(), creds, region.clone());
                let creds =
                    rusoto::AwsCredentialsProvider::new(&region, self.assume_role.clone()).unwrap();
                let s3_client = S3Client::new_with(client.clone(), creds, region.clone());

                SqsIngestor::new(sqs_client, s3_client, sqs.clone(), self.compression).await
            }
            None => unimplemented!("TODO"),
        }
    }
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
    ) -> SqsIngestor {
        // TODO error handling
        let queue_url = sqs_client
            .get_queue_url(GetQueueUrlRequest {
                queue_name: config.queue_name.clone(),
                ..Default::default()
            })
            .await
            .unwrap()
            .queue_url
            .unwrap();

        SqsIngestor {
            s3_client,
            sqs_client,

            compression,

            queue_url,
            poll_interval: Duration::from_secs(config.poll_secs),
            visibility_timeout: Duration::from_secs(config.visibility_timeout_secs),
            delete_message: config.delete_message,
        }
    }

    async fn run(self, out: Pipeline, shutdown: ShutdownSignal) -> Result<(), ()> {
        let mut interval = time::interval(self.poll_interval).map(|_| ());
        let mut shutdown = shutdown.compat();
        let queue_url = &self.queue_url;
        let sqs_client = &self.sqs_client;

        loop {
            select! {
                Some(()) = interval.next() => (),
                _ = &mut shutdown => break,
                else => break,
            };

            let receive_message_result = sqs_client
                .receive_message(ReceiveMessageRequest {
                    // TODO additional parameters?
                    // TODO evaluate wait_time_seconds
                    queue_url: queue_url.to_string(),
                    max_number_of_messages: Some(10),
                    wait_time_seconds: Some(10),
                    ..Default::default()
                })
                .await
                .unwrap();

            //let messages: Box<dyn Stream<Item = (Bytes, String), Error = ()> + Send> =
            for message in receive_message_result.messages.unwrap_or_default() {
                // TODO
                // * Handle empty messages
                //
                // parse message
                // handle_message (process S3 objects)
                // ack message
                let s3_event: S3Event =
                    serde_json::from_str(message.body.unwrap_or_default().as_ref()).unwrap();

                match self.handle_s3_event(s3_event, out.clone()).await {
                    Ok(()) => {}  // TODO ack message
                    Err(()) => {} // TODO emit error
                }
            }
        }
        Ok(())
    }
    //let events = self.poll_events().await;

    // Poll for messages
    // For each message:
    //   * Parse
    //   * Forward logs
    //   * Delete message (if delete_message=true)

    //let (sink, _) = out
    //.send_all(futures01::stream::iter_ok(logs))
    //.compat()
    //.await
    //.map_err(|error| error!(message = "Error sending S3 Logs", %error))?;
    //out = sink;
    //
    async fn handle_s3_event(&self, s3_event: S3Event, out: Pipeline) -> Result<(), ()> {
        for record in s3_event.records {
            match self.handle_s3_event_record(record, out.clone()).await {
                Ok(()) => {}
                Err(()) => return Err(()), // TODO emit error
            }
        }

        Ok(())
    }

    // TODO handle shutdown
    async fn handle_s3_event_record(
        &self,
        s3_event: S3EventRecord,
        out: Pipeline,
    ) -> Result<(), ()> {
        let object = self
            .s3_client
            .get_object(GetObjectRequest {
                bucket: s3_event.s3.bucket.name,
                key: s3_event.s3.object.key,
                ..Default::default()
            })
            .await
            .unwrap();

        // TODO assert event type

        match object.body {
            Some(body) => {
                // .filter_map(|r| )
                // TODO: decompress
                // TODO: other codecs
                let stream = FramedRead::new(
                    body.into_async_read(),
                    BytesDelimitedCodec::new_with_max_length(b'\n', 100000),
                )
                .filter_map(|line| async move {
                    match line {
                        Ok(line) => Some(Ok(Event::from(line))),
                        Err(err) => {
                            // TODO handling IO errors here?
                            dbg!(err);
                            None
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
        // TODO:
        // Fetch SQS message(s)
        // Fetch object for each message
        // Decompress (if needed)
        // Create LogEvent, add tags
        //async fn capture_logs(&self) -> impl Iterator<Item = Event> {
        //vec![].into_iter()
        //}
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

#[cfg(test)]
mod tests {
    use super::*;
}
