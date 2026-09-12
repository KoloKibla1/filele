# filele — fastest LAN file transfer for Windows (Rust)

Custom TCP protocol tuned for 1 / 2.5 / 10 GbE on Windows. Fast with **both**
large files (parallel sharded streams) and many small files (pipelined, no per-file RTT).

Measured on loopback (same PC, Windows, release build):

| workload | size | time | speed |
|---|---|---|---|
| 500 small + 20 MB + 100 MB mixed | 122 MB | 0.8 s | **168 MB/s (1.3 Gb/s)** |
| 1× 100 MB large (4 streams) | 100 MB | 0.3 s | **387 MB/s (3.1 Gb/s)** |
| 500 small files only | 2.1 MB | 0.4 s | ~1250 files/s |

All transfers verified byte-identical (blake2) + xxh3 end-to-end checksums, 0 mismatches.

## GUI (recommended)

Just double-click `filele.exe` — with no arguments it opens a friendly window.
Or run `filele gui`.

- Left sidebar: **Send**, **Receive**, plus one tab per started transfer
  (`"<name> send"`, name cut to 10 letters).
- **Send**: header, file/folder list (x removes), Add files / Add folder /
  Clear all buttons, target device (name + IP, filled by picking a device),
  Devices nearby list that refreshes itself constantly (Select fills the
  target), options (streams, checksum, compress, overwrite), Send button.
- **Transfer tab** (opens automatically on send): per-file list with a
  progress bar next to each file, plus an overall bar at the bottom with
  %, MB sent/total, speed, time left, and current file.
- **Receive**: header, Save to field + Browse, Start being visible button.
  While visible it shows your IPs/hostname, save folder, and incoming progress.
- Settings (target, folder, options) are remembered in `filele-gui.json`.

## Why it's fast

- **Tokio multi-thread (IOCP)** — overlaps network + disk.
- **4 MiB socket buffers** (`SO_SND/RCVBUF`) — overrides tiny Windows defaults.
- **`TCP_NODELAY` ON** — critical: protocol mixes 20 B headers + 4 MiB payloads.
  With Nagle ON each chunk stalled ~200 ms on delayed-ACK (measured 17 MB/s → 387 MB/s after fix).
- **Single `write_all` of header+payload** — halves syscalls, avoids Nagle stalls.
- **Small files: pipelined inline on control stream** — headers + bytes back-to-back,
  no ACK per file; receiver buffers ≤8 MiB and writes concurrently (16-way semaphore).
- **Large files (≥64 MiB): 4× parallel TCP streams, 4 MiB chunks, `seek_write` (pwrite)**
  — sequential disk read on sender (fast on HDD+SSD), parallel network, random-write
  reassembly on receiver via preallocated (`set_len`) files + per-connection handles.
- **xxh3 checksums** (~30 GB/s, negligible): inline files hashed upfront;
  sharded files hashed sequentially on sender, re-hashed sequentially from disk on
  receiver (parallel arrival order is *not* hashable — order-dependent).
- **Optional LZ4** (level 1, small files only, skips media/archives) — off by default for max speed.
- **No HTTP/TLS/JSON overhead** — manual little-endian framing.

## Quick start (two Windows PCs, same LAN)

1. Allow firewall (Admin PowerShell, once per PC):

   ```powershell
   .\allow-firewall.ps1
   ```

   Or manually allow TCP `53317-53318` + UDP `53319` inbound.

2. On receiver (destination):

   ```powershell
   .\target\release\filele.exe recv --out D:\incoming --overwrite
   ```

   Note local IPs printed (e.g. `10.0.0.2`).

3. On sender:

   ```powershell
   # find peers
   .\target\release\filele.exe discover --timeout 3

   # send files / dirs (preserves relative paths)
   .\target\release\filele.exe send .\movie.mkv .\photos\ --to 10.0.0.2 --overwrite

   # large-file tuning: 4 streams is optimal for 1-10 GbE; 0 = single stream
   .\target\release\filele.exe send .\4K-footage\ --to 10.0.0.2 --streams 4

   # max speed (skip checksums) / compress small text
   .\target\release\filele.exe send .\code\ --to 10.0.0.2 --no-checksum
   .\target\release\filele.exe send .\logs\ --to 10.0.0.2 --compress
   ```

Ports: control `53317`, data `53318` (= control+1), discovery UDP `53319`.
Override with `--port`.

## Path semantics

- `send <file>` → `out/<file_name>`
- `send <dir>` (single dir) → contents land directly in `out/` (i.e. `out/<rel>`).
- `send <a> <b> ...` → each top-level name preserved.
- `..` escapes rejected; symlinks skipped (Windows-safe).

## Build

Requires Rust **GNU toolchain on Windows** (no Visual Studio linker needed):

```powershell
# toolchain is pinned in rust-toolchain.toml to stable-x86_64-pc-windows-gnu
cargo build --release
# -> target\release\filele.exe (~2 MB, stripped, LTO thin)
```

`gcc` comes from WinLibs/MinGW (already on PATH if `where gcc` works).
If you have VS Build Tools, `stable-x86_64-pc-windows-msvc` also works (delete `rust-toolchain.toml`).

Release profile: `opt-level=3, lto="thin", codegen-units=1, strip, panic=abort`.

## Options

```
filele send <PATHS...> --to <IP> [--port 53317] [--streams 4] [--compress] [--no-checksum] [--overwrite]
filele recv [--bind 0.0.0.0] [--port 53317] [--out .] [--overwrite]
filele discover [--timeout 3]
```

- `--streams 0..8` — parallel data connections for files ≥64 MiB. Ignored if no large files.
- `--compress` — LZ4 small files ≤8 MiB (skips .zip/.mp4/.jpg/.exe/.iso/...). Default off.
- Checksums ON by default (xxh3). `--no-checksum` for absolute max speed.
- `--overwrite` — else existing files are skipped (sharded files still consume network but aren't truncated).

## Tuning for 10 GbE / NVMe

- Use wired Ethernet, MTU 9000 (jumbo) if both NICs + switch support it.
- `--streams 4` (default) is best for 1-10 GbE; try 8 on 25 GbE.
- NVMe → NVMe: keep checksums ON (cost ~3%); HDD → : single stream (`--streams 1`) can be kinder to seeks.
- Exclude `target\release\` + output dir from Windows Defender real-time scan for benchmarks
  (Defender dominates small-file create cost on Windows).
- Close VPNs / set Network profile to Private.

## Troubleshooting

- `connect timeout — is receiver running?` → start `recv` first, check IP (`discover`), check firewall.
- `receiver reported N checksum mismatches` → data correct but hash mismatch (shouldn't happen post-fix);
  retry; test with `--no-checksum` to isolate disk vs net; check RAM/disk health.
- Slow (<50 MB/s on GbE): firewall DPI / Wi-Fi / VPN / HDD seek / Defender. Try wired + `--no-checksum` + `--streams 4`.
- `bind ... port in use?` → another `recv` running or ports held in TIME_WAIT; `taskkill /F /IM filele.exe`.

## Protocol (v1, little-endian)

Control (`MAGIC u32=0x454C4C46, VER u16=1, FLAGS u16, TOKEN u64, NSTREAMS u8, NFILES u64, TOTAL u64`)
→ reply (`MAGIC, VER, STATUS u8, DATA_PORT u16`).
Then per entry: `DIR(0) | INLINE(1) | SHARDED(2)` + `FLAGS u8, PATH_LEN u16, PATH, SIZE u64, MTIME u64, MODE u32`
+ `XXH3 u64` (inline) / `NCHUNKS u32, CHUNK u32` (sharded), bytes inline or on data streams
(`FILE_IDX u64, OFFSET u64, LEN u32, bytes`). End: `0xFF + N u64 + (IDX, XXH3)*`. Final ack: `STATUS u8, MISMATCH u64`.
Data hello: `MAGIC, VER, TOKEN → STATUS`.

## Layout

```
src/
  main.rs      CLI (send/recv/discover)
  protocol.rs  framing + socket tuning (4 MiB bufs, NODELAY)
  sender.rs    walk (walkdir) + control + N data workers (combined writes)
  receiver.rs  control + data listeners, pipelined + pwrite reassembly, re-hash verify
  discovery.rs UDP broadcast 255.255.255.255:53319
```

License: MIT.
