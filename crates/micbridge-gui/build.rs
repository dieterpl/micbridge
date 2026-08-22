//! Embeds the application icon into the Windows executable.
//!
//! `assets/micbridge.ico` is emitted by `scripts/render-logo.py` from the same
//! geometry as the macOS `.icns` and the menu bar image, so the mark Explorer
//! shows is the mark the Dock shows.
//!
//! Windows hosts only, and Cargo agrees: a `cfg(windows)` on a
//! `build-dependencies` section is evaluated against the host, so `winresource`
//! is never even fetched on the Mac. It would be useless there anyway — the icon
//! is compiled by the SDK's `rc.exe`, and installing a toolchain to get one would
//! cost the property the whole cross-build exists to demonstrate.
//!
//! So a released `.exe`, built on `windows-latest` by the release workflow,
//! carries the icon; one cross-built from the Mac shows Explorer's generic file
//! icon and is otherwise identical. The window, taskbar and tray images are read
//! from `assets/` at runtime, so a *running* program looks the same from both.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/micbridge.ico");

    #[cfg(windows)]
    embed_icon();
}

#[cfg(windows)]
fn embed_icon() {
    // A Windows host can be building for somewhere else, and an ELF binary has
    // nowhere to put a Windows resource.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("../../assets/micbridge.ico");

    // A warning rather than an error: a missing resource compiler should cost the
    // icon, not the build.
    if let Err(err) = resource.compile() {
        println!("cargo:warning=could not embed the Windows icon: {err}");
    }
}
