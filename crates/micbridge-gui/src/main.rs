//! A small desktop window for micbridge.
//!
//! Deliberately plain: pick a mode, pick a device, press Start, watch a level
//! meter and six numbers. The same window binary runs on the Mac that captures and
//! on the Windows box that renders, because all the behaviour is in `micbridge-engine`
//! and cpal hides the difference between CoreAudio and WASAPI.
//!
//! The level meter is the point of having a GUI at all. "Is audio actually
//! flowing" is the first question anyone asks, and a packet counter does not answer
//! it — a counter climbs just as happily when the input is muted.

// Without this, cargo-xwin produces a console-subsystem executable and Windows
// users get a stray terminal window behind the app. Debug builds keep the console
// so `tracing` output is visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod logo;
mod meter;
#[cfg(feature = "screenshot")]
mod screenshot;
mod theme;
mod tray;

use std::io::IsTerminal;

use eframe::egui;
use tracing_subscriber::EnvFilter;

/// # Known limitation: this window will not open over Windows Remote Desktop
///
/// eframe is built here with the OpenGL (glow) renderer, and RDP exposes only
/// Microsoft's GDI generic OpenGL 1.1 driver. glow does not degrade under that, it
/// fails to start (egui issues #2573, #3165).
///
/// The obvious fix — enable wgpu and fall back to Direct3D 12 — is not available:
/// wgpu-hal 29 requires `gpu-allocator ^0.28` and `windows ^0.62`, while the only
/// 0.28 release of gpu-allocator depends on `windows` 0.54, so its DX12 backend does
/// not compile at all. That is an upstream conflict rather than a build-configuration
/// choice, and no version pinning resolves it.
///
/// Over RDP, use the CLI instead: `micbridge.exe recv --device "CABLE Input"` has no
/// renderer and no window, and does exactly the same work.
/// `--screenshot <path>`, if it was given.
#[cfg(feature = "screenshot")]
fn screenshot_target() -> Option<std::path::PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--screenshot" {
            return args.next().map(std::path::PathBuf::from);
        }
    }
    None
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("MICBRIDGE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        // The legacy Windows console prints escape sequences literally, and a
        // redirected log file has no business containing them on any platform.
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let mut viewport = egui::ViewportBuilder::default()
        // Tall enough for the receive layout — eight counters, a buffer gauge and two
        // banners — to reach the log without scrolling. The window scrolls when made
        // smaller, so this is a comfortable default rather than a requirement.
        .with_inner_size([520.0, 860.0])
        .with_min_inner_size([420.0, 560.0])
        .with_title("micbridge");

    // The same PNG the README and the installers use, decoded at startup. A window
    // with no icon gets a grey placeholder in the Windows taskbar and the alt-tab
    // switcher; on macOS the .app bundle's .icns wins, and this is ignored.
    match eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/logo-256.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        // Not fatal: an app that refuses to open because its icon failed to decode
        // would be trading the whole program for a decoration.
        Err(err) => tracing::warn!("could not decode the window icon: {err}"),
    }

    eframe::run_native(
        "micbridge",
        eframe::NativeOptions { viewport, ..Default::default() },
        Box::new(|cc| {
            theme::install(&cc.egui_ctx);
            // `mut` is only needed by the screenshot feature, so a default build
            // would otherwise warn about it.
            #[cfg_attr(not(feature = "screenshot"), allow(unused_mut))]
            let mut app = app::App::new();
            #[cfg(feature = "screenshot")]
            {
                let target = screenshot_target();
                if target.is_some() {
                    // Every screenshot in the repository is the dark theme. Without
                    // this the window follows the appearance the machine taking the
                    // capture happens to be set to, and the README ends up with one
                    // light image among the dark ones.
                    cc.egui_ctx.set_theme(egui::Theme::Dark);
                }
                app.set_screenshot_target(target);
            }
            Ok(Box::new(app))
        }),
    )
}
