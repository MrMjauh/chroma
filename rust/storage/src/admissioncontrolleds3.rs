use super::GetError;
use crate::{
    config::StorageConfig,
    s3::{S3GetError, S3PutError, S3Storage, StorageConfigError},
    stream::ByteStreamItem,
};
use async_trait::async_trait;
use chroma_config::Configurable;
use chroma_error::{ChromaError, ErrorCodes};
use futures::{future::Shared, FutureExt, Stream, StreamExt};
use parking_lot::Mutex;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use thiserror::Error;
use tracing::{Instrument, Span};

// Wrapper over s3 storage that provides proxy features such as
// request coalescing, rate limiting, etc.
#[derive(Clone)]
pub struct AdmissionControlledS3Storage {
    storage: S3Storage,
    outstanding_requests: Arc<
        Mutex<
            HashMap<
                String,
                Shared<
                    Pin<
                        Box<
                            dyn Future<
                                    Output = Result<
                                        Arc<Vec<u8>>,
                                        AdmissionControlledS3StorageError,
                                    >,
                                > + Send
                                + 'static,
                        >,
                    >,
                >,
            >,
        >,
    >,
}

#[derive(Error, Debug, Clone)]
pub enum AdmissionControlledS3StorageError {
    #[error("Error performing a get call from s3 storage {0}")]
    S3GetError(#[from] S3GetError),
}

impl ChromaError for AdmissionControlledS3StorageError {
    fn code(&self) -> ErrorCodes {
        match self {
            AdmissionControlledS3StorageError::S3GetError(e) => e.code(),
        }
    }
}

impl AdmissionControlledS3Storage {
    pub fn new(storage: S3Storage) -> Self {
        Self {
            storage,
            outstanding_requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // TODO: Remove this once the upstream consumers switch to non-streaming APIs.
    pub async fn get_stream(
        &self,
        key: &str,
    ) -> Result<
        Box<dyn Stream<Item = ByteStreamItem> + Unpin + Send>,
        AdmissionControlledS3StorageError,
    > {
        match self
            .storage
            .get_stream(key)
            .instrument(tracing::trace_span!(parent: Span::current(), "Storage get"))
            .await
        {
            Ok(res) => Ok(res),
            Err(e) => {
                tracing::error!("Error reading from storage: {}", e);
                return Err(AdmissionControlledS3StorageError::S3GetError(e));
            }
        }
    }

    async fn read_from_storage(
        storage: S3Storage,
        key: String,
    ) -> Result<Arc<Vec<u8>>, AdmissionControlledS3StorageError> {
        let stream = storage
            .get_stream(&key)
            .instrument(tracing::trace_span!(parent: Span::current(), "Storage get"))
            .await;
        match stream {
            Ok(mut bytes) => {
                let read_block_span =
                    tracing::trace_span!(parent: Span::current(), "Read bytes to end");
                let buf = read_block_span
                    .in_scope(|| async {
                        let mut buf: Vec<u8> = Vec::new();
                        while let Some(res) = bytes.next().await {
                            match res {
                                Ok(chunk) => {
                                    buf.extend(chunk);
                                }
                                Err(err) => {
                                    tracing::error!("Error reading from storage: {}", err);
                                    match err {
                                        GetError::S3Error(e) => {
                                            return Err(
                                                AdmissionControlledS3StorageError::S3GetError(e),
                                            );
                                        }
                                        GetError::NoSuchKey(e) => {
                                            return Err(
                                                AdmissionControlledS3StorageError::S3GetError(
                                                    S3GetError::NoSuchKey(e),
                                                ),
                                            );
                                        }
                                        GetError::LocalError(_) => unreachable!(),
                                    }
                                }
                            }
                        }
                        tracing::info!("Read {:?} bytes from s3", buf.len());
                        Ok(Some(buf))
                    })
                    .await?;
                match buf {
                    Some(buf) => Ok(Arc::new(buf)),
                    None => {
                        // Buffer is empty. Nothing interesting to do.
                        Ok(Arc::new(vec![]))
                    }
                }
            }
            Err(e) => {
                tracing::error!("Error reading from storage: {}", e);
                return Err(AdmissionControlledS3StorageError::S3GetError(e));
            }
        }
    }

    pub async fn get(
        &self,
        key: String,
    ) -> Result<Arc<Vec<u8>>, AdmissionControlledS3StorageError> {
        let future_to_await;
        {
            let mut requests = self.outstanding_requests.lock();
            let maybe_inflight = requests.get(&key).map(|fut| fut.clone());
            future_to_await = match maybe_inflight {
                Some(fut) => fut,
                None => {
                    let get_storage_future = AdmissionControlledS3Storage::read_from_storage(
                        self.storage.clone(),
                        key.clone(),
                    )
                    .boxed()
                    .shared();
                    requests.insert(key.clone(), get_storage_future.clone());
                    get_storage_future
                }
            };
        }
        let res = future_to_await.await;
        {
            let mut requests = self.outstanding_requests.lock();
            requests.remove(&key);
        }
        res
    }

    pub async fn put_file(&self, key: &str, path: &str) -> Result<(), S3PutError> {
        self.storage.put_file(key, path).await
    }

    pub async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> Result<(), S3PutError> {
        self.storage.put_bytes(key, bytes).await
    }
}

#[async_trait]
impl Configurable<StorageConfig> for AdmissionControlledS3Storage {
    async fn try_from_config(config: &StorageConfig) -> Result<Self, Box<dyn ChromaError>> {
        match &config {
            StorageConfig::AdmissionControlledS3(nacconfig) => {
                let s3_storage = S3Storage::try_from_config(&StorageConfig::S3(
                    nacconfig.s3_config.clone(),
                ))
                .await?;
                return Ok(Self::new(s3_storage));
            }
            _ => {
                return Err(Box::new(StorageConfigError::InvalidStorageConfig));
            }
        }
    }
}
