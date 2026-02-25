use {
    crate::{
        config::{deserialize_humansize_usize, deserialize_num_str},
        transports::{RecvError, Subscribe},
    },
    futures::stream::StreamExt,
    memmap2::MmapMut,
    richat_proto::richat::RichatFilter,
    serde::Deserialize,
    std::{
        fs::{self, File, OpenOptions},
        future::Future,
        io,
        path::PathBuf,
        sync::{
            atomic::{AtomicI64, AtomicU64, Ordering},
            Mutex,
        },
        time::Duration,
    },
    thiserror::Error,
    tokio::task::JoinError,
    tokio_util::sync::CancellationToken,
    tracing::{error, info},
};

/// Serde-friendly filter config that maps to `RichatFilter`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigShmFilter {
    pub disable_accounts: bool,
    pub disable_transactions: bool,
    pub disable_entries: bool,
}

impl From<ConfigShmFilter> for RichatFilter {
    fn from(f: ConfigShmFilter) -> Self {
        RichatFilter {
            disable_accounts: f.disable_accounts,
            disable_transactions: f.disable_transactions,
            disable_entries: f.disable_entries,
        }
    }
}

// --- Constants ---

pub const MAGIC: u64 = 0x52494348_41545348; // "RICHATSH"
pub const VERSION: u64 = 1;
pub const VERSION_V2: u64 = 2;
pub const HEADER_SIZE: usize = 64;
pub const INDEX_ENTRY_SIZE: usize = 32;
pub const INDEX_ENTRY_SIZE_V2: usize = 128;

// --- Message types ---

pub const MSG_TYPE_SLOT: u8 = 0;
pub const MSG_TYPE_ACCOUNT: u8 = 1;
pub const MSG_TYPE_TRANSACTION: u8 = 2;
pub const MSG_TYPE_ENTRY: u8 = 3;
pub const MSG_TYPE_BLOCK_META: u8 = 4;

// --- Flags ---
// Transaction: bit0=is_vote, bit1=failed
pub const FLAG_TX_IS_VOTE: u8 = 0x01;
pub const FLAG_TX_FAILED: u8 = 0x02;
// Account: bit0=executable, bit1=nonempty_txn_sig, bit2=is_startup
pub const FLAG_ACC_EXECUTABLE: u8 = 0x01;
pub const FLAG_ACC_HAS_TXN_SIG: u8 = 0x02;
pub const FLAG_ACC_IS_STARTUP: u8 = 0x04;

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
    pub index_entry_size: u64,
}

impl ShmHeader {
    /// # Safety
    ///
    /// `ptr` must point to at least `HEADER_SIZE` bytes of memory that remain valid
    /// for the lifetime `'a`, and must be properly aligned for `ShmHeader`.
    #[inline]
    pub const unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a ShmHeader {
        unsafe { &*(ptr as *const ShmHeader) }
    }

    /// # Safety
    ///
    /// `ptr` must point to at least `HEADER_SIZE` bytes of writable memory that
    /// remain valid for the lifetime `'a`, and must be properly aligned for `ShmHeader`.
    #[inline]
    pub unsafe fn from_ptr_mut<'a>(ptr: *mut u8) -> &'a mut ShmHeader {
        unsafe { &mut *(ptr as *mut ShmHeader) }
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
    #[inline]
    pub const unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a ShmIndexEntry {
        unsafe { &*(ptr as *const ShmIndexEntry) }
    }

    /// # Safety
    ///
    /// `ptr` must point to at least `INDEX_ENTRY_SIZE` bytes of writable memory,
    /// properly aligned for `ShmIndexEntry`, and remain valid for the lifetime `'a`.
    #[inline]
    pub unsafe fn from_ptr_mut<'a>(ptr: *mut u8) -> &'a mut ShmIndexEntry {
        unsafe { &mut *(ptr as *mut ShmIndexEntry) }
    }
}

const _: () = assert!(size_of::<ShmIndexEntry>() == INDEX_ENTRY_SIZE);

// --- Index Entry V2 (128 bytes) ---

/// V2 index ring entry with inline metadata for zero-parse filtering.
#[repr(C)]
pub struct ShmIndexEntryV2 {
    // --- Base (32B, same layout as V1) ---
    /// Commit flag: -1 = empty, >=0 = committed position
    pub seq: AtomicI64,
    /// Byte offset in data ring (monotonic, use % data_capacity)
    pub data_off: u64,
    /// Length of the message in bytes
    pub data_len: u32,
    /// Message type (MSG_TYPE_*)
    pub msg_type: u8,
    /// Bit flags (interpretation depends on msg_type)
    pub flags: u8,
    pub _pad0: u16,
    /// Solana slot number
    pub slot: u64,

    // --- Type-specific metadata (96B) ---
    // Account: pubkey[32] + owner[32] + lamports[8] + pad[24]
    // Transaction: signature[64] + account_bloom[32]
    // Slot: parent[8] + pad[88]
    // Entry: index[8] + executed_tx_count[8] + pad[80]
    // BlockMeta: pad[96]
    pub meta: [u8; 96],
}

impl ShmIndexEntryV2 {
    /// # Safety
    ///
    /// `ptr` must point to at least `INDEX_ENTRY_SIZE_V2` bytes of valid memory,
    /// properly aligned for `ShmIndexEntryV2`, and remain valid for the lifetime `'a`.
    #[inline]
    pub const unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a ShmIndexEntryV2 {
        unsafe { &*(ptr as *const ShmIndexEntryV2) }
    }

    /// # Safety
    ///
    /// `ptr` must point to at least `INDEX_ENTRY_SIZE_V2` bytes of writable memory,
    /// properly aligned for `ShmIndexEntryV2`, and remain valid for the lifetime `'a`.
    #[inline]
    pub unsafe fn from_ptr_mut<'a>(ptr: *mut u8) -> &'a mut ShmIndexEntryV2 {
        unsafe { &mut *(ptr as *mut ShmIndexEntryV2) }
    }

    /// Account pubkey (first 32 bytes of meta). Only valid when msg_type == MSG_TYPE_ACCOUNT.
    #[inline]
    pub fn account_pubkey(&self) -> &[u8; 32] {
        self.meta[0..32].try_into().unwrap()
    }

    /// Account owner (bytes 32..64 of meta). Only valid when msg_type == MSG_TYPE_ACCOUNT.
    #[inline]
    pub fn account_owner(&self) -> &[u8; 32] {
        self.meta[32..64].try_into().unwrap()
    }

    /// Account lamports (bytes 64..72 of meta). Only valid when msg_type == MSG_TYPE_ACCOUNT.
    #[inline]
    pub fn account_lamports(&self) -> u64 {
        u64::from_le_bytes(self.meta[64..72].try_into().unwrap())
    }

    /// Transaction signature (first 64 bytes of meta). Only valid when msg_type == MSG_TYPE_TRANSACTION.
    #[inline]
    pub fn tx_signature(&self) -> &[u8; 64] {
        self.meta[0..64].try_into().unwrap()
    }

    /// Transaction account bloom filter (bytes 64..96 of meta). Only valid when msg_type == MSG_TYPE_TRANSACTION.
    #[inline]
    pub fn tx_account_bloom(&self) -> &[u8; 32] {
        self.meta[64..96].try_into().unwrap()
    }

    /// Slot parent (first 8 bytes of meta). Only valid when msg_type == MSG_TYPE_SLOT.
    #[inline]
    pub fn slot_parent(&self) -> u64 {
        u64::from_le_bytes(self.meta[0..8].try_into().unwrap())
    }

    /// Entry index (first 8 bytes of meta). Only valid when msg_type == MSG_TYPE_ENTRY.
    #[inline]
    pub fn entry_index(&self) -> u64 {
        u64::from_le_bytes(self.meta[0..8].try_into().unwrap())
    }

    /// Entry executed_transaction_count (bytes 8..16 of meta). Only valid when msg_type == MSG_TYPE_ENTRY.
    #[inline]
    pub fn entry_exec_tx_count(&self) -> u64 {
        u64::from_le_bytes(self.meta[8..16].try_into().unwrap())
    }
}

const _: () = assert!(size_of::<ShmIndexEntryV2>() == INDEX_ENTRY_SIZE_V2);

// --- ShmWriteMeta ---

/// Metadata to be written into a V2 index entry alongside the data.
pub struct ShmWriteMeta {
    pub msg_type: u8,
    pub flags: u8,
    pub slot: u64,
    pub meta: [u8; 96],
}

impl Default for ShmWriteMeta {
    fn default() -> Self {
        Self {
            msg_type: 0,
            flags: 0,
            slot: 0,
            meta: [0u8; 96],
        }
    }
}

// --- Ring copy helpers ---

/// Copy `len` bytes from a ring buffer starting at `offset` into `dst`.
/// Handles wrap-around when `offset + len > ring.len()`.
#[inline]
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
#[inline]
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
    pub filter: Option<ConfigShmFilter>,
    /// Channel capacity for ShmServer async-to-blocking bridge
    #[serde(
        default = "ConfigShmServer::default_channel_capacity",
        deserialize_with = "deserialize_num_str"
    )]
    pub channel_capacity: usize,
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

    const fn default_channel_capacity() -> usize {
        4096
    }
}

// --- Writer ---

const fn next_power_of_two(n: usize) -> usize {
    n.next_power_of_two()
}

/// Create the SHM file, mmap it, and initialize the header + index entries.
/// Returns `(mmap, file, idx_capacity, data_capacity, version, entry_size)`.
fn init_shm_mmap(
    config: &ConfigShmServer,
    version: u64,
) -> Result<(MmapMut, File, usize, usize), ShmError> {
    let idx_capacity = next_power_of_two(config.max_messages);
    let data_capacity = next_power_of_two(config.max_bytes);
    let entry_size = if version >= VERSION_V2 {
        INDEX_ENTRY_SIZE_V2
    } else {
        INDEX_ENTRY_SIZE
    };
    let total_size = HEADER_SIZE
        .checked_add(
            idx_capacity
                .checked_mul(entry_size)
                .expect("SHM index region size overflow"),
        )
        .and_then(|v| v.checked_add(data_capacity))
        .expect("SHM total size overflow");

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&config.path)
        .map_err(ShmError::CreateFile)?;

    file.set_len(total_size as u64).map_err(ShmError::SetLen)?;

    // SAFETY: we just created and sized the file, we are the sole writer
    let mut mmap = unsafe { MmapMut::map_mut(&file).map_err(ShmError::Mmap)? };

    // Initialize header
    let ptr = mmap.as_mut_ptr();
    // SAFETY: ptr is valid, aligned (start of mmap), and we have exclusive access
    unsafe {
        let header = ShmHeader::from_ptr_mut(ptr);
        header.magic = MAGIC;
        header.version = version;
        header.idx_capacity = idx_capacity as u64;
        header.data_capacity = data_capacity as u64;
        header.tail = AtomicU64::new(0);
        header.data_tail = AtomicU64::new(0);
        header.closed = AtomicU64::new(0);
        header.index_entry_size = entry_size as u64;
    }

    // Initialize index entries: all seq = -1
    for i in 0..idx_capacity {
        // SAFETY: index region is within mmap bounds
        unsafe {
            let entry_ptr = ptr.add(HEADER_SIZE + i * entry_size);
            // Zero out the entire entry first (handles V2 meta region too)
            std::ptr::write_bytes(entry_ptr, 0, entry_size);
            // Set seq = -1 at the start of each entry (same offset for V1 and V2)
            let seq_ptr = entry_ptr as *mut AtomicI64;
            (*seq_ptr) = AtomicI64::new(-1);
        }
    }

    Ok((mmap, file, idx_capacity, data_capacity))
}

// --- ShmDirectWriter ---

struct ShmDirectWriterInner {
    mmap: MmapMut,
    pos: u64,
    data_pos: u64,
    idx_capacity: usize,
    data_capacity: usize,
    idx_mask: u64,
    entry_size: usize,
    closed: bool,
}

/// Direct writer into the SHM ring buffer, bypassing async channels.
/// Thread-safe via Mutex (Geyser callbacks come from different threads).
pub struct ShmDirectWriter {
    inner: Mutex<ShmDirectWriterInner>,
    path: PathBuf,
    _file: File,
}

impl std::fmt::Debug for ShmDirectWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmDirectWriter")
            .field("path", &self.path)
            .finish()
    }
}

impl ShmDirectWriter {
    /// Create and initialize the SHM file using V2 format.
    pub fn new(config: &ConfigShmServer) -> Result<Self, ShmError> {
        let version = VERSION_V2;
        let (mmap, file, idx_capacity, data_capacity) = init_shm_mmap(config, version)?;
        let idx_mask = (idx_capacity - 1) as u64;
        let entry_size = INDEX_ENTRY_SIZE_V2;
        let path = config.path.clone();

        info!(path = %path.display(), idx_capacity, data_capacity, version, entry_size, "shm direct writer started (V2)");

        Ok(Self {
            inner: Mutex::new(ShmDirectWriterInner {
                mmap,
                pos: 0,
                data_pos: 0,
                idx_capacity,
                data_capacity,
                idx_mask,
                entry_size,
                closed: false,
            }),
            path,
            _file: file,
        })
    }

    /// Acquire the lock, returning None if poisoned (marks closed).
    #[inline]
    fn lock_inner(&self) -> Option<std::sync::MutexGuard<'_, ShmDirectWriterInner>> {
        match self.inner.lock() {
            Ok(guard) => {
                if guard.closed {
                    None
                } else {
                    Some(guard)
                }
            }
            Err(poisoned) => {
                let mut inner = poisoned.into_inner();
                if !inner.closed {
                    error!("ShmDirectWriter mutex poisoned; closing to prevent corruption");
                    inner.closed = true;
                    let header = unsafe { ShmHeader::from_ptr(inner.mmap.as_mut_ptr()) };
                    header.closed.store(1, Ordering::Release);
                }
                None
            }
        }
    }

    /// Write bytes into the ring buffer with V2 metadata.
    #[inline]
    pub fn write(&self, data: &[u8], slot: u64, write_meta: &ShmWriteMeta) {
        let Some(mut inner) = self.lock_inner() else {
            return;
        };

        let ptr = inner.mmap.as_mut_ptr();
        let data_len = data.len();
        let idx = (inner.pos & inner.idx_mask) as usize;
        let entry_size = inner.entry_size;
        let data_base_offset = HEADER_SIZE + inner.idx_capacity * entry_size;

        // Check data length fits in u32 (index entry field)
        if data_len > u32::MAX as usize {
            error!("SHM message too large ({data_len} bytes), dropping");
            return;
        }

        // 1. Write data bytes into data ring with wrap-around
        let data_ring_offset = (inner.data_pos % inner.data_capacity as u64) as usize;
        // SAFETY: data region is within mmap bounds, we are the sole writer (protected by Mutex)
        let data_ring = unsafe {
            std::slice::from_raw_parts_mut(ptr.add(data_base_offset), inner.data_capacity)
        };
        copy_to_ring(data_ring, data_ring_offset, data);

        // 2. Write V2 index entry metadata
        // SAFETY: index region is within bounds
        let entry = unsafe {
            ShmIndexEntryV2::from_ptr_mut(
                ptr.add(HEADER_SIZE + idx * entry_size),
            )
        };
        entry.data_off = inner.data_pos;
        entry.data_len = data_len as u32;
        entry.msg_type = write_meta.msg_type;
        entry.flags = write_meta.flags;
        entry._pad0 = 0;
        entry.slot = slot;
        entry.meta.copy_from_slice(&write_meta.meta);

        // 3. Advance data position
        inner.data_pos += data_len as u64;

        // 4. Update header.data_tail
        let header = unsafe { ShmHeader::from_ptr(ptr) };
        header.data_tail.store(inner.data_pos, Ordering::Relaxed);

        // 5. COMMIT: set seq to pos
        entry.seq.store(inner.pos as i64, Ordering::Release);

        // 6. Advance position
        inner.pos += 1;

        // 7. Update header.tail
        header.tail.store(inner.pos, Ordering::Release);
    }

    /// Mark SHM as closed.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.closed {
            return;
        }
        inner.closed = true;
        let ptr = inner.mmap.as_mut_ptr();
        let header = unsafe { ShmHeader::from_ptr(ptr) };
        header.closed.store(1, Ordering::Release);
    }

    /// Remove the SHM file from disk.
    pub fn remove_file(&self) {
        if let Err(error) = fs::remove_file(&self.path) {
            error!(path = %self.path.display(), %error, "failed to remove shm file");
        }
    }
}

pub struct ShmServer;

/// A Send-safe wrapper around MmapMut.
/// SAFETY: ShmServer is the sole writer; readers use a separate read-only mmap.
/// The mmap is moved into the blocking thread and never shared across threads.
struct SendMmap(MmapMut);

// SAFETY: MmapMut is only accessed from a single thread (the blocking writer thread).
unsafe impl Send for SendMmap {}

impl ShmServer {
    pub async fn spawn(
        config: ConfigShmServer,
        messages: impl Subscribe + Send + 'static,
        shutdown: CancellationToken,
    ) -> Result<impl Future<Output = Result<(), JoinError>>, ShmError> {
        let (mmap, file, idx_capacity, data_capacity) = init_shm_mmap(&config, VERSION)?;
        let idx_mask = (idx_capacity - 1) as u64;

        let filter = config.filter.map(RichatFilter::from);

        let path = config.path.clone();
        info!(path = %path.display(), idx_capacity, data_capacity, "shm server started");

        // Use a channel to bridge async stream -> blocking writer thread
        let (tx, rx) = kanal::bounded(config.channel_capacity);

        // Async task: subscribes to the ring buffer and forwards messages to channel.
        // Uses a resubscribe loop to handle stale initial position (the ring buffer
        // initializes with tail=max_messages but never writes to that position).
        let shutdown_clone = shutdown.clone();
        let reader_jh = tokio::spawn(async move {
            'subscribe: loop {
                let mut stream = match messages.subscribe(None, filter.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("shm writer: failed to subscribe: {e}");
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_secs(1)) => continue 'subscribe,
                            () = shutdown_clone.cancelled() => return,
                        }
                    }
                };

                // Wait for first message with timeout. If we subscribed before any
                // data was pushed, the stream starts at a position that will never
                // be written. Resubscribing after data is flowing fixes this.
                let first = tokio::select! {
                    result = tokio::time::timeout(
                        Duration::from_secs(5),
                        stream.next(),
                    ) => {
                        match result {
                            Ok(Some(Ok(data))) => data,
                            Ok(Some(Err(RecvError::Lagged))) => {
                                info!("shm writer: source lagged, resubscribing");
                                continue 'subscribe;
                            }
                            Ok(Some(Err(RecvError::Closed))) | Ok(None) => {
                                info!("shm writer: source stream closed");
                                return;
                            }
                            Err(_) => {
                                // Timeout - position likely stale, resubscribe
                                continue 'subscribe;
                            }
                        }
                    }
                    () = shutdown_clone.cancelled() => return,
                };

                if tx.as_async().send(first).await.is_err() {
                    return;
                }

                // Normal read loop
                loop {
                    let item = tokio::select! {
                        item = stream.next() => item,
                        () = shutdown_clone.cancelled() => return,
                    };
                    match item {
                        Some(Ok(data)) => {
                            if tx.as_async().send(data).await.is_err() {
                                return;
                            }
                        }
                        Some(Err(RecvError::Lagged)) => {
                            info!("shm writer: source lagged, resubscribing");
                            continue 'subscribe;
                        }
                        Some(Err(RecvError::Closed)) | None => {
                            info!("shm writer: source stream closed");
                            return;
                        }
                    }
                }
            }
        });

        // Blocking writer thread: receives from channel, writes to mmap
        let send_mmap = SendMmap(mmap);
        let writer_jh = tokio::task::spawn_blocking(move || {
            let SendMmap(mut mmap) = send_mmap;
            let ptr = mmap.as_mut_ptr();
            let index_base_offset = HEADER_SIZE;
            let data_base_offset = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE;

            let mut pos: u64 = 0;
            let mut data_pos: u64 = 0;

            while let Ok(data) = rx.recv() {
                let data_bytes: &[u8] = &data;
                let data_len = data_bytes.len();

                // 1. Compute index slot
                let idx = (pos & idx_mask) as usize;

                // 2. Write data bytes into data ring with wrap-around
                let data_ring_offset = (data_pos % data_capacity as u64) as usize;
                // SAFETY: data region is within mmap bounds, we are the sole writer
                let data_ring = unsafe {
                    std::slice::from_raw_parts_mut(ptr.add(data_base_offset), data_capacity)
                };
                copy_to_ring(data_ring, data_ring_offset, data_bytes);

                // 3. Write index entry metadata
                // SAFETY: index region is within bounds
                let entry = unsafe {
                    ShmIndexEntry::from_ptr_mut(
                        ptr.add(index_base_offset + idx * INDEX_ENTRY_SIZE),
                    )
                };
                entry.data_off = data_pos;
                entry.data_len = data_len as u32;
                entry.slot = 0; // slot not tracked in shm

                // 4. Advance data position
                data_pos += data_len as u64;

                // 5. Update header.data_tail (Relaxed is safe: readers synchronize
                // via seq.load(Acquire) which pairs with seq.store(Release) below,
                // guaranteeing visibility of all prior writes including data_tail)
                let header = unsafe { ShmHeader::from_ptr(ptr) };
                header.data_tail.store(data_pos, Ordering::Relaxed);

                // 6. COMMIT: set seq to pos
                entry.seq.store(pos as i64, Ordering::Release);

                // 7. Advance position
                pos += 1;

                // 8. Update header.tail
                header.tail.store(pos, Ordering::Release);
            }

            // Mark closed
            let header = unsafe { ShmHeader::from_ptr(ptr) };
            header.closed.store(1, Ordering::Release);
            drop(mmap);
        });

        let jh = tokio::spawn(async move {
            let _ = reader_jh.await;
            let _ = writer_jh.await;
            drop(file);
            if let Err(error) = fs::remove_file(&path) {
                error!(path = %path.display(), %error, "failed to remove shm file");
            }
        });

        Ok(jh)
    }
}

// --- Bloom filter (256-bit, k=5) for transaction account keys ---

pub mod bloom256 {
    const K: u32 = 5;
    const M: u32 = 256;

    /// Insert a 32-byte key into a 256-bit bloom filter.
    /// Uses 5 hash functions derived from non-overlapping 4-byte slices of the key.
    #[inline]
    pub fn insert(filter: &mut [u8; 32], key: &[u8; 32]) {
        for i in 0..K as usize {
            let h = u32::from_le_bytes([
                key[i * 4],
                key[i * 4 + 1],
                key[i * 4 + 2],
                key[i * 4 + 3],
            ]) % M;
            filter[(h / 8) as usize] |= 1 << (h % 8);
        }
    }

    /// Check if a 32-byte key may be in the bloom filter.
    /// Returns `false` if the key is definitely not present (no false negatives).
    /// Returns `true` if the key might be present (possible false positives).
    #[inline]
    pub fn may_contain(filter: &[u8; 32], key: &[u8; 32]) -> bool {
        for i in 0..K as usize {
            let h = u32::from_le_bytes([
                key[i * 4],
                key[i * 4 + 1],
                key[i * 4 + 2],
                key[i * 4 + 3],
            ]) % M;
            if filter[(h / 8) as usize] & (1 << (h % 8)) == 0 {
                return false;
            }
        }
        true
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
        let idx_capacity: usize = 4;
        let data_capacity: usize = 64;
        let total_size = HEADER_SIZE + idx_capacity * INDEX_ENTRY_SIZE + data_capacity;
        let mut buf = vec![0u8; total_size];

        let ptr = buf.as_mut_ptr();

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

        let read_header = unsafe { ShmHeader::from_ptr(ptr as *const u8) };
        assert_eq!(read_header.tail.load(Ordering::Acquire), 1);

        let read_idx = (0u64 & idx_mask) as usize;
        let read_entry = unsafe {
            ShmIndexEntry::from_ptr(index_base.add(read_idx * INDEX_ENTRY_SIZE) as *const u8)
        };
        assert_eq!(read_entry.seq.load(Ordering::Acquire), 0);
        assert_eq!(read_entry.data_len, msg.len() as u32);

        let data_ring_ro = unsafe {
            std::slice::from_raw_parts(ptr.add(data_base_offset) as *const u8, data_capacity)
        };
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
        let idx_capacity: usize = 2;
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

        let reader_pos: u64 = 0;
        let idx = (reader_pos & idx_mask) as usize;
        let entry = unsafe {
            ShmIndexEntry::from_ptr(index_base.add(idx * INDEX_ENTRY_SIZE) as *const u8)
        };
        let seq = entry.seq.load(Ordering::Acquire);
        assert_ne!(seq, reader_pos as i64, "should detect lag: seq was overwritten");
        assert_eq!(seq, 2);
    }

    #[test]
    fn test_index_entry_v2_size() {
        assert_eq!(size_of::<ShmIndexEntryV2>(), INDEX_ENTRY_SIZE_V2);
        assert_eq!(INDEX_ENTRY_SIZE_V2, 128);
    }

    #[test]
    fn test_write_read_v2() {
        let idx_capacity: usize = 4;
        let data_capacity: usize = 128;
        let entry_size = INDEX_ENTRY_SIZE_V2;
        let total_size = HEADER_SIZE + idx_capacity * entry_size + data_capacity;
        let mut buf = vec![0u8; total_size];
        let ptr = buf.as_mut_ptr();
        let idx_mask = (idx_capacity - 1) as u64;

        // Initialize V2 header
        unsafe {
            let header = ShmHeader::from_ptr_mut(ptr);
            header.magic = MAGIC;
            header.version = VERSION_V2;
            header.idx_capacity = idx_capacity as u64;
            header.data_capacity = data_capacity as u64;
            header.tail = AtomicU64::new(0);
            header.data_tail = AtomicU64::new(0);
            header.closed = AtomicU64::new(0);
            header.index_entry_size = entry_size as u64;
        }

        let index_base = unsafe { ptr.add(HEADER_SIZE) };
        for i in 0..idx_capacity {
            unsafe {
                let entry_ptr = index_base.add(i * entry_size);
                std::ptr::write_bytes(entry_ptr, 0, entry_size);
                let seq_ptr = entry_ptr as *mut AtomicI64;
                (*seq_ptr) = AtomicI64::new(-1);
            }
        }

        // Write an account message with metadata
        let msg = b"account-data-here";
        let data_base_offset = HEADER_SIZE + idx_capacity * entry_size;
        let data_ring =
            unsafe { std::slice::from_raw_parts_mut(ptr.add(data_base_offset), data_capacity) };
        copy_to_ring(data_ring, 0, msg);

        let pos: u64 = 0;
        let idx = (pos & idx_mask) as usize;
        let entry = unsafe { ShmIndexEntryV2::from_ptr_mut(index_base.add(idx * entry_size)) };
        entry.data_off = 0;
        entry.data_len = msg.len() as u32;
        entry.msg_type = MSG_TYPE_ACCOUNT;
        entry.flags = FLAG_ACC_EXECUTABLE;
        entry.slot = 42;

        // Write a fake pubkey (32 bytes of 0xAA)
        entry.meta[0..32].copy_from_slice(&[0xAA; 32]);
        // Write a fake owner (32 bytes of 0xBB)
        entry.meta[32..64].copy_from_slice(&[0xBB; 32]);
        // Write lamports
        entry.meta[64..72].copy_from_slice(&1000u64.to_le_bytes());

        let header = unsafe { ShmHeader::from_ptr(ptr) };
        header
            .data_tail
            .store(msg.len() as u64, Ordering::Release);
        entry.seq.store(pos as i64, Ordering::Release);
        header.tail.store(pos + 1, Ordering::Release);

        // Read back and verify
        let read_entry = unsafe {
            ShmIndexEntryV2::from_ptr(index_base.add(idx * entry_size) as *const u8)
        };
        assert_eq!(read_entry.seq.load(Ordering::Acquire), 0);
        assert_eq!(read_entry.msg_type, MSG_TYPE_ACCOUNT);
        assert_eq!(read_entry.flags, FLAG_ACC_EXECUTABLE);
        assert_eq!(read_entry.slot, 42);
        assert_eq!(read_entry.data_len, msg.len() as u32);
        assert_eq!(read_entry.account_pubkey(), &[0xAA; 32]);
        assert_eq!(read_entry.account_owner(), &[0xBB; 32]);
        assert_eq!(read_entry.account_lamports(), 1000);

        // Verify data ring
        let data_ring_ro = unsafe {
            std::slice::from_raw_parts(ptr.add(data_base_offset) as *const u8, data_capacity)
        };
        let mut result = vec![0u8; msg.len()];
        copy_from_ring(data_ring_ro, 0, msg.len(), &mut result);
        assert_eq!(&result, msg);
    }

    #[test]
    fn test_write_read_v2_transaction() {
        let idx_capacity: usize = 4;
        let data_capacity: usize = 128;
        let entry_size = INDEX_ENTRY_SIZE_V2;
        let total_size = HEADER_SIZE + idx_capacity * entry_size + data_capacity;
        let mut buf = vec![0u8; total_size];
        let ptr = buf.as_mut_ptr();
        let idx_mask = (idx_capacity - 1) as u64;

        unsafe {
            let header = ShmHeader::from_ptr_mut(ptr);
            header.magic = MAGIC;
            header.version = VERSION_V2;
            header.idx_capacity = idx_capacity as u64;
            header.data_capacity = data_capacity as u64;
            header.tail = AtomicU64::new(0);
            header.data_tail = AtomicU64::new(0);
            header.closed = AtomicU64::new(0);
            header.index_entry_size = entry_size as u64;
        }

        let index_base = unsafe { ptr.add(HEADER_SIZE) };
        for i in 0..idx_capacity {
            unsafe {
                let entry_ptr = index_base.add(i * entry_size);
                std::ptr::write_bytes(entry_ptr, 0, entry_size);
                let seq_ptr = entry_ptr as *mut AtomicI64;
                (*seq_ptr) = AtomicI64::new(-1);
            }
        }

        let msg = b"tx-data";
        let data_base_offset = HEADER_SIZE + idx_capacity * entry_size;
        let data_ring =
            unsafe { std::slice::from_raw_parts_mut(ptr.add(data_base_offset), data_capacity) };
        copy_to_ring(data_ring, 0, msg);

        let pos: u64 = 0;
        let idx = (pos & idx_mask) as usize;
        let entry = unsafe { ShmIndexEntryV2::from_ptr_mut(index_base.add(idx * entry_size)) };
        entry.data_off = 0;
        entry.data_len = msg.len() as u32;
        entry.msg_type = MSG_TYPE_TRANSACTION;
        entry.flags = FLAG_TX_IS_VOTE | FLAG_TX_FAILED;
        entry.slot = 100;
        // Signature: 64 bytes of 0xCC
        entry.meta[0..64].copy_from_slice(&[0xCC; 64]);

        let header = unsafe { ShmHeader::from_ptr(ptr) };
        header.data_tail.store(msg.len() as u64, Ordering::Release);
        entry.seq.store(pos as i64, Ordering::Release);
        header.tail.store(pos + 1, Ordering::Release);

        let read_entry = unsafe {
            ShmIndexEntryV2::from_ptr(index_base.add(idx * entry_size) as *const u8)
        };
        assert_eq!(read_entry.msg_type, MSG_TYPE_TRANSACTION);
        assert_eq!(read_entry.flags & FLAG_TX_IS_VOTE, FLAG_TX_IS_VOTE);
        assert_eq!(read_entry.flags & FLAG_TX_FAILED, FLAG_TX_FAILED);
        assert_eq!(read_entry.tx_signature(), &[0xCC; 64]);
        assert_eq!(read_entry.slot, 100);
    }

    #[test]
    fn test_bloom256_insert_contains() {
        use super::bloom256;

        let mut filter = [0u8; 32];
        let key_a = [0xAAu8; 32];
        let key_b = [0xBBu8; 32];

        bloom256::insert(&mut filter, &key_a);
        bloom256::insert(&mut filter, &key_b);

        assert!(bloom256::may_contain(&filter, &key_a));
        assert!(bloom256::may_contain(&filter, &key_b));
    }

    #[test]
    fn test_bloom256_no_false_negatives() {
        use super::bloom256;

        let mut filter = [0u8; 32];
        // Insert 20 distinct keys
        let mut keys = Vec::new();
        for i in 0u8..20 {
            let mut key = [0u8; 32];
            key[0] = i;
            key[1] = i.wrapping_mul(7);
            key[4] = i.wrapping_mul(13);
            key[8] = i.wrapping_mul(19);
            key[12] = i.wrapping_mul(31);
            key[16] = i.wrapping_mul(37);
            keys.push(key);
            bloom256::insert(&mut filter, &key);
        }

        // Every inserted key must be found — zero false negatives
        for key in &keys {
            assert!(
                bloom256::may_contain(&filter, key),
                "false negative for key starting with {}",
                key[0]
            );
        }
    }

    #[test]
    fn test_bloom256_empty() {
        use super::bloom256;

        let filter = [0u8; 32];
        let key = [0x42u8; 32];
        // Empty filter should not contain anything
        assert!(!bloom256::may_contain(&filter, &key));
    }
}
