# Richat SHM Transport: mmap-based Inter-Process Messaging

## What Is This

Richat streams Solana blockchain data (accounts, transactions, entries, slots) from validators
to consumers. It supports three transport layers: **gRPC**, **QUIC**, and **SHM** (shared memory).

SHM is the fastest transport. It uses a memory-mapped file (`/dev/shm/richat`) as a lock-free
ring buffer between processes on the same machine. No sockets, no serialization overhead on
the read path, no kernel network stack. One writer, many readers.

## Why mmap

```
Transport   Latency     Copies    CPU overhead    Use case
─────────   ─────────   ──────    ────────────    ────────────────────
gRPC        1-10ms      2+        high            remote, filtered
QUIC        100us+      1         medium          remote, encrypted
SHM/mmap    <1us        1         minimal         same machine
```

Solana produces ~50K-100K messages/second. On the same machine (plugin-agave -> richat),
SHM eliminates network stack entirely. The data goes: mmap write -> page cache -> mmap read.

## Data Flow

```
┌──────────────────────────┐
│     Solana Validator      │
│                           │
│   plugin-agave            │  Geyser plugin encodes messages
│   (Geyser Plugin)         │  to protobuf, pushes to ring buffer
│         │                 │
│         ▼                 │
│   SHM Writer Thread       │  Writes protobuf bytes to mmap
│         │                 │
│         ▼                 │
│   /dev/shm/richat         │  Memory-mapped file on tmpfs
│   (ring buffer, 16 GiB)  │
└────┬────────┬────────┬────┘
     │        │        │
     │  same physical pages (OS page cache)
     │  no copies between processes
     │        │        │
     │   mmap read-only │
     │        │        │
┌────▼───┐ ┌──▼─────┐ ┌▼───────┐
│richat 1│ │richat 2│ │richat N│   N independent processes,
│        │ │        │ │        │   each opens the SAME file
│ shm-   │ │ shm-   │ │ shm-   │   with mmap(MAP_PRIVATE,
│ reader │ │ reader │ │ reader │   PROT_READ)
│   │    │ │   │    │ │   │    │
│   ▼    │ │   ▼    │ │   ▼    │
│ parse  │ │ parse  │ │ parse  │   protobuf decode in
│ proto  │ │ proto  │ │ proto  │   reader thread
│   │    │ │   │    │ │   │    │
│   ▼    │ │   ▼    │ │   ▼    │
│ gRPC/  │ │ gRPC/  │ │ gRPC/  │   encode & serve
│ QUIC   │ │ QUIC   │ │ QUIC   │   to remote clients
│ serve  │ │ serve  │ │ serve  │
└────────┘ └────────┘ └────────┘
     │          │          │
     ▼          ▼          ▼
  clients    clients    clients
```

All richat instances mmap the same `/dev/shm/richat` file read-only.
The OS maps identical physical RAM pages into each process — zero-copy.
Each instance independently parses and serves its own gRPC/QUIC clients.

## mmap File Layout

```
┌─────────────────────────────────────────────────────────────────┐
│ Header (64 bytes)                                               │
│   magic=RICHATSH  version=1  idx_capacity  data_capacity        │
│   tail (AtomicU64)  data_tail (AtomicU64)  closed (AtomicU64)   │
├─────────────────────────────────────────────────────────────────┤
│ Index Ring (idx_capacity entries x 32 bytes each)               │
│                                                                 │
│   [seq:i64 | data_off:u64 | data_len:u32 | _pad:u32 | slot:u64]│
│   [seq:i64 | data_off:u64 | data_len:u32 | _pad:u32 | slot:u64]│
│   ...                                                           │
├─────────────────────────────────────────────────────────────────┤
│ Data Ring (data_capacity bytes, circular)                       │
│                                                                 │
│   raw protobuf message bytes, written with wrap-around          │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘

Default sizes:
  idx_capacity  = 2M entries  (next power of 2 from max_messages)
  data_capacity = 16 GiB      (next power of 2 from max_bytes)
```

Both capacities are power-of-2, enabling bitwise AND for index/offset calculation
instead of modulo (branchless, single cycle).

## Lock-Free Protocol

Single-writer, multi-reader. No mutexes. Synchronization via atomics only.

**Writer** (dedicated OS thread):
```
1. copy_to_ring(data_ring, data_pos % capacity, message_bytes)
2. entry.data_off = data_pos
   entry.data_len = len
3. header.data_tail.store(data_pos + len, Relaxed)
4. entry.seq.store(pos, Release)          ← COMMIT POINT
5. header.tail.store(pos + 1, Release)
```

**Reader** (dedicated OS thread per consumer):
```
1. tail = header.tail.load(Acquire)       ← new data available?
2. seq = entry.seq.load(Acquire)          ← committed? not overwritten?
3. read data_off, data_len from entry
4. check: data_tail - data_off <= capacity  ← data still valid?
5. copy_from_ring(data_ring, ..., buf)    ← copy message bytes
6. fence(Acquire)                         ← seqlock barrier
7. re-check seq and data_tail (Relaxed)   ← was data stable during copy?
```

If any check fails at steps 2/4/7 — reader has been lapped (lag detected), it exits.

## What We Changed

### Problem

The original SHM transport had several issues:
- Writer ran as async task on tokio, holding mmap pointers across await points
- Reader used `thread::yield_now()` in a tight loop — 99.5% CPU per reader thread
- Two nearly identical read loops (duplicated ~65 lines each)
- No cross-crate inlining of hot-path functions
- Unnecessary memory zeroing on buffer allocation

### Changes Made

**Server side** (`shared/src/transports/shm.rs`):

| Change | Why |
|--------|-----|
| Split into async reader task + blocking writer thread | mmap writes off tokio; `SendMmap` wrapper for Send safety |
| kanal bounded(4096) channel between them | Bridge async stream to blocking thread |
| Resubscribe loop with 5s timeout | Handle stale initial position when source not yet producing |
| `ConfigShmFilter` serde struct | Clean config deserialization for filter options |
| `#[inline]` on `copy_from_ring`, `copy_to_ring` | Enable cross-crate inlining (shared -> client) |
| `#[inline]` on `from_ptr`, `from_ptr_mut` | Same; without it, Rust won't inline across crates without LTO |

**Client side** (`client/src/shm.rs`):

| Change | Why |
|--------|-----|
| `subscribe_map(transform)` | Run parse inline in reader thread, eliminate channel hop + tokio::spawn |
| `MmapOptions::map_raw_read_only()` | Explicit read-only mapping |
| `Vec::with_capacity` + `set_len` | Skip zeroing, buffer fully overwritten by `copy_from_ring` |
| `try_read_next()` with `#[inline(always)]` | Extract common read logic, eliminate duplication |
| `ReadResult` enum | Clean control flow for read_loop and read_loop_map |
| Adaptive backoff: spin -> yield -> sleep | Fix 99.5% CPU waste (see below) |

**Source integration** (`richat/src/source.rs`):

| Change | Why |
|--------|-----|
| SHM path uses `subscribe_map` with inline parsing | One fewer channel hop, parsing on OS thread not tokio |
| Returns `kanal::Receiver.to_async()` directly | Skip `BoxStream` + `tokio::spawn` overhead |

### Adaptive Backoff Detail

```
Step 0-6:   hint::spin_loop()  x 1,2,4,8,16,32,64   (~5us total)
            PAUSE instruction, no syscall, no context switch

Step 7-10:  thread::yield_now()  x 4
            Lets other threads run if they're waiting

Step 11+:   thread::sleep(Duration::from_micros(1))
            Releases CPU. Actual sleep ~1-50us depending on kernel.

On successful read: reset step to 0.
```

Result: **99.5% -> ~0-2% CPU** per idle reader. When data is flowing, reader
stays in spin phase (step 0), adding ~0 latency.

Worst case added latency: ~50us on first message after a long idle gap
(between Solana slots). All subsequent messages in the burst: ~0.

## Future Optimization: Futex Wakeup

The sleep-based backoff trades ~50us worst-case latency for CPU savings.
To get both zero CPU and zero latency, use Linux futex directly on the
shared memory atomic:

```
Writer:
  header.tail.store(pos + 1, Release)
  syscall(SYS_futex, &header.tail, FUTEX_WAKE, 1)    // ~200ns

Reader:
  // spin phase (same as now)
  // if still no data:
  syscall(SYS_futex, &header.tail, FUTEX_WAIT, current_tail, timeout=1ms)
  // kernel suspends until tail changes
  // wakeup latency: ~200-500ns
```

Why this works with mmap:
- futex operates on a **physical memory address**, not virtual
- Multiple processes mapping the same file share the same physical pages
- `FUTEX_WAIT/WAKE` on the same physical address works across processes
- No file descriptors or pipes needed

Trade-offs:
- Linux-only (need `libc::syscall`)
- Writer pays ~200ns per message for `FUTEX_WAKE`
- More code complexity
- Need platform fallback (sleep-based backoff for non-Linux)

This is the logical next step if ~50us latency spikes are unacceptable.

## Files

```
shared/src/transports/shm.rs   SHM server, ring buffer, header/index structs, copy helpers
client/src/shm.rs               SHM client reader, subscribe_map(), adaptive backoff
richat/src/source.rs             Source subscription, SHM inline parsing integration
richat/src/channel.rs            SharedChannel ring buffer (source of data for SHM server)
filter/src/message.rs            Message types and parsing (Limited/Prost)
```
