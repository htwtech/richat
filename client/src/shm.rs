use {
    futures::stream::BoxStream,
    memmap2::MmapRaw,
    richat_shared::transports::shm::{
        HEADER_SIZE, INDEX_ENTRY_SIZE, MAGIC, ShmHeader, ShmIndexEntry, VERSION, copy_from_ring,
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

pub struct ShmSubscription {
    mmap: MmapRaw,
    idx_capacity: u64,
    idx_mask: u64,
    data_capacity: u64,
    index_base_offset: usize,
    data_base_offset: usize,
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

        // SAFETY: file is opened read-only; MmapRaw allows concurrent access
        let mmap = unsafe { MmapRaw::map_raw(&file).map_err(ShmConnectError::Mmap)? };

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

        if header.version != VERSION {
            return Err(ShmConnectError::UnsupportedVersion(header.version));
        }

        let idx_capacity = header.idx_capacity;
        let data_capacity = header.data_capacity;
        let idx_mask = idx_capacity - 1;

        let expected_size =
            HEADER_SIZE + (idx_capacity as usize) * INDEX_ENTRY_SIZE + data_capacity as usize;
        if file_len < expected_size {
            return Err(ShmConnectError::FileTooSmall);
        }

        let index_base_offset = HEADER_SIZE;
        let data_base_offset = HEADER_SIZE + (idx_capacity as usize) * INDEX_ENTRY_SIZE;

        Ok(Self {
            mmap,
            idx_capacity,
            idx_mask,
            data_capacity,
            index_base_offset,
            data_base_offset,
        })
    }

    /// Returns a stream of messages read from shared memory.
    /// Internally spawns a blocking thread that polls the ring buffer.
    pub fn subscribe(self) -> BoxStream<'static, Result<Vec<u8>, ShmReceiveError>> {
        let (tx, rx) = kanal::bounded(4096);

        std::thread::Builder::new()
            .name("shm-reader".to_owned())
            .spawn(move || {
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

    fn read_loop(self, tx: kanal::Sender<Result<Vec<u8>, ShmReceiveError>>) {
        let ptr = self.mmap.as_ptr();

        // Start reading from the current tail (no replay)
        // SAFETY: ptr points to valid header
        let header = unsafe { ShmHeader::from_ptr(ptr) };
        let mut next_pos = header.tail.load(Ordering::Acquire);

        loop {
            // 1. Check for new data
            let tail = header.tail.load(Ordering::Acquire);
            if next_pos >= tail {
                // Check if writer has shut down
                if header.closed.load(Ordering::Acquire) != 0 {
                    let _ = tx.send(Err(ShmReceiveError::Closed));
                    return;
                }
                std::thread::yield_now();
                continue;
            }

            // 2. Check commit
            let idx = (next_pos & self.idx_mask) as usize;
            // SAFETY: idx < idx_capacity, so the offset is within bounds
            let entry = unsafe {
                ShmIndexEntry::from_ptr(
                    ptr.add(self.index_base_offset + idx * INDEX_ENTRY_SIZE),
                )
            };
            let seq = entry.seq.load(Ordering::Acquire);
            if seq != next_pos as i64 {
                let _ = tx.send(Err(ShmReceiveError::Lagged));
                return;
            }

            // 3. Read metadata
            let data_off = entry.data_off;
            let data_len = entry.data_len as usize;

            // 4. Check data not overwritten
            let current_data_tail = header.data_tail.load(Ordering::Acquire);
            if current_data_tail - data_off > self.data_capacity {
                let _ = tx.send(Err(ShmReceiveError::Lagged));
                return;
            }

            // 5. Copy data from data ring
            let ring_offset = (data_off % self.data_capacity) as usize;
            // SAFETY: data_base_offset + data_capacity is within mmap bounds
            let data_ring = unsafe {
                std::slice::from_raw_parts(
                    ptr.add(self.data_base_offset),
                    self.data_capacity as usize,
                )
            };
            let mut buf = vec![0u8; data_len];
            copy_from_ring(data_ring, ring_offset, data_len, &mut buf);

            // 6. Double-check: data was not overwritten during read
            fence(Ordering::Acquire);
            if entry.seq.load(Ordering::Relaxed) != next_pos as i64 {
                let _ = tx.send(Err(ShmReceiveError::Lagged));
                return;
            }
            if header.data_tail.load(Ordering::Relaxed) - data_off > self.data_capacity {
                let _ = tx.send(Err(ShmReceiveError::Lagged));
                return;
            }

            // 7. Send the message
            if tx.send(Ok(buf)).is_err() {
                return; // receiver dropped
            }

            next_pos += 1;
        }
    }
}
