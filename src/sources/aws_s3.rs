jse crate::{
    config::{DataType, GlobalOptions, SourceConfig, SourceDescription},
    dns::Resolver,
    event::Event,
    shutdown::ShutdownSignal,
    sinks::util::rusoto,
    Pipeline,
};
use futures::{
    compat::Future01CompatExt,
    future::{FutureExt, TryFutureExt},
    stream::StreamExt,
};
use futures01::Sink;
use rusoto_core::Region;
use rusoto_s3::{S3Client, S3};
use rusoto_sqs::{GetQueueUrlRequest, ReceiveMessageRequest, Sqs, SqsClient};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::{select, time};

// TODO:
// * Revisit configuration of queue. Should we take the URL instead?
//   * At the least, support setting a differente queue owner
// * Move AWS utils from sink to general

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
            Strategy::Sqs => match self.sqs {
                Some(ref sqs) => Ok(Box::new(
                    SqsIngestor::new(sqs.clone(), self.assume_role.clone(), self.compression)
                        .await
                        .run(out, shutdown)
                        .boxed()
                        .compat(),
                )),
                None => unimplemented!("TODO"),
            },
        }
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "aws_s3"
    }
}

struct SqsIngestor {
    sqs_client: SqsClient,

    assume_role: Option<String>,

    compression: Compression,

    queue_url: String,
    poll_interval: Duration,
    visibility_timeout: Duration,
    delete_message: bool,
}

impl SqsIngestor {
    async fn new(
        config: SqsConfig,
        assume_role: Option<String>,
        compression: Compression,
    ) -> SqsIngestor {
        // TODO error handling
        // TODO move resolver?
        let resolver = Resolver;
        let client = rusoto::client(resolver).unwrap();
        let creds =
            rusoto::AwsCredentialsProvider::new(&config.region, assume_role.clone()).unwrap();
        let sqs_client = SqsClient::new_with(client, creds, config.region.clone());

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
            sqs_client,

            assume_role,

            compression,

            queue_url,
            poll_interval: Duration::from_secs(config.poll_secs),
            visibility_timeout: Duration::from_secs(config.visibility_timeout_secs),
            delete_message: config.delete_message,
        }
    }

    async fn run(self, mut out: Pipeline, shutdown: ShutdownSignal) -> Result<(), ()> {
        let mut interval = time::interval(self.poll_interval).map(|_| ());
        let mut shutdown = shutdown.compat();

        loop {
            select! {
                Some(()) = interval.next() => (),
                _ = &mut shutdown => break,
                else => break,
            };

            let events = self.poll_events().await;

            // Poll for messages
            // For each message:
            //   * Parse
            //   * Forward logs
            //   * Delete message (if delete_message=true)

            let (sink, _) = out
                .send_all(futures01::stream::iter_ok(logs))
                .compat()
                .await
                .map_err(|error| error!(message = "Error sending S3 Logs", %error))?;
            out = sink;
        }

        Ok(())
    }

    async fn capture_logs(&self) -> impl Iterator<Item = Event> {
        self.sqs_client
            .receive_message(ReceiveMessageRequest {
                // TODO additional parameters?
                // TODO evaluate wait_time_seconds
                queue_url: self.queue_url.clone(),
                max_number_of_messages: Some(10),
                wait_time_seconds: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        vec![].into_iter()
        // TODO:
        // Fetch SQS message(s)
        // Fetch object for each message
        // Decompress (if needed)
        // Create LogEvent, add tags
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    event_version: String, // TODO compare >=
    event_source: String,
    aws_region: Region,
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
