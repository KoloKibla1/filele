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
use crate::progress::{human_bytes, human_speed, ApprovalRequest, TransferUpdate, STOPPED_BY_RECEIVER, STOPPED_BY_SENDER, DECLINED};
use crate::protocol::{OFFER_FILE, DEFAULT_PORT};

#[derive(Clone, Copy)]
struct Palette {
    accent: egui::Color32,
    accent_soft: egui::Color32,
    good: egui::Color32,
    good_bg: egui::Color32,
    warn: egui::Color32,
    bad: egui::Color32,
    bad_bg: egui::Color32,
    ink: egui::Color32,
    ink_dim: egui::Color32,
    kind_file: egui::Color32,
    kind_dir: egui::Color32,
    main_bg: egui::Color32,
    sidebar_bg: egui::Color32,
    card_bg: egui::Color32,
    nav_bg: egui::Color32,
    widget_bg: egui::Color32,
    widget_hover: egui::Color32,
}

impl Palette {
    fn dark() -> Self {
        Self {
            accent: egui::Color32::from_rgb(47, 129, 247),
            accent_soft: egui::Color32::from_rgb(31, 72, 133),
            good: egui::Color32::from_rgb(63, 185, 80),
            good_bg: egui::Color32::from_rgb(24, 64, 36),
            warn: egui::Color32::from_rgb(210, 153, 34),
            bad: egui::Color32::from_rgb(248, 81, 73),
            bad_bg: egui::Color32::from_rgb(76, 28, 30),
            ink: egui::Color32::from_rgb(230, 237, 243),
            ink_dim: egui::Color32::from_rgb(139, 148, 158),
            kind_file: egui::Color32::from_rgb(121, 192, 255),
            kind_dir: egui::Color32::from_rgb(227, 179, 65),
            main_bg: egui::Color32::from_rgb(13, 17, 23),
            sidebar_bg: egui::Color32::from_rgb(7, 10, 16),
            card_bg: egui::Color32::from_rgb(19, 25, 34),
            nav_bg: egui::Color32::from_rgb(15, 20, 29),
            widget_bg: egui::Color32::from_rgb(30, 36, 46),
            widget_hover: egui::Color32::from_rgb(38, 45, 58),
        }
    }
    fn light() -> Self {
        Self {
            accent: egui::Color32::from_rgb(9, 105, 218),
            accent_soft: egui::Color32::from_rgb(208, 228, 252),
            good: egui::Color32::from_rgb(40, 158, 74),
            good_bg: egui::Color32::from_rgb(218, 245, 225),
            warn: egui::Color32::from_rgb(154, 103, 0),
            bad: egui::Color32::from_rgb(207, 34, 46),
            bad_bg: egui::Color32::from_rgb(255, 222, 222),
            ink: egui::Color32::from_rgb(26, 29, 35),
            ink_dim: egui::Color32::from_rgb(99, 108, 120),
            kind_file: egui::Color32::from_rgb(9, 105, 218),
            kind_dir: egui::Color32::from_rgb(154, 103, 0),
            main_bg: egui::Color32::from_rgb(242, 244, 247),
            sidebar_bg: egui::Color32::from_rgb(226, 230, 235),
            card_bg: egui::Color32::from_rgb(255, 255, 255),
            nav_bg: egui::Color32::from_rgb(255, 255, 255),
            widget_bg: egui::Color32::from_rgb(228, 232, 237),
            widget_hover: egui::Color32::from_rgb(210, 216, 224),
        }
    }
}

static THEME_PALETTE: std::sync::RwLock<Palette> = std::sync::RwLock::new(Palette {
    accent: egui::Color32::from_rgb(47, 129, 247),
    accent_soft: egui::Color32::from_rgb(31, 72, 133),
    good: egui::Color32::from_rgb(63, 185, 80),
    good_bg: egui::Color32::from_rgb(24, 64, 36),
    warn: egui::Color32::from_rgb(210, 153, 34),
    bad: egui::Color32::from_rgb(248, 81, 73),
    bad_bg: egui::Color32::from_rgb(76, 28, 30),
    ink: egui::Color32::from_rgb(230, 237, 243),
    ink_dim: egui::Color32::from_rgb(139, 148, 158),
    kind_file: egui::Color32::from_rgb(121, 192, 255),
    kind_dir: egui::Color32::from_rgb(227, 179, 65),
    main_bg: egui::Color32::from_rgb(13, 17, 23),
    sidebar_bg: egui::Color32::from_rgb(7, 10, 16),
    card_bg: egui::Color32::from_rgb(19, 25, 34),
    nav_bg: egui::Color32::from_rgb(15, 20, 29),
    widget_bg: egui::Color32::from_rgb(30, 36, 46),
    widget_hover: egui::Color32::from_rgb(38, 45, 58),
});

/// Current theme palette (copy). Follows the setting in the bottom-right corner.
fn pal() -> Palette {
    *THEME_PALETTE.read().unwrap()
}

fn set_palette(dark: bool) {
    *THEME_PALETTE.write().unwrap() = if dark { Palette::dark() } else { Palette::light() };
    THEME_DARK.store(dark, std::sync::atomic::Ordering::Relaxed);
}

static THEME_DARK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Sidebar tab with right-aligned "text icon" and a background fill that
/// grows left-to-right with transfer progress (no numbers, no spinner).
#[allow(clippy::too_many_arguments)]
fn tab_row(
    ui: &mut egui::Ui,
    id: egui::Id,
    width: f32,
    height: f32,
    selected: bool,
    frac: Option<f32>,
    icon: char,
    text: &str,
    text_size: f32,
    radius: u8,
) -> egui::Response {
    let (_auto, rect) = ui.allocate_space(egui::vec2(width, height));
    let resp = ui.interact(rect, id, egui::Sense::click());
    let mut base = if selected { pal().accent } else { pal().nav_bg };
    if resp.hovered() && !selected {
        base = pal().widget_hover;
    }
    ui.painter().rect_filled(rect, radius, base);
    if let Some(f) = frac {
        let f = f.clamp(0.0, 1.0);
        if f > 0.0 {
            let overlay = if THEME_DARK.load(std::sync::atomic::Ordering::Relaxed) {
                egui::Color32::from_white_alpha(72)
            } else {
                egui::Color32::from_black_alpha(30)
            };
            let cut = egui::Rect::from_min_max(
                rect.min,
                egui::pos2(rect.min.x + rect.width() * f, rect.max.y),
            );
            ui.painter().with_clip_rect(cut).rect_filled(rect, radius, overlay);
        }
    }
    let fg = if selected { egui::Color32::WHITE } else { pal().ink };
    ui.painter().text(
        rect.right_center() + egui::vec2(-14.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        format!("{}   {}", text, icon),
        egui::FontId::new(text_size, egui::FontFamily::Proportional),
        fg,
    );
    resp
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Theme {
    Dark,
    Light,
    System,
}

impl Theme {
    fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::System => "System",
        }
    }
}

// Backgrounds come from pal() so the light theme applies (see Palette).

// Segoe MDL2 Assets (in-box on Windows 10/11) icon glyphs — no extra font files.
const ICON_SEND: char = '\u{E122}'; // paper plane
const ICON_RECEIVE: char = '\u{E896}'; // download
const ICON_TRANSFER: char = '\u{E122}';
const ICON_CHECK: char = '\u{E73E}';
const ICON_ERROR: char = '\u{E783}';
const ICON_LOGO: char = '\u{E945}'; // lightning — fast transfer
const ICON_DEVICE: char = '\u{E772}';
const ICON_REFRESH: char = '\u{E72C}';
const ICON_SETTINGS: char = '\u{E713}'; // gear
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
        fill: pal().card_bg,
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

/// Open a folder in Explorer via ShellExecuteW (the supported API —
/// spawning explorer.exe directly fails with OS error 50 on some setups).
fn open_folder(path: &str) -> std::io::Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> isize;
    }
    let op: Vec<u16> = OsStr::new("open").encode_wide().chain(std::iter::once(0)).collect();
    let f: Vec<u16> = OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect();
    // SW_SHOWNORMAL = 1. Returns a value > 32 on success.
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), f.as_ptr(), std::ptr::null(), std::ptr::null(), 1)
    };
    if r > 32 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(r as i32))
    }
}

fn logo_badge(ui: &mut egui::Ui) {
    egui::Frame {
        fill: pal().accent,
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

/// Default save folder: the user's Downloads.
fn downloads_dir() -> String {
    std::env::var("USERPROFILE")
        .map(|home| format!("{}\\Downloads", home))
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

/// Apply the full visual theme (palette + egui visuals). Called once at
/// startup and again whenever the setting (or the OS theme) changes it.
fn apply_theme_visuals(ctx: &egui::Context, dark: bool) {
    set_palette(dark);
    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
    let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    // Layered flat palette, no pale outlines.
    visuals.dark_mode = dark;
    visuals.window_fill = pal().main_bg;
    visuals.panel_fill = pal().main_bg;
    visuals.faint_bg_color = pal().card_bg;
    visuals.extreme_bg_color = pal().sidebar_bg;
    visuals.code_bg_color = pal().card_bg;
    visuals.hyperlink_color = egui::Color32::from_rgb(68, 147, 248);
    visuals.warn_fg_color = pal().warn;
    visuals.error_fg_color = pal().bad;
    visuals.selection.bg_fill = pal().accent.gamma_multiply(0.35);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, pal().accent);
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
        w.bg_fill = pal().widget_bg;
        w.bg_stroke = egui::Stroke::NONE;
        w.fg_stroke = egui::Stroke::new(1.0_f32, pal().ink);
        w.corner_radius = 8.into();
    }
    visuals.widgets.noninteractive.bg_fill = pal().card_bg;
    visuals.widgets.hovered.bg_fill = pal().widget_hover;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, pal().accent);
    visuals.widgets.active.bg_fill = pal().accent_soft;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, pal().accent);
    ctx.set_visuals(visuals);
    apply_text_and_spacing(ctx);
}

/// Button padding, gaps and font sizes. Re-applied after every theme swap
/// because egui's set_theme resets the whole style (that reset is what made
/// the layout jump between dark and light).
fn apply_text_and_spacing(ctx: &egui::Context) {
    let mut style = ctx.style().as_ref().clone();
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
    ctx.set_style(style);
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
    // First paint follows the OS theme (the in-app setting can override it).
    let initial_dark = cc
        .egui_ctx
        .system_theme()
        .map(|t| t == egui::Theme::Dark)
        .unwrap_or(true);
    apply_theme_visuals(&cc.egui_ctx, initial_dark);
    apply_text_and_spacing(&cc.egui_ctx);
}

/// Shorten long names with an ellipsis (egui has no single-line ellipsis).
fn trunc(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", head)
}

/// Full-width button with centered content. egui 0.32 left-aligns
/// single-atom buttons, so the content is sandwiched between spacers.
fn centered_button(text: egui::RichText) -> egui::Button<'static> {
    egui::Button::new((egui::Atom::grow(), text, egui::Atom::grow()))
}

/// Small tinted badge, e.g. icon-only file/folder marker.
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

/// Centered error popup (transfer failures, open-folder failures, ...).
#[derive(Clone)]
struct ErrorPopup {
    title: String,
    message: String,
    context: String,
    /// Closing the popup also closes this tab (transfer errors).
    tab_id: Option<u64>,
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
    /// Incoming (receiver-side) tab instead of outgoing send.
    incoming: bool,
    /// Incoming tab waiting for the local user's Allow/Deny.
    pending: bool,
    verdict_tx: Option<tokio::sync::oneshot::Sender<bool>>,
    /// Outgoing tab waiting for the remote side to approve.
    awaiting: bool,
    /// Outgoing send task (Stop button aborts it).
    task: Option<tokio::task::JoinHandle<()>>,
    /// Incoming transfer stop flag (Stop button sets it; engine polls it).
    stop_flag: Option<Arc<AtomicBool>>,
    /// Incoming save folder snapshot (Open folder button).
    save_dir: String,
    /// Seconds from tab open/allow to done (snapshot at completion).
    took_secs: f64,
    /// First byte flowed at (transfer time excludes approval wait).
    active_since: Option<Instant>,
}

impl TransferTab {
    fn tab_label(&self) -> String {
        if self.incoming {
            format!("{} recv", short_name(&self.target_name))
        } else {
            format!("{} send", short_name(&self.target_name))
        }
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
    approval_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ApprovalRequest>>,
    // theme
    theme: Theme,
    applied_dark: bool,
    show_settings: bool,
    error_popup: Option<ErrorPopup>,
    // animation state
    view_born: Instant,
    last_view_key: u64,
}

impl GuiApp {
    fn new(rt: tokio::runtime::Handle) -> Self {
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
            target_name: String::new(),
            target_ip: String::new(),
            port: DEFAULT_PORT,
            streams: 4,
            checksum: true,
            compress: false,
            overwrite: true,
            send_error: String::new(),
            peers: Vec::new(),
            scanning: true,
            last_scan: None,
            disc_rx: Some(disc_rx),
            disc_refresh,
            transfers: Vec::new(),
            out_dir: downloads_dir(),
            recv_port: DEFAULT_PORT,
            recv_overwrite: true,
            listening: false,
            recv_stop: None,
            recv_rx: None,
            recv: RecvProg::default(),
            recv_error: String::new(),
            approval_rx: None,
            theme: Theme::System,
            applied_dark: true,
            show_settings: false,
            error_popup: None,
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

    /// 0..1 eased fade for the freshly switched view (~260 ms).
    fn view_fade(&self) -> f32 {
        let t = self.view_born.elapsed().as_secs_f32() / 0.26;
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
    }

    /// Saved target that is actually on the network right now.
    fn target_online(&self) -> bool {
        let ip = self.target_ip.trim();
        if ip.is_empty() {
            return false;
        }
        self.peers
            .iter()
            .any(|p| p.addr.ip().to_string() == ip && p.port == self.port)
    }

    fn clear_target(&mut self) {
        self.target_name.clear();
        self.target_ip.clear();
        self.send_error.clear();
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
        if !self.target_online() {
            self.send_error = "That device is offline — pick one from Devices nearby.".to_string();
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
            sender_name: my_hostname(),
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
            incoming: false,
            pending: false,
            verdict_tx: None,
            awaiting: true,
            task: None,
            stop_flag: None,
            save_dir: String::new(),
            took_secs: 0.0,
            active_since: None,
        });
        self.view = View::Transfer(id);
        let h = self.rt.spawn(async move {
            match crate::sender::run_send_with_progress(files, opt, Some(tx.clone())).await {
                Ok(()) => {}
                Err(e) => {
                    // A dropped connection means the receiver stopped it.
                    let msg = if crate::protocol::is_disconnect(&e) {
                        STOPPED_BY_RECEIVER.to_string()
                    } else {
                        format!("{:#}", e)
                    };
                    let _ = tx.send(TransferUpdate {
                        sent_bytes: 0,
                        total_bytes: 0,
                        label: String::new(),
                        done: true,
                        error: Some(msg),
                        files: None,
                        file_index: None,
                        file_sent: None,
                    });
                }
            }
        });
        if let Some(t) = self.transfers.last_mut() {
            t.task = Some(h);
        }
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
        let (atx, arx) = tokio::sync::mpsc::unbounded_channel();
        self.approval_rx = Some(arx);
        let stop = Arc::new(AtomicBool::new(false));
        self.recv_stop = Some(stop.clone());
        self.listening = true;
        self.rt.spawn(async move {
            if let Err(e) = crate::receiver::run_recv_gui(opt, Some(tx.clone()), stop, Some(atx)).await {
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
        // incoming approval requests -> receiver tabs (drained first so a
        // same-tick verdict update already finds its tab)
        if let Some(rx) = self.approval_rx.as_mut() {
            let mut reqs: Vec<ApprovalRequest> = Vec::new();
            while let Ok(r) = rx.try_recv() {
                reqs.push(r);
            }
            for r in reqs {
                let id = self.next_id;
                self.next_id += 1;
                let files: Vec<FileProg> = r
                    .offer
                    .iter()
                    .filter(|f| f.kind == OFFER_FILE)
                    .map(|f| FileProg { name: f.rel.clone(), size: f.size, sent: 0 })
                    .collect();
                self.transfers.push(TransferTab {
                    id,
                    target_name: r.sender_name.clone(),
                    target_ip: r.sender_ip.clone(),
                    files,
                    sent: 0,
                    total: r.total,
                    label: "Waiting for your approval...".to_string(),
                    speed: 0.0,
                    started: Instant::now(),
                    last_bytes: 0,
                    last_tick: Instant::now(),
                    done: false,
                    error: None,
                    done_msg: String::new(),
                    rx: Some(r.progress_rx),
                    incoming: true,
                    pending: true,
                    verdict_tx: Some(r.verdict),
                    awaiting: false,
                    task: None,
                    stop_flag: Some(r.stop.clone()),
                    save_dir: self.out_dir.clone(),
                    took_secs: 0.0,
                    active_since: None,
                });
                self.view = View::Transfer(id);
            }
        }
        // transfer progress
        let mut remove_ids: Vec<(u64, bool)> = Vec::new();
        let mut popup_req: Option<ErrorPopup> = None;
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
                if (u.sent_bytes > 0 || u.done) && t.awaiting {
                    t.awaiting = false;
                    t.active_since = Some(Instant::now());
                }
                if u.done {
                    t.done = true;
                    finished = true;
                    let first = t.took_secs == 0.0;
                    if first {
                        // Transfer time excludes the approval wait.
                        t.took_secs = t
                            .active_since
                            .map(|a| a.elapsed().as_secs_f64())
                            .unwrap_or_else(|| t.started.elapsed().as_secs_f64());
                    }
                    if let Some(e) = u.error {
                        let stopped = e == STOPPED_BY_SENDER || e == STOPPED_BY_RECEIVER;
                        if first && !stopped && popup_req.is_none() {
                            let dir = if t.incoming { "Receiving" } else { "Sending" };
                            popup_req = Some(ErrorPopup {
                                title: "Transfer failed".to_string(),
                                message: e.clone(),
                                context: format!("{} · {}", dir, t.target_name),
                                tab_id: Some(t.id),
                            });
                        }
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
            // A stop from either side closes the tab on both ends.
            if t.done {
                let stopped = t.error.as_deref() == Some(STOPPED_BY_SENDER)
                    || t.error.as_deref() == Some(STOPPED_BY_RECEIVER);
                if stopped {
                    remove_ids.push((t.id, t.incoming));
                }
            }
        }
        if !remove_ids.is_empty() {
            self.transfers.retain(|t| !remove_ids.iter().any(|(id, _)| *id == t.id));
            if let View::Transfer(id) = self.view {
                if let Some((_, incoming)) = remove_ids.iter().find(|(rid, _)| *rid == id) {
                    self.view = if *incoming { View::Receive } else { View::Send };
                }
            }
        }
        if popup_req.is_some() && self.error_popup.is_none() {
            self.error_popup = popup_req;
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

/// Elapsed time for a finished transfer ("Took 0.4 s", "Took 8 s", "Took 2.5 min").
fn took_text(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs < 1.0 {
        format!("Took {:.1} s", secs)
    } else if secs < 90.0 {
        format!("Took {:.0} s", secs)
    } else {
        format!("Took {:.1} min", secs / 60.0)
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
        // Follow the theme setting (System tracks the OS theme live).
        let want_dark = match self.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => ctx.system_theme().map(|t| t == egui::Theme::Dark).unwrap_or(true),
        };
        if want_dark != self.applied_dark {
            self.applied_dark = want_dark;
            apply_theme_visuals(ctx, want_dark);
        }
        // Track view switches for the fade-in animation.
        let key = Self::view_key(self.view);
        if key != self.last_view_key {
            self.last_view_key = key;
            self.view_born = Instant::now();
        }
        // Smooth 30 fps while something animates (full-rate repaint made
        // big file lists stutter tab switches), calm 150 ms polling otherwise.
        let fading = self.view_fade() < 1.0;
        let active_transfer = self.transfers.iter().any(|t| !t.done);
        let pending_approval = self.transfers.iter().any(|t| t.pending);
        let incoming = self.listening && self.recv.total > 0;
        if fading || active_transfer || pending_approval || incoming || self.listening || self.scanning {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        // ---- left sidebar ----
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .default_width(188.0)
            .frame(egui::Frame {
                fill: pal().sidebar_bg,
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
                                .color(pal().ink_dim),
                        );
                    });
                });
                ui.add_space(12.0);
                let w = ui.available_width();
                if tab_row(ui, egui::Id::new("nav-send"), w, 46.0, self.view == View::Send, None, ICON_SEND, "Send", 15.0, 11).clicked() {
                    self.view = View::Send;
                }
                ui.add_space(6.0);
                if tab_row(ui, egui::Id::new("nav-recv"), w, 46.0, self.view == View::Receive, None, ICON_RECEIVE, "Receive", 15.0, 11).clicked() {
                    self.view = View::Receive;
                }
                if self.transfers.iter().any(|t| !t.incoming) {
                    ui.add_space(10.0);
                    ui.separator();
                    ui.label(egui::RichText::new("TRANSFERS").small().color(pal().ink_dim).strong());
                    // clone ids to avoid borrow issues on click
                    let ids: Vec<(u64, String, bool, bool, f32)> = self
                        .transfers
                        .iter()
                        .filter(|t| !t.incoming)
                        .map(|t| (t.id, t.tab_label(), t.done, t.error.is_some(), t.frac()))
                        .collect();
                    for (id, label, done, failed, frac) in ids {
                        let t = self.transfers.iter().find(|t| t.id == id);
                        let awaiting = t.map(|t| t.awaiting).unwrap_or(false);
                        let status = if failed {
                            format!("   {}", ICON_ERROR)
                        } else if done {
                            format!("   {}", ICON_CHECK)
                        } else if awaiting {
                            "   ?".to_string()
                        } else if frac > 0.0 {
                            format!("   {}%", (frac * 100.0) as u32)
                        } else {
                            String::new()
                        };
                        let bar = (!done && !awaiting).then_some(frac);
                        if tab_row(ui, egui::Id::new(("tab", id)), w, 38.0, self.view == View::Transfer(id), bar, ICON_TRANSFER, &format!("{}{}", label, status), 13.0, 9).clicked() {
                            self.view = View::Transfer(id);
                        }
                    }
                }
                if self.transfers.iter().any(|t| t.incoming) {
                    ui.add_space(10.0);
                    ui.separator();
                    ui.label(egui::RichText::new("INCOMING").small().color(pal().ink_dim).strong());
                    let ids: Vec<(u64, String, bool, bool, bool, f32)> = self
                        .transfers
                        .iter()
                        .filter(|t| t.incoming)
                        .map(|t| (t.id, t.tab_label(), t.pending, t.done, t.error.is_some(), t.frac()))
                        .collect();
                    for (id, label, pending, done, failed, frac) in ids {
                        let status = if failed {
                            format!("   {}", ICON_ERROR)
                        } else if done {
                            format!("   {}", ICON_CHECK)
                        } else if pending {
                            "   ?".to_string()
                        } else if frac > 0.0 {
                            format!("   {}%", (frac * 100.0) as u32)
                        } else {
                            String::new()
                        };
                        let bar = (!done && !pending).then_some(frac);
                        if tab_row(ui, egui::Id::new(("tab", id)), w, 38.0, self.view == View::Transfer(id), bar, ICON_RECEIVE, &format!("{}{}", label, status), 13.0, 9).clicked() {
                            self.view = View::Transfer(id);
                        }
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(egui::RichText::new("v0.1.0").small().color(pal().ink_dim));
                });
            });

        // ---- main ----
        egui::CentralPanel::default()
            .frame(egui::Frame {
                fill: pal().main_bg,
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

        // ---- settings gear, top-right corner ----
        egui::Area::new(egui::Id::new("settings-gear"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
            .show(ctx, |ui| {
                let btn = egui::Button::new(icon_text(ICON_SETTINGS, 16.0, pal().ink_dim))
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(8);
                if ui.add_sized(egui::vec2(32.0, 32.0), btn).on_hover_text("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_settings = false;
        }
        if self.show_settings {
            egui::Area::new(egui::Id::new("settings-dropdown"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 52.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame {
                        fill: pal().card_bg,
                        stroke: egui::Stroke::NONE,
                        corner_radius: 12.into(),
                        inner_margin: 10.into(),
                        shadow: egui::Shadow {
                            offset: [0, 6],
                            blur: 18,
                            spread: 0,
                            color: egui::Color32::from_black_alpha(70),
                        },
                        ..Default::default()
                    }
                    .show(ui, |ui| {
                        ui.set_min_width(170.0);
                        ui.label(
                            egui::RichText::new("Theme")
                                .small()
                                .strong()
                                .color(pal().ink_dim),
                        );
                        ui.add_space(2.0);
                        for opt in [Theme::Dark, Theme::Light, Theme::System] {
                            let sel = self.theme == opt;
                            let mark = if sel { "✓  " } else { "      " };
                            if ui
                                .add_sized(
                                    [ui.available_width(), 34.0],
                                    egui::Button::selectable(
                                        sel,
                                        (
                                            egui::Atom::grow(),
                                            egui::RichText::new(format!("{}{}", mark, opt.label()))
                                                .size(14.0)
                                                .color(if sel { egui::Color32::WHITE } else { pal().ink }),
                                            egui::Atom::grow(),
                                        ),
                                    )
                                    .fill(if sel { pal().accent } else { egui::Color32::TRANSPARENT })
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(8),
                                )
                                .clicked()
                            {
                                self.theme = opt;
                                self.show_settings = false;
                            }
                        }
                    });
                });
        }
        // ---- centered error popup (dims the UI behind) ----
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.error_popup = None;
        }
        if let Some(p) = self.error_popup.clone() {
            ctx.layer_painter(egui::LayerId::background())
                .rect_filled(ctx.screen_rect(), 0.0, egui::Color32::from_black_alpha(110));
            egui::Window::new("error-popup")
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .order(egui::Order::Foreground)
                .frame(egui::Frame {
                    fill: pal().card_bg,
                    stroke: egui::Stroke::NONE,
                    corner_radius: 14.into(),
                    inner_margin: 22.into(),
                    shadow: egui::Shadow {
                        offset: [0, 8],
                        blur: 28,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(90),
                    },
                    ..Default::default()
                })
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(
                        egui::RichText::new(&p.title).strong().size(18.0).color(pal().bad),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(&p.message).size(14.0).color(pal().ink),
                    );
                    if !p.context.is_empty() {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(&p.context).small().color(pal().ink_dim),
                        );
                    }
                    ui.add_space(12.0);
                    if ui
                        .add_sized([ui.available_width(), 42.0], centered_button(egui::RichText::new("Close")))
                        .clicked()
                    {
                        if let Some(id) = p.tab_id {
                            self.transfers.retain(|t| t.id != id);
                            if self.view == View::Transfer(id) {
                                self.view = View::Send;
                            }
                        }
                        self.error_popup = None;
                    }
                });
        }
    }
}

impl GuiApp {
    fn ui_send(&mut self, ui: &mut egui::Ui) {
        ui.heading("Send files to another PC");
        ui.label(
            egui::RichText::new("Choose what to send, pick a nearby device, then press Send.")
                .small()
                .color(pal().ink_dim),
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
                        .color(pal().ink),
                    );
                });
            });
            ui.add_space(2.0);
            if self.files.is_empty() {
                ui.label(
                    egui::RichText::new("Nothing here yet — add files below or drop them onto the window.")
                        .color(pal().ink_dim)
                        .italics(),
                );
            } else {
                egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                    let mut remove: Option<usize> = None;
                    for (i, f) in self.files.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if f.is_dir {
                                badge(ui, &ICON_FOLDER.to_string(), pal().kind_dir);
                            } else {
                                badge(ui, &ICON_FILE.to_string(), pal().kind_file);
                            }
                            ui.label(
                                egui::RichText::new(trunc(&f.name, 42))
                                    .strong()
                                    .color(pal().ink),
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
                                            .color(pal().ink_dim),
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
                        .color(pal().ink_dim)
                        .italics(),
                );
            } else {
                let name = if self.target_name.trim().is_empty() {
                    "Device".to_string()
                } else {
                    self.target_name.trim().to_string()
                };
                if self.target_online() {
                    ui.horizontal(|ui| {
                        dot(ui, pal().good, 8.0);
                        ui.label(egui::RichText::new(&name).strong().size(16.0).color(pal().ink));
                        ui.label(
                            egui::RichText::new(format!("{}:{}", self.target_ip.trim(), self.port))
                                .monospace()
                                .color(pal().ink_dim),
                        );
                    });
                } else {
                    ui.horizontal(|ui| {
                        dot(ui, pal().warn, 8.0);
                        ui.label(egui::RichText::new(&name).strong().size(16.0).color(pal().ink));
                        badge(ui, "Offline", pal().warn);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("Clear").on_hover_text("Forget this device").clicked() {
                                    self.clear_target();
                                }
                            },
                        );
                    });
                    ui.label(
                        egui::RichText::new("This device isn't on the network right now.")
                            .small()
                            .color(pal().ink_dim)
                            .italics(),
                    );
                }
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{} Devices nearby", ICON_DEVICE)).small().color(pal().ink_dim));
                if self.peers.is_empty() {
                    // Nothing found yet — keep the spinner going until a device appears.
                    ui.spinner();
                } else {
                    if ui
                        .small_button(format!("{}", ICON_REFRESH))
                        .on_hover_text("Refresh — scan for devices now (auto-refresh keeps running)")
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
                    .color(pal().ink_dim)
                    .italics(),
                );
            } else {
                egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                    for peer in self.peers.clone() {
                        let ip = peer.addr.ip().to_string();
                        let selected = ip == self.target_ip.trim() && peer.port == self.port;
                        ui.horizontal(|ui| {
                            if selected {
                                dot(ui, pal().good, 8.0);
                            } else {
                                dot_hollow(ui, pal().ink_dim, 8.0);
                            }
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&peer.name).strong().color(pal().ink));
                                ui.label(
                                    egui::RichText::new(format!("{}:{}", ip, peer.port))
                                        .small()
                                        .color(pal().ink_dim)
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
                                                .color(pal().good),
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
                    .color(pal().ink_dim),
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
                    .color(pal().bad)
                    .background_color(pal().bad_bg),
            );
        }

        ui.add_space(6.0);
        let send_label = if self.target_name.trim().is_empty() {
            format!("{} Send", ICON_SEND)
        } else {
            format!("{} Send to {}", ICON_SEND, short_name(self.target_name.trim()))
        };
        let btn = centered_button(egui::RichText::new(send_label).size(17.0).strong().color(egui::Color32::WHITE))
            .min_size(egui::vec2(ui.available_width(), 52.0))
            .fill(pal().accent)
            .corner_radius(12);
        if ui.add(btn).clicked() {
            self.start_send();
        }
    }

    fn ui_approval(&mut self, ui: &mut egui::Ui, pos: usize) {
        let name = self.transfers[pos].target_name.clone();
        let ip = self.transfers[pos].target_ip.clone();
        let files: Vec<FileProg> = self.transfers[pos].files.clone();
        let total = self.transfers[pos].total;
        ui.heading(format!("{} wants to send you files", name));
        ui.label(
            egui::RichText::new(format!("From: {}  ·  {}", name, ip))
                .small()
                .color(pal().ink_dim),
        );
        ui.add_space(6.0);

        card(ui, |ui| {
            ui.horizontal(|ui| {
                section_title(ui, &format!("Files offered ({})", files.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(human_bytes(total))
                            .strong()
                            .color(pal().ink),
                    );
                });
            });
            ui.add_space(2.0);
            if files.is_empty() {
                ui.label(
                    egui::RichText::new("No files in this offer.")
                        .small()
                        .color(pal().ink_dim)
                        .italics(),
                );
            } else {
                egui::ScrollArea::vertical().max_height(330.0).show(ui, |ui| {
                    for f in &files {
                        ui.horizontal(|ui| {
                            badge(ui, &ICON_FILE.to_string(), pal().kind_file);
                            ui.label(
                                egui::RichText::new(trunc(&f.name, 44))
                                    .strong()
                                    .color(pal().ink),
                            )
                            .on_hover_text(&f.name);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(human_bytes(f.size)).color(pal().ink_dim),
                                    );
                                },
                            );
                        });
                        ui.separator();
                    }
                });
            }
        });

        ui.add_space(8.0);
        let allow = centered_button(egui::RichText::new("Allow — receive files").size(17.0).strong().color(egui::Color32::WHITE))
            .min_size(egui::vec2(ui.available_width(), 52.0))
            .fill(pal().good)
            .corner_radius(12);
        if ui.add(allow).clicked() {
            if let Some(v) = self.transfers[pos].verdict_tx.take() {
                let _ = v.send(true);
            }
            self.transfers[pos].pending = false;
            self.transfers[pos].label = "Starting...".to_string();
            self.transfers[pos].started = Instant::now();
            self.transfers[pos].last_tick = Instant::now();
        }
        ui.add_space(6.0);
        if ui
            .add_sized([ui.available_width(), 40.0], centered_button(egui::RichText::new("Deny")))
            .clicked()
        {
            if let Some(v) = self.transfers[pos].verdict_tx.take() {
                let _ = v.send(false);
            }
            // Deny closes the receiver tab right away.
            self.transfers.remove(pos);
            self.view = View::Receive;
            return;
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
        if self.transfers[pos].incoming && self.transfers[pos].pending {
            return self.ui_approval(ui, pos);
        }
        let name = self.transfers[pos].target_name.clone();
        let ip = self.transfers[pos].target_ip.clone();
        let incoming = self.transfers[pos].incoming;
        let declined = !incoming && self.transfers[pos].error.as_deref() == Some(DECLINED);
        if declined {
            ui.label(
                egui::RichText::new(format!("Sending to {}", name))
                    .strong()
                    .size(22.0)
                    .color(pal().bad),
            );
            ui.label(
                egui::RichText::new(format!("{} declined the transfer", name))
                    .small()
                    .strong()
                    .color(pal().bad),
            );
        } else {
            if incoming {
                ui.heading(format!("Receiving from {}", name));
            } else {
                ui.heading(format!("Sending to {}", name));
            }
            ui.label(
                egui::RichText::new(if incoming {
                    format!("From: {}  ·  {}", name, ip)
                } else {
                    format!("Target: {}  ·  {}", name, ip)
                })
                .small()
                .color(pal().ink_dim),
            );
        }
        if !incoming && self.transfers[pos].awaiting && !self.transfers[pos].done {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                pulse_dot(ui, pal().warn, 8.0);
                ui.label(
                    egui::RichText::new(format!("Waiting for {} to accept...", name))
                        .strong()
                        .color(pal().warn),
                );
            });
        }
        ui.add_space(6.0);

        // file list with per-file bars (read-only snapshot to keep borrow short)
        let files: Vec<FileProg> = self.transfers[pos].files.clone();
        card(ui, |ui| {
            section_title(ui, &format!("Files ({})", files.len()));
            if files.is_empty() {
                ui.label(
                    egui::RichText::new(Self::waiting_text(ui.ctx(), "Preparing file list"))
                        .small()
                        .color(pal().ink_dim)
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
                                dot(ui, pal().good, 8.0);
                            } else {
                                dot_hollow(ui, pal().ink_dim, 8.0);
                            }
                            ui.label(
                                egui::RichText::new(trunc(&f.name, 44))
                                    .strong()
                                    .color(pal().ink),
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
                                                .color(pal().good),
                                        );
                                    } else {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{}% · {}",
                                                (frac * 100.0) as u32,
                                                human_bytes(f.size),
                                            ))
                                            .small()
                                            .color(pal().ink_dim),
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
        let finished_ok = self.transfers[pos].done && self.transfers[pos].error.is_none();
        let incoming_done = finished_ok && self.transfers[pos].incoming;
        let save_dir = self.transfers[pos].save_dir.clone();
        let took = took_text(self.transfers[pos].took_secs);
        card(ui, |ui| {
            if finished_ok {
                ui.label(
                    egui::RichText::new("Overall Progress - Done")
                        .strong()
                        .size(15.0)
                        .color(pal().good),
                );
            } else if self.transfers[pos].done || self.transfers[pos].awaiting {
                section_title(ui, "Overall progress");
            } else {
                ui.horizontal(|ui| {
                    pulse_dot(ui, pal().accent, 8.0);
                    section_title(ui, "Overall progress");
                });
            }
            let t = &self.transfers[pos];
            let frac = ui.ctx().animate_value_with_time(
                egui::Id::new(("toverall", id)),
                t.frac(),
                0.2,
            );
            if !finished_ok {
                let elapsed = t.started.elapsed().as_secs_f64().max(0.1);
                let avg = t.sent as f64 / elapsed;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{}%", (frac * 100.0) as u32)).strong().size(20.0).color(pal().ink));
                    let spd = if t.done { avg } else { t.speed.max(avg * 0.15) };
                    ui.label(
                        egui::RichText::new(format!(
                            "{} of {}  ·  {}  ·  {}",
                            human_bytes(t.sent),
                            human_bytes(t.total.max(t.sent)),
                            human_speed(spd),
                            eta_text(t)
                        ))
                        .color(pal().ink_dim),
                    );
                });
            }
            // Static percentage while waiting for approval; animate once bytes flow.
            let active = !t.done && !t.awaiting;
            ui.add(egui::ProgressBar::new(frac).show_percentage().animate(active));
            if finished_ok {
                ui.add_space(8.0);
                let done_label = egui::RichText::new(format!("{}   Done, click to close this tab", ICON_CHECK))
                    .size(16.0)
                    .strong()
                    .color(egui::Color32::WHITE);
                let mut closed = false;
                if incoming_done && !save_dir.is_empty() {
                    ui.horizontal(|ui| {
                        let w = ui.available_width();
                        let ow = 150.0;
                        let dw = (w - ow - 8.0).max(140.0);
                        let done_btn = centered_button(done_label)
                            .min_size(egui::vec2(dw, 52.0))
                            .fill(pal().good)
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(12);
                        if ui.add(done_btn).clicked() {
                            closed = true;
                        }
                        if ui
                            .add_sized(
                                [ow, 52.0],
                                centered_button(
                                    egui::RichText::new("Open folder").size(15.0).strong(),
                                )
                                .corner_radius(12),
                            )
                            .clicked()
                        {
                            // Trimmed + verified: an untrimmed or vanished path
                            // made explorer silently do nothing.
                            let dir = save_dir.trim();
                            let target = if std::path::Path::new(dir).is_dir() {
                                dir.to_string()
                            } else {
                                std::path::Path::new(dir)
                                    .parent()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .filter(|p| std::path::Path::new(p).is_dir())
                                    .unwrap_or_else(|| dir.to_string())
                            };
                            if let Err(e) = open_folder(&target) {
                                self.error_popup = Some(ErrorPopup {
                                    title: "Couldn't open folder".to_string(),
                                    message: format!("{e}"),
                                    context: target,
                                    tab_id: None,
                                });
                            }
                        }
                    });
                } else {
                    let done_btn = centered_button(done_label)
                        .min_size(egui::vec2(ui.available_width(), 52.0))
                        .fill(pal().good)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(12);
                    if ui.add(done_btn).clicked() {
                        closed = true;
                    }
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new(took).small().color(pal().ink_dim));
                if closed {
                    self.transfers.remove(pos);
                    self.view = View::Send;
                    return;
                }
            } else {
                if !t.label.is_empty() {
                    ui.label(egui::RichText::new(format!("Current file: {}", trunc(&t.label, 60))).small().color(pal().ink_dim));
                }
                // Declined is already shown red in the tab title — no pill.
                let declined = t.error.as_deref() == Some(DECLINED);
                if let Some(e) = t.error.clone() {
                    if !declined {
                        ui.label(
                            egui::RichText::new(format!("Failed: {e}"))
                                .strong()
                                .color(pal().bad)
                                .background_color(pal().bad_bg),
                        );
                    }
                }
            }
        });

        if !finished_ok {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let done = self.transfers[pos].done;
                let pending = self.transfers[pos].pending;
                if !done && !pending {
                    // Stop aborts local work; the dropped connection closes
                    // the tab on the other side too.
                    if ui.button(egui::RichText::new("Stop").color(pal().bad)).clicked() {
                        if let Some(h) = self.transfers[pos].task.take() {
                            h.abort();
                        }
                        if let Some(f) = self.transfers[pos].stop_flag.take() {
                            f.store(true, Ordering::Relaxed);
                        }
                        let incoming = self.transfers[pos].incoming;
                        self.transfers.remove(pos);
                        self.view = if incoming { View::Receive } else { View::Send };
                        return;
                    }
                    ui.label(egui::RichText::new("Stopping closes this tab on both sides.").small().weak());
                } else if done {
                    if ui.button("Close").clicked() {
                        self.transfers.remove(pos);
                        self.view = View::Send;
                    }
                }
            });
        }
    }

    fn ui_receive(&mut self, ui: &mut egui::Ui) {
        ui.heading("Receive files on this PC");
        ui.label(
            egui::RichText::new("Get visible so nearby computers can find and send to you.")
                .small()
                .color(pal().ink_dim),
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
                    }
                }
                if ui.button("Open").clicked() {
                    if let Err(e) = open_folder(self.out_dir.trim()) {
                        self.recv_error = format!("Couldn't open folder: {e}");
                    }
                }
            });
        });

        ui.add_space(8.0);
        let btn_text = if self.listening { format!("{} Stop being visible", ICON_RECEIVE) } else { format!("{} Start being visible", ICON_RECEIVE) };
        let btn = centered_button(egui::RichText::new(btn_text).size(17.0).strong().color(egui::Color32::WHITE))
            .min_size(egui::vec2(ui.available_width(), 48.0))
            .fill(if self.listening { pal().bad } else { pal().good });
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
                    .color(pal().bad)
                    .background_color(pal().bad_bg),
            );
        }

        if self.listening {
            ui.add_space(8.0);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    pulse_dot(ui, pal().good, 8.0);
                    ui.label(
                        egui::RichText::new(format!("Visible to others as {}", my_hostname()))
                            .strong()
                            .size(15.0)
                            .color(pal().ink),
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
                            egui::RichText::new(format!("{}%", (frac * 100.0) as u32)).strong().size(18.0).color(pal().ink),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "{} of {} · {}",
                                human_bytes(self.recv.sent),
                                human_bytes(self.recv.total.max(self.recv.sent)),
                                human_speed(self.recv.speed)
                            ))
                            .color(pal().ink_dim),
                        );
                    });
                    ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                    if !self.recv.label.is_empty() {
                        ui.label(egui::RichText::new(format!("Current file: {}", trunc(&self.recv.label, 60))).small().color(pal().ink_dim));
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
                    .color(pal().ink_dim)
                    .italics(),
                );
            }
            if !self.recv.last_done.is_empty() {
                ui.label(
                    egui::RichText::new(format!("Last received: {}", self.recv.last_done))
                        .strong()
                        .color(pal().good)
                        .background_color(pal().good_bg),
                );
            }
        } else {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Press the button above so nearby PCs can find and send to you.")
                    .small()
                    .color(pal().ink_dim)
                    .italics(),
            );
            ui.label(
                egui::RichText::new("Firewall must allow TCP 53317-53318 + UDP 53319 (see allow-firewall.ps1).")
                    .small()
                    .color(pal().warn),
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
