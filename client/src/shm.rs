use {
    futures::stream::BoxStream,
    memmap2::{MmapOptions, MmapRaw},
    richat_shared::transports::shm::{
        HEADER_SIZE, INDEX_ENTRY_SIZE, MAGIC, ShmHeader, ShmIndexEntry, ShmIndexEntryV2, VERSION,
        VERSION_V2, copy_from_ring,
    },
    serde::Deserialize,
    std::{
        fs::File,
        io,
        path::PathBuf,
        sync::atomic::{Ordering, fence},
    },
    thiserror::Error,
};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigShmClient {
    #[serde(default = "ConfigShmClient::default_path")]
    pub path: PathBuf,
    pub affinity: Option<usize>,
}

impl ConfigShmClient {
    fn default_path() -> PathBuf {
        PathBuf::from("/dev/shm/richat")
    }
}

#[derive(Debug, Error)]
pub enum ShmConnectError {
    #[error("failed to open shm file: {0}")]
    Open(io::Error),
    #[error("failed to mmap shm file: {0}")]
    Mmap(io::Error),
    #[error("invalid magic: expected {expected:#x}, got {got:#x}")]
    InvalidMagic { expected: u64, got: u64 },
    #[error("unsupported version: {0}")]
    UnsupportedVersion(u64),
    #[error("file too small")]
    FileTooSmall,
}

#[derive(Debug, Error)]
pub enum ShmReceiveError {
    #[error("reader lagged behind writer")]
    Lagged,
    #[error("writer closed")]
    Closed,
}

enum ReadResult {
    Data(Vec<u8>),
    Lagged,
    Closed,
    WouldBlock,
}

enum ReadResultV2 {
    Data(ShmEntryMeta, Vec<u8>),
    Skipped,
    Lagged,
    Closed,
    WouldBlock,
}

/// Metadata extracted from a V2 index entry.
/// For V1, all fields except data are zero-initialized.
#[derive(Debug, Clone)]
pub struct ShmEntryMeta {
    pub msg_type: u8,
    pub flags: u8,
    pub slot: u64,
    pub meta: [u8; 96],
}

impl Default for ShmEntryMeta {
    fn default() -> Self {
        Self {
            msg_type: 0,
            flags: 0,
            slot: 0,
            meta: [0u8; 96],
        }
    }
}

pub struct ShmSubscription {
    mmap: MmapRaw,
    idx_mask: u64,
    data_capacity: u64,
    index_base_offset: usize,
    data_base_offset: usize,
    entry_size: usize,
    version: u64,
    affinity: Option<usize>,
}

impl ShmSubscription {
    pub fn open(config: &ConfigShmClient) -> Result<Self, ShmConnectError> {
        let file = File::open(&config.path).map_err(ShmConnectError::Open)?;

        let file_len = file
            .metadata()
            .map_err(ShmConnectError::Open)?
            .len() as usize;

        if file_len < HEADER_SIZE {
            return Err(ShmConnectError::FileTooSmall);
        }

        let mmap = MmapOptions::new()
            .map_raw_read_only(&file)
            .map_err(ShmConnectError::Mmap)?;

        // Validate header
        let ptr = mmap.as_ptr();
        // SAFETY: we checked file_len >= HEADER_SIZE
        let header = unsafe { ShmHeader::from_ptr(ptr) };

        if header.magic != MAGIC {
            return Err(ShmConnectError::InvalidMagic {
                expected: MAGIC,
                got: header.magic,
            });
        }

        let version = header.version;
        if version < VERSION || version > VERSION_V2 {
            return Err(ShmConnectError::UnsupportedVersion(version));
        }

        let idx_capacity = header.idx_capacity;
        let data_capacity = header.data_capacity;
        let idx_mask = idx_capacity - 1;

        let entry_size = if version >= VERSION_V2 {
            header.index_entry_size as usize
        } else {
            INDEX_ENTRY_SIZE
        };

        let expected_size =
            HEADER_SIZE + (idx_capacity as usize) * entry_size + data_capacity as usize;
        if file_len < expected_size {
            return Err(ShmConnectError::FileTooSmall);
        }

        let index_base_offset = HEADER_SIZE;
        let data_base_offset = HEADER_SIZE + (idx_capacity as usize) * entry_size;

        Ok(Self {
            mmap,
            idx_mask,
            data_capacity,
            index_base_offset,
            data_base_offset,
            entry_size,
            version,
            affinity: config.affinity,
        })
    }

    /// Returns a stream of messages read from shared memory.
    /// Internally spawns a blocking thread that polls the ring buffer.
    pub fn subscribe(self) -> BoxStream<'static, Result<Vec<u8>, ShmReceiveError>> {
        let (tx, rx) = kanal::bounded(4096);
        let affinity = self.affinity;

        std::thread::Builder::new()
            .name("shm-reader".to_owned())
            .spawn(move || {
                if let Some(cpu) = affinity {
                    if let Err(e) = affinity_linux::set_thread_affinity([cpu].into_iter()) {
                        tracing::warn!("failed to set shm-reader affinity: {e}");
                    }
                }
                self.read_loop(tx);
            })
            .expect("failed to spawn shm reader thread");

        Box::pin(futures::stream::unfold(
            rx.to_async(),
            |rx| async move {
                match rx.recv().await {
                    Ok(msg) => Some((msg, rx)),
                    Err(_) => None,
                }
            },
        ))
    }

    /// Subscribe with an inline transform that runs in the reader thread.
    /// Eliminates a channel hop by processing data directly in the reader.
    pub fn subscribe_map<F, T>(
        self,
        channel_size: usize,
        transform: F,
    ) -> kanal::Receiver<T>
    where
        F: Fn(Vec<u8>) -> Option<T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = kanal::bounded(channel_size);
        let affinity = self.affinity;

        std::thread::Builder::new()
            .name("shm-reader".to_owned())
            .spawn(move || {
                if let Some(cpu) = affinity {
                    if let Err(e) = affinity_linux::set_thread_affinity([cpu].into_iter()) {
                        tracing::warn!("failed to set shm-reader affinity: {e}");
                    }
                }
                self.read_loop_map(tx, transform);
            })
            .expect("failed to spawn shm reader thread");

        rx
    }

    /// Attempt to read the next entry from the ring buffer (V1 path).
    #[inline(always)]
    fn try_read_next(&self, next_pos: u64) -> ReadResult {
        let ptr = self.mmap.as_ptr();
        // SAFETY: ptr points to valid header
        let header = unsafe { ShmHeader::from_ptr(ptr) };

        let tail = header.tail.load(Ordering::Acquire);
        if next_pos >= tail {
            if header.closed.load(Ordering::Acquire) != 0 {
                return ReadResult::Closed;
            }
            return ReadResult::WouldBlock;
        }

        let idx = (next_pos & self.idx_mask) as usize;
        // SAFETY: idx < idx_capacity, offset is within bounds
        let entry = unsafe {
            ShmIndexEntry::from_ptr(
                ptr.add(self.index_base_offset + idx * self.entry_size),
            )
        };
        let seq = entry.seq.load(Ordering::Acquire);
        if seq != next_pos as i64 {
            return ReadResult::Lagged;
        }

        let data_off = entry.data_off;
        let data_len = entry.data_len as usize;

        let current_data_tail = header.data_tail.load(Ordering::Acquire);
        if current_data_tail - data_off > self.data_capacity {
            return ReadResult::Lagged;
        }

        let ring_offset = (data_off % self.data_capacity) as usize;
        // SAFETY: data_base_offset + data_capacity is within mmap bounds
        let data_ring = unsafe {
            std::slice::from_raw_parts(
                ptr.add(self.data_base_offset),
                self.data_capacity as usize,
            )
        };
        // SAFETY: buf will be fully overwritten by copy_from_ring
        let mut buf = Vec::with_capacity(data_len);
        unsafe { buf.set_len(data_len) };
        copy_from_ring(data_ring, ring_offset, data_len, &mut buf);

        // Double-check: data was not overwritten during read
        fence(Ordering::Acquire);
        if entry.seq.load(Ordering::Relaxed) != next_pos as i64 {
            return ReadResult::Lagged;
        }
        if header.data_tail.load(Ordering::Relaxed) - data_off > self.data_capacity {
            return ReadResult::Lagged;
        }

        ReadResult::Data(buf)
    }

    /// Attempt to read the next V2 entry. Returns metadata first, then data.
    /// The pre_filter is called with the metadata; if it returns false, the data
    /// is not read from the ring buffer.
    #[inline(always)]
    fn try_read_next_v2<P>(
        &self,
        next_pos: u64,
        pre_filter: &P,
    ) -> ReadResultV2
    where
        P: Fn(&ShmEntryMeta) -> bool,
    {
        let ptr = self.mmap.as_ptr();
        let header = unsafe { ShmHeader::from_ptr(ptr) };

        let tail = header.tail.load(Ordering::Acquire);
        if next_pos >= tail {
            if header.closed.load(Ordering::Acquire) != 0 {
                return ReadResultV2::Closed;
            }
            return ReadResultV2::WouldBlock;
        }

        let idx = (next_pos & self.idx_mask) as usize;
        let entry = unsafe {
            ShmIndexEntryV2::from_ptr(
                ptr.add(self.index_base_offset + idx * self.entry_size),
            )
        };
        let seq = entry.seq.load(Ordering::Acquire);
        if seq != next_pos as i64 {
            return ReadResultV2::Lagged;
        }

        // Extract metadata from the index entry
        let meta = ShmEntryMeta {
            msg_type: entry.msg_type,
            flags: entry.flags,
            slot: entry.slot,
            meta: entry.meta,
        };

        // Pre-filter: skip reading data if the filter rejects this entry
        if !pre_filter(&meta) {
            return ReadResultV2::Skipped;
        }

        let data_off = entry.data_off;
        let data_len = entry.data_len as usize;

        let current_data_tail = header.data_tail.load(Ordering::Acquire);
        if current_data_tail - data_off > self.data_capacity {
            return ReadResultV2::Lagged;
        }

        let ring_offset = (data_off % self.data_capacity) as usize;
        let data_ring = unsafe {
            std::slice::from_raw_parts(
                ptr.add(self.data_base_offset),
                self.data_capacity as usize,
            )
        };
        let mut buf = Vec::with_capacity(data_len);
        unsafe { buf.set_len(data_len) };
        copy_from_ring(data_ring, ring_offset, data_len, &mut buf);

        // Double-check: data was not overwritten during read
        fence(Ordering::Acquire);
        if entry.seq.load(Ordering::Relaxed) != next_pos as i64 {
            return ReadResultV2::Lagged;
        }
        if header.data_tail.load(Ordering::Relaxed) - data_off > self.data_capacity {
            return ReadResultV2::Lagged;
        }

        ReadResultV2::Data(meta, buf)
    }

    /// Adaptive backoff: spin → yield → sleep.
    /// Resets to zero when data arrives.
    #[inline]
    fn backoff_wait(step: &mut u32) {
        if *step < 7 {
            // Spin phase: 1,2,4,8,16,32,64 PAUSE iterations (~5μs total)
            for _ in 0..(1u32 << *step) {
                std::hint::spin_loop();
            }
        } else if *step < 11 {
            // Yield phase: 4 iterations
            std::thread::yield_now();
        } else {
            // Sleep phase: release CPU, ~1-50μs actual granularity
            std::thread::sleep(std::time::Duration::from_micros(1));
        }
        *step = (*step + 1).min(12);
    }

    fn read_loop(self, tx: kanal::Sender<Result<Vec<u8>, ShmReceiveError>>) {
        let header = unsafe { ShmHeader::from_ptr(self.mmap.as_ptr()) };
        let mut next_pos = header.tail.load(Ordering::Acquire);
        let mut backoff = 0u32;

        loop {
            match self.try_read_next(next_pos) {
                ReadResult::Data(buf) => {
                    backoff = 0;
                    if tx.send(Ok(buf)).is_err() {
                        return;
                    }
                    next_pos += 1;
                }
                ReadResult::Lagged => {
                    let _ = tx.send(Err(ShmReceiveError::Lagged));
                    return;
                }
                ReadResult::Closed => {
                    let _ = tx.send(Err(ShmReceiveError::Closed));
                    return;
                }
                ReadResult::WouldBlock => Self::backoff_wait(&mut backoff),
            }
        }
    }

    fn read_loop_map<F, T>(self, tx: kanal::Sender<T>, transform: F)
    where
        F: Fn(Vec<u8>) -> Option<T>,
    {
        let header = unsafe { ShmHeader::from_ptr(self.mmap.as_ptr()) };
        let mut next_pos = header.tail.load(Ordering::Acquire);
        let mut backoff = 0u32;

        loop {
            match self.try_read_next(next_pos) {
                ReadResult::Data(buf) => {
                    backoff = 0;
                    if let Some(value) = transform(buf) {
                        if tx.send(value).is_err() {
                            return;
                        }
                    }
                    next_pos += 1;
                }
                ReadResult::Lagged | ReadResult::Closed => return,
                ReadResult::WouldBlock => Self::backoff_wait(&mut backoff),
            }
        }
    }

    /// Subscribe with a pre-filter on V2 metadata + a transform on data.
    /// If the SHM is V1, the pre-filter always receives zeroed metadata.
    ///
    /// The `pre_filter` runs on the index entry metadata BEFORE reading the data ring.
    /// If it returns `false`, the data is never read — saving both memcpy and parse.
    ///
    /// The `transform` runs on matched entries and can further filter/parse.
    pub fn subscribe_filter_map_v2<P, F, T>(
        self,
        channel_size: usize,
        pre_filter: P,
        transform: F,
    ) -> kanal::Receiver<T>
    where
        P: Fn(&ShmEntryMeta) -> bool + Send + 'static,
        F: Fn(ShmEntryMeta, Vec<u8>) -> Option<T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = kanal::bounded(channel_size);
        let affinity = self.affinity;
        let is_v2 = self.version >= VERSION_V2;

        std::thread::Builder::new()
            .name("shm-reader".to_owned())
            .spawn(move || {
                if let Some(cpu) = affinity {
                    if let Err(e) = affinity_linux::set_thread_affinity([cpu].into_iter()) {
                        tracing::warn!("failed to set shm-reader affinity: {e}");
                    }
                }
                if is_v2 {
                    self.read_loop_filter_map_v2(tx, pre_filter, transform);
                } else {
                    // V1 fallback: no pre-filter (always accept), pass zeroed meta
                    self.read_loop_map(tx, move |data| {
                        let meta = ShmEntryMeta::default();
                        transform(meta, data)
                    });
                }
            })
            .expect("failed to spawn shm reader thread");

        rx
    }

    fn read_loop_filter_map_v2<P, F, T>(
        self,
        tx: kanal::Sender<T>,
        pre_filter: P,
        transform: F,
    ) where
        P: Fn(&ShmEntryMeta) -> bool,
        F: Fn(ShmEntryMeta, Vec<u8>) -> Option<T>,
    {
        let header = unsafe { ShmHeader::from_ptr(self.mmap.as_ptr()) };
        let mut next_pos = header.tail.load(Ordering::Acquire);
        let mut backoff = 0u32;

        loop {
            match self.try_read_next_v2(next_pos, &pre_filter) {
                ReadResultV2::Data(meta, buf) => {
                    backoff = 0;
                    if let Some(value) = transform(meta, buf) {
                        if tx.send(value).is_err() {
                            return;
                        }
                    }
                    next_pos += 1;
                }
                ReadResultV2::Skipped => {
                    backoff = 0;
                    next_pos += 1;
                }
                ReadResultV2::Lagged | ReadResultV2::Closed => return,
                ReadResultV2::WouldBlock => Self::backoff_wait(&mut backoff),
            }
        }
    }
}
