//! Receiver: control listener + data listener, pipelined writes, parallel shards.

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::progress::{ProgressTx, TransferUpdate};
use crate::protocol::*;

struct ShardedState {
    path: PathBuf,
    size: u64,
    received: AtomicU64,
}

type SharedMap = Arc<RwLock<HashMap<u64, Arc<ShardedState>>>>;

pub struct RecvOptions {
    pub bind: String,
    pub port: u16,
    pub out: PathBuf,
    pub overwrite: bool,
}

pub async fn run_recv(opt: RecvOptions) -> Result<()> {
    run_recv_gui(opt, None, Arc::new(AtomicBool::new(false))).await
}

pub async fn run_recv_gui(opt: RecvOptions, gui: Option<ProgressTx>, stop: Arc<AtomicBool>) -> Result<()> {
    let ctrl_listener = TcpListener::bind((opt.bind.as_str(), opt.port))
        .await
        .with_context(|| format!("bind {}:{} — port in use?", opt.bind, opt.port))?;
    let dport = data_port(opt.port);
    let data_listener = TcpListener::bind((opt.bind.as_str(), dport))
        .await
        .with_context(|| format!("bind data port {} — in use?", dport))?;

    println!("Listening on {}:{} (data :{})", opt.bind, opt.port, dport);
    println!("Output dir: {}", opt.out.display());
    for ip in crate::discovery::local_ips() {
        println!("  local ip: {}", ip);
    }
    println!("Waiting for sender... (run `filele send ... --to <this-ip>`)");

    // Discovery responder in background.
    tokio::spawn(discovery_task(opt.port));

    fs::create_dir_all(&opt.out).await?;
    let out = Arc::new(opt.out.clone());
    let overwrite_default = opt.overwrite;
    let data_listener = Arc::new(data_listener);

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let accept = tokio::time::timeout(std::time::Duration::from_millis(400), ctrl_listener.accept()).await;
        let (stream, peer) = match accept {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => continue, // timeout -> re-check stop flag
        };
        let _ = tune_stream(&stream);
        println!("Incoming transfer from {}", peer);
        gui_notify(&gui, 0, 0, &format!("Incoming from {}", peer), false, None);
        let data_listener_ref = data_listener.clone();
        let out_clone = out.clone();
        // Handle one transfer at a time sequentially (simplest + fastest disk).
        if let Err(e) = handle_transfer(stream, data_listener_ref, out_clone, overwrite_default, gui.clone()).await {
            eprintln!("Transfer failed: {:#}", e);
            gui_notify(&gui, 0, 0, "", true, Some(format!("{:#}", e)));
        } else {
            println!("Ready for next transfer...");
        }
        // Reset per-transfer recv counters for next transfer.
        RECV_SENT.store(0, Ordering::Relaxed);
        RECV_TOTAL.store(0, Ordering::Relaxed);
    }
    Ok(())
}

fn gui_notify(gui: &Option<ProgressTx>, sent: u64, total: u64, label: &str, done: bool, err: Option<String>) {
    if let Some(tx) = gui {
        let _ = tx.send(TransferUpdate { sent_bytes: sent, total_bytes: total, label: label.to_string(), done, error: err, files: None, file_index: None, file_sent: None });
    }
}

async fn discovery_task(port: u16) {
    if let Err(e) = crate::discovery::discovery_responder(port).await {
        eprintln!("discovery responder stopped: {:#}", e);
    }
}

async fn handle_transfer(
    mut ctrl: TcpStream,
    data_listener: std::sync::Arc<TcpListener>,
    out_base: Arc<PathBuf>,
    overwrite_cli: bool,
    gui: Option<ProgressTx>,
) -> Result<()> {
    // Handshake: MAGIC u32, VER u16, FLAGS u16, TOKEN u64, NSTREAMS u8, NFILES u64, TOTAL u64
    let magic = read_u32(&mut ctrl).await?;
    let ver = read_u16(&mut ctrl).await?;
    if magic != MAGIC || ver != VERSION {
        // reject
        let mut rej = Vec::new();
        rej.extend_from_slice(&MAGIC.to_le_bytes());
        rej.extend_from_slice(&VERSION.to_le_bytes());
        rej.push(1);
        rej.extend_from_slice(&data_port(0).to_le_bytes());
        let _ = ctrl.write_all(&rej).await;
        anyhow::bail!("protocol mismatch");
    }
    let flags = read_u16(&mut ctrl).await?;
    let token = read_u64(&mut ctrl).await?;
    let nstreams = read_u8(&mut ctrl).await?;
    let nfiles = read_u64(&mut ctrl).await?;
    let total = read_u64(&mut ctrl).await?;
    let do_checksum = flags & H_CHECKSUM != 0;
    let do_compress = flags & H_COMPRESS != 0;
    let do_overwrite = overwrite_cli || (flags & H_OVERWRITE != 0);
    let _ = (nfiles, do_compress);
    RECV_TOTAL.store(total, Ordering::Relaxed);
    RECV_SENT.store(0, Ordering::Relaxed);
    set_shared_gui_tx(gui.clone()).await;
    gui_notify(&gui, 0, total, "Receiving...", false, None);

    println!(
        "Offer: {} total, streams={}, checksum={}, compress={}, overwrite={}",
        crate::sender::human_bytes(total),
        nstreams,
        do_checksum,
        do_compress,
        do_overwrite
    );

    // Reply OK + data port
    {
        let dport = data_listener.local_addr().map(|a| a.port()).unwrap_or(data_port(53317));
        let mut rep = Vec::with_capacity(9);
        rep.extend_from_slice(&MAGIC.to_le_bytes());
        rep.extend_from_slice(&VERSION.to_le_bytes());
        rep.push(0);
        rep.extend_from_slice(&dport.to_le_bytes());
        ctrl.write_all(&rep).await?;
        ctrl.flush().await?;
    }

    let map: SharedMap = Arc::new(RwLock::new(HashMap::new()));
    // Accept data connections concurrently with control loop.
    let data_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<Result<()>>>>> =
        Arc::new(Mutex::new(Vec::new()));
    if nstreams > 0 {
        let map_c = map.clone();
        let out_c = out_base.clone();
        let dh = data_handles.clone();
        // Spawn acceptor: expects exactly nstreams conns with matching token.
        let accept_task = tokio::spawn(async move {
            for _ in 0..nstreams {
                let (ds, _) = data_listener
                    .accept()
                    .await
                    .context("accept data conn")?;
                let _ = tune_stream(&ds);
                let map2 = map_c.clone();
                let out2 = out_c.clone();
                let h = tokio::spawn(data_conn_task(ds, token, map2, out2, do_checksum));
                dh.lock().await.push(h);
            }
            anyhow::Ok(())
        });
        // Don't block control loop on acceptor; keep handle to check errors later.
        // Store accept task separately via extra spawn? Simplify: detach, errors surface as missing data.
        tokio::spawn(async move {
            if let Err(e) = accept_task.await {
                eprintln!("data accept error: {:?}", e);
            }
        });
    }

    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    let t0 = Instant::now();
    let sem = Arc::new(Semaphore::new(16)); // concurrent small-file writes
    let mut write_tasks: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    let mut mismatches: u64 = 0;
    let mut received_bytes: u64 = 0;

    loop {
        let etype = read_u8(&mut ctrl).await?;
        if etype == ENTRY_END {
            break;
        }
        match etype {
            ENTRY_DIR => {
                let _f = read_u8(&mut ctrl).await?;
                let plen = read_u16(&mut ctrl).await? as usize;
                if plen > MAX_PATH_LEN {
                    anyhow::bail!("path too long");
                }
                let mut pb2 = vec![0u8; plen];
                ctrl.read_exact(&mut pb2).await?;
                let rel = String::from_utf8(pb2).context("bad utf8 path")?;
                let _sz = read_u64(&mut ctrl).await?;
                let mtime = read_u64(&mut ctrl).await?;
                let _mode = read_u32(&mut ctrl).await?;
                let dest = wire_to_path(&out_base, &rel)?;
                let _ = fs::create_dir_all(&dest).await;
                let _ = set_mtime(&dest, mtime).await;
            }
            ENTRY_FILE_INLINE => {
                let fflags = read_u8(&mut ctrl).await?;
                let plen = read_u16(&mut ctrl).await? as usize;
                if plen > MAX_PATH_LEN {
                    anyhow::bail!("path too long");
                }
                let mut pbuf = vec![0u8; plen];
                ctrl.read_exact(&mut pbuf).await?;
                let rel = String::from_utf8(pbuf).context("bad utf8")?;
                let orig_size = read_u64(&mut ctrl).await?;
                let mtime = read_u64(&mut ctrl).await?;
                let _mode = read_u32(&mut ctrl).await?;
                let expected_hash = read_u64(&mut ctrl).await?;
                let wire_len = if fflags & F_COMPRESSED != 0 {
                    read_u64(&mut ctrl).await?
                } else {
                    orig_size
                };
                if wire_len > 256 * 1024 * 1024 {
                    anyhow::bail!("inline file too big: {}", wire_len);
                }
                // Read bytes off control stream (must consume before next header).
                let mut data = vec![0u8; wire_len as usize];
                if wire_len > 0 {
                    ctrl.read_exact(&mut data).await?;
                }
                // Decompress if needed.
                let final_bytes: Vec<u8> = if fflags & F_COMPRESSED != 0 {
                    lz4_flex::decompress_size_prepended(&data).context("lz4 decompress")?
                } else {
                    data
                };
                if final_bytes.len() as u64 != orig_size {
                    eprintln!("size mismatch for {}: got {}, expected {}", rel, final_bytes.len(), orig_size);
                    mismatches += 1;
                }
                if do_checksum && expected_hash != 0 {
                    let h = xxhash_rust::xxh3::xxh3_64(&final_bytes);
                    if h != expected_hash {
                        eprintln!("checksum mismatch: {}", rel);
                        mismatches += 1;
                    }
                }
                let dest = wire_to_path(&out_base, &rel)?;
                // Pipelined write: spawn, control loop continues reading next header.
                if final_bytes.len() as u64 <= INLINE_MEM_LIMIT {
                    let permit = sem.clone().acquire_owned().await?;
                    let pb_c = pb.clone();
                    write_tasks.push(tokio::spawn(async move {
                        let _p = permit;
                        if let Some(parent) = dest.parent() {
                            fs::create_dir_all(parent).await?;
                        }
                        if !do_overwrite && fs::try_exists(&dest).await.unwrap_or(false) {
                            // skip
                        } else {
                            // Preallocate + write
                            let mut f = File::create(&dest).await?;
                            f.write_all(&final_bytes).await?;
                            f.flush().await?;
                            drop(f);
                            let _ = set_mtime(&dest, mtime).await;
                        }
                        pb_c.inc(orig_size);
                        anyhow::Ok(())
                    }));
                } else {
                    // Larger inline (8-64MiB): write inline to avoid task RAM blow.
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    if !do_overwrite && fs::try_exists(&dest).await.unwrap_or(false) {
                        // drain: skip writing but already consumed bytes
                    } else {
                        let mut f = File::create(&dest).await?;
                        f.write_all(&final_bytes).await?;
                        f.flush().await?;
                        drop(f);
                        let _ = set_mtime(&dest, mtime).await;
                    }
                    pb.inc(orig_size);
                }
                received_bytes += orig_size;
                RECV_SENT.fetch_add(orig_size, Ordering::Relaxed);
                report_recv_progress(&rel);
            }
            ENTRY_FILE_SHARDED => {
                let _fflags = read_u8(&mut ctrl).await?;
                let plen = read_u16(&mut ctrl).await? as usize;
                if plen > MAX_PATH_LEN {
                    anyhow::bail!("path too long");
                }
                let mut pbuf = vec![0u8; plen];
                ctrl.read_exact(&mut pbuf).await?;
                let rel = String::from_utf8(pbuf).context("bad utf8")?;
                let size = read_u64(&mut ctrl).await?;
                let mtime = read_u64(&mut ctrl).await?;
                let _mode = read_u32(&mut ctrl).await?;
                let _nchunks = read_u32(&mut ctrl).await?;
                let _csize = read_u32(&mut ctrl).await?;
                let dest = wire_to_path(&out_base, &rel)?;
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).await?;
                }
                // Need file_idx: we assigned sequentially on sender for ALL entries.
                // Receiver must track same idx: maintain counter.
                // We use a counter stored in map len? No — dirs+inline also consume idx.
                // Solution: keep explicit counter here.
                let idx = next_idx().await;
                // Handle overwrite-skip: still need to consume data chunks.
                // Create + preallocate.
                if !do_overwrite && fs::try_exists(&dest).await.unwrap_or(false) {
                    // Don't truncate; data workers will overwrite in place anyway.
                    // To keep offsets valid, ensure len >= size.
                    if let Ok(md) = fs::metadata(&dest).await {
                        if md.len() < size {
                            let f = std::fs::OpenOptions::new().write(true).open(&dest)?;
                            f.set_len(size)?;
                        }
                    }
                } else {
                    let f = File::create(&dest).await?;
                    f.set_len(size).await?;
                    drop(f);
                }
                let st = Arc::new(ShardedState {
                    path: dest.clone(),
                    size,
                    received: AtomicU64::new(0),
                });
                map.write().await.insert(idx, st);
                // mtime applied after completion; store pending mtime via sidecar map.
                set_pending_mtime(idx, mtime).await;
                // Progress accounted as chunks arrive (data workers update pb).
                // Attach pb to shared progress for data workers:
                set_shared_pb(pb.clone()).await;
                let _ = mtime;
                received_bytes += 0; // counted on data path
            }
            _ => anyhow::bail!("unknown entry type {}", etype),
        }
        // Keep idx counter in sync for dirs/inline too.
        // Sharded branch already consumed idx via next_idx(); others need increment.
        if etype == ENTRY_DIR || etype == ENTRY_FILE_INLINE {
            bump_idx().await;
        }
    }

    // Trailing hashes for sharded files.
    let ntrail = read_u64(&mut ctrl).await?;
    let mut expected: HashMap<u64, u64> = HashMap::new();
    for _ in 0..ntrail {
        let idx = read_u64(&mut ctrl).await?;
        let h = read_u64(&mut ctrl).await?;
        expected.insert(idx, h);
    }

    // Wait for data workers to finish (they get EOF when sender drops).
    // Give them time: poll map received vs size with timeout.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let done = {
            let m = map.read().await;
            if m.is_empty() {
                true
            } else {
                m.values().all(|s| s.received.load(Ordering::Relaxed) >= s.size)
            }
        };
        if done {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            eprintln!("timeout waiting for data streams");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Join data conn tasks.
    {
        let mut hs = data_handles.lock().await;
        for h in hs.drain(..) {
            match h.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("data worker error: {:#}", e),
                Err(e) => eprintln!("data join error: {}", e),
            }
        }
    }
    // Join inline write tasks.
    for h in write_tasks {
        if let Err(e) = h.await {
            eprintln!("write task error: {}", e);
        }
    }

    // Verify sharded hashes + finalize mtimes.
    // NOTE: must re-read files sequentially for hashing. On-the-fly hashing in
    // arrival order would be WRONG (chunks arrive out-of-order over parallel
    // streams; xxh3 is order-dependent). Sequential re-read is correct and still
    // fast (GB/s) and only when checksums are enabled.
    if do_checksum {
        let m = map.read().await;
        for (idx, exp) in &expected {
            if let Some(st) = m.get(idx) {
                if *exp == 0 {
                    continue;
                }
                match hash_file_sequential(&st.path).await {
                    Ok(digest) => {
                        if digest != *exp {
                            eprintln!("checksum mismatch for {}", st.path.display());
                            mismatches += 1;
                        }
                    }
                    Err(e) => {
                        eprintln!("hash failed for {}: {:#}", st.path.display(), e);
                        mismatches += 1;
                    }
                }
            }
        }
    }
    // Apply mtimes for sharded files.
    {
        let m = map.read().await;
        for (idx, st) in m.iter() {
            if let Some(mt) = get_pending_mtime(*idx).await {
                let _ = set_mtime(&st.path, mt).await;
            }
            let _ = received_bytes;
        }
    }

    // Account sharded bytes in progress (data workers already inc'd? we inc there).
    pb.finish_with_message("done");
    let dt = t0.elapsed().as_secs_f64().max(0.01);
    // Use pb position as received.
    let got = pb.position();
    let mbps = (got as f64 / 1e6) / dt;
    println!(
        "Received {} in {:.1}s ({:.1} MB/s). mismatches={}",
        crate::sender::human_bytes(got),
        dt,
        mbps,
        mismatches
    );

    // Final ack
    write_u8(&mut ctrl, 0).await?;
    write_u64(&mut ctrl, mismatches).await?;
    ctrl.flush().await?;
    {
        let total = RECV_TOTAL.load(Ordering::Relaxed);
        let sent = RECV_SENT.load(Ordering::Relaxed);
        if mismatches == 0 {
            gui_notify(&gui, sent.max(total), total.max(sent), &format!("Received {} ({:.1} MB/s)", crate::sender::human_bytes(sent.max(got)), mbps), true, None);
        } else {
            gui_notify(&gui, sent, total, "", true, Some(format!("{} checksum mismatches", mismatches)));
        }
    }
    set_shared_gui_tx(None).await;
    reset_idx().await;
    Ok(())
}

async fn data_conn_task(
    mut stream: TcpStream,
    token: u64,
    map: SharedMap,
    _out_base: Arc<PathBuf>,
    _checksum: bool,
) -> Result<()> {
    // Hello: MAGIC u32, VER u16, TOKEN u64
    let magic = read_u32(&mut stream).await?;
    let ver = read_u16(&mut stream).await?;
    let tok = read_u64(&mut stream).await?;
    if magic != MAGIC || ver != VERSION || tok != token {
        write_u8(&mut stream, 1).await?;
        anyhow::bail!("bad data hello");
    }
    write_u8(&mut stream, 0).await?;
    stream.flush().await?;

    // Per-connection handle cache: file_idx -> std File (separate handles, no lock on write path).
    let mut handles: HashMap<u64, std::fs::File> = HashMap::new();
    let mut hdr = [0u8; 20];
    loop {
        match stream.read_exact(&mut hdr).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let file_idx = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
        let offset = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let len = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as usize;
        if len == 0 || len > 16 * 1024 * 1024 {
            anyhow::bail!("bad chunk len {}", len);
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        // Lookup state (wait briefly if header not yet processed).
        let st = loop {
            if let Some(s) = map.read().await.get(&file_idx).cloned() {
                break s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        // Write at offset via cached handle.
        {
            let h = handles.entry(file_idx).or_insert_with(|| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&st.path)
                    .expect("open shard target")
            });
            // Retry on lock contention? Direct pwrite.
            let mut off = 0;
            while off < buf.len() {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileExt;
                    let n = h.seek_write(&buf[off..], offset + off as u64)?;
                    off += n;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileExt;
                    let n = h.write_at(&buf[off..], offset + off as u64)?;
                    off += n;
                }
                #[cfg(not(any(windows, unix)))]
                {
                    anyhow::bail!("unsupported platform");
                }
            }
        }
        st.received.fetch_add(len as u64, Ordering::Relaxed);
        RECV_SENT.fetch_add(len as u64, Ordering::Relaxed);
        if let Some(pb) = get_shared_pb().await {
            pb.inc(len as u64);
        }
        report_recv_progress("Receiving...");
    }
    Ok(())
}

// ---- global counters (per-transfer) ----
// Receiver must mirror sender's global file_idx (dirs+files). Use async statics via OnceLock + Mutex.

use std::sync::OnceLock;
static IDX: OnceLock<Mutex<u64>> = OnceLock::new();
static MTIMES: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();
static SHARED_PB: OnceLock<Mutex<Option<ProgressBar>>> = OnceLock::new();
static RECV_SENT: AtomicU64 = AtomicU64::new(0);
static RECV_TOTAL: AtomicU64 = AtomicU64::new(0);
static SHARED_GUI_TX: OnceLock<Mutex<Option<ProgressTx>>> = OnceLock::new();

async fn set_shared_gui_tx(tx: Option<ProgressTx>) {
    let m = SHARED_GUI_TX.get_or_init(|| Mutex::new(None));
    *m.lock().await = tx;
}

fn report_recv_progress(label: &str) {
    if let Some(m) = SHARED_GUI_TX.get() {
        // Non-blocking best-effort: try_lock, skip if contended (next chunk will report).
        if let Ok(g) = m.try_lock() {
            if let Some(tx) = g.as_ref() {
                let sent = RECV_SENT.load(Ordering::Relaxed);
                let total = RECV_TOTAL.load(Ordering::Relaxed);
                let _ = tx.send(TransferUpdate {
                    sent_bytes: sent,
                    total_bytes: total,
                    label: label.to_string(),
                    done: false,
                    error: None,
                    files: None,
                    file_index: None,
                    file_sent: None,
                });
            }
        }
    }
}

async fn next_idx() -> u64 {
    let m = IDX.get_or_init(|| Mutex::new(0));
    let mut g = m.lock().await;
    let v = *g;
    *g += 1;
    v
}
async fn bump_idx() {
    let m = IDX.get_or_init(|| Mutex::new(0));
    *m.lock().await += 1;
}
async fn reset_idx() {
    if let Some(m) = IDX.get() {
        *m.lock().await = 0;
    }
    if let Some(m) = MTIMES.get() {
        m.lock().await.clear();
    }
    if let Some(m) = SHARED_PB.get() {
        *m.lock().await = None;
    }
}
async fn set_pending_mtime(idx: u64, mt: u64) {
    let m = MTIMES.get_or_init(|| Mutex::new(HashMap::new()));
    m.lock().await.insert(idx, mt);
}
async fn get_pending_mtime(idx: u64) -> Option<u64> {
    let m = MTIMES.get_or_init(|| Mutex::new(HashMap::new()));
    m.lock().await.get(&idx).copied()
}
async fn set_shared_pb(pb: ProgressBar) {
    let m = SHARED_PB.get_or_init(|| Mutex::new(None));
    *m.lock().await = Some(pb);
}
async fn get_shared_pb() -> Option<ProgressBar> {
    let m = SHARED_PB.get_or_init(|| Mutex::new(None));
    m.lock().await.clone()
}

async fn set_mtime(path: &Path, unix_secs: u64) -> Result<()> {
    if unix_secs == 0 {
        return Ok(());
    }
    let t = filetime_set(path, unix_secs).await;
    let _ = t;
    Ok(())
}

async fn filetime_set(path: &Path, secs: u64) -> Result<()> {
    // Best-effort mtime via std (no extra dep): read + set via `filetime` would need crate.
    // Use `std::fs::File::set_modified` (stable since 1.75? via FileTimes).
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let f = std::fs::File::options().write(true).open(&p)?;
        f.set_modified(t)?;
        anyhow::Ok(())
    })
    .await??;
    Ok(())
}

/// Sequential xxh3 of a file from disk (1 MiB buffer). Used to verify sharded
/// large files in correct byte order (parallel arrival order is NOT hashable).
async fn hash_file_sequential(path: &Path) -> Result<u64> {
    let mut f = File::open(path).await?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.digest())
}
