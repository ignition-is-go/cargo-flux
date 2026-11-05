//! Native S3-compatible transport. Every client is built from explicit Flux
//! credentials; compiler-cache credentials and ambient cloud providers are not read.
use anyhow::{Context, Result, anyhow, ensure};
use futures_util::TryStreamExt;
use object_store::{
    ClientOptions, ObjectStoreExt, RetryConfig, aws::AmazonS3Builder, path::Path as ObjectPath,
};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{S3, configured_value};

const PART_SIZE: u64 = 8 * 1024 * 1024;

fn credential(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{name} is missing or empty"))
}

fn storage_error(operation: &str, error: impl std::fmt::Display) -> anyhow::Error {
    // Do not forward HTTP diagnostics that may contain request/auth material.
    let message = error.to_string();
    let category = [
        "AccessDenied",
        "Forbidden",
        "InvalidAccessKeyId",
        "SignatureDoesNotMatch",
        "NoSuchBucket",
        "RequestTimeTooSkewed",
        "certificate",
        "Connection refused",
    ]
    .into_iter()
    .find(|code| message.contains(code))
    .unwrap_or("storage error");
    anyhow!(
        "S3 cache {operation} failed ({category}); check endpoint, client and bucket permissions"
    )
}

pub(crate) fn transfer(config: &S3, key: &str, operation: &str, file: &Path) -> Result<bool> {
    let bucket = configured_value(&config.bucket, &config.bucket_env, "bucket")?
        .context("S3 cache requires bucket or bucket_env")?;
    ensure!(
        !bucket.is_empty() && !bucket.contains('/'),
        "invalid cache bucket"
    );
    let endpoint = configured_value(&config.endpoint, &config.endpoint_env, "endpoint")?;
    if let Some(endpoint) = &endpoint {
        ensure!(
            (endpoint.starts_with("http://") || endpoint.starts_with("https://"))
                && !endpoint.contains(['@', '?', '#']),
            "cache endpoint must not contain credentials or query parameters"
        );
    }
    let duration = Duration::from_secs(config.timeout_seconds.max(1));
    let region = std::env::var("CARGO_FLUX_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    ensure!(!region.is_empty(), "CARGO_FLUX_S3_REGION is empty");
    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(bucket)
        .with_access_key_id(credential("CARGO_FLUX_S3_ACCESS_KEY_ID")?)
        .with_secret_access_key(credential("CARGO_FLUX_S3_SECRET_ACCESS_KEY")?)
        .with_region(region)
        .with_virtual_hosted_style_request(false)
        .with_client_options(
            ClientOptions::new()
                .with_timeout_disabled()
                .with_connect_timeout(duration.min(Duration::from_secs(5))),
        )
        .with_retry(RetryConfig {
            max_retries: 2,
            retry_timeout: duration,
            ..Default::default()
        });
    if let Some(endpoint) = endpoint {
        builder = builder
            .with_allow_http(endpoint.starts_with("http://"))
            .with_endpoint(endpoint);
    }
    if let Ok(token) = std::env::var("CARGO_FLUX_S3_SESSION_TOKEN")
        && !token.is_empty()
    {
        builder = builder.with_token(token);
    }
    let location = ObjectPath::parse(format!("{}/{key}.tar", config.prefix.trim_matches('/')))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let store = builder
            .build()
            .map_err(|error| storage_error("configuration", error))?;
        let deadline = tokio::time::Instant::now() + duration;
        let expired = || {
            anyhow!(
                "S3 cache {operation} exceeded {} seconds",
                config.timeout_seconds
            )
        };
        if operation == "download" {
            return tokio::time::timeout_at(deadline, async {
                let result = match store.get(&location).await {
                    Ok(result) => result,
                    Err(object_store::Error::NotFound { .. }) => return Ok(false),
                    Err(error) => return Err(storage_error(operation, error)),
                };
                let mut output = tokio::fs::File::create(file).await?;
                let mut stream = result.into_stream();
                while let Some(bytes) = stream
                    .try_next()
                    .await
                    .map_err(|error| storage_error(operation, error))?
                {
                    output.write_all(&bytes).await?;
                }
                output.flush().await?;
                Ok(true)
            })
            .await
            .map_err(|_| expired())?;
        }

        let length = std::fs::metadata(file)?.len();
        if length <= PART_SIZE {
            return tokio::time::timeout_at(deadline, async {
                let bytes = tokio::fs::read(file).await?;
                store
                    .put(&location, bytes.into())
                    .await
                    .map_err(|error| storage_error(operation, error))?;
                Ok(true)
            })
            .await
            .map_err(|_| expired())?;
        }

        // Fixed-size parts bound memory and work with MinIO and other S3 stores.
        let mut upload = tokio::time::timeout_at(deadline, store.put_multipart(&location))
            .await
            .map_err(|_| expired())?
            .map_err(|error| storage_error(operation, error))?;
        let result = tokio::time::timeout_at(deadline, async {
            let mut input = tokio::fs::File::open(file).await?;
            let mut remaining = length;
            while remaining > 0 {
                let size = remaining.min(PART_SIZE) as usize;
                let mut bytes = vec![0; size];
                input.read_exact(&mut bytes).await?;
                upload
                    .put_part(bytes.into())
                    .await
                    .map_err(|error| storage_error(operation, error))?;
                remaining -= size as u64;
            }
            upload
                .complete()
                .await
                .map_err(|error| storage_error(operation, error))?;
            Ok::<_, anyhow::Error>(true)
        })
        .await
        .map_err(|_| expired())
        .and_then(|result| result);
        if result.is_err() {
            // Cleanup has its own short bound; never turn a failed upload into
            // a published cache entry or an unbounded teardown.
            let _ = tokio::time::timeout(Duration::from_secs(5), upload.abort()).await;
        }
        result
    })
}
