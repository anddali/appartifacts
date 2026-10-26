use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use axum::body::Bytes;
use std::path::PathBuf;

use crate::config::StorageConfig;

/// Blob storage for artifact files: S3 in production, a local directory for development.
pub enum Storage {
    S3 { client: aws_sdk_s3::Client, bucket: String, prefix: String },
    Fs { root: PathBuf },
}

impl Storage {
    pub async fn from_config(cfg: &StorageConfig) -> Result<Self> {
        Ok(match cfg {
            StorageConfig::S3 { bucket, prefix, force_path_style } => {
                let shared = aws_config::load_from_env().await;
                let conf = aws_sdk_s3::config::Builder::from(&shared).force_path_style(*force_path_style).build();
                Storage::S3 { client: aws_sdk_s3::Client::from_conf(conf), bucket: bucket.clone(), prefix: prefix.clone() }
            }
            StorageConfig::Fs { root } => {
                tokio::fs::create_dir_all(root).await.with_context(|| format!("creating {root}"))?;
                Storage::Fs { root: PathBuf::from(root) }
            }
        })
    }

    fn s3_key(prefix: &str, key: &str) -> String {
        if prefix.is_empty() { key.to_string() } else { format!("{prefix}/{key}") }
    }

    pub async fn put(&self, key: &str, data: Bytes, content_type: &str) -> Result<()> {
        match self {
            Storage::S3 { client, bucket, prefix } => {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(Self::s3_key(prefix, key))
                    .content_type(content_type)
                    .body(ByteStream::from(data))
                    .send()
                    .await
                    .with_context(|| format!("s3 put {key}"))?;
            }
            Storage::Fs { root } => {
                let path = root.join(key);
                if let Some(dir) = path.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                tokio::fs::write(&path, &data).await.with_context(|| format!("writing {}", path.display()))?;
            }
        }
        Ok(())
    }

    pub async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        match self {
            Storage::S3 { client, bucket, prefix } => {
                match client.get_object().bucket(bucket).key(Self::s3_key(prefix, key)).send().await {
                    Ok(out) => Ok(Some(out.body.collect().await.context("reading s3 body")?.into_bytes())),
                    Err(e) => {
                        let svc = e.into_service_error();
                        if svc.is_no_such_key() { Ok(None) } else { Err(anyhow::Error::new(svc).context(format!("s3 get {key}"))) }
                    }
                }
            }
            Storage::Fs { root } => match tokio::fs::read(root.join(key)).await {
                Ok(b) => Ok(Some(Bytes::from(b))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
        }
    }

    pub async fn delete_prefix(&self, key_prefix: &str) -> Result<()> {
        match self {
            Storage::S3 { client, bucket, prefix } => {
                let full = Self::s3_key(prefix, key_prefix);
                let mut token = None;
                loop {
                    let page = client
                        .list_objects_v2()
                        .bucket(bucket)
                        .prefix(&full)
                        .set_continuation_token(token.take())
                        .send()
                        .await
                        .context("s3 list")?;
                    let ids = page
                        .contents()
                        .iter()
                        .filter_map(|o| o.key())
                        .map(|k| ObjectIdentifier::builder().key(k).build())
                        .collect::<Result<Vec<_>, _>>()?;
                    if !ids.is_empty() {
                        client
                            .delete_objects()
                            .bucket(bucket)
                            .delete(Delete::builder().set_objects(Some(ids)).quiet(true).build()?)
                            .send()
                            .await
                            .context("s3 delete")?;
                    }
                    match page.next_continuation_token() {
                        Some(t) => token = Some(t.to_string()),
                        None => break,
                    }
                }
            }
            Storage::Fs { root } => match tokio::fs::remove_dir_all(root.join(key_prefix)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }
}
