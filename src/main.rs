//! Rustcord – a lightweight third-party chat client.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod api;
mod auth;
mod autostart;
mod backend;
mod cache;
mod config;
mod emoji;
mod gateway;
mod media;
mod model;
mod notify;
mod overrides;
mod plugins;
mod quests;
mod state;
mod theme;
mod ui;
mod voice;
#[cfg(windows)]
mod winreg;

use std::time::Instant;

/// Flags: `--minimized` (sign-in autostart: start hidden in the tray), `--quit` (asks the running
/// Rustcord to quit and waits for it; the installer uses it before replacing files).
fn main() -> eframe::Result {
    let started = Instant::now();
    trace_startup("main", started);
    ui::images::mem_trace("main");

    // One Rustcord per data dir: a second start brings the running one to the front and exits.
    // Screenshot runs skip it (and the tray): they must never touch another window.
    let test_run = std::env::var_os("RUSTCORD_SCREENSHOT").is_some();
    let quit = std::env::args().any(|a| a == "--quit");
    if !test_run && !notify::claim_instance() {
        notify::signal_running(quit);
        return Ok(());
    }
    if quit {
        // Nothing ran: no socket file left behind (Linux / macOS).
        notify::shutdown();
        return Ok(());
    }

    // Read settings, the disk cache and the keyring while the window is being created.
    let preload = std::thread::spawn(|| {
        let mut settings = config::Settings::load();
        // Autostart on once for an installed copy (never for dev builds or test runs).
        autostart::default_once(&mut settings);
        let snapshot = cache::load_snapshot();
        ui::images::mem_trace("snapshot_loaded");
        // The RUSTCORD_LOGIN_TAB test hook always shows the login screen, never a stored account.
        let skip = config::offline_mode() || std::env::var_os("RUSTCORD_LOGIN_TAB").is_some();
        let token = if skip { None } else { auth::load_token() };
        (settings, snapshot, token)
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("rustcord-net")
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Create the egui context up front so the backend can start connecting immediately.
    let ctx = egui::Context::default();
    let backend = backend::spawn(rt.handle(), ctx.clone());
    if !test_run {
        notify::start(&ctx);
    }

    // eframe's defaults are already the lean ones: no MSAA, depth or stencil buffer, vsync on
    // (measured: none of these, nor dithering, changes RAM; the GL driver's own ~80 MB does not move).
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(config::APP_NAME)
            .with_app_id("rustcord")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 420.0])
            // Frameless: the 32 px title bar row is the window's title bar (ui/chrome.rs).
            .with_decorations(false)
            .with_icon(window_icon()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native_ext(
        config::APP_NAME,
        options,
        Some(ctx),
        Box::new(move |cc| {
            trace_startup("window_created", started);
            ui::images::mem_trace("gl_ready");
            let (settings, snapshot, token) = preload.join().unwrap_or_default();
            trace_startup("preload_joined", started);
            // On the main thread with the event loop running (macOS' menu bar icon needs both).
            if !test_run {
                notify::start(&cc.egui_ctx);
            }
            let app = ui::RustcordApp::new(&cc.egui_ctx, rt, backend, settings, snapshot, token, started);
            ui::images::mem_trace("app_built");
            Ok(Box::new(app))
        }),
    )
}

fn window_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icons/rustcord-64.png")).map(|i| i.to_rgba8());
    match img {
        Ok(img) => egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() },
        Err(_) => egui::IconData::default(),
    }
}

/// `RUSTCORD_TRACE_STARTUP=<file>` appends "<label> <ms since start>" lines (for measurements).
pub fn trace_startup(label: &str, started: Instant) {
    if let Some(path) = std::env::var_os("RUSTCORD_TRACE_STARTUP") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{label} {:.1}", started.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
