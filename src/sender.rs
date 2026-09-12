//! Sender: walks inputs, opens control + parallel data streams, pipelines files.

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::time::Instant;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::progress::{report_done, report_error, report_file, report_list, report_status, ProgressTx, TransferFile};
use crate::protocol::*;

pub struct SendOptions {
    pub target: String,
    pub port: u16,
    pub streams: u8,
    pub compress: bool,
    pub checksum: bool,
    pub overwrite: bool,
    /// Shown on the receiver ("Allow files from X?"). Defaults to this PC's hostname.
    pub sender_name: String,
}

/// This PC's hostname for the approval prompt on the receiver.
pub fn my_sender_name() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown PC".to_string())
}

struct Job {
    abs: PathBuf,
    rel: String,
    size: u64,
    mtime: u64,
    mode: u32,
    is_dir: bool,
}

fn file_mtime_mode(md: &std::fs::Metadata) -> (u64, u32) {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (mtime, md.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        (mtime, 0o644)
    }
}

/// Collect jobs from CLI paths. Preserves top-level names:
/// - if single dir given, its *contents* are sent (rel = stripped prefix)
/// - if multiple / files, rel = file_name or dir_name + subtree
fn collect_jobs(inputs: &[PathBuf]) -> Result<(Vec<Job>, u64)> {
    let mut jobs: Vec<Job> = Vec::new();
    let mut total: u64 = 0;
    for input in inputs {
        let md = std::fs::metadata(input)
            .with_context(|| format!("stat {}", input.display()))?;
        if md.is_file() {
            let name = input
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            let (mtime, mode) = file_mtime_mode(&md);
            total += md.len();
            jobs.push(Job {
                abs: input.clone(),
                rel: name,
                size: md.len(),
                mtime,
                mode,
                is_dir: false,
            });
        } else if md.is_dir() {
            let base = input;
            // If only one input and it's a dir, send contents (not the dir itself).
            let single_root = inputs.len() == 1;
            for entry in walkdir::WalkDir::new(base)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let p = entry.path();
                if p == base.as_path() {
                    continue;
                }
                let rel_path = p.strip_prefix(base).unwrap();
                let rel = rel_to_wire(rel_path);
                let m = match std::fs::symlink_metadata(p) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if m.file_type().is_symlink() {
                    continue; // skip symlinks for speed/simplicity on Windows
                }
                let (mtime, mode) = file_mtime_mode(&m);
                if m.is_dir() {
                    jobs.push(Job {
                        abs: p.to_path_buf(),
                        rel,
                        size: 0,
                        mtime,
                        mode,
                        is_dir: true,
                    });
                } else if m.is_file() {
                    total += m.len();
                    jobs.push(Job {
                        abs: p.to_path_buf(),
                        rel,
                        size: m.len(),
                        mtime,
                        mode,
                        is_dir: false,
                    });
                }
            }
            // Deterministic order: dirs first then files sorted — helps receiver pre-create dirs.
            let _ = single_root;
        }
    }
    // Sort: dirs first, then by rel for determinism.
    jobs.sort_by(|a, b| {
        (!a.is_dir, &a.rel)
            .partial_cmp(&(!b.is_dir, &b.rel))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok((jobs, total))
}

type ChunkMsg = (u64, u64, Vec<u8>); // (file_idx, offset, bytes)

async fn data_worker(mut stream: TcpStream, mut rx: mpsc::Receiver<ChunkMsg>) -> Result<()> {
    while let Some((file_idx, offset, bytes)) = rx.recv().await {
        // Single write_all of header+payload: avoids Nagle delayed-ACK stall
        // (20 B header alone would stall ~200 ms on Windows) and halves syscalls.
        // One 4 MiB memcpy here costs ~0.2 ms (20 GB/s), negligible vs network.
        let mut combined = Vec::with_capacity(20 + bytes.len());
        combined.extend_from_slice(&file_idx.to_le_bytes());
        combined.extend_from_slice(&offset.to_le_bytes());
        combined.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        combined.extend_from_slice(&bytes);
        stream.write_all(&combined).await?;
    }
    stream.flush().await?;
    // Half-close write side so receiver sees EOF after all chunks.
    let _ = stream.shutdown().await;
    Ok(())
}

pub async fn run_send(files: Vec<PathBuf>, opt: SendOptions) -> Result<()> {
    run_send_with_progress(files, opt, None).await
}

/// A data worker died: join the workers and return the first real (usually
/// TCP) error instead of "channel closed", so a killed receiver reports as
/// "Stopped by receiver" and the tab auto-closes on both sides.
async fn worker_failure(handles: Vec<tokio::task::JoinHandle<Result<()>>>) -> anyhow::Error {
    let mut first: Option<anyhow::Error> = None;
    for mut h in handles {
        tokio::select! {
            r = &mut h => {
                if let Ok(Err(e)) = r {
                    if first.is_none() {
                        first = Some(e);
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                h.abort();
            }
        }
    }
    first.unwrap_or_else(|| anyhow::anyhow!("data worker gone"))
}

pub async fn run_send_with_progress(
    files: Vec<PathBuf>,
    opt: SendOptions,
    gui: Option<ProgressTx>,
) -> Result<()> {
    let (jobs, total_bytes) = collect_jobs(&files)?;
    if jobs.is_empty() {
        anyhow::bail!("nothing to send (no files found)");
    }
    let num_large = jobs.iter().filter(|j| !j.is_dir && j.size >= LARGE_THRESHOLD).count();
    let num_files = jobs.iter().filter(|j| !j.is_dir).count();

    println!(
        "Found {} entries ({} files, {} large >=64MiB), total {}",
        jobs.len(),
        num_files,
        num_large,
        human_bytes(total_bytes)
    );

    let target = resolve_target(&opt.target, opt.port).await?;
    println!("Connecting to {} ...", target);

    let mut ctrl = tokio::time::timeout(std::time::Duration::from_secs(10), TcpStream::connect(target))
        .await
        .context("connect timeout — is receiver running (`filele recv`)?")??;
    tune_stream(&ctrl)?;
    // Bigger write buffer via socket2 already; also disable Nagle? keep Nagle ON for throughput.

    let token: u64 = rand::random();
    let mut nstreams = opt.streams.min(8);
    if num_large == 0 {
        nstreams = 0; // no need for parallel streams
    }
    let mut flags: u16 = 0;
    if opt.compress {
        flags |= H_COMPRESS;
    }
    if opt.checksum {
        flags |= H_CHECKSUM;
    }
    if opt.overwrite {
        flags |= H_OVERWRITE;
    }

    // Handshake (v2: sender name appended so the receiver can ask for approval)
    {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&token.to_le_bytes());
        buf.push(nstreams);
        buf.extend_from_slice(&(jobs.len() as u64).to_le_bytes());
        buf.extend_from_slice(&total_bytes.to_le_bytes());
        ctrl.write_all(&buf).await?;
        let name = if opt.sender_name.trim().is_empty() { my_sender_name() } else { opt.sender_name.clone() };
        write_name(&mut ctrl, name.trim()).await?;
        ctrl.flush().await?;
    }
    // Reply: MAGIC u32, VERSION u16, STATUS u8, DATA_PORT u16
    let status: u8;
    let data_port_num: u16;
    {
        let magic = read_u32(&mut ctrl).await?;
        let ver = read_u16(&mut ctrl).await?;
        if magic != MAGIC || ver != VERSION {
            anyhow::bail!("protocol mismatch from receiver");
        }
        status = read_u8(&mut ctrl).await?;
        data_port_num = read_u16(&mut ctrl).await?;
        if status != 0 {
            anyhow::bail!("receiver rejected transfer (status={})", status);
        }
    }
    println!("Handshake OK (data_port={}, streams={})", data_port_num, nstreams);

    // Offer: file list for the receiver's allow/deny prompt, then verdict.
    // GUI file list (files only, in send order) + per-file positions.
    let gui_files: Vec<TransferFile> = jobs
        .iter()
        .filter(|j| !j.is_dir)
        .map(|j| TransferFile { name: j.rel.clone(), size: j.size })
        .collect();
    report_list(&gui, total_bytes, "Waiting for approval...", gui_files);
    {
        let offer: Vec<OfferFile> = jobs
            .iter()
            .map(|j| OfferFile {
                kind: if j.is_dir { OFFER_DIR } else { OFFER_FILE },
                rel: j.rel.clone(),
                size: j.size,
            })
            .collect();
        write_offer(&mut ctrl, &offer).await?;
        ctrl.flush().await?;
    }
    println!("Waiting for receiver approval...");
    report_status(&gui, 0, total_bytes, "Waiting for approval...");
    let verdict = read_u8(&mut ctrl).await.context("eof waiting for receiver approval")?;
    if verdict != VERDICT_ALLOW {
        report_error(&gui, 0, total_bytes, crate::progress::DECLINED);
        anyhow::bail!("receiver declined the transfer");
    }
    println!("Approved — sending...");

    // Open data connections
    let mut txs: Vec<mpsc::Sender<ChunkMsg>> = Vec::new();
    let mut worker_handles = Vec::new();
    if nstreams > 0 {
        let data_addr = {
            let mut a = target;
            a.set_port(data_port_num);
            a
        };
        for _ in 0..nstreams {
            let mut ds = tokio::time::timeout(std::time::Duration::from_secs(10), TcpStream::connect(data_addr))
                .await
                .context("data connect timeout")??;
            tune_stream(&ds)?;
            // data hello: MAGIC + VERSION + TOKEN
            let mut hello = Vec::with_capacity(14);
            hello.extend_from_slice(&MAGIC.to_le_bytes());
            hello.extend_from_slice(&VERSION.to_le_bytes());
            hello.extend_from_slice(&token.to_le_bytes());
            ds.write_all(&hello).await?;
            ds.flush().await?;
            let ack = read_u8(&mut ds).await?;
            if ack != 0 {
                anyhow::bail!("data stream rejected (token mismatch?)");
            }
            let (tx, rx) = mpsc::channel::<ChunkMsg>(4);
            txs.push(tx);
            worker_handles.push(tokio::spawn(data_worker(ds, rx)));
        }
        println!("Opened {} parallel data streams", nstreams);
    }

    let pb = ProgressBar::new(total_bytes);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );

    let t0 = Instant::now();
    let mut rr: usize = 0; // round-robin
    let mut sharded_hashes: Vec<(u64, u64)> = Vec::new(); // (file_idx, xxh3)
    let mut sent_bytes: u64 = 0;

    let mut file_idx: u64 = 0;
    let mut gui_pos: usize = 0;
    report_status(&gui, 0, total_bytes, "Starting...");
    for job in &jobs {
        if job.is_dir {
            write_u8(&mut ctrl, ENTRY_DIR).await?;
            write_u8(&mut ctrl, 0).await?;
            write_u16(&mut ctrl, job.rel.len() as u16).await?;
            ctrl.write_all(job.rel.as_bytes()).await?;
            write_u64(&mut ctrl, 0).await?;
            write_u64(&mut ctrl, job.mtime).await?;
            write_u32(&mut ctrl, job.mode).await?;
            file_idx += 1;
            continue;
        }
        let large = job.size >= LARGE_THRESHOLD && nstreams > 0;
        if !large {
            // Inline path: buffer whole file (up to 64 MiB), hash, optionally compress.
            let data = tokio::fs::read(&job.abs)
                .await
                .with_context(|| format!("read {}", job.abs.display()))?;
            if data.len() as u64 != job.size {
                // File changed during walk; use actual len.
            }
            let orig_len = data.len() as u64;
            let hash = if opt.checksum {
                xxhash_rust::xxh3::xxh3_64(&data)
            } else {
                0
            };
            let mut flags_f: u8 = 0;
            let mut payload: &[u8] = &data;
            let compressed: Vec<u8>;
            if opt.compress && orig_len > 1024 && orig_len <= INLINE_MEM_LIMIT && !is_incompressible(&job.rel) {
                let c = lz4_flex::compress_prepend_size(&data);
                if (c.len() as u64) < orig_len {
                    flags_f |= F_COMPRESSED;
                    compressed = c;
                    payload = &compressed;
                } else {
                    compressed = Vec::new();
                    let _ = compressed;
                    // keep uncompressed; need owned to live long enough — use data
                    payload = &data;
                    // trick: we already have `compressed` empty; reassign below
                    write_u8(&mut ctrl, ENTRY_FILE_INLINE).await?;
                    write_u8(&mut ctrl, flags_f).await?;
                    write_u16(&mut ctrl, job.rel.len() as u16).await?;
                    ctrl.write_all(job.rel.as_bytes()).await?;
                    write_u64(&mut ctrl, orig_len).await?;
                    write_u64(&mut ctrl, job.mtime).await?;
                    write_u32(&mut ctrl, job.mode).await?;
                    write_u64(&mut ctrl, hash).await?;
                    if flags_f & F_COMPRESSED != 0 {
                        write_u64(&mut ctrl, payload.len() as u64).await?;
                    }
                    ctrl.write_all(payload).await?;
                    pb.inc(orig_len);
                    sent_bytes += orig_len;
                    report_file(&gui, sent_bytes, total_bytes, &job.rel, gui_pos, orig_len);
                    gui_pos += 1;
                    file_idx += 1;
                    continue;
                }
                write_u8(&mut ctrl, ENTRY_FILE_INLINE).await?;
                write_u8(&mut ctrl, flags_f).await?;
                write_u16(&mut ctrl, job.rel.len() as u16).await?;
                ctrl.write_all(job.rel.as_bytes()).await?;
                write_u64(&mut ctrl, orig_len).await?;
                write_u64(&mut ctrl, job.mtime).await?;
                write_u32(&mut ctrl, job.mode).await?;
                write_u64(&mut ctrl, hash).await?;
                write_u64(&mut ctrl, payload.len() as u64).await?;
                ctrl.write_all(payload).await?;
                pb.inc(orig_len);
                sent_bytes += orig_len;
                report_file(&gui, sent_bytes, total_bytes, &job.rel, gui_pos, orig_len);
                gui_pos += 1;
                file_idx += 1;
                continue;
            }
            // uncompressed inline
            write_u8(&mut ctrl, ENTRY_FILE_INLINE).await?;
            write_u8(&mut ctrl, flags_f).await?;
            write_u16(&mut ctrl, job.rel.len() as u16).await?;
            ctrl.write_all(job.rel.as_bytes()).await?;
            write_u64(&mut ctrl, orig_len).await?;
            write_u64(&mut ctrl, job.mtime).await?;
            write_u32(&mut ctrl, job.mode).await?;
            write_u64(&mut ctrl, hash).await?;
            ctrl.write_all(payload).await?;
            pb.inc(orig_len);
            sent_bytes += orig_len;
            report_file(&gui, sent_bytes, total_bytes, &job.rel, gui_pos, orig_len);
            gui_pos += 1;
            file_idx += 1;
        } else {
            // Sharded path: header on control, chunks over data workers.
            let nchunks = ((job.size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64) as u32;
            write_u8(&mut ctrl, ENTRY_FILE_SHARDED).await?;
            write_u8(&mut ctrl, 0).await?;
            write_u16(&mut ctrl, job.rel.len() as u16).await?;
            ctrl.write_all(job.rel.as_bytes()).await?;
            write_u64(&mut ctrl, job.size).await?;
            write_u64(&mut ctrl, job.mtime).await?;
            write_u32(&mut ctrl, job.mode).await?;
            write_u32(&mut ctrl, nchunks).await?;
            write_u32(&mut ctrl, CHUNK_SIZE).await?;
            // Stream chunks sequentially from disk, hash on the fly.
            let mut f = File::open(&job.abs)
                .await
                .with_context(|| format!("open {}", job.abs.display()))?;
            let mut hasher = opt.checksum.then(|| xxhash_rust::xxh3::Xxh3::new());
            let mut offset: u64 = 0;
            let mut buf = vec![0u8; CHUNK_SIZE as usize];
            loop {
                let n = f.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                if let Some(h) = hasher.as_mut() {
                    h.update(&buf[..n]);
                }
                let chunk = buf[..n].to_vec();
                let w = rr % txs.len();
                rr += 1;
                if txs[w].send((file_idx, offset, chunk)).await.is_err() {
                    // A dead worker means its TCP stream died — surface the
                    // worker's real error (not "channel closed").
                    return Err(worker_failure(std::mem::take(&mut worker_handles)).await);
                }
                offset += n as u64;
                pb.inc(n as u64);
                sent_bytes += n as u64;
                // Throttle GUI updates for large files (every chunk = 4 MiB is fine).
                report_file(&gui, sent_bytes, total_bytes, &job.rel, gui_pos, offset);
            }
            let digest = hasher.map(|h| h.digest()).unwrap_or(0);
            sharded_hashes.push((file_idx, digest));
            gui_pos += 1;
            file_idx += 1;
        }
    }

    // All inline + headers queued; wait for data workers to flush chunks.
    drop(txs);
    for h in worker_handles {
        h.await??;
    }

    // END + trailing hashes for sharded files.
    write_u8(&mut ctrl, ENTRY_END).await?;
    write_u64(&mut ctrl, sharded_hashes.len() as u64).await?;
    for (idx, digest) in &sharded_hashes {
        write_u64(&mut ctrl, *idx).await?;
        write_u64(&mut ctrl, *digest).await?;
    }
    ctrl.flush().await?;

    // Final ack: STATUS u8 + MISMATCH u64
    let ack_status = read_u8(&mut ctrl).await?;
    let mism = read_u64(&mut ctrl).await.unwrap_or(999);
    pb.finish_with_message("done");
    let dt = t0.elapsed().as_secs_f64().max(0.01);
    let mbps = (sent_bytes as f64 / 1e6) / dt;
    if ack_status == 0 && mism == 0 {
        println!(
            "Done: {} in {:.1}s ({:.1} MB/s, {:.1} Gb/s)",
            human_bytes(sent_bytes),
            dt,
            mbps,
            mbps * 8.0 / 1000.0
        );
        report_done(
            &gui,
            sent_bytes,
            total_bytes,
            &format!("Done: {} in {:.1}s ({:.1} MB/s)", human_bytes(sent_bytes), dt, mbps),
        );
    } else {
        report_error(&gui, sent_bytes, total_bytes, &format!("{} checksum mismatches", mism));
        anyhow::bail!("receiver reported {} checksum mismatches", mism);
    }
    Ok(())
}

async fn resolve_target(target: &str, port: u16) -> Result<std::net::SocketAddr> {
    if let Ok(ip) = target.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, port));
    }
    // hostname -> lookup via tokio DNS
    let mut addrs = tokio::net::lookup_host(format!("{}:{}", target, port))
        .await
        .with_context(|| format!("DNS lookup failed for {}", target))?;
    addrs.next().context("no address found")
}

pub fn human_bytes(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < U.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, U[u])
    } else {
        format!("{:.2} {}", v, U[u])
    }
}
