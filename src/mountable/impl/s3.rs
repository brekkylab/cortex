//! A read-oriented [`Mountable`] backend over an S3 bucket (spike).
//!
//! Object keys map to paths; a "directory" is a key prefix. Metadata/list use
//! `head`/`list_with_delimiter`; reads are ranged GETs on an open handle. The
//! `object_store` client is async, so each op blocks on an owned Tokio runtime —
//! fine because the FUSE binding drives these on its own worker threads, not a
//! Tokio thread.
//!
//! Writes are unsupported here (this is the S3-over-virtiofs read spike).

use std::io;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};

use object_store::aws::AmazonS3Builder;
use object_store::path::Path as OsPath;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt};
use tokio::runtime::Runtime;

use crate::mountable::{FileExt, FileHandle};
use crate::{CortexError, Dirent, DirentKind, Mountable, Result, Stat};

/// Connection settings for [`S3Volume`].
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Custom endpoint (MinIO / R2 / localstack); `None` for real AWS.
    pub endpoint: Option<String>,
    /// Optional key prefix all paths are rooted under.
    pub key_prefix: Option<String>,
}

/// An S3-backed volume.
pub struct S3Volume {
    store: Arc<dyn ObjectStore>,
    rt: Arc<Runtime>,
    prefix: String,
}

impl S3Volume {
    /// Build the S3 client. The runtime is created up front and shared with
    /// every open handle.
    pub fn new(cfg: &S3Config) -> Result<Self> {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(CortexError::Io)?,
        );
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region)
            .with_access_key_id(&cfg.access_key_id)
            .with_secret_access_key(&cfg.secret_access_key);
        if let Some(ep) = &cfg.endpoint {
            builder = builder.with_endpoint(ep).with_allow_http(true);
        }
        // Build inside the runtime so the async HTTP client has a reactor.
        let store = rt
            .block_on(async { builder.build() })
            .map_err(|e| CortexError::Io(io::Error::other(e)))?;
        Ok(Self {
            store: Arc::new(store),
            rt,
            prefix: cfg.key_prefix.clone().unwrap_or_default(),
        })
    }

    /// Map a request path to an S3 key string (prefix + normalized components).
    /// `..`/prefixes are rejected so a request can't escape the key prefix.
    fn key(&self, path: &Path) -> Result<String> {
        let mut parts: Vec<&str> = Vec::new();
        if !self.prefix.is_empty() {
            parts.extend(self.prefix.split('/').filter(|s| !s.is_empty()));
        }
        for comp in path.components() {
            match comp {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    parts.push(name.to_str().ok_or(CortexError::InvalidName)?)
                }
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(CortexError::InvalidName);
                }
            }
        }
        Ok(parts.join("/"))
    }

    /// `object_store::Path::parse` preserves special bytes (`%`, `#`, …) so a
    /// key surfaced by a listing round-trips to the same object on read.
    fn os_path(&self, key: &str) -> Result<OsPath> {
        OsPath::parse(key).map_err(|_| CortexError::NotFound)
    }
}

fn map_err(e: object_store::Error) -> CortexError {
    match e {
        object_store::Error::NotFound { .. } => CortexError::NotFound,
        other => CortexError::Io(io::Error::other(other)),
    }
}

impl Mountable for S3Volume {
    type Handle = S3Handle;

    fn stat(&self, path: &Path) -> Result<Stat> {
        let key = self.key(path)?;
        if key.is_empty() {
            return Ok(Stat::new(DirentKind::Dir, 0)); // bucket root
        }
        let os = self.os_path(&key)?;
        self.rt.block_on(async {
            match self.store.head(&os).await {
                Ok(meta) => {
                    let mut st = Stat::new(DirentKind::File, meta.size);
                    st.mtime = Some(meta.last_modified.into());
                    st.etag = meta.e_tag.clone();
                    st.version = meta.version.clone();
                    Ok(st)
                }
                // No object with that exact key: it may still be a prefix (dir).
                Err(object_store::Error::NotFound { .. }) => {
                    let res = self
                        .store
                        .list_with_delimiter(Some(&os))
                        .await
                        .map_err(map_err)?;
                    if res.common_prefixes.is_empty() && res.objects.is_empty() {
                        Err(CortexError::NotFound)
                    } else {
                        Ok(Stat::new(DirentKind::Dir, 0))
                    }
                }
                Err(e) => Err(map_err(e)),
            }
        })
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let key = self.key(path)?;
        let prefix = if key.is_empty() {
            None
        } else {
            Some(self.os_path(&key)?)
        };
        self.rt.block_on(async {
            let res = self
                .store
                .list_with_delimiter(prefix.as_ref())
                .await
                .map_err(map_err)?;
            // The listed prefix itself may surface as a zero-byte marker object;
            // skip it.
            let marker = prefix.as_ref().map(|p| p.as_ref()).unwrap_or("");
            let mut out = Vec::new();
            for cp in res.common_prefixes {
                if let Some(name) = cp.filename() {
                    out.push(Dirent::Dir(name.to_string()));
                }
            }
            for obj in res.objects {
                if obj.location.as_ref() == marker {
                    continue;
                }
                if let Some(name) = obj.location.filename() {
                    out.push(Dirent::File(name.to_string()));
                }
            }
            Ok(out)
        })
    }

    fn mkdir(&self, _path: &Path) -> Result<()> {
        Err(CortexError::Unsupported)
    }

    fn unlink(&self, _path: &Path) -> Result<()> {
        Err(CortexError::Unsupported)
    }

    fn open(&self, path: &Path) -> Result<Self::Handle> {
        let key = self.key(path)?;
        if key.is_empty() {
            return Err(CortexError::IsADirectory);
        }
        Ok(S3Handle {
            store: self.store.clone(),
            rt: self.rt.clone(),
            key: self.os_path(&key)?,
            chunk: Mutex::new(None),
        })
    }
}

/// Read-ahead chunk size: each backing GET fetches this many bytes and caches
/// them, so a stream of small sequential FUSE reads (the guest kernel issues
/// ~128 KiB reads even for `dd bs=1M`) is served from the buffer instead of one
/// S3 round-trip per read. This is the single biggest lever for S3 throughput —
/// it collapses ~40 serial ranged GETs for a 5 MiB object down to one.
const READAHEAD_CHUNK: u64 = 8 << 20; // 8 MiB

/// An open S3 object. Reads fetch a [`READAHEAD_CHUNK`]-sized window with a
/// ranged GET and serve subsequent in-window reads from the cached buffer;
/// writes are unsupported.
pub struct S3Handle {
    store: Arc<dyn ObjectStore>,
    rt: Arc<Runtime>,
    key: OsPath,
    /// The last fetched window as `(start_offset, bytes)`. A read whose offset
    /// falls inside it is served from memory; anything else triggers a fresh
    /// GET starting at that offset.
    chunk: Mutex<Option<(u64, Vec<u8>)>>,
}

impl FileExt for S3Handle {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut cache = self.chunk.lock().unwrap();
        // Serve from the cached window when the requested offset lands in it.
        if let Some((start, data)) = cache.as_ref() {
            if offset >= *start && offset < *start + data.len() as u64 {
                let from = (offset - *start) as usize;
                let n = (data.len() - from).min(buf.len());
                buf[..n].copy_from_slice(&data[from..from + n]);
                return Ok(n);
            }
        }
        // Miss: fetch a read-ahead window starting at `offset` and cache it.
        let end = offset + READAHEAD_CHUNK;
        let data = self.rt.block_on(async {
            let opts = GetOptions {
                range: Some(GetRange::Bounded(offset..end)),
                ..Default::default()
            };
            match self.store.get_opts(&self.key, opts).await {
                Ok(res) => Ok(res.bytes().await.map_err(io::Error::other)?.to_vec()),
                // A range at/after EOF returns 416; treat it as a clean EOF so
                // sequential readers stop cleanly.
                Err(e) => match self.store.head(&self.key).await {
                    Ok(meta) if offset >= meta.size => Ok(Vec::new()),
                    _ => Err(io::Error::other(e)),
                },
            }
        })?;
        if data.is_empty() {
            return Ok(0);
        }
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        *cache = Some((offset, data));
        Ok(n)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

impl FileHandle for S3Handle {
    fn truncate(&self, _size: u64) -> Result<()> {
        Err(CortexError::Unsupported)
    }
}
