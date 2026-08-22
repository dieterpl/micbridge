//! The window itself.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use micbridge_audio::devices::{list_devices, Direction};
use micbridge_engine::config::{
    DEFAULT_GAIN_DB, DEFAULT_PACKET_FRAMES, DEFAULT_TARGET_BUFFER_MS, MAX_GAIN_DB, MIN_GAIN_DB,
};
use micbridge_engine::{ReceiverConfig, SenderConfig, Session, Sink, Snapshot, Source, Status};

use crate::meter::{self, MeterState};
use crate::theme::{self, Palette};
use crate::tray::{Tray, TrayCommand, TrayState};

/// Frame interval while a session is running. Fast enough that the meter looks
/// continuous, slow enough to stay negligible next to the audio threads.
const ACTIVE_REPAINT: Duration = Duration::from_millis(33);

/// Frame interval while idle. Still repainting so a status change shows up, but
/// not spinning a GPU for a static window.
const IDLE_REPAINT: Duration = Duration::from_millis(250);

/// Width of the label column in every settings grid.
///
/// Set explicitly, and identically, in all of them: the gain row lives in its own
/// grid because it stays enabled while a session runs, and two grids only line up
/// if both are told the same column width rather than each sizing to its own
/// longest label.
const LABEL_COLUMN: f32 = 88.0;

/// Meter fall-off per frame once the signal drops.
///
/// A meter drawn straight from peak-since-last-poll flickers unreadably. Rising
/// instantly and falling slowly is what makes a level display legible, and it is
/// how every hardware meter behaves.
const METER_DECAY: f32 = 0.90;

/// Test tone frequency. 1 kHz because it sits in the middle of what any speaker or
/// meter reproduces honestly.
const TONE_HZ: f64 = 1_000.0;

/// What a background network search produces: receivers, or a message to show.
/// What a search produced.
#[derive(Debug, Default)]
struct FindReport {
    found: Vec<micbridge_engine::discovery::Found>,
    /// Set when discovery found nothing, but the address already in the host field
    /// is accepting control connections regardless.
    ///
    /// That combination is not a corner case — it is what a firewall permitting the
    /// control port while dropping the discovery port looks like, and it leaves a
    /// receiver perfectly usable and completely unfindable. Telling the user to
    /// "start the receiver" then is actively wrong.
    host_answers: bool,
}

type FindOutcome = Result<FindReport, String>;

/// The slot a search thread writes its outcome into, polled by the paint loop.
type FindSlot = Arc<Mutex<Option<FindOutcome>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Send,
    Receive,
}

/// Either a real device or the mode's synthetic option.
#[derive(Debug, Clone, PartialEq)]
enum Choice {
    Device(String),
    /// `--tone` in the send direction, a WAV file in the receive direction.
    Synthetic,
}

impl Choice {
    fn label(&self, mode: Mode) -> String {
        match (self, mode) {
            (Self::Device(name), _) => name.clone(),
            (Self::Synthetic, Mode::Send) => format!("Test tone ({} kHz)", TONE_HZ / 1000.0),
            (Self::Synthetic, Mode::Receive) => "WAV file".to_string(),
        }
    }
}

pub struct App {
    mode: Mode,

    // Send settings.
    host: String,
    send_choice: Choice,
    inputs: Vec<String>,

    // Receive settings.
    recv_choice: Choice,
    outputs: Vec<String>,
    target_buffer_ms: u32,
    wav_path: String,

    port: u16,

    /// Amplification in decibels, applied to whichever direction is running.
    ///
    /// Kept here rather than read back from the session so the setting survives
    /// stopping and starting — a user who found the right level once should not
    /// have to find it again.
    gain_db: f32,

    /// Ways of getting audio onto a microphone on this machine, refreshed with the
    /// device lists. Cached rather than recomputed per frame, since it costs an
    /// enumeration.
    routes: Vec<micbridge_audio::MicRoute>,

    /// An in-flight or finished network search.
    ///
    /// Discovery blocks for most of a second, so it runs on its own thread and the
    /// paint loop polls this — a `find` call inside `ui` would visibly stutter the
    /// window every time the button was pressed.
    finding: Option<FindSlot>,
    /// What the last search turned up, for display.
    found: Vec<micbridge_engine::discovery::Found>,
    find_error: Option<String>,
    /// Set once a search has finished, so an empty result reads as "nothing answered"
    /// rather than "not tried".
    searched: bool,
    /// Discovery found nothing, but the typed address is answering anyway.
    host_answers: bool,

    /// What is registered to run at logon, refreshed after each change so the
    /// checkbox reflects the registry rather than what was last clicked.
    autostart: micbridge_engine::autostart::Status,
    autostart_error: Option<String>,

    session: Option<Session>,
    /// Kept after the session ends so the final status and counters stay on screen
    /// rather than blanking the moment it stops.
    last: Option<Snapshot>,
    meter: MeterState,
    device_error: Option<String>,

    /// Keeps the window above other applications. The point of the whole program is
    /// to be used while something else is fullscreen, so this is not a novelty.
    always_on_top: bool,

    /// The menu bar item, where the platform has one. `None` on Linux, and also
    /// when it simply could not be created — neither is a reason to refuse to run.
    tray: Option<Tray>,

    /// Where to write a self-portrait, and how many frames are left before taking
    /// it. Development only; see `screenshot.rs`.
    #[cfg(feature = "screenshot")]
    screenshot: Option<(std::path::PathBuf, u32)>,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            mode: Mode::Send,
            host: String::new(),
            send_choice: Choice::Synthetic,
            inputs: Vec::new(),
            recv_choice: Choice::Synthetic,
            outputs: Vec::new(),
            target_buffer_ms: DEFAULT_TARGET_BUFFER_MS,
            gain_db: DEFAULT_GAIN_DB,
            wav_path: "captures/micbridge.wav".to_string(),
            port: micbridge_protocol::DEFAULT_CONTROL_PORT,
            routes: Vec::new(),
            finding: None,
            found: Vec::new(),
            find_error: None,
            searched: false,
            host_answers: false,
            autostart: micbridge_engine::autostart::status().unwrap_or_default(),
            autostart_error: None,
            session: None,
            last: None,
            meter: MeterState::default(),
            device_error: None,
            always_on_top: false,
            tray: None,
            #[cfg(feature = "screenshot")]
            screenshot: None,
        };
        app.refresh_devices();

        // Started by the logon entry: go straight to receiving, so the machine is
        // ready without anyone touching the window.
        if std::env::args().any(|arg| arg == "--auto-receive") {
            app.mode = Mode::Receive;
            app.start();
        }
        app
    }

    /// Registers or removes the logon entry, then re-reads what the registry says.
    fn set_autostart(&mut self, wanted: bool) {
        use micbridge_engine::autostart;

        self.autostart_error = None;
        let outcome = if wanted {
            // Register the *windowed* build with a flag that starts receiving, so the
            // logon entry does not leave a console window open on the desktop.
            autostart::enable(&["--auto-receive".to_string()]).map(|_| ())
        } else {
            autostart::disable()
        };

        if let Err(err) = outcome {
            self.autostart_error = Some(format!("{err:#}"));
        }
        // Re-read rather than assuming: if the write failed, the checkbox must not
        // claim otherwise.
        self.autostart = autostart::status().unwrap_or_default();
    }

    /// Enumerates devices and picks sensible defaults.
    ///
    /// Called on startup and from the Refresh button, never from the paint loop —
    /// enumeration is a syscall, and doing it per frame would be thirty a second.
    fn refresh_devices(&mut self) {
        self.device_error = None;

        match list_devices(Direction::Input) {
            Ok(names) => self.inputs = names,
            Err(err) => {
                self.inputs.clear();
                self.device_error = Some(format!("{err:#}"));
            }
        }
        match list_devices(Direction::Output) {
            Ok(names) => self.outputs = names,
            Err(err) => {
                self.outputs.clear();
                self.device_error = Some(format!("{err:#}"));
            }
        }

        // A capture headed for the repository must not photograph the devices
        // attached to the machine that took it. Filtered before `routes`, so the
        // pairings are derived from the same list the window draws. See
        // `screenshot.rs`.
        #[cfg(feature = "screenshot")]
        {
            if self.screenshot.is_some() {
                crate::screenshot::retain_documented_devices(&mut self.inputs);
                crate::screenshot::retain_documented_devices(&mut self.outputs);
            }
        }

        self.routes = micbridge_audio::virtual_device::routes(&self.inputs, &self.outputs);

        if let Choice::Synthetic = self.send_choice {
            if let Some(first) = self.inputs.first() {
                self.send_choice = Choice::Device(first.clone());
            }
        }
        // The receive side is chosen by microphone, and `routes` is ordered with the
        // reliable pairings first — so the head of the list is a safe default and a
        // duplex interface never becomes one.
        if let Choice::Synthetic = self.recv_choice {
            if let Some(route) = self.routes.first() {
                self.recv_choice = Choice::Device(route.game_mic.clone());
            }
        }
    }

    /// The route for the currently selected game microphone.
    fn selected_route(&self) -> Option<&micbridge_audio::MicRoute> {
        match &self.recv_choice {
            Choice::Device(mic) => self.routes.iter().find(|r| &r.game_mic == mic),
            Choice::Synthetic => None,
        }
    }

    fn running(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_active())
    }

    fn start(&mut self) {
        self.stop();
        // Clears the peak hold and the clip latch too: they describe the session
        // that just ended, and carrying them into a new one would be a lie.
        self.meter.reset();

        let session = match self.mode {
            Mode::Send => Session::start_sender(SenderConfig {
                host: self.host.trim().to_string(),
                port: self.port,
                source: match &self.send_choice {
                    Choice::Device(name) => Source::Device(Some(name.clone())),
                    Choice::Synthetic => Source::Tone(TONE_HZ),
                },
                packet_frames: DEFAULT_PACKET_FRAMES,
                gain_db: self.gain_db,
                ..Default::default()
            }),
            Mode::Receive => {
                // The user chose a microphone; the engine renders into the playback
                // device that feeds it. Translating here is what lets the window stay
                // in the user's terms rather than the audio stack's.
                let render_into = self.selected_route().map(|route| route.render_into.clone());
                Session::start_receiver(ReceiverConfig {
                    port: self.port,
                    sink: match (&self.recv_choice, render_into) {
                        (Choice::Device(_), Some(playback)) => Sink::Device(Some(playback)),
                        // No known route: fall back to treating the choice as a
                        // playback device so an unrecognised setup is still usable.
                        (Choice::Device(name), None) => Sink::Device(Some(name.clone())),
                        (Choice::Synthetic, _) => Sink::Wav(PathBuf::from(self.wav_path.trim())),
                    },
                    target_buffer_ms: self.target_buffer_ms,
                    gain_db: self.gain_db,
                    ..Default::default()
                })
            }
        };
        self.session = Some(session);
    }

    fn stop(&mut self) {
        if let Some(session) = self.session.take() {
            // Not joined here: joining would block the paint loop for up to a
            // control-poll interval and stutter the window. The thread winds itself
            // down, and the last snapshot stays on screen meanwhile.
            session.stop();
        }
    }

    /// Kicks off a network search on a background thread.
    fn start_find(&mut self) {
        if self.finding.is_some() {
            return;
        }
        self.find_error = None;
        let slot: FindSlot = Arc::new(Mutex::new(None));
        let target = Arc::clone(&slot);
        let typed_host = self.host.trim().to_string();
        let port = self.port;
        std::thread::Builder::new()
            .name("micbridge-find".into())
            .spawn(move || {
                let outcome = micbridge_engine::discovery::find(
                    micbridge_protocol::discovery::DEFAULT_DISCOVERY_PORT,
                )
                .map_err(|err| format!("{err:#}"))
                .map(|found| {
                    // Only when discovery came back empty, and only against an
                    // address the user already typed: this opens a TCP connection,
                    // and doing that speculatively would be a port scan.
                    let host_answers = found.is_empty()
                        && !typed_host.is_empty()
                        && micbridge_engine::discovery::control_port_answers(&typed_host, port);
                    FindReport { found, host_answers }
                });
                *target.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
            })
            .ok();
        self.finding = Some(slot);
    }

    /// Collects a finished search, if there is one.
    fn poll_find(&mut self) {
        let Some(slot) = self.finding.clone() else { return };
        let outcome = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(outcome) = outcome else { return };

        self.finding = None;
        self.searched = true;
        match outcome {
            Ok(report) => {
                // One result is unambiguous, so use it rather than making the user
                // confirm what they already asked for.
                if let Some(first) = report.found.first() {
                    self.host = first.address();
                    self.port = first.control_port;
                }
                self.found = report.found;
                self.host_answers = report.host_answers;
            }
            Err(err) => self.find_error = Some(err),
        }
    }

    /// The tray's view of the session, rebuilt each frame from the same snapshot
    /// the window draws from — so the two can never disagree.
    fn tray_state(&self) -> TrayState {
        let status = match self.last.as_ref().map(|s| &s.status) {
            Some(Status::Running) => "Running",
            Some(Status::Starting(_)) => "Starting",
            Some(Status::Failed(_)) => "Failed",
            Some(Status::Stopped) => "Stopped",
            Some(Status::Idle) | None => "Idle",
        };

        // Level and buffer, which are the two numbers worth reading without opening
        // the window. Silence is written as a dash rather than a very negative dB,
        // matching what the meter shows.
        let detail = if self.running() {
            let db = micbridge_engine::state::level_dbfs(self.meter.level, meter::FLOOR_DB);
            let level = if self.meter.level > 0.0 {
                format!("{db:.0} dB")
            } else {
                "no signal".to_string()
            };
            match self.last.as_ref().map(|s| s.stats.fill_ms) {
                Some(fill) if self.mode == Mode::Receive => format!("{level} · {fill:.1} ms"),
                _ => level,
            }
        } else {
            String::new()
        };

        TrayState { status: status.to_string(), detail, running: self.running() }
    }

    /// Creates the tray on the first frame and services it on every one after.
    ///
    /// Not created in `new`: on macOS the item has to be made on the main thread
    /// with an NSApplication already running, and the first paint is the earliest
    /// point where that is guaranteed.
    fn poll_tray(&mut self, ctx: &egui::Context) {
        let state = self.tray_state();

        let tray = match self.tray.as_mut() {
            Some(tray) => tray,
            None => {
                self.tray = Tray::new(state.clone());
                match self.tray.as_mut() {
                    Some(tray) => tray,
                    // No tray on this platform, or it failed to build. Either way
                    // the window is fully usable, so there is nothing to report.
                    None => return,
                }
            }
        };

        tray.update(&state);

        // One command per frame is enough: they arrive from clicks, and a person
        // cannot out-click thirty frames a second.
        match tray.poll() {
            Some(TrayCommand::StartStop) => {
                if self.running() {
                    self.stop();
                } else {
                    self.start();
                }
            }
            Some(TrayCommand::ShowWindow) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            Some(TrayCommand::Quit) => {
                self.stop();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            None => {}
        }
    }

    /// Reads the session's state and folds the meter's fall-off in.
    fn poll(&mut self) {
        let Some(session) = self.session.as_ref() else { return };
        #[cfg_attr(not(feature = "screenshot"), allow(unused_mut))]
        let mut snapshot = session.state().snapshot();

        // The "send to" banner and the log carry this machine's addresses.
        #[cfg(feature = "screenshot")]
        {
            if self.screenshot.is_some() {
                crate::screenshot::redact(&mut snapshot);
            }
        }

        self.meter.observe(snapshot.level, METER_DECAY, meter::FLOOR_DB);
        self.last = Some(snapshot);
    }
}

#[cfg(feature = "screenshot")]
impl App {
    pub fn set_screenshot_target(&mut self, path: Option<std::path::PathBuf>) {
        self.screenshot = path.map(|path| (path, crate::screenshot::warmup_frames()));

        // Devices were enumerated in `new`, before this target existed, so the lists
        // still hold everything attached to the machine, and the defaults were
        // picked out of them. Drop both selections and enumerate again, so the
        // choices come from the list `refresh_devices` now filters — otherwise a
        // collapsed dropdown still prints the name of a device that was removed.
        if self.screenshot.is_some() {
            self.send_choice = Choice::Synthetic;
            self.recv_choice = Choice::Synthetic;
            self.refresh_devices();
        }
    }

    /// Counts down, asks for the frame, collects it, writes it, and exits.
    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, remaining)) = self.screenshot.as_mut() else { return };

        if *remaining > 0 {
            *remaining -= 1;
            // Keep frames coming: with no pointer moving there is nothing else to
            // drive a repaint, and the countdown would stall forever.
            ctx.request_repaint();
            if *remaining == 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            }
            return;
        }

        let shot = ctx.input(|i| {
            i.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = shot else {
            ctx.request_repaint();
            return;
        };

        match crate::screenshot::save(&image, path) {
            Ok(()) => tracing::info!(path = %path.display(), "wrote screenshot"),
            Err(err) => tracing::error!(%err, "could not write the screenshot"),
        }
        self.screenshot = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Closing the window must stop the audio devices. Without this the process
        // can linger with a live capture stream while the window is already gone.
        self.stop();
    }
}

impl eframe::App for App {
    /// eframe hands the app a `Ui` directly rather than a `Context`, so there is no
    /// panel to open here.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.poll_find();
        self.poll_tray(ui.ctx());
        #[cfg(feature = "screenshot")]
        self.drive_screenshot(ui.ctx());

        let palette = Palette::of(ui.ctx());

        // eframe hands over a Ui that fills the window edge to edge, so the margin
        // has to be added here. Without it every row — the header, the meter, the
        // right-aligned controls — sits flush against the frame and the rightmost
        // widget of each is clipped by the window border.
        // Scrolled, because the receive layout is taller than the default window:
        // eight counters, a buffer gauge and two banners do not fit in 720 px, and
        // the alternative to a scrollbar is silently cutting the log off the bottom.
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin { left: 16, right: 16, top: 12, bottom: 12 })
                .show(ui, |ui| {
                    self.header(ui, &palette);
                    ui.add_space(12.0);

                    self.mode_selector(ui, &palette);
                    ui.add_space(10.0);

                    theme::card(&palette).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // egui sizes every slider to a fixed 100 px, which leaves one
                        // stranded beside a full-width combo box in the row above. Set
                        // once here so both the buffer and the gain slider fill their
                        // column; the value box beside them keeps its own width.
                        ui.spacing_mut().slider_width =
                            (ui.available_width() - LABEL_COLUMN - 84.0).max(90.0);
                        match self.mode {
                            Mode::Send => self.send_settings(ui, &palette),
                            Mode::Receive => self.receive_settings(ui, &palette),
                        }
                    });

                    ui.add_space(12.0);
                    self.controls(ui, &palette);
                    ui.add_space(14.0);

                    self.meter_row(ui, &palette);
                    ui.add_space(14.0);
                    self.stats_tiles(ui, &palette);
                    ui.add_space(12.0);
                    self.log_pane(ui, &palette);
                });
        });

        // Repaint on a timer rather than only on input: the meter and the counters
        // change on their own, with no mouse or keyboard event to trigger a frame.
        ui.ctx().request_repaint_after(if self.running() { ACTIVE_REPAINT } else { IDLE_REPAINT });
    }
}

impl App {
    /// Mark, wordmark, what this machine is doing, and the state — on one line.
    ///
    /// The status lives here rather than beside the Start button because it is the
    /// answer to "is this working", which is asked far more often than anything is
    /// pressed, and the eye goes to the top of a window first.
    fn header(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        ui.horizontal(|ui| {
            crate::logo::ui(ui, 30.0, palette.accent);
            ui.add_space(4.0);

            // The pill is placed first, from the right, so the wordmark and caption
            // are laid out in what is actually left over. Done the other way round
            // the caption claims the full width and the pill is pushed off the edge.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (label, colour) = match self.last.as_ref().map(|s| &s.status) {
                    Some(Status::Running) => ("Running", palette.good),
                    Some(Status::Starting(_)) => ("Starting", palette.warn),
                    Some(Status::Failed(_)) => ("Failed", palette.bad),
                    Some(Status::Stopped) => ("Stopped", palette.muted),
                    Some(Status::Idle) | None => ("Idle", palette.muted),
                };
                meter::pill(ui, palette, label, colour);

                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(
                        egui::RichText::new("micbridge").size(19.0).strong().color(palette.ink),
                    );
                    ui.label(
                        egui::RichText::new(match self.mode {
                            Mode::Send => "This machine captures and sends",
                            Mode::Receive => "This machine receives and plays",
                        })
                        .size(11.0)
                        .color(palette.muted),
                    );
                });
            });
        });
    }

    /// A segmented control rather than two loose buttons.
    ///
    /// The two modes are one exclusive choice, and a pair of ordinary buttons does
    /// not say that — a segment inside a shared track does.
    fn mode_selector(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        // Mode cannot change mid-session: the two directions build different
        // configs, and a half-switched session would be a confusing state to
        // represent. Disabling is clearer than silently restarting.
        ui.add_enabled_ui(!self.running(), |ui| {
            egui::Frame::new()
                .fill(palette.panel2)
                .stroke(egui::Stroke::new(1.0, palette.line))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(3))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.x = 3.0;
                    // Halved explicitly: the two segments must stay equal whatever
                    // their labels are, which a horizontal layout would not do.
                    let each = (ui.available_width() - 3.0) / 2.0;
                    ui.horizontal(|ui| {
                        for (mode, label) in [(Mode::Send, "Send"), (Mode::Receive, "Receive")] {
                            let selected = self.mode == mode;
                            let text = egui::RichText::new(label).size(13.0).color(if selected {
                                egui::Color32::WHITE
                            } else {
                                palette.muted
                            });
                            let segment = egui::Button::new(text)
                                .min_size(egui::vec2(each, 26.0))
                                .corner_radius(egui::CornerRadius::same(6))
                                .fill(if selected {
                                    palette.accent
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .stroke(egui::Stroke::NONE);
                            if ui.add(segment).clicked() {
                                self.mode = mode;
                            }
                        }
                    });
                });
        });
    }

    /// The gain slider, as a row of whichever settings grid is showing.
    ///
    /// Placed in the grid rather than under the card so its label lines up with
    /// Source, Host and Port. It is also the one control left enabled while a
    /// session runs: gain is a shared atomic the audio callback reads each block,
    /// so moving it takes effect immediately, and requiring a restart to change a
    /// multiply would mean an audible gap for no reason.
    fn gain_row(&mut self, ui: &mut egui::Ui, id: &str) {
        egui::Grid::new(id).num_columns(2).spacing([12.0, 8.0]).min_col_width(LABEL_COLUMN).show(
            ui,
            |ui| {
                ui.label("Gain");
                let slider = ui.add(
                    egui::Slider::new(&mut self.gain_db, MIN_GAIN_DB..=MAX_GAIN_DB)
                        .suffix(" dB")
                        .fixed_decimals(1)
                        .text(""),
                );
                // Double-click returns to unity, the one value anyone needs to get
                // back to exactly and cannot hit reliably by dragging.
                if slider.double_clicked() {
                    self.gain_db = DEFAULT_GAIN_DB;
                }
                if slider.changed() || slider.double_clicked() {
                    if let Some(session) = self.session.as_ref() {
                        session.state().gain().set_db(self.gain_db);
                    }
                }
                ui.end_row();
            },
        );
    }

    fn gain_warning(&self, ui: &mut egui::Ui, palette: &Palette) {
        if self.gain_db > 12.0 {
            ui.label(
                egui::RichText::new(
                    "Heavy boost — watch the clip indicator; amplifying a quiet signal raises \
                     its noise with it.",
                )
                .small()
                .color(palette.warn),
            );
        }
    }

    fn send_settings(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let enabled = !self.running();
        ui.add_enabled_ui(enabled, |ui| {
            egui::Grid::new("send-settings")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .min_col_width(LABEL_COLUMN)
                .show(ui, |ui| {
                    ui.label("Source");
                    let inputs = self.inputs.clone();
                    combo(ui, "send-source", &mut self.send_choice, &inputs, Mode::Send);
                    ui.end_row();

                    ui.label("Host");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.host)
                                .desired_width(190.0)
                                .hint_text("address of the receiver"),
                        );
                        let searching = self.finding.is_some();
                        let button =
                            egui::Button::new(if searching { "Finding..." } else { "Find" });
                        if ui.add_enabled(!searching, button).clicked() {
                            self.start_find();
                        }
                    });
                    ui.end_row();

                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut self.port).range(1..=65535));
                    ui.end_row();
                });
        });
        self.gain_row(ui, "send-gain");
        self.gain_warning(ui, palette);

        // Results of a network search, and an honest account of its limits — a user
        // who finds nothing needs to know whether to keep looking or start typing.
        if let Some(err) = &self.find_error {
            ui.label(
                egui::RichText::new(format!("search failed: {err}")).small().color(palette.bad),
            );
        } else if self.finding.is_some() {
            ui.label(egui::RichText::new("Looking on the local network...").weak().small());
        } else if self.found.len() > 1 {
            ui.label(
                egui::RichText::new(format!(
                    "Found {} receivers; using {}. Others: {}",
                    self.found.len(),
                    self.host,
                    self.found.iter().skip(1).map(|f| f.address()).collect::<Vec<_>>().join(", ")
                ))
                .weak()
                .small(),
            );
        } else if let Some(one) = self.found.first() {
            ui.label(egui::RichText::new(format!("Found {}", one.label)).weak().small());
        } else if !self.found_ran() {
            ui.label(
                egui::RichText::new(
                    "Find searches the local network only — it does not reach across \
                     Tailscale, a VPN, or a routed subnet. Type the address the receiver \
                     prints if it comes back empty.",
                )
                .weak()
                .small(),
            );
        } else if self.host_answers {
            // The receiver is running and reachable; only its discovery replies are
            // being dropped. Saying "start the receiver" here would send someone off
            // to fix something that is not broken.
            ui.label(
                egui::RichText::new(format!(
                    "Discovery found nothing, but {} is answering on {} — press Start. \
                     Something on the receiver is blocking UDP {}, usually a firewall. \
                     That affects Find only, not audio.",
                    self.host.trim(),
                    self.port,
                    micbridge_protocol::discovery::DEFAULT_DISCOVERY_PORT,
                ))
                .small()
                .color(palette.good),
            );
        } else {
            ui.label(
                egui::RichText::new(
                    "Nothing answered. Discovery is a local broadcast: it does not cross \
                     Tailscale, a VPN, or a routed network, and a firewall on the receiver \
                     can block it even on the same network. Type the address the receiver \
                     prints when it starts.",
                )
                .small()
                .color(palette.warn),
            );
        }

        if matches!(self.send_choice, Choice::Synthetic) {
            ui.label(
                egui::RichText::new(
                    "Test tone needs no microphone permission. If the receiver's meter moves, \
                     everything except capture works.",
                )
                .weak()
                .small(),
            );
        }
    }

    /// Whether a search has completed at least once, so "nothing found" can be told
    /// apart from "not searched yet".
    fn found_ran(&self) -> bool {
        self.searched
    }

    fn receive_settings(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let enabled = !self.running();
        // The list offers microphones, not outputs. Asking which output to render
        // into is the wrong question: it invites choosing real speakers, which puts
        // the audio in the room and leaves the game with no microphone at all.
        let mics: Vec<String> = self.routes.iter().map(|r| r.game_mic.clone()).collect();

        ui.add_enabled_ui(enabled, |ui| {
            egui::Grid::new("recv-settings")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .min_col_width(LABEL_COLUMN)
                .show(ui, |ui| {
                    ui.label("Game mic");
                    combo(ui, "recv-sink", &mut self.recv_choice, &mics, Mode::Receive);
                    ui.end_row();

                    if matches!(self.recv_choice, Choice::Synthetic) {
                        ui.label("File");
                        ui.add(egui::TextEdit::singleline(&mut self.wav_path));
                        ui.end_row();
                    }

                    ui.label("Buffer");
                    ui.add(
                        egui::Slider::new(&mut self.target_buffer_ms, 5..=100)
                            .suffix(" ms")
                            .text(""),
                    )
                    .on_hover_text(
                        "The jitter buffer, and the dominant term in end-to-end latency. \
                         Larger tolerates more network jitter: 10 ms works on wired \
                         Ethernet, 40 ms is safer over Tailscale.",
                    );
                    ui.end_row();

                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut self.port).range(1..=65535));
                    ui.end_row();
                });
        });

        self.gain_row(ui, "recv-gain");
        self.gain_warning(ui, palette);

        if matches!(self.recv_choice, Choice::Synthetic) {
            ui.label(
                egui::RichText::new("Writes to a file. Useful for testing with no hardware.")
                    .weak()
                    .small(),
            );
            return;
        }

        match self.selected_route() {
            Some(route) => {
                // Shown, not hidden: if the audio ends up somewhere unexpected, this
                // line is the first place to look.
                ui.label(
                    egui::RichText::new(format!("Audio is sent to {:?}", route.render_into))
                        .weak()
                        .small(),
                );
                if !route.how.is_reliable() {
                    ui.label(
                        egui::RichText::new(format!("Note: {}", route.how.describe()))
                            .small()
                            .color(palette.warn),
                    );
                }
            }
            None if mics.is_empty() => {
                ui.label(
                    egui::RichText::new(
                        "No microphone on this machine can be fed by an app. Install a virtual \
                         audio cable, or choose the WAV file option to test without one.",
                    )
                    .small()
                    .color(palette.warn),
                );
            }
            None => {
                ui.label(
                    egui::RichText::new(
                        "Could not work out which device feeds this microphone; treating the \
                         selection as a playback device.",
                    )
                    .small()
                    .color(palette.warn),
                );
            }
        }

        // Only offered where it can actually be done, rather than shown disabled or
        // shown and then failing.
        if self.autostart.supported {
            ui.add_space(4.0);
            let mut enabled = self.autostart.enabled;
            if ui
                .checkbox(&mut enabled, "Start receiving at login")
                .on_hover_text(
                    "Adds a per-user entry under Task Manager's Startup tab. No \
                     administrator rights, and no console window.",
                )
                .changed()
            {
                self.set_autostart(enabled);
            }

            if let Some(err) = &self.autostart_error {
                ui.label(
                    egui::RichText::new(format!("could not change it: {err}"))
                        .small()
                        .color(palette.bad),
                );
            } else if self.autostart.stale {
                ui.label(
                    egui::RichText::new(
                        "The registered entry points at a different copy of this program; \
                         it will fail silently at logon. Untick and retick to repoint it.",
                    )
                    .small()
                    .color(palette.warn),
                );
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        ui.horizontal(|ui| {
            let running = self.running();

            if running {
                if ui.add(egui::Button::new("  Stop  ").min_size([90.0, 30.0].into())).clicked() {
                    self.stop();
                }
            } else {
                // A sender with no host cannot start, so say so on the button rather
                // than letting it fail a moment later.
                let missing_host = self.mode == Mode::Send && self.host.trim().is_empty();
                let button = ui.add_enabled(
                    !missing_host,
                    egui::Button::new("  Start  ").min_size([90.0, 30.0].into()),
                );
                if button.clicked() {
                    self.start();
                }
                if missing_host {
                    button.on_hover_text("Enter the address of the receiving machine first");
                }
            }

            if ui.button("Refresh devices").clicked() {
                self.refresh_devices();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Toggling has to issue the viewport command, not just record the
                // flag: the window level is owned by the windowing system, and
                // nothing re-reads this on a later frame.
                if ui.checkbox(&mut self.always_on_top, "On top").changed() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                        if self.always_on_top {
                            egui::WindowLevel::AlwaysOnTop
                        } else {
                            egui::WindowLevel::Normal
                        },
                    ));
                }
            });
        });

        // The detail line: what it is starting, or why it failed. The failure text
        // is the full cause chain, which is usually where the answer is.
        if let Some(snapshot) = self.last.as_ref() {
            match &snapshot.status {
                Status::Starting(what) => {
                    ui.label(egui::RichText::new(what).small().color(palette.muted));
                }
                Status::Failed(err) => {
                    ui.label(egui::RichText::new(err).small().color(palette.bad));
                }
                _ => {}
            }
        }
        if let Some(err) = &self.device_error {
            ui.label(
                egui::RichText::new(format!("device enumeration failed: {err}"))
                    .small()
                    .color(palette.bad),
            );
        }
    }

    /// Level, peak, clip, and the buffer against its target.
    fn meter_row(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        let db = micbridge_engine::state::level_dbfs(self.meter.level, meter::FLOOR_DB);

        ui.horizontal(|ui| {
            ui.label(theme::label("Level", palette));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(theme::value(
                    if self.meter.level > 0.0 {
                        format!("{db:>5.0} dB")
                    } else {
                        "  --  ".to_string()
                    },
                    palette,
                ));
                ui.add_space(6.0);
                if meter::clip_badge(ui, palette, self.meter.clipped) {
                    self.meter.clipped = false;
                }
            });
        });

        meter::meter(ui, palette, db, self.meter.peak_db, meter::FLOOR_DB);
        meter::scale(ui, palette, meter::FLOOR_DB);

        if self.running() && self.meter.level <= 0.0 {
            ui.label(
                egui::RichText::new("no signal — check the source device, gain, and permissions")
                    .small()
                    .color(palette.muted),
            );
        }

        // The buffer only exists on the receiving side; showing an empty gauge to a
        // sender would be inviting a question with no answer.
        if self.mode == Mode::Receive {
            let fill_ms = self.last.as_ref().map(|s| s.stats.fill_ms).unwrap_or(0.0);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(theme::label("Buffer", palette));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(theme::value(
                        format!("{fill_ms:.1} / {} ms", self.target_buffer_ms),
                        palette,
                    ));
                });
            });
            meter::buffer_gauge(ui, palette, fill_ms, self.target_buffer_ms as f32);
        }
    }

    /// The counters, as tiles rather than a two-column grid.
    ///
    /// A grid of label/value pairs reads as a list to be worked through. Tiles read
    /// as a dashboard to be glanced at, which is what these numbers are for — and it
    /// lets the two values a user has to *act* on be promoted out of the run.
    fn stats_tiles(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        const COLUMNS: f32 = 4.0;
        const GAP: f32 = 6.0;
        /// Frame margin on both sides plus its one-pixel border, which sit *outside*
        /// the width a tile sets for itself. Leaving this out made every tile 18 px
        /// wider than its share, so only three fitted on a row and the grid wrapped
        /// ragged.
        const TILE_CHROME: f32 = 8.0 * 2.0 + 2.0;

        let stats = self.last.as_ref().map(|s| s.stats.clone()).unwrap_or_default();
        let running = self.running();
        let total = ui.available_width();
        let inner = ((total - GAP * (COLUMNS - 1.0)) / COLUMNS - TILE_CHROME).max(40.0);

        let tile = |ui: &mut egui::Ui, name: &str, value: String, colour: Option<egui::Color32>| {
            theme::card(palette).inner_margin(egui::Margin::symmetric(8, 7)).show(ui, |ui| {
                ui.set_width(inner);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(theme::label(name, palette));
                    let text = egui::RichText::new(value).monospace().size(14.0);
                    ui.label(text.color(colour.unwrap_or(palette.ink)));
                });
            });
        };
        let warn_if = |bad: bool| if bad { Some(palette.bad) } else { None };
        // An em dash rather than a zero before anything has run: a counter reading 0
        // looks like a measurement, and there has not been one yet.
        let count = |n: u64| if running { n.to_string() } else { "—".to_string() };

        // Collected first, then laid out, so the row structure does not depend on
        // the order the counters happen to be written in.
        let mut cells: Vec<(&str, String, Option<egui::Color32>)> = vec![
            ("packets", count(stats.packets), None),
            (
                "format",
                if stats.sample_rate > 0 {
                    format!("{}k·{}ch", stats.sample_rate / 1000, stats.channels)
                } else {
                    "—".to_string()
                },
                None,
            ),
        ];
        match self.mode {
            Mode::Receive => cells.extend([
                (
                    "buffer",
                    if running { format!("{:.1}", stats.fill_ms) } else { "—".into() },
                    None,
                ),
                (
                    "trim ppm",
                    if running { format!("{:+.0}", stats.trim_ppm) } else { "—".into() },
                    None,
                ),
                ("underruns", count(stats.underruns), warn_if(stats.underruns > 0)),
                ("overruns", count(stats.overruns), warn_if(stats.overruns > 0)),
                ("lost", count(stats.frames_lost), warn_if(stats.frames_lost > 0)),
                ("late", count(stats.packets_late), warn_if(stats.packets_late > 0)),
            ]),
            Mode::Send => cells.extend([
                ("dropped", count(stats.frames_dropped), warn_if(stats.frames_dropped > 0)),
                ("gain", format!("{:+.1}", self.gain_db), None),
            ]),
        }

        // A Grid rather than horizontal_wrapped: a wrapped layout measures each
        // Frame only as it places it, so a row that exactly fills the width takes
        // one tile too many and clips it against the window edge.
        egui::Grid::new("stat-tiles").num_columns(COLUMNS as usize).spacing([GAP, GAP]).show(
            ui,
            |ui| {
                for (index, (name, value, colour)) in cells.into_iter().enumerate() {
                    tile(ui, name, value, colour);
                    if (index + 1) % COLUMNS as usize == 0 {
                        ui.end_row();
                    }
                }
                ui.end_row();
            },
        );

        // The lines that are instructions rather than telemetry: what to select in
        // the game, and what to type on the other machine. Full width and in the
        // good colour, because everything else on screen is only informative.
        let mut banner = |name: &str, value: String| {
            ui.add_space(6.0);
            theme::card(palette).inner_margin(egui::Margin::symmetric(9, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(theme::label(name, palette));
                    ui.label(egui::RichText::new(value).monospace().size(13.0).color(palette.good));
                });
            });
        };
        if !stats.game_device.is_empty() {
            banner("select this in the game", stats.game_device.clone());
        }
        if !stats.reachable.is_empty() {
            banner("send to", stats.reachable.join("  or  "));
        }
    }

    /// The last few log lines.
    ///
    /// Deliberately not a scroll area. Nested inside the window's own one, an inner
    /// `stick_to_bottom` drags the outer view down with it — which opened the window
    /// already scrolled past its own header. The engine caps the log anyway, so the
    /// tail is all there is to show.
    fn log_pane(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        const VISIBLE_LINES: usize = 6;

        ui.label(theme::label("Log", palette));
        ui.add_space(3.0);
        theme::card(palette).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.vertical(|ui| match self.last.as_ref() {
                Some(snapshot) if !snapshot.log.is_empty() => {
                    let lines = &snapshot.log;
                    let first = lines.len().saturating_sub(VISIBLE_LINES);
                    for line in lines.iter().skip(first) {
                        ui.label(
                            egui::RichText::new(line).monospace().size(10.5).color(palette.muted),
                        );
                    }
                }
                _ => {
                    ui.label(egui::RichText::new("nothing yet").small().color(palette.muted));
                }
            });
        });
    }
}

/// A device dropdown with the mode's synthetic option appended.
fn combo(ui: &mut egui::Ui, id: &str, choice: &mut Choice, devices: &[String], mode: Mode) {
    egui::ComboBox::from_id_salt(id).selected_text(choice.label(mode)).width(280.0).show_ui(
        ui,
        |ui| {
            for name in devices {
                let value = Choice::Device(name.clone());
                let selected = *choice == value;
                if ui.selectable_label(selected, name).clicked() {
                    *choice = value;
                }
            }
            let selected = matches!(choice, Choice::Synthetic);
            if ui.selectable_label(selected, Choice::Synthetic.label(mode)).clicked() {
                *choice = Choice::Synthetic;
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_option_is_labelled_per_mode() {
        assert_eq!(Choice::Synthetic.label(Mode::Send), "Test tone (1 kHz)");
        assert_eq!(Choice::Synthetic.label(Mode::Receive), "WAV file");
        assert_eq!(Choice::Device("X".into()).label(Mode::Send), "X");
    }
}
