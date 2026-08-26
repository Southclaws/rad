use std::fmt;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use futures::stream::BoxStream;
use slatedb::object_store::path::Path;
use slatedb::object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions,
    Result as ObjectStoreResult,
};

use super::telemetry::{
    PhysicalRequestClass, PhysicalRequestRecorder, PhysicalServiceTier, current_request_recorder,
};

pub(crate) fn observe_remote_object_store(inner: Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore> {
    Arc::new(RemoteObjectStore { inner })
}

struct RemoteObjectStore {
    inner: Arc<dyn ObjectStore>,
}

impl fmt::Debug for RemoteObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RemoteObjectStore")
            .field(&self.inner)
            .finish()
    }
}

impl fmt::Display for RemoteObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "observed remote {}", self.inner)
    }
}

struct RemoteRequest {
    recorder: Arc<PhysicalRequestRecorder>,
    class: PhysicalRequestClass,
    started: Instant,
    bytes: u64,
}

impl RemoteRequest {
    fn finish(self, completed: bool, error: bool) {
        self.recorder.record_request(
            PhysicalServiceTier::Remote,
            self.class,
            completed,
            error,
            self.bytes,
            self.started.elapsed(),
        );
    }
}

struct ObservedGetStream {
    inner: BoxStream<'static, ObjectStoreResult<Bytes>>,
    request: Option<RemoteRequest>,
}

impl Stream for ObservedGetStream {
    type Item = ObjectStoreResult<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(bytes))) => {
                if let Some(request) = &mut self.request {
                    request.bytes = request.bytes.saturating_add(bytes.len() as u64);
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(request) = self.request.take() {
                    request.finish(true, true);
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(request) = self.request.take() {
                    request.finish(true, false);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ObservedGetStream {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            request.finish(false, false);
        }
    }
}

fn finish_direct<T>(
    recorder: Option<Arc<PhysicalRequestRecorder>>,
    class: PhysicalRequestClass,
    started: Instant,
    bytes: u64,
    result: &ObjectStoreResult<T>,
) {
    if let Some(recorder) = recorder {
        recorder.record_request(
            PhysicalServiceTier::Remote,
            class,
            true,
            result.is_err(),
            bytes,
            started.elapsed(),
        );
    }
}

#[async_trait]
impl ObjectStore for RemoteObjectStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> ObjectStoreResult<PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> ObjectStoreResult<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> ObjectStoreResult<GetResult> {
        let class = if options.head {
            PhysicalRequestClass::MetadataRead
        } else if options.range.is_some() {
            PhysicalRequestClass::RangeRead
        } else {
            PhysicalRequestClass::Read
        };
        let recorder = current_request_recorder();
        let started = Instant::now();
        let result = self.inner.get_opts(location, options).await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if let Some(recorder) = recorder {
                    recorder.record_request(
                        PhysicalServiceTier::Remote,
                        class,
                        true,
                        true,
                        0,
                        started.elapsed(),
                    );
                }
                return Err(error);
            }
        };
        let Some(recorder) = recorder else {
            return Ok(result);
        };
        let GetResult {
            payload,
            meta,
            range,
            attributes,
            extensions,
        } = result;
        let payload = match payload {
            GetResultPayload::Stream(inner) => {
                GetResultPayload::Stream(Box::pin(ObservedGetStream {
                    inner,
                    request: Some(RemoteRequest {
                        recorder,
                        class,
                        started,
                        bytes: 0,
                    }),
                }))
            }
            #[cfg(not(target_arch = "wasm32"))]
            GetResultPayload::File(file, path) => {
                recorder.record_request(
                    PhysicalServiceTier::Remote,
                    class,
                    true,
                    false,
                    range.end.saturating_sub(range.start),
                    started.elapsed(),
                );
                GetResultPayload::File(file, path)
            }
        };
        Ok(GetResult {
            payload,
            meta,
            range,
            attributes,
            extensions,
        })
    }

    async fn get_ranges(
        &self,
        location: &Path,
        ranges: &[Range<u64>],
    ) -> ObjectStoreResult<Vec<Bytes>> {
        let recorder = current_request_recorder();
        let started = Instant::now();
        let result = self.inner.get_ranges(location, ranges).await;
        let bytes = result
            .as_ref()
            .map(|parts| parts.iter().map(|part| part.len() as u64).sum())
            .unwrap_or_default();
        finish_direct(
            recorder,
            PhysicalRequestClass::RangeRead,
            started,
            bytes,
            &result,
        );
        result
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, ObjectStoreResult<Path>>,
    ) -> BoxStream<'static, ObjectStoreResult<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, ObjectStoreResult<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, ObjectStoreResult<ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> ObjectStoreResult<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> ObjectStoreResult<()> {
        self.inner.copy_opts(from, to, options).await
    }

    async fn rename_opts(
        &self,
        from: &Path,
        to: &Path,
        options: RenameOptions,
    ) -> ObjectStoreResult<()> {
        self.inner.rename_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use slatedb::object_store::ObjectStoreExt as _;
    use slatedb::object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn remote_reads_are_attributed_after_the_body_finishes() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let location = Path::from("object");
        inner
            .put(&location, Bytes::from_static(b"payload").into())
            .await
            .unwrap();
        let observed = observe_remote_object_store(inner);

        let (bytes, trace) = super::super::telemetry::observe_request("slatedb", async {
            observed
                .get(&location)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        })
        .await;

        assert_eq!(bytes, Bytes::from_static(b"payload"));
        assert_eq!(trace.requests.len(), 1);
        assert_eq!(trace.coverage, "cache_and_backing_reads");
        assert_eq!(trace.requests[0].service_tier, PhysicalServiceTier::Remote);
        assert_eq!(trace.requests[0].class, PhysicalRequestClass::Read);
        assert_eq!(trace.requests[0].requests, 1);
        assert_eq!(trace.requests[0].completed, 1);
        assert_eq!(trace.requests[0].errors, 0);
        assert_eq!(trace.requests[0].bytes, 7);
    }
}
