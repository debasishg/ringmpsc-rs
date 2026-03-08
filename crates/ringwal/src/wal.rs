//! WAL engine — background flusher, segment management, group commit.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;

use crate::config::WalConfig;
use crate::config::SyncMode;
use crate::error::WalError;
use crate::invariants::debug_assert_commit_durable;
use crate::recovery;
use crate::segment::SegmentManager;
use crate::writer::{Envelope, WalWriterFactory};
use ringmpsc_rs::Config as RingConfig;
use ringmpsc_stream::{channel_with_stream_config, RingReceiver, StreamConfig, StreamExt};

/// Shared state used by the flusher to notify commit waiters.
pub(crate) struct CommitRegistry {
    _private: (),
}

impl CommitRegistry {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

/// The WAL engine.
///
/// Owns the background flusher task and the segment manager.
/// Created via [`Wal::open`], which returns both the `Wal` handle
/// and a [`WalWriterFactory`] for creating writers.
pub struct Wal {
    dir: PathBuf,
    shutdown_notify: Arc<Notify>,
    flusher_handle: Option<JoinHandle<()>>,
    checkpoint_handle: Option<JoinHandle<()>>,
    next_lsn: Arc<AtomicU64>,
}

impl Wal {
    /// Opens or creates a WAL at the configured directory.
    ///
    /// Spawns a background flusher task on the tokio runtime.
    /// Returns the `Wal` handle and a `WalWriterFactory` for
    /// registering per-writer ring buffers.
    pub fn open<K, V>(config: WalConfig) -> Result<(Self, WalWriterFactory<K, V>), WalError>
    where
        K: Serialize + DeserializeOwned + Send + 'static,
        V: Serialize + DeserializeOwned + Send + 'static,
    {
        let ring_config = RingConfig::new(
            config.ring_bits,
            config.max_writers,
            config.enable_metrics,
        );
        let stream_config = StreamConfig::default()
            .with_poll_interval(config.flush_interval)
            .with_batch_hint(config.batch_hint);

        let (sender_factory, receiver) =
            channel_with_stream_config::<Envelope<K, V>>(ring_config, stream_config);

        let segment_mgr = SegmentManager::open(&config.dir, config.max_segment_size)?;

        let shutdown_notify = Arc::new(Notify::new());
        let next_lsn = Arc::new(AtomicU64::new(1));
        let commit_registry = Arc::new(CommitRegistry::new());

        let flusher_handle = {
            let shutdown_notify = Arc::clone(&shutdown_notify);
            let next_lsn = Arc::clone(&next_lsn);
            tokio::spawn(flusher_task(
                receiver,
                segment_mgr,
                shutdown_notify,
                next_lsn,
                config.batch_hint,
                config.sync_mode,
            ))
        };

        let writer_factory = WalWriterFactory::new(sender_factory, commit_registry);

        Ok((
            Self {
                dir: config.dir.clone(),
                shutdown_notify,
                flusher_handle: Some(flusher_handle),
                checkpoint_handle: None,
                next_lsn,
            },
            writer_factory,
        ))
    }

    /// Returns the current (next-to-assign) LSN.
    pub fn current_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::Relaxed)
    }

    /// Initiates graceful shutdown.
    ///
    /// Signals the flusher to drain remaining entries and stop.
    /// Also cancels the checkpoint scheduler if running.
    /// Awaits until all background tasks complete.
    pub async fn shutdown(&mut self) -> Result<(), WalError> {
        // Cancel the checkpoint scheduler first (best-effort, idempotent)
        if let Some(handle) = self.checkpoint_handle.take() {
            handle.abort();
            let _ = handle.await; // ignore JoinError from abort
        }
        // Signal the flusher to drain and stop (notify_one stores a permit
        // so the flusher will see it even if not currently polled)
        self.shutdown_notify.notify_one();
        if let Some(handle) = self.flusher_handle.take() {
            handle.await.map_err(|_| {
                WalError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "flusher task panicked",
                ))
            })?;
        }
        Ok(())
    }

    /// Returns the WAL directory path.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Advances the checkpoint to the highest committed transaction.
    ///
    /// Scans all segment files, finds the highest committed `tx_id`,
    /// writes a checkpoint file, and removes old segment files whose
    /// entries are all below the new checkpoint.
    ///
    /// Returns the new checkpoint LSN, or `WalError::NoNewCheckpoints`
    /// if no new committed transactions exist beyond the current checkpoint.
    pub fn checkpoint<K, V>(&self) -> Result<u64, WalError>
    where
        K: DeserializeOwned + Send + 'static,
        V: DeserializeOwned + Send + 'static,
    {
        let lsn = recovery::checkpoint::<K, V>(&self.dir)?;
        let _ = recovery::truncate_segments_before(&self.dir, lsn);
        Ok(lsn)
    }

    /// Starts a background task that periodically checkpoints committed
    /// transactions and truncates old segment files.
    ///
    /// The scheduler runs until `shutdown()` is called. Only one scheduler
    /// can be active at a time; calling this again replaces the previous one.
    ///
    /// This matches async-wal-db's `start_checkpoint_scheduler(interval_secs)`.
    pub fn start_checkpoint_scheduler<K, V>(&mut self, interval: Duration)
    where
        K: DeserializeOwned + Send + 'static,
        V: DeserializeOwned + Send + 'static,
    {
        // Cancel existing scheduler if any
        if let Some(handle) = self.checkpoint_handle.take() {
            handle.abort();
        }

        let dir = self.dir.clone();
        let shutdown_notify = Arc::clone(&self.shutdown_notify);

        self.checkpoint_handle = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick is immediate, skip it

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // Best-effort checkpoint — ignore NoNewCheckpoints
                        match recovery::checkpoint::<K, V>(&dir) {
                            Ok(lsn) => {
                                let _ = recovery::truncate_segments_before(&dir, lsn);
                            }
                            Err(WalError::NoNewCheckpoints) => {}
                            Err(e) => {
                                eprintln!("ringwal: checkpoint scheduler error: {e}");
                            }
                        }
                    }
                    _ = shutdown_notify.notified() => {
                        return;
                    }
                }
            }
        }));
    }
}

/// Background task: drains entries from the ring receiver, serialises them,
/// writes to the segment manager, fsyncs, and notifies commit waiters.
async fn flusher_task<K, V>(
    mut receiver: RingReceiver<Envelope<K, V>>,
    mut segment_mgr: SegmentManager,
    shutdown_notify: Arc<Notify>,
    next_lsn: Arc<AtomicU64>,
    batch_hint: usize,
    sync_mode: SyncMode,
) where
    K: Serialize + DeserializeOwned + Send + 'static,
    V: Serialize + DeserializeOwned + Send + 'static,
{
    let mut batch: Vec<(u64, Vec<u8>)> = Vec::with_capacity(batch_hint);
    let mut commit_waiters: Vec<oneshot::Sender<()>> = Vec::new();

    loop {
        let mut drained = 0usize;

        // Accumulate a batch: block on first item, then drain opportunistically
        while drained < batch_hint {
            let maybe_item = if drained == 0 {
                // First item: block until data arrives OR shutdown is signalled
                tokio::select! {
                    item = receiver.next() => item,
                    _ = shutdown_notify.notified() => {
                        // Shutdown requested — trigger receiver shutdown to drain
                        receiver.shutdown();
                        // Drain remaining items from the stream
                        while let Some(envelope) = receiver.next().await {
                            collect_envelope(envelope, &next_lsn, &mut batch, &mut commit_waiters);
                        }
                        flush_and_notify(&mut segment_mgr, &mut batch, &mut commit_waiters, sync_mode).await;
                        return;
                    }
                }
            } else {
                // Subsequent items: try to fill the batch without blocking long
                tokio::select! {
                    biased;
                    item = receiver.next() => item,
                    _ = tokio::time::sleep(std::time::Duration::from_micros(100)) => break,
                }
            };

            match maybe_item {
                Some(envelope) => {
                    collect_envelope(envelope, &next_lsn, &mut batch, &mut commit_waiters);
                    drained += 1;
                }
                None => {
                    // Stream ended (all senders dropped)
                    flush_and_notify(&mut segment_mgr, &mut batch, &mut commit_waiters, sync_mode).await;
                    return;
                }
            }
        }

        if !batch.is_empty() {
            flush_and_notify(&mut segment_mgr, &mut batch, &mut commit_waiters, sync_mode).await;
        }
    }
}

/// Extracts an envelope into the batch and commit waiters.
fn collect_envelope<K, V>(
    envelope: Envelope<K, V>,
    next_lsn: &AtomicU64,
    batch: &mut Vec<(u64, Vec<u8>)>,
    commit_waiters: &mut Vec<oneshot::Sender<()>>,
) where
    K: Serialize,
    V: Serialize,
{
    let lsn = next_lsn.fetch_add(1, Ordering::Relaxed);
    match envelope {
        Envelope::Entry(entry) => {
            if let Ok(data) = bincode::serialize(&entry) {
                batch.push((lsn, data));
            }
        }
        Envelope::CommitBarrier { entry, tx } => {
            if let Ok(data) = bincode::serialize(&entry) {
                batch.push((lsn, data));
            }
            commit_waiters.push(tx);
        }
    }
}

/// Writes a batch to the segment manager, optionally fsyncs, and notifies commit waiters.
async fn flush_and_notify(
    segment_mgr: &mut SegmentManager,
    batch: &mut Vec<(u64, Vec<u8>)>,
    commit_waiters: &mut Vec<oneshot::Sender<()>>,
    sync_mode: SyncMode,
) {
    if batch.is_empty() {
        return;
    }

    // Write batch to active segment (may rotate)
    if let Err(e) = segment_mgr.write_batch(batch) {
        eprintln!("ringwal: segment write error: {e}");
        batch.clear();
        commit_waiters.clear();
        return;
    }

    match sync_mode {
        SyncMode::Full => {
            // Fsync the active segment — guarantees durability
            let _fsynced = segment_mgr.fsync().is_ok();
            debug_assert_commit_durable!(_fsynced);
        }
        SyncMode::DataOnly => {
            // Fsync data only (skip metadata) — faster on Linux/ext4
            let _fsynced = segment_mgr.fsync_data().is_ok();
            debug_assert_commit_durable!(_fsynced);
        }
        SyncMode::Background => {
            // Offload fsync to Tokio's blocking thread pool.
            // flush_and_clone_fd() flushes the BufWriter and dup()s the fd;
            // sync_all() on the clone syncs the same inode.
            match segment_mgr.flush_and_clone_fd() {
                Ok(fd) => {
                    let result = tokio::task::spawn_blocking(move || fd.sync_all())
                        .await;
                    let _fsynced = matches!(result, Ok(Ok(())));
                    debug_assert_commit_durable!(_fsynced);
                }
                Err(e) => {
                    eprintln!("ringwal: flush_and_clone_fd error: {e}");
                }
            }
        }
        SyncMode::None => {
            // Flush BufWriter to kernel buffer but skip fsync —
            // faster but data may be lost on crash.
            let _ = segment_mgr.flush();
        }
    }

    // Notify all commit waiters — group commit
    for waiter in commit_waiters.drain(..) {
        let _ = waiter.send(());
    }

    batch.clear();
}
