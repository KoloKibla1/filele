//! Wire protocol + socket tuning shared by sender and receiver.
//!
//! Design goals:
//! - near-zero per-file overhead (pipelined headers, no per-file RTT)
//! - large files sharded over N parallel TCP streams (4 MB chunks)
//! - small files inlined on the control stream (batched back-to-back)
//! - xxh3 checksums (GB/s, negligible overhead), optional lz4 for small files
//! - v2: receiver approval — sender shows its name + file offer first,
//!   streams bytes only after the receiver allows (CLI receivers auto-allow).

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const MAGIC: u32 = 0x454C_4C46; // "FLLE" little-endian
pub const VERSION: u16 = 2;

pub const DEFAULT_PORT: u16 = 53317;
/// Data port is always control_port + 1 (parallel streams for large files).
pub fn data_port(control_port: u16) -> u16 {
    control_port.wrapping_add(1)
}
pub const DISCOVERY_PORT: u16 = 53319;
pub const DISCOVERY_MAGIC: &[u8] = b"FLLE_DISCOVER_v1";
pub const DISCOVERY_REPLY_PREFIX: &[u8] = b"FLLE_HERE_v1 ";

/// Entry types on the control stream.
pub const ENTRY_DIR: u8 = 0;
pub const ENTRY_FILE_INLINE: u8 = 1;
pub const ENTRY_FILE_SHARDED: u8 = 2;
pub const ENTRY_END: u8 = 0xFF;

/// Verdict sent by the receiver after the offer (allow / deny).
pub const VERDICT_ALLOW: u8 = 1;
pub const VERDICT_DENY: u8 = 0;

/// Offer entry kinds (mirror ENTRY_DIR / ENTRY_FILE_* for files).
pub const OFFER_DIR: u8 = 0;
pub const OFFER_FILE: u8 = 1;

/// Per-file flags.
pub const F_COMPRESSED: u8 = 0x01;

/// Handshake flags (u16).
pub const H_COMPRESS: u16 = 0x01;
pub const H_CHECKSUM: u16 = 0x02;
pub const H_OVERWRITE: u16 = 0x04;

/// Tuning constants — chosen for 1/2.5/10 GbE on Windows.
pub const CHUNK_SIZE: u32 = 4 * 1024 * 1024; // 4 MiB shards
pub const LARGE_THRESHOLD: u64 = 64 * 1024 * 1024; // >=64 MiB -> parallel streams
pub const INLINE_MEM_LIMIT: u64 = 8 * 1024 * 1024; // <=8 MiB buffered for pipelining
pub const SOCKET_BUF: u32 = 4 * 1024 * 1024; // 4 MiB SO_SND/RCVBUF
pub const MAX_PATH_LEN: usize = 16 * 1024;

/// True for transport drops mid-transfer (peer went away / stopped).
/// Used to report "Stopped by ..." instead of a raw EOF/reset error.
pub fn is_disconnect(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind;
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().map(|io| {
            matches!(io.kind(), ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted)
        })
        .unwrap_or(false)
    })
}
/// Tune a connected Tokio TcpStream for throughput on Windows (IOCP).
/// - 4 MiB send/recv buffers (overrides tiny Windows defaults)
/// - Nagle OFF (nodelay=true): our protocol mixes tiny headers (20 B) + big
///   payloads (4 MiB). With Nagle ON, the header stalls ~200 ms on delayed-ACK.
///   Disabling Nagle is critical for chunked/pipelined speed; bulk throughput
///   is unaffected because we send MB-sized buffers.
/// - keepalive on
pub fn tune_stream(s: &TcpStream) -> Result<()> {
    use std::os::windows::io::AsRawSocket;
    let sock = socket2::SockRef::from(s);
    let _ = sock.set_send_buffer_size(SOCKET_BUF as usize);
    let _ = sock.set_recv_buffer_size(SOCKET_BUF as usize);
    let _ = sock.set_nodelay(true);
    let _ = sock.set_keepalive(true);
    // Non-blocking already set by tokio; ensure raw socket valid.
    let _ = s.as_raw_socket();
    Ok(())
}

/// Extensions that skip compression (already compressed / media).
pub fn is_incompressible(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    const EXTS: &[&str] = &[
        ".zip", ".7z", ".rar", ".gz", ".bz2", ".xz", ".zst", ".mp4", ".mkv", ".avi", ".mov",
        ".mp3", ".flac", ".jpg", ".jpeg", ".png", ".webp", ".gif", ".pdf", ".exe", ".msi",
        ".iso", ".vhd", ".vhdx", ".qcow2", ".parquet", ".lz4", ".br",
    ];
    EXTS.iter().any(|e| lower.ends_with(e))
}

pub async fn write_u8(w: &mut (impl AsyncWriteExt + Unpin), v: u8) -> Result<()> {
    w.write_all(&[v]).await?;
    Ok(())
}
pub async fn write_u16(w: &mut (impl AsyncWriteExt + Unpin), v: u16) -> Result<()> {
    w.write_all(&v.to_le_bytes()).await?;
    Ok(())
}
pub async fn write_u32(w: &mut (impl AsyncWriteExt + Unpin), v: u32) -> Result<()> {
    w.write_all(&v.to_le_bytes()).await?;
    Ok(())
}
pub async fn write_u64(w: &mut (impl AsyncWriteExt + Unpin), v: u64) -> Result<()> {
    w.write_all(&v.to_le_bytes()).await?;
    Ok(())
}
pub async fn read_u8(r: &mut (impl AsyncReadExt + Unpin)) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).await.context("eof reading u8")?;
    Ok(b[0])
}
pub async fn read_u16(r: &mut (impl AsyncReadExt + Unpin)) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b).await.context("eof reading u16")?;
    Ok(u16::from_le_bytes(b))
}
pub async fn read_u32(r: &mut (impl AsyncReadExt + Unpin)) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).await.context("eof reading u32")?;
    Ok(u32::from_le_bytes(b))
}
pub async fn read_u64(r: &mut (impl AsyncReadExt + Unpin)) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).await.context("eof reading u64")?;
    Ok(u64::from_le_bytes(b))
}

/// Normalize a relative path to `/` separators for the wire.
pub fn rel_to_wire(rel: &std::path::Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Convert wire path to OS path joined under base. Rejects `..` escapes.
pub fn wire_to_path(base: &std::path::Path, wire: &str) -> Result<std::path::PathBuf> {
    if wire.is_empty() || wire.len() > MAX_PATH_LEN {
        anyhow::bail!("bad path len");
    }
    let mut out = base.to_path_buf();
    for comp in wire.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            anyhow::bail!("path escape: {}", wire);
        }
        // Strip Windows-illegal trailing dots/spaces? Keep as-is; OS will error.
        out.push(comp);
    }
    Ok(out)
}

/// One entry of the pre-transfer offer (shown on the receiver for approval).
#[derive(Debug, Clone)]
pub struct OfferFile {
    /// OFFER_DIR or OFFER_FILE.
    pub kind: u8,
    /// Relative path with `/` separators.
    pub rel: String,
    /// Byte size (0 for dirs).
    pub size: u64,
}

/// Sender side: write the offer block (file list for approval).
/// Wire: NFILES u64, then per entry KIND u8, PATH_LEN u16, PATH, SIZE u64.
pub async fn write_offer(w: &mut (impl AsyncWriteExt + Unpin), files: &[OfferFile]) -> Result<()> {
    write_u64(w, files.len() as u64).await?;
    for f in files {
        write_u8(w, f.kind).await?;
        if f.rel.len() > MAX_PATH_LEN {
            anyhow::bail!("path too long: {}", f.rel);
        }
        write_u16(w, f.rel.len() as u16).await?;
        w.write_all(f.rel.as_bytes()).await?;
        write_u64(w, f.size).await?;
    }
    Ok(())
}

/// Receiver side: read the offer block.
pub async fn read_offer(r: &mut (impl AsyncReadExt + Unpin)) -> Result<Vec<OfferFile>> {
    let n = read_u64(r).await? as usize;
    if n > 10_000_000 {
        anyhow::bail!("offer too big: {} entries", n);
    }
    let mut out = Vec::with_capacity(n.min(65536));
    for _ in 0..n {
        let kind = read_u8(r).await?;
        if kind != OFFER_DIR && kind != OFFER_FILE {
            anyhow::bail!("bad offer kind {}", kind);
        }
        let plen = read_u16(r).await? as usize;
        if plen == 0 || plen > MAX_PATH_LEN {
            anyhow::bail!("bad path len");
        }
        let mut pbuf = vec![0u8; plen];
        r.read_exact(&mut pbuf).await.context("eof reading offer path")?;
        let rel = String::from_utf8(pbuf).context("bad utf8 offer path")?;
        if rel.split('/').any(|c| c == "..") {
            anyhow::bail!("path escape: {}", rel);
        }
        let size = read_u64(r).await?;
        out.push(OfferFile { kind, rel, size });
    }
    Ok(out)
}

/// Write a length-prefixed UTF-8 name (sender hostname in the v2 handshake).
pub async fn write_name(w: &mut (impl AsyncWriteExt + Unpin), name: &str) -> Result<()> {
    let b = name.as_bytes();
    if b.len() > 1024 {
        anyhow::bail!("name too long");
    }
    write_u16(w, b.len() as u16).await?;
    w.write_all(b).await?;
    Ok(())
}

/// Read a length-prefixed UTF-8 name.
pub async fn read_name(r: &mut (impl AsyncReadExt + Unpin)) -> Result<String> {
    let len = read_u16(r).await? as usize;
    if len > 1024 {
        anyhow::bail!("name too long: {}", len);
    }
    let mut b = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut b).await.context("eof reading name")?;
    }
    Ok(String::from_utf8_lossy(&b).into_owned())
}
