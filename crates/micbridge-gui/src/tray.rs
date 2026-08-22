//! A menu bar item on macOS and a notification-area icon on Windows.
//!
//! The program is used while something else is fullscreen, which is exactly when a
//! 720-pixel settings window is in the way. The tray keeps the two things a running
//! session actually needs — is it working, and stop it — reachable without a window
//! on screen at all.
//!
//! Restricted to macOS and Windows deliberately. `tray-icon` is objc2 there and
//! `windows-sys` on Windows, both pure Rust, so the Windows cross-build from a Mac
//! is unaffected; on Linux it needs GTK and libayatana-appindicator, which would
//! pull a C dependency into a build that does not otherwise need one. The stub at
//! the bottom of this file keeps the call sites free of `cfg` noise.

/// What the user picked from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Bring the window to the front.
    ///
    /// Raise rather than toggle, deliberately. Hiding the window would be the more
    /// obvious "tray app" behaviour, and it is a trap: this menu is only serviced
    /// from the paint loop, and a hidden window is not guaranteed to keep being
    /// painted — so hiding could leave the user with no window *and* a menu that no
    /// longer responds. Raising has no such failure, and covers the case that
    /// actually comes up: the window is buried behind a fullscreen game.
    ShowWindow,
    /// Start a session, or stop the running one.
    StartStop,
    Quit,
}

/// What the tray should display. Kept as plain data so the platform code below has
/// no opinion about sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    /// Short status word: "Running", "Idle", "Failed".
    pub status: String,
    /// The detail line, e.g. "-14 dB · 19.4 ms". Empty when there is nothing to say.
    pub detail: String,
    /// Whether a session is active, which decides the Start/Stop wording.
    pub running: bool,
}

impl TrayState {
    /// The menu's first, disabled line.
    pub fn summary(&self) -> String {
        if self.detail.is_empty() {
            self.status.clone()
        } else {
            format!("{} · {}", self.status, self.detail)
        }
    }

    pub fn action_label(&self) -> &'static str {
        if self.running {
            "Stop"
        } else {
            "Start"
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform {
    use super::{TrayCommand, TrayState};

    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{TrayIcon, TrayIconBuilder};

    pub struct Tray {
        /// Held only to keep the icon alive: dropping it removes it from the bar.
        _icon: TrayIcon,
        summary: MenuItem,
        action: MenuItem,
        ids: Ids,
        shown: TrayState,
    }

    struct Ids {
        action: MenuId,
        window: MenuId,
        quit: MenuId,
    }

    /// macOS wants a black-plus-alpha template image, which it re-tints for a light
    /// or dark menu bar; Windows wants the real thing.
    ///
    /// The macOS one is cropped to the artwork rather than being the full square.
    /// The bar scales whatever it is handed to 18 pt tall, so a square image whose
    /// mark fills less than half its height arrives at about 8 pt and sits low.
    #[cfg(target_os = "macos")]
    const ICON_PNG: &[u8] = include_bytes!("../../../assets/tray@2x.png");
    #[cfg(not(target_os = "macos"))]
    const ICON_PNG: &[u8] = include_bytes!("../../../assets/logo-32.png");

    fn icon() -> Option<tray_icon::Icon> {
        let decoded = eframe::icon_data::from_png_bytes(ICON_PNG).ok()?;
        tray_icon::Icon::from_rgba(decoded.rgba, decoded.width, decoded.height).ok()
    }

    impl Tray {
        /// Builds the tray item, or returns `None`.
        ///
        /// Returning an `Option` rather than propagating an error because there is
        /// nothing a caller could usefully do: a window that refuses to open because
        /// its menu bar extra failed would be trading the program for a convenience.
        pub fn new(initial: TrayState) -> Option<Self> {
            let menu = Menu::new();

            let summary = MenuItem::new(initial.summary(), false, None);
            let action = MenuItem::new(initial.action_label(), true, None);
            let window = MenuItem::new("Bring window to front", true, None);
            let quit = MenuItem::new("Quit micbridge", true, None);

            let ids = Ids {
                action: action.id().clone(),
                window: window.id().clone(),
                quit: quit.id().clone(),
            };

            menu.append(&summary).ok()?;
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&action).ok()?;
            menu.append(&window).ok()?;
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&quit).ok()?;

            let mut builder = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip(format!("micbridge — {}", initial.summary()));
            if let Some(icon) = icon() {
                builder = builder.with_icon(icon);
            }
            #[cfg(target_os = "macos")]
            {
                builder = builder.with_icon_as_template(true);
            }

            let icon = builder.build().ok()?;
            Some(Self { _icon: icon, summary, action, ids, shown: initial })
        }

        /// Rewrites the menu when the state has actually changed.
        ///
        /// Guarded because this is called every frame: on macOS each setter crosses
        /// into Objective-C, and doing that thirty times a second to write back the
        /// text that is already there would be pure waste.
        pub fn update(&mut self, state: &TrayState) {
            if &self.shown == state {
                return;
            }
            self.summary.set_text(state.summary());
            self.action.set_text(state.action_label());
            self.shown = state.clone();
        }

        /// Non-blocking: returns a command if one was clicked since the last call.
        pub fn poll(&mut self) -> Option<TrayCommand> {
            // The receiver is a process-global channel rather than something owned
            // by this icon, which is why polling it here is enough.
            let event = MenuEvent::receiver().try_recv().ok()?;
            if event.id == self.ids.action {
                Some(TrayCommand::StartStop)
            } else if event.id == self.ids.window {
                Some(TrayCommand::ShowWindow)
            } else if event.id == self.ids.quit {
                Some(TrayCommand::Quit)
            } else {
                None
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{TrayCommand, TrayState};

    /// Linux: no tray, and the call sites do not need to know.
    pub struct Tray;

    impl Tray {
        pub fn new(_initial: TrayState) -> Option<Self> {
            None
        }
        pub fn update(&mut self, _state: &TrayState) {}
        pub fn poll(&mut self) -> Option<TrayCommand> {
            None
        }
    }
}

pub use platform::Tray;

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: &str, detail: &str, running: bool) -> TrayState {
        TrayState { status: status.into(), detail: detail.into(), running }
    }

    #[test]
    fn the_summary_omits_an_empty_detail() {
        // Otherwise an idle tray reads "Idle · ", which looks like a bug.
        assert_eq!(state("Idle", "", false).summary(), "Idle");
        assert_eq!(state("Running", "-14 dB", true).summary(), "Running · -14 dB");
    }

    /// The menu entry has to say what clicking it will do, not what is happening.
    #[test]
    fn the_action_names_the_next_action_not_the_current_state() {
        assert_eq!(state("Running", "", true).action_label(), "Stop");
        assert_eq!(state("Idle", "", false).action_label(), "Start");
    }
}
