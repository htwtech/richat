# Richat Project

## Remote Build Server
- Host: `tds2` (104.37.188.2:40102, user bubba, key ~/.ssh/bubba.key)
- Repo path: `~/richat`
- Cargo: `source ~/.cargo/env && cd ~/richat && cargo build --release`
- Copy files: `scp -P 40102 -i ~/.ssh/bubba.key <local> bubba@104.37.188.2:~/richat/<remote>`

## Build-time Version Override

GetVersion response can be customized via environment variables at build time:

```bash
RICHAT_DISPLAY_VERSION="9.0.0-custom" RICHAT_DISPLAY_PACKAGE="my-richat" cargo build --release
```

| Variable | What it changes | Default |
|---|---|---|
| `RICHAT_DISPLAY_VERSION` | version in GetVersion | `CARGO_PKG_VERSION` |
| `RICHAT_DISPLAY_PACKAGE` | package name | `CARGO_PKG_NAME` |
| `RICHAT_DISPLAY_PROTO` | proto version | `yellowstone-grpc-proto` version from Cargo.lock |
| `RICHAT_DISPLAY_HOSTNAME` | show hostname (`true`/`false`) | `true` |

Defined in `richat/src/version.rs` via `option_env!()`.

## Branch: shm-metadata-v2 — SHM V2 Metadata + Zero-Parse Pre-Filtering

Full status, next steps, and file list: **`docs/shm-metadata-v2-status.md`**

### Summary
- SHM V2 index entries (128B): inline msg_type, flags, slot, meta[96]
- Transaction bloom filter (256-bit, k=5) in meta[64..96] for account pre-filtering
- `extract_shm_meta()` in plugin builds V2 metadata from ProtobufMessage
- Benchmark: bloom pre-filter is **~1370× faster** than full protobuf parse for non-matching txs
- Architecture doc: `docs/architecture-shm-v2.md`
- gRPC pricing analysis: `docs/grpc-subscription-load-ranking.md`

### Key files changed
- `shared/src/transports/shm.rs` — ShmIndexEntryV2, bloom256, ShmWriteMeta
- `plugin-agave/src/plugin.rs` — extract_shm_meta(), dispatch() with V2
- `richat/src/source.rs` — ShmPreFilter with bloom check
- `richat/src/config.rs` — ConfigShmPreFilter
- `benches/benches/bloom-prefilter/` — criterion benchmark (NEW)

## Previous branch: update-shm1 — SHM Transport Optimization

Merged into shm-metadata-v2. Key optimizations:
- `encode_into()` — zero-alloc encoding into reusable buffer
- `ShmDirectWriter` — direct mmap write, bypasses async channels
- `dispatch()` — 3 modes: SHM-only, channel-only, combined
- `push_pre_encoded()` — encode once for both SHM and gRPC/QUIC
