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

## Branch: update-shm1 — SHM Transport Optimization

### What was done
4 optimizations to eliminate unnecessary hops in SHM data path:

1. **encode_into() methods** (`plugin-agave/src/protobuf/message.rs`)
   - `encode_into()`, `encode_into_with_timestamp()`, `encode_prost_into()`, `encode_raw_into()`
   - Encode into existing `Vec<u8>` buffer instead of allocating new one
   - `buf.clear()` reuses capacity — zero alloc after warmup

2. **encode_into tests** (`plugin-agave/src/protobuf/mod.rs`)
   - 5 tests: `test_encode_into_{account,block_meta,entry,slot,transaction}`
   - Verify `encode_into() == encode()` for both Prost and Raw encoders

3. **Sender refactor** (`plugin-agave/src/channel.rs`)
   - `push()` -> delegates to `push_inner()`
   - New `push_pre_encoded()` accepts already-encoded bytes (for combined mode)

4. **ShmDirectWriter** (`shared/src/transports/shm.rs`)
   - Direct mmap ring buffer writer, bypasses async channel/kanal
   - Extracted `init_shm_mmap()` shared init function (used by both ShmServer and ShmDirectWriter)
   - `write()`, `close()`, `remove_file()` methods

5. **Plugin integration** (`plugin-agave/src/plugin.rs`)
   - `dispatch()` method with 3 modes: SHM-only, channel-only, combined
   - Thread-local `ENCODE_BUF` for zero-alloc encoding
   - `PluginInner.messages` is now `Option<Sender>` (None in SHM-only mode)
   - `PluginInner.shm_direct: Option<Arc<ShmDirectWriter>>`
   - ShmServer::spawn() no longer called when ShmDirectWriter is used
   - All Geyser callbacks use `dispatch()` instead of `messages.push()`

### Build status
- `cargo check` — OK (0 errors, 0 warnings)
- `cargo test` — 19/19 passed
- `cargo build --release` — OK on tds2

### Performance gains (SHM-only mode)
| Metric | Before | After |
|--------|--------|-------|
| Alloc/msg | 1-2 (Vec+Arc) | 0 (after warmup) |
| Mutex locks/msg | 2 | 1 |
| Channel hops | 2 | 0 |
| memcpy | 2 | 1 |
| Thread switches | 2 | 0 |
