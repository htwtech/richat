use {
    crate::{
        config::{deserialize_humansize_usize, deserialize_num_str},
        transports::{RecvError, RecvStream, Subscribe, SubscribeError},
    },
    futures::stream::{BoxStream, StreamExt},
    memmap2::MmapMut,
    richat_proto::richat::RichatFilter,
    serde::Deserialize,
    solana_clock::Slot,
    std::{
        fs::{self, File, OpenOptions},
        future::Future,
        io,
        path::PathBuf,
        sync::atomic::{AtomicI64, AtomicU64, Ordering, fence},
    },
    thiserror::Error,
    tokio::task::JoinError,
    tokio_util::sync::CancellationToken,
    tracing::{error, info},
};

// --- Constants ---

pub const MAGIC: u64 = 0x52494348_41545348; // "RICHATSH"
pub const VERSION: u64 = 1;
pub const HEADER_SIZE: usize = 64;
pub const INDEX_ENTRY_SIZE: usize = 32;

// --- Header ---

/// Shared memory file header (64 bytes, repr(C) for stable layout).
#[repr(C)]
pub struct ShmHeader {
    pub magic: u64,
    pub version: u64,
    pub idx_capacity: u64,
    pub data_capacity: u64,
    pub tail: AtomicU64,
    pub data_tail: AtomicU64,
    pub closed: AtomicU64,
    pub _reserved: u64,
}

impl ShmHeader {
    /// # Safety
    ///
    /// `ptr` must point to at least `HEADER_SIZE` bytes of memory that remain valid
    /// for the lifetime `'a`, and must be properly aligned for `ShmHeader`.
    pub unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a ShmHeader {
        &*(ptr as *const ShmHeader)
    }

    /// # Safety
    ///
    /// `ptr` must point to at least `HEADER_SIZE` bytes of writable memory that
    /// remain valid for the lifetime `'a`, and must be properly aligned for `ShmHeader`.
    pub unsafe fn from_ptr_mut<'a>(ptr: *mut u8) -> &'a mut ShmHeader {
        &mut *(ptr as *mut ShmHeader)
    }
}

const _: () = assert!(size_of::<ShmHeader>() == HEADER_SIZE);

// --- Index Entry ---

/// Index ring entry (32 bytes, repr(C)).
#[repr(C)]
pub struct ShmIndexEntry {
    /// Commit flag: -1 = empty, >=0 = committed position
    pub seq: AtomicI64,
    /// Byte offset in data ring (monotonic, use % data_capacity)
    pub data_off: u64,
    /// Length of the message in bytes
    pub data_len: u32,
    pub _reserved: u32,
    /// Solana slot number (0 = not tracked)
    pub slot: u64,
}

impl ShmIndexEntry {
    /// # Safety
    ///
    /// `ptr` must point to at least `INDEX_ENTRY_SIZE` bytes of valid memory,
    /// properly aligned for `ShmIndexEntry`, and remain valid for the lifetime `'a`.
    pub unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a ShmIndexEntry {
        &*(ptr as *const ShmIndexEntry)
    }

    /// # Safety
    ///
    /// `ptr` must point to at least `INDEX_ENTRY_SIZE` bytes of writable memory,
    /// properly aligned for `ShmIndexEntry`, and remain valid for the lifetime `'a`.
    pub unsafe fn from_ptr_mut<'a>(ptr: *mut u8) -> &'a mut ShmIndexEntry {
        &mut *(ptr as *mut ShmIndexEntry)
    }
}

const _: () = assert!(size_of::<ShmIndexEntry>() == INDEX_ENTRY_SIZE);

// --- Ring copy helpers ---

/// Copy `len` bytes from a ring buffer starting at `offset` into `dst`.
/// Handles wrap-around when `offset + len > ring.len()`.
pub fn copy_from_ring(ring: &[u8], offset: usize, len: usize, dst: &mut [u8]) {
    debug_assert!(dst.len() >= len);
    debug_assert!(offset < ring.len());

    let first = ring.len() - offset;
    if first >= len {
        dst[..len].copy_from_slice(&ring[offset..offset + len]);
    } else {
        dst[..first].copy_from_slice(&ring[offset..]);
        dst[first..len].copy_from_slice(&ring[..len - first]);
    }
}

/// Copy `src` into a ring buffer starting at `offset`.
/// Handles wrap-around when `offset + src.len() > ring.len()`.
pub fn copy_to_ring(ring: &mut [u8], offset: usize, src: &[u8]) {
    debug_assert!(offset < ring.len());

    let first = ring.len() - offset;
    if first >= src.len() {
        ring[offset..offset + src.len()].copy_from_slice(src);
    } else {
        ring[offset..].copy_from_slice(&src[..first]);
        ring[..src.len() - first].copy_from_slice(&src[first..]);
    }
}

// --- Errors ---

#[derive(Debug, Error)]
pub enum ShmError {
    #[error("failed to create shm file: {0}")]
    CreateFile(io::Error),
    #[error("failed to set shm file length: {0}")]
    SetLen(io::Error),
    #[error("failed to mmap shm file: {0}")]
    Mmap(io::Error),
    #[error("failed to subscribe: {0}")]
    Subscribe(#[from] SubscribeError),
}

// --- Config ---

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigShmServer {
    #[serde(default = "ConfigShmServer::default_path")]
    pub path: PathBuf,
    #[serde(
        default = "ConfigShmServer::default_max_messages",
        deserialize_with = "deserialize_num_str"
    )]
    pub max_messages: usize,
    #[serde(
        default = "ConfigShmServer::default_max_bytes",
        deserialize_with = "deserialize_humansize_usize"
    )]
    pub max_bytes: usize,
    #[serde(default)]
    pub filter: Option<RichatFilter>,
}

impl ConfigShmServer {
    fn default_path() -> PathBuf {
        PathBuf::from("/dev/shm/richat")
    }

    const fn default_max_messages() -> usize {
        2_097_152
    }

    const fn default_max_bytes() -> usize {
        15 * 1024 * 1024 * 1024 // 15 GiB
    }
}

// --- Writer ---

fn next_power_of_two(n: usize) -> usize {
    n.next_power_of_two()
}

pub struct ShmServer;

impl ShmServer {
    pub async fn spawn(
        config: ConfigShmServer,
        messages: impl Subscribe + Send + 'static,
        shutdown: CancellationToken,
    ) -> Result<impl Future<Output = Result<(), JoinError>>, ShmError> {
        let idx_capacity = next_power_of_two(config.max_messages);
        let data_capacity = next_power_of_two(config.max_bytes);
        let idx_mask = (idx_capacity - 1) as u64;

        let total_size = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE + data_capacity;

        // Create and size the file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&config.path)
            .map_err(ShmError::CreateFile)?;

        file.set_len(total_size as u64).map_err(ShmError::SetLen)?;

        // mmap the file
        // SAFETY: we just created and sized the file, we are the sole writer
        let mut mmap = unsafe { MmapMut::map_mut(&file).map_err(ShmError::Mmap)? };

        // Initialize header
        let ptr = mmap.as_mut_ptr();
        // SAFETY: ptr is valid, aligned (start of mmap), and we have exclusive access
        unsafe {
            let header = ShmHeader::from_ptr_mut(ptr);
            header.magic = MAGIC;
            header.version = VERSION;
            header.idx_capacity = idx_capacity as u64;
            header.data_capacity = data_capacity as u64;
            header.tail = AtomicU64::new(0);
            header.data_tail = AtomicU64::new(0);
            header.closed = AtomicU64::new(0);
            header._reserved = 0;
        }

        // Initialize index entries: all seq = -1
        let index_base = unsafe { ptr.add(HEADER_SIZE) };
        for i in 0..idx_capacity {
            // SAFETY: index_base + i * INDEX_ENTRY_SIZE is within the mmap
            unsafe {
                let entry = ShmIndexEntry::from_ptr_mut(index_base.add(i * INDEX_ENTRY_SIZE));
                entry.seq = AtomicI64::new(-1);
                entry.data_off = 0;
                entry.data_len = 0;
                entry._reserved = 0;
                entry.slot = 0;
            }
        }

        // Subscribe to the ring buffer
        let stream = messages.subscribe(None, config.filter)?;

        let path = config.path.clone();
        info!(path = %path.display(), idx_capacity, data_capacity, "shm server started");

        let jh = tokio::spawn(async move {
            Self::run(
                mmap,
                stream,
                idx_capacity,
                data_capacity,
                idx_mask,
                shutdown.clone(),
            )
            .await;

            // On shutdown: mark closed and remove the file
            let ptr = mmap.as_ptr();
            // SAFETY: header is still valid, we have not unmapped
            unsafe {
                let header = ShmHeader::from_ptr(ptr);
                header.closed.store(1, Ordering::Release);
            }
            drop(mmap);
            drop(file);
            if let Err(error) = fs::remove_file(&path) {
                error!(path = %path.display(), %error, "failed to remove shm file");
            }
        });

        Ok(jh)
    }

    async fn run(
        mut mmap: MmapMut,
        mut stream: RecvStream,
        idx_capacity: usize,
        data_capacity: usize,
        idx_mask: u64,
        shutdown: CancellationToken,
    ) {
        let ptr = mmap.as_mut_ptr();
        let index_base = unsafe { ptr.add(HEADER_SIZE) };
        let data_base_offset = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE;

        let mut pos: u64 = 0;
        let mut data_pos: u64 = 0;

        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                () = shutdown.cancelled() => break,
            };

            let data = match item {
                Some(Ok(data)) => data,
                Some(Err(RecvError::Lagged)) => {
                    error!("shm writer: source stream lagged");
                    continue;
                }
                Some(Err(RecvError::Closed)) | None => {
                    info!("shm writer: source stream closed");
                    break;
                }
            };

            let data_bytes: &[u8] = &data;
            let data_len = data_bytes.len();

            // 1. Compute index slot
            let idx = (pos & idx_mask) as usize;

            // 2. Write data bytes into data ring with wrap-around
            let data_ring_offset = (data_pos % data_capacity as u64) as usize;
            // SAFETY: data_base_offset + data_ring is within mmap bounds
            let data_ring =
                unsafe { std::slice::from_raw_parts_mut(ptr.add(data_base_offset), data_capacity) };
            copy_to_ring(data_ring, data_ring_offset, data_bytes);

            // 3. Write index entry metadata
            // SAFETY: index_base + idx * INDEX_ENTRY_SIZE is within bounds
            let entry =
                unsafe { ShmIndexEntry::from_ptr_mut(index_base.add(idx * INDEX_ENTRY_SIZE)) };
            entry.data_off = data_pos;
            entry.data_len = data_len as u32;
            entry.slot = 0; // slot not tracked in shm

            // 4. Advance data position
            data_pos += data_len as u64;

            // 5. Update header.data_tail
            // SAFETY: header is valid
            let header = unsafe { ShmHeader::from_ptr(ptr) };
            header.data_tail.store(data_pos, Ordering::Release);

            // 6. COMMIT: set seq to pos
            entry.seq.store(pos as i64, Ordering::Release);

            // 7. Advance position
            pos += 1;

            // 8. Update header.tail
            header.tail.store(pos, Ordering::Release);
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_copy_to_ring_no_wrap() {
        let mut ring = vec![0u8; 16];
        copy_to_ring(&mut ring, 4, &[1, 2, 3, 4]);
        assert_eq!(&ring[4..8], &[1, 2, 3, 4]);
    }

    #[test]
    fn test_copy_to_ring_wrap() {
        let mut ring = vec![0u8; 8];
        copy_to_ring(&mut ring, 6, &[1, 2, 3, 4]);
        assert_eq!(&ring[6..8], &[1, 2]);
        assert_eq!(&ring[0..2], &[3, 4]);
    }

    #[test]
    fn test_copy_from_ring_no_wrap() {
        let ring = vec![0, 1, 2, 3, 4, 5, 6, 7];
        let mut dst = vec![0u8; 4];
        copy_from_ring(&ring, 2, 4, &mut dst);
        assert_eq!(dst, vec![2, 3, 4, 5]);
    }

    #[test]
    fn test_copy_from_ring_wrap() {
        let ring = vec![10, 11, 12, 13, 14, 15, 16, 17];
        let mut dst = vec![0u8; 4];
        copy_from_ring(&ring, 6, 4, &mut dst);
        assert_eq!(dst, vec![16, 17, 10, 11]);
    }

    #[test]
    fn test_header_size() {
        assert_eq!(size_of::<ShmHeader>(), HEADER_SIZE);
    }

    #[test]
    fn test_index_entry_size() {
        assert_eq!(size_of::<ShmIndexEntry>(), INDEX_ENTRY_SIZE);
    }

    #[test]
    fn test_write_read_single_message() {
        // Create an in-memory buffer simulating mmap
        let idx_capacity: usize = 4; // power of 2
        let data_capacity: usize = 64;
        let total_size = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE + data_capacity;
        let mut buf = vec![0u8; total_size];

        let ptr = buf.as_mut_ptr();

        // Init header
        unsafe {
            let header = ShmHeader::from_ptr_mut(ptr);
            header.magic = MAGIC;
            header.version = VERSION;
            header.idx_capacity = idx_capacity as u64;
            header.data_capacity = data_capacity as u64;
            header.tail = AtomicU64::new(0);
            header.data_tail = AtomicU64::new(0);
            header.closed = AtomicU64::new(0);
        }

        // Init index entries
        let index_base = unsafe { ptr.add(HEADER_SIZE) };
        for i in 0..idx_capacity {
            unsafe {
                let entry = ShmIndexEntry::from_ptr_mut(index_base.add(i * INDEX_ENTRY_SIZE));
                entry.seq = AtomicI64::new(-1);
            }
        }

        // Write a message
        let msg = b"hello world";
        let idx_mask = (idx_capacity - 1) as u64;
        let pos: u64 = 0;
        let data_pos: u64 = 0;

        let data_base_offset = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE;
        let data_ring =
            unsafe { std::slice::from_raw_parts_mut(ptr.add(data_base_offset), data_capacity) };
        copy_to_ring(data_ring, 0, msg);

        let idx = (pos & idx_mask) as usize;
        let entry =
            unsafe { ShmIndexEntry::from_ptr_mut(index_base.add(idx * INDEX_ENTRY_SIZE)) };
        entry.data_off = data_pos;
        entry.data_len = msg.len() as u32;
        entry.slot = 0;

        let header = unsafe { ShmHeader::from_ptr(ptr) };
        header
            .data_tail
            .store(data_pos + msg.len() as u64, Ordering::Release);
        entry.seq.store(pos as i64, Ordering::Release);
        header.tail.store(pos + 1, Ordering::Release);

        // Read back
        let read_header = unsafe { ShmHeader::from_ptr(ptr as *const u8) };
        assert_eq!(read_header.tail.load(Ordering::Acquire), 1);

        let read_idx = (0u64 & idx_mask) as usize;
        let read_entry = unsafe {
            ShmIndexEntry::from_ptr(index_base.add(read_idx * INDEX_ENTRY_SIZE) as *const u8)
        };
        assert_eq!(read_entry.seq.load(Ordering::Acquire), 0);
        assert_eq!(read_entry.data_len, msg.len() as u32);

        let data_ring_ro =
            unsafe { std::slice::from_raw_parts(ptr.add(data_base_offset) as *const u8, data_capacity) };
        let mut result = vec![0u8; msg.len()];
        copy_from_ring(
            data_ring_ro,
            (read_entry.data_off % data_capacity as u64) as usize,
            read_entry.data_len as usize,
            &mut result,
        );
        assert_eq!(&result, msg);
    }

    #[test]
    fn test_write_read_sequence() {
        let idx_capacity: usize = 4;
        let data_capacity: usize = 128;
        let total_size = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE + data_capacity;
        let mut buf = vec![0u8; total_size];
        let ptr = buf.as_mut_ptr();
        let idx_mask = (idx_capacity - 1) as u64;

        // Init header
        unsafe {
            let header = ShmHeader::from_ptr_mut(ptr);
            header.magic = MAGIC;
            header.version = VERSION;
            header.idx_capacity = idx_capacity as u64;
            header.data_capacity = data_capacity as u64;
            header.tail = AtomicU64::new(0);
            header.data_tail = AtomicU64::new(0);
            header.closed = AtomicU64::new(0);
        }

        let index_base = unsafe { ptr.add(HEADER_SIZE) };
        for i in 0..idx_capacity {
            unsafe {
                let entry = ShmIndexEntry::from_ptr_mut(index_base.add(i * INDEX_ENTRY_SIZE));
                entry.seq = AtomicI64::new(-1);
            }
        }

        let data_base_offset = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE;
        let messages: Vec<&[u8]> = vec![b"msg0", b"msg1", b"msg2"];
        let mut pos: u64 = 0;
        let mut data_pos: u64 = 0;

        // Write all messages
        for msg in &messages {
            let idx = (pos & idx_mask) as usize;
            let data_ring = unsafe {
                std::slice::from_raw_parts_mut(ptr.add(data_base_offset), data_capacity)
            };
            copy_to_ring(
                data_ring,
                (data_pos % data_capacity as u64) as usize,
                msg,
            );

            let entry =
                unsafe { ShmIndexEntry::from_ptr_mut(index_base.add(idx * INDEX_ENTRY_SIZE)) };
            entry.data_off = data_pos;
            entry.data_len = msg.len() as u32;
            entry.slot = 0;

            data_pos += msg.len() as u64;
            let header = unsafe { ShmHeader::from_ptr(ptr) };
            header.data_tail.store(data_pos, Ordering::Release);
            entry.seq.store(pos as i64, Ordering::Release);
            pos += 1;
            header.tail.store(pos, Ordering::Release);
        }

        // Read all messages back
        let header = unsafe { ShmHeader::from_ptr(ptr as *const u8) };
        assert_eq!(header.tail.load(Ordering::Acquire), 3);

        for (i, expected) in messages.iter().enumerate() {
            let idx = (i as u64 & idx_mask) as usize;
            let entry = unsafe {
                ShmIndexEntry::from_ptr(index_base.add(idx * INDEX_ENTRY_SIZE) as *const u8)
            };
            assert_eq!(entry.seq.load(Ordering::Acquire), i as i64);

            let data_ring_ro = unsafe {
                std::slice::from_raw_parts(
                    ptr.add(data_base_offset) as *const u8,
                    data_capacity,
                )
            };
            let mut result = vec![0u8; entry.data_len as usize];
            copy_from_ring(
                data_ring_ro,
                (entry.data_off % data_capacity as u64) as usize,
                entry.data_len as usize,
                &mut result,
            );
            assert_eq!(&result[..], *expected);
        }
    }

    #[test]
    fn test_lag_detection() {
        let idx_capacity: usize = 2; // very small ring
        let data_capacity: usize = 32;
        let total_size = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE + data_capacity;
        let mut buf = vec![0u8; total_size];
        let ptr = buf.as_mut_ptr();
        let idx_mask = (idx_capacity - 1) as u64;

        unsafe {
            let header = ShmHeader::from_ptr_mut(ptr);
            header.magic = MAGIC;
            header.version = VERSION;
            header.idx_capacity = idx_capacity as u64;
            header.data_capacity = data_capacity as u64;
            header.tail = AtomicU64::new(0);
            header.data_tail = AtomicU64::new(0);
            header.closed = AtomicU64::new(0);
        }

        let index_base = unsafe { ptr.add(HEADER_SIZE) };
        for i in 0..idx_capacity {
            unsafe {
                let entry = ShmIndexEntry::from_ptr_mut(index_base.add(i * INDEX_ENTRY_SIZE));
                entry.seq = AtomicI64::new(-1);
            }
        }

        let data_base_offset = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE;
        let mut pos: u64 = 0;
        let mut data_pos: u64 = 0;

        // Write 3 messages into a ring of size 2 (overwrites first)
        for msg in [b"aa" as &[u8], b"bb", b"cc"] {
            let idx = (pos & idx_mask) as usize;
            let data_ring = unsafe {
                std::slice::from_raw_parts_mut(ptr.add(data_base_offset), data_capacity)
            };
            copy_to_ring(
                data_ring,
                (data_pos % data_capacity as u64) as usize,
                msg,
            );

            let entry =
                unsafe { ShmIndexEntry::from_ptr_mut(index_base.add(idx * INDEX_ENTRY_SIZE)) };
            entry.data_off = data_pos;
            entry.data_len = msg.len() as u32;
            entry.slot = 0;

            data_pos += msg.len() as u64;
            let header = unsafe { ShmHeader::from_ptr(ptr) };
            header.data_tail.store(data_pos, Ordering::Release);
            entry.seq.store(pos as i64, Ordering::Release);
            pos += 1;
            header.tail.store(pos, Ordering::Release);
        }

        // Reader trying to read position 0: should detect lag
        // because index slot 0 now has seq=2 (from message "cc"), not 0
        let reader_pos: u64 = 0;
        let idx = (reader_pos & idx_mask) as usize;
        let entry = unsafe {
            ShmIndexEntry::from_ptr(index_base.add(idx * INDEX_ENTRY_SIZE) as *const u8)
        };
        let seq = entry.seq.load(Ordering::Acquire);
        assert_ne!(seq, reader_pos as i64, "should detect lag: seq was overwritten");
        assert_eq!(seq, 2); // position 2 wrote to slot 0
    }
}
