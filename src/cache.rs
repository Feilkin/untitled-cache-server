//! TLRU cache :)
use std::borrow::Borrow;
use std::future::Future;
use std::hash::{BuildHasher, Hash, Hasher};
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use dashmap::mapref::entry::{Entry, VacantEntry};
use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Error, Clone, Debug)]
pub enum CacheError {
    #[error("failed to fetch resource")]
    Fetch(#[from] Arc<dyn std::error::Error + Send + Sync>),
    #[error("unexpected error")]
    Unknown,
}

pub type CacheResult = Result<(Bytes, Instant), CacheError>;

type Watcher = watch::Receiver<Option<CacheResult>>;

enum CacheEntry {
    Cached {
        /// The cached bytes
        data: Bytes,
        /// Time last accessed
        last_accessed: Instant,
        /// Expiration time
        expires: Instant,
    },
    Fetching(Watcher),
}

pub struct TlruCache {
    /// Maximum allowed size in bytes.
    max_size: usize,
    /// Currently used memory.
    current_size: AtomicUsize,
    cache: DashMap<u64, CacheEntry>,
    // diagnostics
    hits: AtomicUsize,
    misses: AtomicUsize,
}

impl TlruCache {
    pub fn new(max_size: usize) -> Self {
        TlruCache {
            max_size,

            current_size: Default::default(),
            cache: Default::default(),
            hits: Default::default(),
            misses: Default::default(),
        }
    }

    pub async fn get_or_fetch<F, R>(&self, resource: &str, fetch_function: F) -> CacheResult
    where
        F: FnOnce() -> R + Send + Sync + 'static,
        R: Future<Output = CacheResult> + Send + Sync,
    {
        // double hashing the key :)
        let mut hasher = self.cache.hasher().build_hasher();
        resource.hash(&mut hasher);
        let hash = hasher.finish();

        // First, check if the resource is currently in the cache or being fetched, if not spawn
        // a fetch task.
        let (maybe_entry, fetch_task) = match self.cache.entry(hash.clone()) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                CacheEntry::Cached { expires, .. } if *expires <= Instant::now() => {
                    let (fetch_handle, watcher) = fetch_entry(fetch_function);
                    entry.insert(CacheEntry::Fetching(watcher));

                    (None, Some(fetch_handle))
                }
                CacheEntry::Cached {
                    data,
                    last_accessed,
                    expires,
                } => {
                    *last_accessed = Instant::now();

                    (
                        Some(CacheEntry::Cached {
                            data: data.clone(),
                            last_accessed: last_accessed.clone(),
                            expires: expires.clone(),
                        }),
                        None,
                    )
                }
                CacheEntry::Fetching(watcher) => {
                    (Some(CacheEntry::Fetching(watcher.clone())), None)
                }
            },
            Entry::Vacant(entry) => {
                let (fetch_handle, watcher) = fetch_entry(fetch_function);
                entry.insert(CacheEntry::Fetching(watcher));

                (None, Some(fetch_handle))
            }
        };

        if let Some(entry) = maybe_entry {
            // resource was in cache, or being fetched to the cache
            match entry {
                CacheEntry::Cached { data, expires, .. } => Ok((data, expires)),
                CacheEntry::Fetching(mut watcher) => loop {
                    if let Some(result) = watcher.borrow_and_update().deref() {
                        break result.clone();
                    }

                    if let Err(_) = watcher.changed().await {
                        // TODO: log error
                        break Err(CacheError::Unknown);
                    }
                },
            }
        } else {
            let task_handle = fetch_task.expect("should have either result or fetch task");

            match task_handle.await {
                Ok(result) => {
                    match &result {
                        Ok((data, expires)) => {
                            self.make_fit(data.len());

                            self.cache.insert(
                                hash,
                                CacheEntry::Cached {
                                    data: data.clone(),
                                    last_accessed: Instant::now(),
                                    expires: expires.clone(),
                                },
                            );

                            // TODO: figure out the best ordering
                            self.current_size.fetch_add(data.len(), Ordering::SeqCst);
                        }
                        Err(_) => {
                            self.cache.remove(&hash);
                        }
                    }

                    result
                }
                Err(_) => {
                    self.cache.remove(&hash);

                    Err(CacheError::Unknown)
                }
            }
        }
    }
}

fn fetch_entry<F, R>(fetch_function: F) -> (JoinHandle<CacheResult>, Watcher)
where
    F: FnOnce() -> R + Send + Sync + 'static,
    R: Future<Output = CacheResult> + Send + Sync,
{
    let (sender, watcher) = watch::channel(None);

    let join_handle = tokio::task::spawn(async move {
        let result = fetch_function().await;

        sender.send(Some(result.clone()));

        result
    });
    (join_handle, watcher)
}

impl TlruCache {
    /// Evict entries until there is room for required number of bytes
    fn make_fit(&self, bytes: usize) {
        let current_size = self.current_size.load(Ordering::SeqCst);
        if current_size + bytes >= self.max_size {
            let mut to_remove = (current_size + bytes - self.max_size) as isize;
            println!("freeing {} bytes", to_remove);

            while to_remove > 0 {
                to_remove - self.evict_one() as isize;
            }
        }
    }

    /// Evict one entry from the cache, returning how many bytes were freed
    fn evict_one(&self) -> usize {
        let mut lowest = (*self.cache.iter().next().unwrap().key(), Instant::now(), 0);

        for entry in self.cache.iter() {
            let key = *entry.key();
            match entry.borrow().deref() {
                CacheEntry::Cached {
                    last_accessed,
                    data,
                    ..
                } => {
                    if last_accessed <= &lowest.1 {
                        lowest = (key, last_accessed.clone(), data.len());
                    }
                }
                CacheEntry::Fetching(_) => {}
            }
        }

        self.cache
            .remove(&lowest.0)
            .expect("cache entry disappeared");

        self.current_size.fetch_sub(lowest.2, Ordering::SeqCst);

        lowest.2
    }
}
