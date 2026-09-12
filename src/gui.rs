//! Friendly GUI for filele (eframe/egui).
//! Double-click the exe (no args) or run `filele gui`.
//! Same fast engine underneath: N parallel streams + pipelined small files.

use anyhow::Result;
use eframe::egui;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::discovery::Peer;
use crate::progress::{human_bytes, human_speed, TransferUpdate};
use crate::protocol::DEFAULT_PORT;

const ACCENT: egui::Color32 = egui::Color32::from_rgb(47, 129, 247);
const ACCENT_SOFT: egui::Color32 = egui::Color32::from_rgb(31, 72, 133);
const GOOD: egui::Color32 = egui::Color32::from_rgb(63, 185, 80);
const GOOD_BG: egui::Color32 = egui::Color32::from_rgb(24, 64, 36);
const WARN: egui::Color32 = egui::Color32::from_rgb(210, 153, 34);
const BAD: egui::Color32 = egui::Color32::from_rgb(248, 81, 73);
const BAD_BG: egui::Color32 = egui::Color32::from_rgb(76, 28, 30);
const INK: egui::Color32 = egui::Color32::from_rgb(230, 237, 243);
const INK_DIM: egui::Color32 = egui::Color32::from_rgb(139, 148, 158);
const KIND_FILE: egui::Color32 = egui::Color32::from_rgb(121, 192, 255);
const KIND_DIR: egui::Color32 = egui::Color32::from_rgb(227, 179, 65);

// Layered backgrounds: sidebar darkest, main a step lighter, cards float above.
const MAIN_BG: egui::Color32 = egui::Color32::from_rgb(13, 17, 23);
const SIDEBAR_BG: egui::Color32 = egui::Color32::from_rgb(7, 10, 16);
const CARD_BG: egui::Color32 = egui::Color32::from_rgb(19, 25, 34);

// Segoe MDL2 Assets (in-box on Windows 10/11) icon glyphs — no extra font files.
const ICON_SEND: char = '\u{E122}'; // paper plane
const ICON_RECEIVE: char = '\u{E896}'; // download
const ICON_TRANSFER: char = '\u{E122}';
const ICON_CHECK: char = '\u{E73E}';
const ICON_ERROR: char = '\u{E783}';
const ICON_LOGO: char = '\u{E945}'; // lightning — fast transfer
const ICON_DEVICE: char = '\u{E772}';
const ICON_REFRESH: char = '\u{E72C}';
const ICON_FILE: char = '\u{E7C3}';
const ICON_FOLDER: char = '\u{E8B7}';

fn icon_family() -> egui::FontFamily {
    egui::FontFamily::Name("mdl2".into())
}

fn icon_text(c: char, size: f32, color: egui::Color32) -> egui::RichText {
    egui::RichText::new(c.to_string())
        .family(icon_family())
        .size(size)
        .color(color)
}

/// Borderless floating card frame — no pale outline.
fn card_frame() -> egui::Frame {
    egui::Frame {
        fill: CARD_BG,
        stroke: egui::Stroke::NONE,
        corner_radius: 12.into(),
        inner_margin: 14.into(),
        shadow: egui::Shadow::NONE,
        ..Default::default()
    }
}

/// Borderless full-width floating card.
fn card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        add_contents(ui)
    })
}

/// Crisp status dot drawn with the painter — never tofu, unlike "●"/"○" text.
fn dot(ui: &mut egui::Ui, color: egui::Color32, diameter: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(diameter, diameter), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), diameter / 2.0, color);
}

fn dot_hollow(ui: &mut egui::Ui, color: egui::Color32, diameter: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(diameter, diameter), egui::Sense::hover());
    ui.painter()
        .circle_stroke(rect.center(), diameter / 2.0 - 0.5, egui::Stroke::new(1.5, color));
}

/// Live-status dot that gently pulses while something is active.
fn pulse_dot(ui: &mut egui::Ui, color: egui::Color32, diameter: f32) {
    let a = GuiApp::pulse_alpha(ui.ctx());
    let c = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (a * 255.0) as u8);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(diameter + 8.0, diameter + 8.0), egui::Sense::hover());
    let center = rect.center();
    // soft halo + solid core
    let halo = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), (a * 60.0) as u8);
    ui.painter().circle_filled(center, diameter / 2.0 + 4.0, halo);
    ui.painter().circle_filled(center, diameter / 2.0, c);
}

fn logo_badge(ui: &mut egui::Ui) {
    egui::Frame {
        fill: ACCENT,
        stroke: egui::Stroke::NONE,
        corner_radius: 9.into(),
        inner_margin: egui::Margin::same(7),
        ..Default::default()
    }
    .show(ui, |ui| {
        ui.label(icon_text(ICON_LOGO, 17.0, egui::Color32::WHITE));
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Send,
    Receive,
    Transfer(u64),
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct GuiConfig {
    target_name: String,
    target: String,
    out_dir: String,
    port: u16,
    streams: u8,
    checksum: bool,
    compress: bool,
    overwrite: bool,
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("filele-gui.json")
}

fn load_config() -> GuiConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(GuiConfig {
            port: DEFAULT_PORT,
            streams: 4,
            checksum: true,
            overwrite: true,
            out_dir: default_out_dir(),
            ..Default::default()
        })
}

fn default_out_dir() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

fn short_name(name: &str) -> String {
    let short: String = name.chars().take(10).collect();
    if short.is_empty() {
        "device".to_string()
    } else {
        short
    }
}

fn my_hostname() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "this PC".to_string())
}

pub async fn launch_gui() -> Result<()> {
    let rt = tokio::runtime::Handle::current();
    let app = GuiApp::new(rt);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Filele — Fast LAN Transfer")
            .with_inner_size([920.0, 640.0])
            .with_min_inner_size([700.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Filele — Fast LAN Transfer",
        options,
        Box::new(|cc| {
            style_app(cc);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))?;
    Ok(())
}

fn style_app(cc: &eframe::CreationContext) {
    // Modern native look: Segoe UI for text, Segoe MDL2 Assets for icons.
    // Both ship with Windows — no downloads, no tofu squares.
    {
        let mut fonts = egui::FontDefinitions::default();
        if let Ok(data) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") {
            fonts.font_data.insert("segoe_ui".to_owned(), std::sync::Arc::new(egui::FontData::from_owned(data)));
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                list.insert(0, "segoe_ui".to_owned());
            }
        }
        if let Ok(data) = std::fs::read(r"C:\Windows\Fonts\segmdl2.ttf") {
            fonts.font_data.insert("mdl2".to_owned(), std::sync::Arc::new(egui::FontData::from_owned(data)));
            // Fallback so MDL2 PUA glyphs render inside normal labels/buttons…
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                list.push("mdl2".to_owned());
            }
            // …and a dedicated family for pure-icon labels.
            fonts.families.insert(icon_family(), vec!["mdl2".to_owned()]);
        }
        cc.egui_ctx.set_fonts(fonts);
    }
    let mut visuals = egui::Visuals::dark();
    // Layered dark palette, flat with no pale outlines.
    visuals.dark_mode = true;
    visuals.window_fill = MAIN_BG;
    visuals.panel_fill = MAIN_BG;
    visuals.faint_bg_color = CARD_BG;
    visuals.extreme_bg_color = SIDEBAR_BG;
    visuals.code_bg_color = CARD_BG;
    visuals.hyperlink_color = egui::Color32::from_rgb(68, 147, 248);
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = BAD;
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
    visuals.window_stroke = egui::Stroke::NONE;
    visuals.window_shadow = egui::Shadow::NONE;
    visuals.popup_shadow = egui::Shadow::NONE;
    // Flat controls: no pale borders; hover/active get the accent edge.
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.bg_fill = egui::Color32::from_rgb(30, 36, 46);
        w.bg_stroke = egui::Stroke::NONE;
        w.fg_stroke = egui::Stroke::new(1.0_f32, INK);
        w.corner_radius = 8.into();
    }
    visuals.widgets.noninteractive.bg_fill = CARD_BG;
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(38, 45, 58);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.bg_fill = ACCENT_SOFT;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT);
    cc.egui_ctx.set_visuals(visuals);
    let mut style = (*cc.egui_ctx.style()).clone();
    style.spacing.button_padding = egui::vec2(12.0, 8.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.indent = 16.0;
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(22.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(11.5, egui::FontFamily::Proportional),
    );
    cc.egui_ctx.set_style(style);
}

/// Shorten long names with an ellipsis (egui has no single-line ellipsis).
fn trunc(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", head)
}

/// Small tinted badge, e.g. `badge(ui, "Folder", KIND_DIR)`.
fn badge(ui: &mut egui::Ui, text: &str, fg: egui::Color32) {
    ui.label(
        egui::RichText::new(text)
            .small()
            .strong()
            .color(fg)
            .background_color(egui::Color32::from_rgba_unmultiplied(88, 96, 106, 40)),
    );
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(15.0));
}

#[derive(Clone)]
struct FileProg {
    name: String,
    size: u64,
    sent: u64,
}

struct TransferTab {
    id: u64,
    target_name: String,
    target_ip: String,
    files: Vec<FileProg>,
    sent: u64,
    total: u64,
    label: String,
    speed: f64,
    started: Instant,
    last_bytes: u64,
    last_tick: Instant,
    done: bool,
    error: Option<String>,
    done_msg: String,
    rx: Option<tokio::sync::mpsc::UnboundedReceiver<TransferUpdate>>,
}

impl TransferTab {
    fn tab_label(&self) -> String {
        format!("{} send", short_name(&self.target_name))
    }
    fn frac(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.sent as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }
}

struct RecvProg {
    sent: u64,
    total: u64,
    label: String,
    speed: f64,
    last_bytes: u64,
    last_tick: Instant,
    last_done: String,
}

impl Default for RecvProg {
    fn default() -> Self {
        Self {
            sent: 0,
            total: 0,
            label: String::new(),
            speed: 0.0,
            last_bytes: 0,
            last_tick: Instant::now(),
            last_done: String::new(),
        }
    }
}

#[derive(Clone)]
struct Picked {
    path: PathBuf,
    name: String,
    size: u64,
    is_dir: bool,
}

struct GuiApp {
    rt: tokio::runtime::Handle,
    view: View,
    next_id: u64,
    // send form (sizes cached so the list renders instantly)
    files: Vec<Picked>,
    files_size: u64,
    target_name: String,
    target_ip: String,
    port: u16,
    streams: u8,
    checksum: bool,
    compress: bool,
    overwrite: bool,
    send_error: String,
    // devices (auto-discovered)
    peers: Vec<Peer>,
    scanning: bool,
    last_scan: Option<Instant>,
    disc_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<Peer>>>,
    disc_refresh: Arc<tokio::sync::Notify>,
    // transfers
    transfers: Vec<TransferTab>,
    // receive
    out_dir: String,
    recv_port: u16,
    recv_overwrite: bool,
    listening: bool,
    recv_stop: Option<Arc<AtomicBool>>,
    recv_rx: Option<tokio::sync::mpsc::UnboundedReceiver<TransferUpdate>>,
    recv: RecvProg,
    recv_error: String,
    // animation state
    view_born: Instant,
    last_view_key: u64,
}

impl GuiApp {
    fn new(rt: tokio::runtime::Handle) -> Self {
        let cfg = load_config();
        let (disc_tx, disc_rx) = tokio::sync::mpsc::unbounded_channel();
        let disc_refresh = Arc::new(tokio::sync::Notify::new());
        // Constant background discovery: fast cycle so new PCs appear in ~2 s.
        // A manual refresh (Notify) cuts the pause short for an instant rescan.
        let disc_wake = disc_refresh.clone();
        rt.spawn(async move {
            loop {
                let peers = crate::discovery::discover(Duration::from_secs(1))
                    .await
                    .unwrap_or_default();
                if disc_tx.send(peers).is_err() {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    _ = disc_wake.notified() => {}
                }
            }
        });
        Self {
            rt,
            view: View::Send,
            next_id: 1,
            files: Vec::new(),
            files_size: 0,
            target_name: cfg.target_name,
            target_ip: cfg.target,
            port: if cfg.port == 0 { DEFAULT_PORT } else { cfg.port },
            streams: cfg.streams.min(8),
            checksum: cfg.checksum,
            compress: cfg.compress,
            overwrite: cfg.overwrite,
            send_error: String::new(),
            peers: Vec::new(),
            scanning: true,
            last_scan: None,
            disc_rx: Some(disc_rx),
            disc_refresh,
            transfers: Vec::new(),
            out_dir: if cfg.out_dir.is_empty() { default_out_dir() } else { cfg.out_dir },
            recv_port: DEFAULT_PORT,
            recv_overwrite: true,
            listening: false,
            recv_stop: None,
            recv_rx: None,
            recv: RecvProg::default(),
            recv_error: String::new(),
            view_born: Instant::now(),
            last_view_key: 0,
        }
    }

    fn view_key(v: View) -> u64 {
        match v {
            View::Send => 0,
            View::Receive => 1,
            View::Transfer(id) => 2u64.wrapping_add(id.wrapping_mul(1000003)),
        }
    }

    /// 0..1 eased fade for the freshly switched view (~180 ms).
    fn view_fade(&self) -> f32 {
        let t = self.view_born.elapsed().as_secs_f32() / 0.18;
        let t = t.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }

    /// Pulsing alpha for live status (listening / active transfer).
    fn pulse_alpha(ctx: &egui::Context) -> f32 {
        let t = ctx.input(|i| i.time);
        0.55 + 0.45 * ((t * 4.0).sin() as f32)
    }

    /// Animated ellipsis: "Waiting for a sender", ".", "..", "...".
    fn waiting_text(ctx: &egui::Context, base: &str) -> String {
        let t = ctx.input(|i| i.time);
        let n = ((t * 2.0) as usize) % 4;
        format!("{}{}", base, ".".repeat(n))
    }

    fn save_config(&self) {
        let cfg = GuiConfig {
            target_name: self.target_name.clone(),
            target: self.target_ip.clone(),
            out_dir: self.out_dir.clone(),
            port: self.port,
            streams: self.streams,
            checksum: self.checksum,
            compress: self.compress,
            overwrite: self.overwrite,
        };
        if let Ok(s) = serde_json::to_string_pretty(&cfg) {
            let _ = std::fs::write(config_path(), s);
        }
    }

    fn add_files(&mut self, paths: Vec<PathBuf>) {
        let mut added = 0;
        for p in paths {
            if !p.exists() || self.files.iter().any(|f| f.path == p) {
                continue;
            }
            let md = std::fs::metadata(&p).ok();
            let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let name = file_name_of(&p);
            let size = dir_size(&p);
            self.files.push(Picked { path: p, name, size, is_dir });
            added += 1;
        }
        if added > 0 {
            self.recalc_size();
        }
    }

    fn recalc_size(&mut self) {
        self.files_size = self.files.iter().map(|f| f.size).sum();
    }

    fn select_device(&mut self, peer: &Peer) {
        self.target_name = peer.name.clone();
        self.target_ip = peer.addr.ip().to_string();
        self.port = peer.port;
        self.send_error.clear();
        self.save_config();
    }

    fn start_send(&mut self) {
        self.send_error.clear();
        if self.files.is_empty() {
            self.send_error = "Add at least one file or folder first.".to_string();
            return;
        }
        if self.target_ip.trim().is_empty() {
            self.send_error = "Select a device below first.".to_string();
            return;
        }
        for f in &self.files {
            if !f.path.exists() {
                self.send_error = format!("Not found: {}", f.path.display());
                return;
            }
        }
        let display_name = if self.target_name.trim().is_empty() {
            self.target_ip.trim().to_string()
        } else {
            self.target_name.trim().to_string()
        };
        let id = self.next_id;
        self.next_id += 1;
        let files: Vec<PathBuf> = self.files.iter().map(|f| f.path.clone()).collect();
        let opt = crate::sender::SendOptions {
            target: self.target_ip.trim().to_string(),
            port: self.port,
            streams: self.streams,
            compress: self.compress,
            checksum: self.checksum,
            overwrite: self.overwrite,
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.transfers.push(TransferTab {
            id,
            target_name: display_name,
            target_ip: opt.target.clone(),
            files: Vec::new(),
            sent: 0,
            total: 0,
            label: "Connecting...".to_string(),
            speed: 0.0,
            started: Instant::now(),
            last_bytes: 0,
            last_tick: Instant::now(),
            done: false,
            error: None,
            done_msg: String::new(),
            rx: Some(rx),
        });
        self.view = View::Transfer(id);
        self.save_config();
        self.rt.spawn(async move {
            match crate::sender::run_send_with_progress(files, opt, Some(tx.clone())).await {
                Ok(()) => {}
                Err(e) => {
                    let _ = tx.send(TransferUpdate {
                        sent_bytes: 0,
                        total_bytes: 0,
                        label: String::new(),
                        done: true,
                        error: Some(format!("{:#}", e)),
                        files: None,
                        file_index: None,
                        file_sent: None,
                    });
                }
            }
        });
    }

    fn start_listen(&mut self) {
        self.recv_error.clear();
        if self.listening {
            return;
        }
        if self.out_dir.trim().is_empty() {
            self.recv_error = "Pick a save folder first.".to_string();
            return;
        }
        let out = PathBuf::from(self.out_dir.trim());
        if let Err(e) = std::fs::create_dir_all(&out) {
            self.recv_error = format!("Cannot create folder: {e}");
            return;
        }
        let opt = crate::receiver::RecvOptions {
            bind: "0.0.0.0".to_string(),
            port: self.recv_port,
            out,
            overwrite: self.recv_overwrite,
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.recv_rx = Some(rx);
        self.recv = RecvProg::default();
        let stop = Arc::new(AtomicBool::new(false));
        self.recv_stop = Some(stop.clone());
        self.listening = true;
        self.rt.spawn(async move {
            if let Err(e) = crate::receiver::run_recv_gui(opt, Some(tx.clone()), stop).await {
                let _ = tx.send(TransferUpdate {
                    sent_bytes: 0,
                    total_bytes: 0,
                    label: String::new(),
                    done: true,
                    error: Some(format!("{:#}", e)),
                    files: None,
                    file_index: None,
                    file_sent: None,
                });
            }
        });
    }

    fn stop_listen(&mut self) {
        if let Some(s) = self.recv_stop.take() {
            s.store(true, Ordering::Relaxed);
        }
        self.listening = false;
    }

    fn poll_channels(&mut self) {
        // discovery results (keep latest)
        if let Some(rx) = self.disc_rx.as_mut() {
            let mut latest: Option<Vec<Peer>> = None;
            while let Ok(peers) = rx.try_recv() {
                latest = Some(peers);
            }
            if let Some(peers) = latest {
                self.peers = peers;
                self.last_scan = Some(Instant::now());
                self.scanning = false;
            }
            // a scan takes ~1s; mark scanning while waiting for next batch
            if self.last_scan.map(|t| t.elapsed() > Duration::from_secs(3)).unwrap_or(true) {
                self.scanning = true;
            }
        }
        // transfer progress
        for t in self.transfers.iter_mut() {
            let updates: Vec<TransferUpdate> = match t.rx.as_mut() {
                Some(rx) => {
                    let mut v = Vec::new();
                    while let Ok(u) = rx.try_recv() {
                        v.push(u);
                    }
                    v
                }
                None => Vec::new(),
            };
            let mut finished = false;
            for u in updates {
                if let Some(files) = u.files {
                    t.files = files
                        .into_iter()
                        .map(|f| FileProg { name: f.name, size: f.size, sent: 0 })
                        .collect();
                }
                if u.total_bytes > 0 {
                    t.total = u.total_bytes;
                }
                // per-file bookkeeping
                if let (Some(fi), Some(fs)) = (u.file_index, u.file_sent) {
                    for (i, f) in t.files.iter_mut().enumerate() {
                        if i < fi {
                            f.sent = f.size;
                        }
                    }
                    if let Some(f) = t.files.get_mut(fi) {
                        f.sent = fs.min(f.size);
                    }
                    // if engine reports overall label of a file not in list, ignore
                } else if !u.label.is_empty() && !t.files.is_empty() {
                    // fallback: mark files before the labelled one complete
                    if let Some(pos) = t.files.iter().position(|f| f.name == u.label) {
                        let base: u64 = t.files[..pos].iter().map(|f| f.size).sum();
                        for (i, f) in t.files.iter_mut().enumerate() {
                            if i < pos {
                                f.sent = f.size;
                            } else if i == pos {
                                f.sent = f.sent.max(u.sent_bytes.saturating_sub(base));
                            }
                        }
                    }
                }
                let now = Instant::now();
                let dt = now.duration_since(t.last_tick).as_secs_f64().max(0.001);
                let delta = u.sent_bytes.saturating_sub(t.last_bytes);
                if dt > 0.05 || u.done {
                    t.speed = delta as f64 / dt;
                    t.last_tick = now;
                    t.last_bytes = u.sent_bytes;
                }
                t.sent = u.sent_bytes;
                if !u.label.is_empty() {
                    t.label = u.label.clone();
                }
                if u.done {
                    t.done = true;
                    finished = true;
                    if let Some(e) = u.error {
                        t.error = Some(e);
                    } else {
                        t.done_msg = t.label.clone();
                        // mark all files complete
                        for f in t.files.iter_mut() {
                            f.sent = f.size;
                        }
                        t.sent = t.total.max(t.sent);
                    }
                }
            }
            if finished {
                t.rx = None;
            }
        }
        // receive progress
        if let Some(rx) = self.recv_rx.as_mut() {
            let mut v = Vec::new();
            while let Ok(u) = rx.try_recv() {
                v.push(u);
            }
            for u in v {
                let now = Instant::now();
                let dt = now.duration_since(self.recv.last_tick).as_secs_f64().max(0.001);
                if u.total_bytes > 0 {
                    self.recv.total = u.total_bytes;
                }
                let delta = u.sent_bytes.saturating_sub(self.recv.last_bytes);
                if dt > 0.05 || u.done {
                    self.recv.speed = delta as f64 / dt;
                    self.recv.last_tick = now;
                    self.recv.last_bytes = u.sent_bytes;
                }
                self.recv.sent = u.sent_bytes;
                if !u.label.is_empty() {
                    self.recv.label = u.label.clone();
                }
                if u.done {
                    if let Some(e) = u.error {
                        // "bind ... port in use?" surfaces here
                        self.recv_error = e.clone();
                        self.listening = false;
                        self.recv_stop = None;
                        self.recv_rx = None;
                    } else {
                        self.recv.last_done = u.label.clone();
                        self.recv.sent = 0;
                        self.recv.total = 0;
                        self.recv.last_bytes = 0;
                    }
                    break;
                }
            }
        }
    }
}

fn dir_size(p: &PathBuf) -> u64 {
    let md = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if md.is_file() {
        return md.len();
    }
    walkdir::WalkDir::new(p)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

fn file_name_of(p: &PathBuf) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn eta_text(t: &TransferTab) -> String {
    if t.done || t.speed <= 0.0 || t.total == 0 {
        return String::new();
    }
    let remain = t.total.saturating_sub(t.sent) as f64;
    let secs = (remain / t.speed).max(0.0);
    if secs < 1.0 {
        "a second left".to_string()
    } else if secs < 60.0 {
        format!("{:.0}s left", secs)
    } else {
        format!("{:.1} min left", secs / 60.0)
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // drag & drop files onto the window -> send form
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.view = View::Send;
            self.add_files(dropped);
        }

        self.poll_channels();
        // Track view switches for the fade-in animation.
        let key = Self::view_key(self.view);
        if key != self.last_view_key {
            self.last_view_key = key;
            self.view_born = Instant::now();
        }
        // Smooth 60 fps while something animates, calm 150 ms polling otherwise.
        let fading = self.view_fade() < 1.0;
        let active_transfer = self.transfers.iter().any(|t| !t.done);
        let incoming = self.listening && self.recv.total > 0;
        if fading || active_transfer || incoming || self.listening || self.scanning {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        // ---- left sidebar ----
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .default_width(188.0)
            .frame(egui::Frame {
                fill: SIDEBAR_BG,
                inner_margin: 12.into(),
                ..Default::default()
            })
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    logo_badge(ui);
                    ui.vertical(|ui| {
                        ui.heading("Filele");
                        ui.label(
                            egui::RichText::new("Fast LAN transfer")
                                .small()
                                .color(INK_DIM),
                        );
                    });
                });
                ui.add_space(12.0);
                let w = ui.available_width();
                let send_sel = self.view == View::Send;
                if ui
                    .add_sized(
                        [w, 46.0],
                        egui::Button::selectable(
                            send_sel,
                            egui::RichText::new(format!("{}   Send", ICON_SEND))
                                .size(15.0)
                                .strong()
                                .color(if send_sel { egui::Color32::WHITE } else { INK }),
                        )
                        .fill(if send_sel { ACCENT } else { egui::Color32::from_rgb(15, 20, 29) })
                        .stroke(if send_sel { egui::Stroke::NONE } else { egui::Stroke::NONE })
                        .corner_radius(11),
                    )
                    .clicked()
                {
                    self.view = View::Send;
                }
                ui.add_space(6.0);
                let recv_sel = self.view == View::Receive;
                if ui
                    .add_sized(
                        [w, 46.0],
                        egui::Button::selectable(
                            recv_sel,
                            egui::RichText::new(format!("{}   Receive", ICON_RECEIVE))
                                .size(15.0)
                                .strong()
                                .color(if recv_sel { egui::Color32::WHITE } else { INK }),
                        )
                        .fill(if recv_sel { ACCENT } else { egui::Color32::from_rgb(15, 20, 29) })
                        .stroke(if recv_sel { egui::Stroke::NONE } else { egui::Stroke::NONE })
                        .corner_radius(11),
                    )
                    .clicked()
                {
                    self.view = View::Receive;
                }
                if !self.transfers.is_empty() {
                    ui.add_space(10.0);
                    ui.separator();
                    ui.label(egui::RichText::new("TRANSFERS").small().color(INK_DIM).strong());
                    // clone ids to avoid borrow issues on click
                    let ids: Vec<(u64, String, bool, bool, f32)> = self
                        .transfers
                        .iter()
                        .map(|t| (t.id, t.tab_label(), t.done, t.error.is_some(), t.frac()))
                        .collect();
                    for (id, label, done, failed, frac) in ids {
                        let mut text = format!("{}   {}", ICON_TRANSFER, label);
                        if failed {
                            text = format!("{}   {}   {}", ICON_TRANSFER, label, ICON_ERROR);
                        } else if done {
                            text = format!("{}   {}   {}", ICON_TRANSFER, label, ICON_CHECK);
                        } else if frac > 0.0 {
                            text = format!("{}   {}   {}%", ICON_TRANSFER, label, (frac * 100.0) as u32);
                        }
                        let tsel = self.view == View::Transfer(id);
                        if ui
                            .add_sized(
                                [w, 38.0],
                                egui::Button::selectable(
                                    tsel,
                                    egui::RichText::new(text)
                                        .size(13.0)
                                        .strong()
                                        .color(if tsel { egui::Color32::WHITE } else { INK }),
                                )
                                .fill(if tsel { ACCENT } else { egui::Color32::from_rgb(15, 20, 29) })
                                .stroke(egui::Stroke::NONE)
                                .corner_radius(9),
                            )
                            .clicked()
                        {
                            self.view = View::Transfer(id);
                        }
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(egui::RichText::new("v0.1.0").small().color(INK_DIM));
                });
            });

        // ---- main ----
        egui::CentralPanel::default()
            .frame(egui::Frame {
                fill: MAIN_BG,
                inner_margin: 16.into(),
                ..Default::default()
            })
            .show(ctx, |ui| {
            // Fade the freshly switched view in.
            ui.multiply_opacity(self.view_fade());
            // Send manages its own scroll so the Send button stays pinned to the bottom.
            match self.view {
                View::Send => self.ui_send(ui),
                View::Receive => {
                    egui::ScrollArea::vertical().show(ui, |ui| self.ui_receive(ui));
                }
                View::Transfer(id) => {
                    egui::ScrollArea::vertical().show(ui, |ui| self.ui_transfer(ui, id));
                }
            }
        });
    }
}

impl GuiApp {
    fn ui_send(&mut self, ui: &mut egui::Ui) {
        ui.heading("Send files to another PC");
        ui.label(
            egui::RichText::new("Choose what to send, pick a nearby device, then press Send.")
                .small()
                .color(INK_DIM),
        );
        ui.add_space(6.0);

        // Scrollable middle; the Send button below stays pinned to the bottom.
        let bottom_h = if self.send_error.is_empty() { 72.0 } else { 108.0 };
        let scroll_h = (ui.available_height() - bottom_h).max(140.0);
        egui::ScrollArea::vertical()
            .max_height(scroll_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
        // ---- files card ----
        card(ui, |ui| {
            ui.horizontal(|ui| {
                section_title(ui, "Files and folders");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} item(s) · {}",
                            self.files.len(),
                            human_bytes(self.files_size)
                        ))
                        .strong()
                        .color(INK),
                    );
                });
            });
            ui.add_space(2.0);
            if self.files.is_empty() {
                ui.label(
                    egui::RichText::new("Nothing here yet — add files below or drop them onto the window.")
                        .color(INK_DIM)
                        .italics(),
                );
            } else {
                egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                    let mut remove: Option<usize> = None;
                    for (i, f) in self.files.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if f.is_dir {
                                badge(ui, &format!("{} Folder", ICON_FOLDER), KIND_DIR);
                            } else {
                                badge(ui, &format!("{} File", ICON_FILE), KIND_FILE);
                            }
                            ui.label(
                                egui::RichText::new(trunc(&f.name, 42))
                                    .strong()
                                    .color(INK),
                            )
                            .on_hover_text(f.path.to_string_lossy());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("x").on_hover_text("Remove").clicked() {
                                        remove = Some(i);
                                    }
                                    ui.label(
                                        egui::RichText::new(human_bytes(f.size))
                                            .color(INK_DIM),
                                    );
                                },
                            );
                        });
                        ui.separator();
                    }
                    if let Some(i) = remove {
                        self.files.remove(i);
                        self.recalc_size();
                    }
                });
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Add files").clicked() {
                    if let Some(paths) = rfd::FileDialog::new().pick_files() {
                        self.add_files(paths);
                    }
                }
                if ui.button("Add folder").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.add_files(vec![p]);
                    }
                }
                if ui.button("Clear all").clicked() {
                    self.files.clear();
                    self.files_size = 0;
                }
            });
        });

        ui.add_space(8.0);

        // ---- target card ----
        card(ui, |ui| {
            section_title(ui, "Target device");
            if self.target_ip.trim().is_empty() {
                ui.label(
                    egui::RichText::new("No device selected — pick one from Devices nearby.")
                        .color(INK_DIM)
                        .italics(),
                );
            } else {
                let name = if self.target_name.trim().is_empty() {
                    "Device"
                } else {
                    self.target_name.trim()
                };
                ui.horizontal(|ui| {
                    dot(ui, GOOD, 8.0);
                    ui.label(egui::RichText::new(name).strong().size(16.0).color(INK));
                    ui.label(
                        egui::RichText::new(format!("{}:{}", self.target_ip.trim(), self.port))
                            .monospace()
                            .color(INK_DIM),
                    );
                });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{} Devices nearby", ICON_DEVICE)).small().color(INK_DIM));
                if self.peers.is_empty() {
                    // Nothing found yet — keep the spinner going until a device appears.
                    ui.spinner();
                } else {
                    if ui
                        .small_button(format!("{} Refresh", ICON_REFRESH))
                        .on_hover_text("Scan for devices now (auto-refresh keeps running)")
                        .clicked()
                    {
                        self.scanning = true;
                        self.disc_refresh.notify_one();
                    }
                    if self.scanning {
                        ui.spinner();
                    }
                }
            });
            if self.peers.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "Searching for computers… start Receive on the other PC and allow the app in its firewall.",
                    )
                    .small()
                    .color(INK_DIM)
                    .italics(),
                );
            } else {
                egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                    for peer in self.peers.clone() {
                        let ip = peer.addr.ip().to_string();
                        let selected = ip == self.target_ip.trim() && peer.port == self.port;
                        ui.horizontal(|ui| {
                            if selected {
                                dot(ui, GOOD, 8.0);
                            } else {
                                dot_hollow(ui, INK_DIM, 8.0);
                            }
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&peer.name).strong().color(INK));
                                ui.label(
                                    egui::RichText::new(format!("{}:{}", ip, peer.port))
                                        .small()
                                        .color(INK_DIM)
                                        .monospace(),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if selected {
                                        ui.label(
                                            egui::RichText::new("Selected")
                                                .small()
                                                .strong()
                                                .color(GOOD)
                                                .background_color(GOOD_BG),
                                        );
                                    } else if ui.button("Select").clicked() {
                                        self.select_device(&peer);
                                    }
                                },
                            );
                        });
                        ui.separator();
                    }
                });
            }
        });

        ui.add_space(8.0);

        // ---- options card ----
        card(ui, |ui| {
            section_title(ui, "Options");
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Slider::new(&mut self.streams, 0..=8).text("Streams"));
                ui.checkbox(&mut self.checksum, "Checksum");
                ui.checkbox(&mut self.compress, "Compress");
                ui.checkbox(&mut self.overwrite, "Overwrite");
            });
            ui.label(
                egui::RichText::new("Streams: parallel connections for files over 64 MB (0 = auto). Checksums keep transfers safe.")
                    .small()
                    .color(INK_DIM),
            );
        });
            });

        // ---- pinned bottom ----
        ui.add_space(8.0);
        if !self.send_error.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(&self.send_error)
                    .strong()
                    .color(BAD)
                    .background_color(BAD_BG),
            );
        }

        ui.add_space(6.0);
        let send_label = if self.target_name.trim().is_empty() {
            format!("{} Send", ICON_SEND)
        } else {
            format!("{} Send to {}", ICON_SEND, short_name(self.target_name.trim()))
        };
        let btn = egui::Button::new(egui::RichText::new(send_label).size(17.0).strong())
            .min_size(egui::vec2(ui.available_width(), 52.0))
            .fill(ACCENT)
            .corner_radius(12);
        if ui.add(btn).clicked() {
            self.start_send();
        }
    }

    fn ui_transfer(&mut self, ui: &mut egui::Ui, id: u64) {
        let pos = match self.transfers.iter().position(|t| t.id == id) {
            Some(p) => p,
            None => {
                self.view = View::Send;
                return;
            }
        };
        let name = self.transfers[pos].target_name.clone();
        let ip = self.transfers[pos].target_ip.clone();
        ui.heading(format!("Sending to {}", name));
        ui.label(
            egui::RichText::new(format!("Target: {}  ·  {}", name, ip))
                .small()
                .color(INK_DIM),
        );
        ui.add_space(6.0);

        // file list with per-file bars (read-only snapshot to keep borrow short)
        let files: Vec<FileProg> = self.transfers[pos].files.clone();
        card(ui, |ui| {
            section_title(ui, &format!("Files ({})", files.len()));
            if files.is_empty() {
                ui.label(
                    egui::RichText::new(Self::waiting_text(ui.ctx(), "Preparing file list"))
                        .small()
                        .color(INK_DIM)
                        .italics(),
                );
            } else {
                egui::ScrollArea::vertical().max_height(330.0).show(ui, |ui| {
                    for (fi, f) in files.iter().enumerate() {
                        let frac = if f.size == 0 {
                            1.0
                        } else {
                            (f.sent as f32 / f.size as f32).clamp(0.0, 1.0)
                        };
                        // Ease each bar toward its target so chunks don't jump.
                        let frac = ui.ctx().animate_value_with_time(
                            egui::Id::new(("tfile", id, fi)),
                            frac,
                            0.25,
                        );
                        ui.horizontal(|ui| {
                            if frac >= 1.0 {
                                dot(ui, GOOD, 8.0);
                            } else {
                                dot_hollow(ui, INK_DIM, 8.0);
                            }
                            ui.label(
                                egui::RichText::new(trunc(&f.name, 44))
                                    .strong()
                                    .color(INK),
                            )
                            .on_hover_text(&f.name);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if frac >= 1.0 {
                                        ui.label(
                                            egui::RichText::new("done")
                                                .small()
                                                .strong()
                                                .color(GOOD)
                                                .background_color(GOOD_BG),
                                        );
                                    } else {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{}% · {}",
                                                (frac * 100.0) as u32,
                                                human_bytes(f.size),
                                            ))
                                            .small()
                                            .color(INK_DIM),
                                        );
                                    }
                                },
                            );
                        });
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(ui.available_width())
                                .show_percentage(),
                        );
                        ui.add_space(2.0);
                    }
                });
            }
        });

        // overall bar + info
        ui.add_space(8.0);
        card(ui, |ui| {
            if self.transfers[pos].done {
                section_title(ui, "Overall progress");
            } else {
                ui.horizontal(|ui| {
                    pulse_dot(ui, ACCENT, 8.0);
                    section_title(ui, "Overall progress");
                });
            }
            let t = &self.transfers[pos];
            let frac = ui.ctx().animate_value_with_time(
                egui::Id::new(("toverall", id)),
                t.frac(),
                0.2,
            );
            let elapsed = t.started.elapsed().as_secs_f64().max(0.1);
            let avg = t.sent as f64 / elapsed;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{}%", (frac * 100.0) as u32)).strong().size(20.0).color(INK));
                let spd = if t.done { avg } else { t.speed.max(avg * 0.15) };
                ui.label(
                    egui::RichText::new(format!(
                        "{} of {}  ·  {}  ·  {}",
                        human_bytes(t.sent),
                        human_bytes(t.total.max(t.sent)),
                        human_speed(spd),
                        eta_text(t)
                    ))
                    .color(INK_DIM),
                );
            });
            ui.add(egui::ProgressBar::new(frac).show_percentage().animate(!t.done));
            if !t.label.is_empty() {
                ui.label(egui::RichText::new(format!("Current file: {}", trunc(&t.label, 60))).small().color(INK_DIM));
            }
            if let Some(e) = t.error.clone() {
                ui.label(
                    egui::RichText::new(format!("Failed: {e}"))
                        .strong()
                        .color(BAD)
                        .background_color(BAD_BG),
                );
            } else if t.done {
                ui.label(
                    egui::RichText::new(format!("Done — {}", t.done_msg))
                        .strong()
                        .color(GOOD)
                        .background_color(GOOD_BG),
                );
            }
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_close = self.transfers[pos].done;
            if ui.add_enabled(can_close, egui::Button::new("Close")).clicked() {
                self.transfers.remove(pos);
                self.view = View::Send;
            }
            if !can_close {
                ui.label(egui::RichText::new("You can go back to Send — this keeps running.").small().weak());
            }
        });
    }

    fn ui_receive(&mut self, ui: &mut egui::Ui) {
        ui.heading("Receive files on this PC");
        ui.label(
            egui::RichText::new("Get visible so nearby computers can find and send to you.")
                .small()
                .color(INK_DIM),
        );
        ui.add_space(6.0);

        card(ui, |ui| {
            section_title(ui, "Save to");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.out_dir)
                        .hint_text("destination folder")
                        .desired_width(360.0),
                );
                if ui.button("Browse").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.out_dir = p.to_string_lossy().into_owned();
                        self.save_config();
                    }
                }
                if ui.button("Open").clicked() {
                    let _ = std::process::Command::new("explorer")
                        .arg(self.out_dir.trim())
                        .spawn();
                }
            });
        });

        ui.add_space(8.0);
        let btn_text = if self.listening { format!("{} Stop being visible", ICON_RECEIVE) } else { format!("{} Start being visible", ICON_RECEIVE) };
        let btn = egui::Button::new(egui::RichText::new(btn_text).size(17.0).strong())
            .min_size(egui::vec2(ui.available_width(), 48.0))
            .fill(if self.listening { WARN } else { GOOD });
        if ui.add(btn).clicked() {
            if self.listening {
                self.stop_listen();
            } else {
                self.start_listen();
            }
        }

        if !self.recv_error.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(&self.recv_error)
                    .strong()
                    .color(BAD)
                    .background_color(BAD_BG),
            );
        }

        if self.listening {
            ui.add_space(8.0);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    pulse_dot(ui, GOOD, 8.0);
                    ui.label(
                        egui::RichText::new(format!("Visible to others as {}", my_hostname()))
                            .strong()
                            .size(15.0)
                            .color(INK),
                    );
                });
            });
            ui.add_space(4.0);
            if self.recv.total > 0 {
                card(ui, |ui| {
                    section_title(ui, "Incoming transfer");
                    let frac = ui.ctx().animate_value_with_time(
                        egui::Id::new("rincoming"),
                        (self.recv.sent as f32 / self.recv.total.max(1) as f32).clamp(0.0, 1.0),
                        0.2,
                    );
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{}%", (frac * 100.0) as u32)).strong().size(18.0).color(INK),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} of {} · {}",
                                human_bytes(self.recv.sent),
                                human_bytes(self.recv.total.max(self.recv.sent)),
                                human_speed(self.recv.speed)
                            ))
                            .color(INK_DIM),
                        );
                    });
                    ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                    if !self.recv.label.is_empty() {
                        ui.label(egui::RichText::new(format!("Current file: {}", trunc(&self.recv.label, 60))).small().color(INK_DIM));
                    }
                });
            } else {
                ui.label(
                    egui::RichText::new(if self.recv.label.is_empty() {
                        Self::waiting_text(ui.ctx(), "Waiting for a sender")
                    } else {
                        self.recv.label.clone()
                    })
                    .small()
                    .color(INK_DIM)
                    .italics(),
                );
            }
            if !self.recv.last_done.is_empty() {
                ui.label(
                    egui::RichText::new(format!("Last received: {}", self.recv.last_done))
                        .strong()
                        .color(GOOD)
                        .background_color(GOOD_BG),
                );
            }
        } else {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Press the button above so nearby PCs can find and send to you.")
                    .small()
                    .color(INK_DIM)
                    .italics(),
            );
            ui.label(
                egui::RichText::new("Firewall must allow TCP 53317-53318 + UDP 53319 (see allow-firewall.ps1).")
                    .small()
                    .color(WARN),
            );
        }

        ui.add_space(8.0);
        card(ui, |ui| {
            section_title(ui, "Options");
            ui.horizontal(|ui| {
                ui.label("Port:");
                ui.add(egui::DragValue::new(&mut self.recv_port).range(1..=65530));
                ui.checkbox(&mut self.recv_overwrite, "Overwrite existing");
            });
        });
    }
}
