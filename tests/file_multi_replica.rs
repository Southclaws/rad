mod support;

use support::http_process::RadProcess;
use support::multi_replica;
use support::s3::TestResult;

#[tokio::test]
async fn one_writer_and_multiple_readers_share_a_file_database() -> TestResult {
    let directory = tempfile::tempdir()?;
    let writer = RadProcess::start_file(directory.path(), "multi-replica").await?;
    multi_replica::seed(&writer).await?;

    let reader_one = RadProcess::start_file_reader(directory.path(), "multi-replica").await?;
    let reader_two = RadProcess::start_file_reader(directory.path(), "multi-replica").await?;
    multi_replica::qualify_replicas(&writer, &reader_one, &reader_two).await?;

    reader_two.stop().await?;
    reader_one.stop().await?;
    writer.stop().await
}
