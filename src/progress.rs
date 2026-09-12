//! Shared progress / event types for CLI + GUI.
//! GUI talks to transfers through tokio mpsc channels; CLI ignores them.

use tokio::sync::mpsc::UnboundedSender;

use std::sync::{atomic::AtomicBool, Arc};

use crate::protocol::OfferFile;

/// One file in a transfer (for per-file progress bars).
#[derive(Clone, Debug)]
pub struct TransferFile {
    pub name: String,
    pub size: u64,
}

/// Periodic update from a running transfer.
#[derive(Clone, Debug)]
pub struct TransferUpdate {
    pub sent_bytes: u64,
    pub total_bytes: u64,
    /// Human label: current file, peer, or status.
    pub label: String,
    pub done: bool,
    pub error: Option<String>,
    /// Full file list, sent once at transfer start.
    pub files: Option<Vec<TransferFile>>,
    /// Index into `files` of the file currently being sent.
    pub file_index: Option<usize>,
    /// Bytes sent of that file so far.
    pub file_sent: Option<u64>,
}

pub type ProgressTx = UnboundedSender<TransferUpdate>;

/// Receiver -> GUI: a sender asks for approval before any bytes flow.
/// The GUI answers through `verdict` (true = allow, false = deny).
pub struct ApprovalRequest {
    pub sender_name: String,
    pub sender_ip: String,
    /// Full offer in send order (dirs + files) for the approval screen.
    pub offer: Vec<OfferFile>,
    pub total: u64,
    pub verdict: tokio::sync::oneshot::Sender<bool>,
    pub progress_rx: tokio::sync::mpsc::UnboundedReceiver<TransferUpdate>,
    /// Set by the GUI Stop button; the engine polls it between entries.
    pub stop: Arc<AtomicBool>,
}

pub type ApprovalTx = UnboundedSender<ApprovalRequest>;

/// Tab auto-close markers: a transfer killed from the other side (TCP drop)
/// closes the tab on both ends instead of lingering as Failed.
pub const STOPPED_BY_SENDER: &str = "Stopped by sender";
pub const STOPPED_BY_RECEIVER: &str = "Stopped by receiver";

/// Sender verdict-deny message: shown red in the tab title + error popup.
pub const DECLINED: &str = "Declined by receiver";

/// Rich progress report including per-file position.
pub fn report_file(
    tx: &Option<ProgressTx>,
    sent: u64,
    total: u64,
    label: &str,
    file_index: usize,
    file_sent: u64,
) {
    if let Some(t) = tx {
        let _ = t.send(TransferUpdate {
            sent_bytes: sent,
            total_bytes: total,
            label: label.to_string(),
            done: false,
            error: None,
            files: None,
            file_index: Some(file_index),
            file_sent: Some(file_sent),
        });
    }
}

pub fn report_list(
    tx: &Option<ProgressTx>,
    total: u64,
    label: &str,
    files: Vec<TransferFile>,
) {
    if let Some(t) = tx {
        let _ = t.send(TransferUpdate {
            sent_bytes: 0,
            total_bytes: total,
            label: label.to_string(),
            done: false,
            error: None,
            files: Some(files),
            file_index: Some(0),
            file_sent: Some(0),
        });
    }
}

pub fn report_done(tx: &Option<ProgressTx>, sent: u64, total: u64, label: &str) {
    if let Some(t) = tx {
        let _ = t.send(TransferUpdate {
            sent_bytes: sent,
            total_bytes: total,
            label: label.to_string(),
            done: true,
            error: None,
            files: None,
            file_index: None,
            file_sent: None,
        });
    }
}

pub fn report_error(tx: &Option<ProgressTx>, sent: u64, total: u64, err: &str) {
    if let Some(t) = tx {
        let _ = t.send(TransferUpdate {
            sent_bytes: sent,
            total_bytes: total,
            label: String::new(),
            done: true,
            error: Some(err.to_string()),
            files: None,
            file_index: None,
            file_sent: None,
        });
    }
}

/// Status-only update (e.g. "Waiting for approval…"): keeps current
/// files/bars, just swaps the label. Never marks done.
pub fn report_status(tx: &Option<ProgressTx>, sent: u64, total: u64, label: &str) {
    if let Some(t) = tx {
        let _ = t.send(TransferUpdate {
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

pub fn human_speed(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "-- MB/s".to_string();
    }
    format!("{:.1} MB/s", bytes_per_sec / 1e6)
}
