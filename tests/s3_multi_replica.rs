use uuid::Uuid;

mod support;

use support::http_process::RadProcess;
use support::multi_replica;
use support::s3::{RustFs, TestResult};

#[tokio::test]
#[ignore = "requires a Docker daemon"]
async fn one_writer_and_multiple_readers_share_an_s3_database() -> TestResult {
    let rustfs = RustFs::start_or_external().await?;
    let prefix = format!("multi-replica-{}", Uuid::new_v4().simple());
    let writer = RadProcess::start_s3(&rustfs.config, &rustfs.config.endpoint, &prefix).await?;
    multi_replica::seed(&writer).await?;

    let reader_one =
        RadProcess::start_s3_reader(&rustfs.config, &rustfs.config.endpoint, &prefix).await?;
    let reader_two =
        RadProcess::start_s3_reader(&rustfs.config, &rustfs.config.endpoint, &prefix).await?;
    multi_replica::qualify_replicas(&writer, &reader_one, &reader_two).await?;

    reader_two.stop().await?;
    reader_one.stop().await?;
    writer.stop().await
}
