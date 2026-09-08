//! The window.
//!
//! A thin horizontal strip rather than a panel, so it can sit along the top or
//! bottom of a screen while you play without covering anything. Three areas,
//! left to right, in the order you use them: pick a source, get a code, send
//! the link.
//!
//! The UI owns no streaming state of its own. It reads `Session`, which the
//! capture threads write to, so there is one source of truth and no way for
//! the display to disagree with the engine.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::icons::IconCache;
use crate::mark;
use crate::session::{Phase, Rates, Session};
use crate::settings::Settings;
use crate::{hotkey, sources, stream};

const BG: Color32 = Color32::from_rgb(0x14, 0x18, 0x1d);
const SURFACE: Color32 = Color32::from_rgb(0x1b, 0x20, 0x27);
const SURFACE_HI: Color32 = Color32::from_rgb(0x23, 0x2a, 0x32);
const LINE: Color32 = Color32::from_rgb(0x2c, 0x33, 0x3c);
const INK: Color32 = Color32::from_rgb(0xe4, 0xe9, 0xee);
const MUTED: Color32 = Color32::from_rgb(0x98, 0xa4, 0xb1);
const ACCENT: Color32 = Color32::from_rgb(0xf0, 0xa9, 0x3b);
const GOOD: Color32 = Color32::from_rgb(0x6f, 0xcf, 0x97);
const BAD: Color32 = Color32::from_rgb(0xf0, 0x83, 0x7a);
const FAINT: Color32 = Color32::from_rgb(0x6c, 0x78, 0x84);

/// How often the source list is rebuilt while idle. Enumerating windows is
/// cheap but not free, and a list that reshuffles mid-click is worse than one
/// that is a second stale.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// The strip's two heights. A dropdown has nowhere to go in a window this
/// short, so choosing a source grows the window instead and it snaps back
/// afterwards, the thin shape is the point, and it should only be given up
/// for the one moment it genuinely gets in the way.
const HEIGHT_STRIP: f32 = 142.0;
const HEIGHT_PICKING: f32 = 420.0;

/// Width is the user's to choose; height is not. Pinning min and max height
/// together means the bottom edge cannot be dragged at all, there is nothing
/// below the strip to reveal, so letting it stretch only ever produced a band
/// of empty background.
const MIN_WIDTH: f32 = 840.0;
const MAX_WIDTH: f32 = 4000.0;

#[derive(Clone)]
struct Source {
    pid: u32,
    exe: String,
    title: String,
    path: String,
    /// Carried through from the enumeration: this window's audio cannot be
    /// captured. See `sources::Source::frame_hosted`.
    frame_hosted: bool,
}

pub struct App {
    sources: Vec<Source>,
    refreshed: Instant,
    selected: Option<u32>,
    relay: String,
    /// What is on disk, so the file is only rewritten when the box actually
    /// changes rather than on every frame it is looked at.
    saved_relay: String,
    session: Option<Arc<Session>>,
    rates: Rates,
    /// Whether the source list is expanded. Mirrored so the viewport is only
    /// resized on a change rather than every frame.
    picking: bool,
    was_picking: bool,
    /// Codes are read out loud, not kept on screen. Hidden by default so
    /// the strip is safe to leave visible while streaming or recording.
    code_visible: bool,
    /// Decoded icons, and the renderer textures made from them. Both are
    /// cached: the shell lookup touches the disk, and uploading a texture per
    /// frame would be worse still.
    icons: IconCache,
    textures: HashMap<String, egui::TextureHandle>,
}

impl Default for App {
    fn default() -> Self {
        // Whatever relay was last used. Typing one in should be something you
        // do once, not once a session.
        let remembered = Settings::load().relay;

        Self {
            sources: Vec::new(),
            // Far enough in the past to force an immediate first load.
            refreshed: Instant::now() - REFRESH_EVERY * 2,
            selected: None,
            relay: remembered.clone(),
            saved_relay: remembered,
            session: None,
            rates: Rates::default(),
            picking: false,
            was_picking: false,
            code_visible: false,
            icons: IconCache::default(),
            textures: HashMap::new(),
        }
    }
}

pub fn run() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, HEIGHT_STRIP])
            .with_min_inner_size([MIN_WIDTH, HEIGHT_STRIP])
            .with_max_inner_size([MAX_WIDTH, HEIGHT_STRIP])
            .with_title("Sideband")
            .with_icon(window_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "Sideband",
        options,
        Box::new(|cc| {
            theme(&cc.egui_ctx);
            Ok(Box::<App>::default())
        }),
    )
    .map_err(|e| format!("could not open the window: {e}"))
}

/// The mark, rasterised for the title bar and the task switcher.
///
/// 64 pixels because that is the largest Windows asks a window for, and the
/// shell scales down from it; the executable's own icon is a separate set of
/// sizes drawn at build time from the same geometry.
fn window_icon() -> egui::IconData {
    const SIZE: u32 = 64;
    egui::IconData { rgba: mark::rgba(SIZE), width: SIZE, height: SIZE }
}

fn theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = SURFACE;
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.inactive.bg_fill = SURFACE_HI;
    v.widgets.inactive.weak_bg_fill = SURFACE_HI;
    v.widgets.hovered.bg_fill = SURFACE_HI;
    v.widgets.hovered.weak_bg_fill = SURFACE_HI;
    v.widgets.active.bg_fill = SURFACE_HI;
    v.widgets.active.weak_bg_fill = SURFACE_HI;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.30);
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    v.override_text_color = Some(INK);
    ctx.set_visuals(v);
}

impl App {
    fn refresh_sources(&mut self) {
        if self.refreshed.elapsed() < REFRESH_EVERY {
            return;
        }
        self.refreshed = Instant::now();

        if let Ok(list) = sources::list() {
            self.sources = list
                .into_iter()
                .map(|s| Source {
                    pid: s.pid,
                    exe: s.exe,
                    title: s.title,
                    path: s.path,
                    frame_hosted: s.frame_hosted,
                })
                .collect();
        }

        // A window that has closed should not stay selected.
        if let Some(pid) = self.selected {
            if !self.sources.iter().any(|s| s.pid == pid) {
                self.selected = None;
            }
        }
    }

    fn selected_label(&self) -> String {
        match self.selected.and_then(|pid| self.sources.iter().find(|s| s.pid == pid)) {
            Some(s) => s.exe.clone(),
            None => "Choose an application".to_owned(),
        }
    }

    /// Writes the relay down, if it has moved since it was last written.
    fn remember_relay(&mut self) {
        let current = self.relay.trim().to_owned();
        if current == self.saved_relay {
            return;
        }
        Settings { relay: current.clone() }.save();
        self.saved_relay = current;
    }

    fn start(&mut self) {
        let Some(pid) = self.selected else { return };
        self.remember_relay();

        let session = Arc::new(Session::default());
        self.session = Some(Arc::clone(&session));
        self.rates = Rates::default();

        let relay = self.relay.trim().to_owned();
        std::thread::spawn(move || {
            // Failures are recorded in the session as a Failed phase; there is
            // nowhere else for a result to go from a detached thread.
            let _ = if relay.is_empty() {
                stream::run_local(pid, 8088, session)
            } else {
                stream::run_relay(pid, &relay, session)
            };
        });
    }

    fn stop(&mut self) {
        if let Some(s) = &self.session {
            s.request_stop();
        }
        self.session = None;
    }
}

impl eframe::App for App {
    /// Closing the window counts as finishing with the box. Without this, a
    /// relay pasted in and never used would be gone by the next launch, which
    /// is exactly the thing remembering it is meant to stop.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.remember_relay();
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A live stream needs a moving picture of itself; an idle window does
        // not need to burn a core redrawing a static strip.
        ctx.request_repaint_after(Duration::from_millis(
            if self.session.is_some() { 150 } else { 600 },
        ));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh_sources();

        // Height is enforced here rather than left to the window manager.
        // `with_max_inner_size` is only a hint, and on Windows it is not
        // honoured, the bottom edge drags freely and leaves a band of empty
        // background under the strip, because there is nothing below it to
        // reveal. Correcting the size each frame is what actually holds it.
        let wanted_height = if self.picking { HEIGHT_PICKING } else { HEIGHT_STRIP };
        let viewport = ui.ctx().viewport_rect();
        let mode_changed = self.picking != self.was_picking;

        // A tolerance, because the reported size and the requested one differ
        // by a fraction under some display scalings; reacting to that would
        // fight itself every frame.
        if mode_changed || (viewport.height() - wanted_height).abs() > 2.0 {
            self.was_picking = self.picking;
            let width = viewport.width().clamp(MIN_WIDTH, MAX_WIDTH);

            // Raise the ceiling before asking for the new size, then lower the
            // floor after, the other order clamps the request against limits
            // that still describe the previous mode.
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::MaxInnerSize(egui::vec2(
                MAX_WIDTH,
                wanted_height,
            )));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                MIN_WIDTH,
                wanted_height,
            )));
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                width,
                wanted_height,
            )));
        }

        if self.picking && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.picking = false;
        }

        let phase = self.session.as_ref().map(|s| s.phase());
        if let Some(s) = &self.session {
            self.rates.update(s);

            // The engine has the last word on what is being shared. A switch
            // it could not make puts the old choice back, and following it
            // here is what makes the picker snap back rather than claim to be
            // showing something it is not.
            match s.selected_source() {
                0 => {}
                pid => self.selected = Some(pid),
            }
        }

        ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 6.0);
        ui.style_mut().spacing.button_padding = egui::vec2(12.0, 6.0);

        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                self.masthead(ui, phase.as_ref());
                ui.add_space(8.0);

                self.body(ui, phase.as_ref());
            });
    }
}

impl App {
    fn masthead(&mut self, ui: &mut egui::Ui, phase: Option<&Phase>) {
        ui.horizontal(|ui| {
            glyph(ui, 18.0);
            ui.add_space(8.0);
            ui.label(RichText::new("SIDEBAND").color(ACCENT).size(13.0).strong());
            ui.add_space(12.0);

            match phase {
                None | Some(Phase::Idle) => {
                    ui.label(
                        RichText::new("one window, and only that window's audio")
                            .color(FAINT)
                            .size(11.0),
                    );
                }
                Some(Phase::Preparing(what)) => {
                    ui.spinner();
                    ui.label(RichText::new(what).color(MUTED).size(11.0));
                }
                Some(Phase::Waiting { .. }) => {
                    ui.spinner();
                    ui.label(RichText::new("waiting for a viewer").color(MUTED).size(11.0));
                }
                Some(Phase::Approving { .. }) => {
                    status_dot(ui, ACCENT);
                    ui.label(
                        RichText::new("someone is asking to watch")
                            .color(ACCENT)
                            .size(11.0)
                            .strong(),
                    );
                }
                Some(Phase::Live) => {
                    let paused = self.session.as_ref().is_some_and(|s| s.paused());
                    let (text, colour) = if paused { ("paused", ACCENT) } else { ("live", GOOD) };
                    status_dot(ui, colour);
                    ui.label(RichText::new(text).color(colour).size(11.0).strong());
                    if let Some(sess) = &self.session {
                        ui.label(
                            RichText::new(format!(
                                "{}   {:.0} fps   {:.1} Mbit/s",
                                sess.resolution(),
                                self.rates.fps,
                                self.rates.mbps
                            ))
                            .color(FAINT)
                            .size(11.0),
                        );

                        // Only when the viewer's connection has actually
                        // forced something down. At full quality this would be
                        // a number saying "normal", which is noise, and a
                        // notice below is more urgent than either.
                        if sess.notice().is_none() {
                            if let Some(note) = throttle_note(sess) {
                                ui.label(RichText::new(note).color(ACCENT).size(11.0));
                            }
                        }
                    }
                }
                Some(Phase::Failed(why)) => {
                    ui.label(RichText::new(truncate(why, 70)).color(BAD).size(11.0));
                }
                Some(Phase::Ended) => {
                    ui.label(RichText::new("ended").color(FAINT).size(11.0));
                }
            }

            // Something the engine could not do, most often an application
            // that refused to be captured when it was picked. It expires on
            // its own; a permanent banner for a transient problem would be
            // worse than not saying anything.
            if let Some(note) = self.session.as_ref().and_then(|s| s.notice()) {
                ui.label(RichText::new(truncate(&note, 58)).color(BAD).size(11.0));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let field = ui.add_enabled(
                    phase.is_none(),
                    egui::TextEdit::singleline(&mut self.relay)
                        .hint_text("relay URL")
                        .desired_width(210.0)
                        .font(egui::TextStyle::Small),
                );
                // On leaving the field rather than on every keystroke, so a
                // half-typed URL is never the one that gets remembered.
                if field.lost_focus() {
                    self.remember_relay();
                }
                ui.label(RichText::new("relay").color(FAINT).size(10.0));
            });
        });
    }

    /// One surface, three zones, hairline dividers. Boxing each zone
    /// separately made a small window look busy; a single card with rules
    /// between the zones reads as one object.
    fn body(&mut self, ui: &mut egui::Ui, phase: Option<&Phase>) {
        egui::Frame::new()
            .fill(SURFACE)
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(0, 12))
            .show(ui, |ui| {
                if let Some(Phase::Approving { viewer }) = phase {
                    self.approval_prompt(ui, viewer);
                    return;
                }

                let total = ui.available_width();
                let col = (total - 2.0) / 3.0;

                ui.horizontal_top(|ui| {
                    zone(ui, col, |ui| self.zone_source(ui, phase.is_some()));
                    divider(ui);
                    zone(ui, col, |ui| self.zone_code(ui, phase));
                    divider(ui);
                    let rest = ui.available_width();
                    zone(ui, rest, |ui| self.zone_link(ui, phase));
                });
            });

        if self.picking {
            ui.add_space(8.0);
            self.source_list(ui);
        }
    }

    /// Allow or refuse a viewer. Deliberately not defaulted either way and
    /// deliberately not dismissible by clicking elsewhere, this is the one
    /// point where a person, rather than a secret, decides.
    fn approval_prompt(&mut self, ui: &mut egui::Ui, viewer: &str) {
        let session = self.session.clone();
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(16, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("SOMEONE IS ASKING TO WATCH")
                                .color(FAINT)
                                .size(9.0)
                                .strong(),
                        );
                        ui.add_space(3.0);
                        ui.label(
                            RichText::new(format!("from {viewer}"))
                                .color(INK)
                                .size(15.0)
                                .monospace(),
                        );
                        ui.label(
                            RichText::new(
                                "They have your code. Only allow this if you were expecting them.",
                            )
                            .color(MUTED)
                            .size(10.0),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("allow").size(12.0).strong().color(BG),
                                )
                                .fill(ACCENT)
                                .corner_radius(egui::CornerRadius::same(4)),
                            )
                            .clicked()
                        {
                            if let Some(s) = &session {
                                s.approve();
                            }
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("refuse").size(12.0).color(BAD))
                                    .fill(SURFACE_HI)
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .stroke(egui::Stroke::new(1.0, LINE)),
                            )
                            .clicked()
                        {
                            if let Some(s) = &session {
                                s.deny();
                            }
                        }
                    });
                });
            });
    }

    fn zone_source(&mut self, ui: &mut egui::Ui, running: bool) {
        caption(ui, "APPLICATION");

        // The picker stays live while streaming. Swapping what you are
        // sharing mid-call is the ordinary case, the viewer keeps the same
        // connection, the same code, and simply sees something else.
        //
        // While running the name comes from the session rather than from the
        // window list, because the list is refreshed on its own timer and a
        // moment where it does not yet contain the running source should not
        // make the strip read "Choose an application".
        let (live_exe, live_title) = self
            .session
            .as_ref()
            .filter(|_| running)
            .map(|s| s.source())
            .unwrap_or_default();

        let chosen = running || self.selected.is_some();
        let label = if running {
            truncate(&live_exe, 22)
        } else {
            truncate(&self.selected_label(), 22)
        };

        let path = self
            .selected
            .and_then(|pid| self.sources.iter().find(|s| s.pid == pid))
            .map(|s| s.path.clone());
        let texture = path.and_then(|p| self.icon(ui.ctx(), &p));

        if combo_field(ui, &label, chosen, self.picking, texture.as_ref()).clicked() {
            self.picking = !self.picking;
        }

        let detail = if running {
            truncate(&live_title, 32)
        } else {
            self.selected
                .and_then(|pid| self.sources.iter().find(|s| s.pid == pid))
                .map(|s| truncate(&s.title, 32))
                .unwrap_or_else(|| format!("{} windows", self.sources.len()))
        };
        ui.label(RichText::new(detail).color(FAINT).size(10.0));
    }

    fn zone_code(&mut self, ui: &mut egui::Ui, phase: Option<&Phase>) {
        let code = match phase {
            Some(Phase::Waiting { code: Some(c), .. }) => Some(c.clone()),
            _ => None,
        };
        caption(ui, if code.is_some() { "CODE" } else { "SESSION" });

        match (&code, phase) {
            (Some(c), _) => {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(masked(c, self.code_visible))
                            .color(if self.code_visible { ACCENT } else { MUTED })
                            .size(18.0)
                            .strong()
                            .monospace(),
                    );
                    ui.add_space(2.0);
                    if tiny(ui, if self.code_visible { "hide" } else { "show" }).clicked() {
                        self.code_visible = !self.code_visible;
                    }
                    // Copies the real code whether or not it is on screen.
                    // Hiding it is about shoulders and screen recordings, not
                    // about making it harder to send.
                    if tiny(ui, "copy").clicked() {
                        ui.ctx().copy_text(c.clone());
                    }
                });
            }
            (None, Some(Phase::Live)) => {
                ui.label(RichText::new("connected").color(GOOD).size(15.0).strong());
            }
            _ => {
                ui.label(
                    RichText::new(if self.relay.trim().is_empty() {
                        "local network"
                    } else {
                        "not started"
                    })
                    .color(MUTED)
                    .size(14.0),
                );
            }
        }

        ui.add_space(6.0);
        self.buttons(ui, phase);
    }

    fn zone_link(&mut self, ui: &mut egui::Ui, phase: Option<&Phase>) {
        caption(ui, "SEND THEM THIS LINK");

        let link = match phase {
            Some(Phase::Waiting { link, .. }) => Some(link.clone()),
            _ => None,
        };

        match link {
            Some(link) => {
                ui.label(RichText::new(shorten_url(&link)).color(INK).size(11.0).monospace());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if tiny(ui, "copy link").clicked() {
                        ui.ctx().copy_text(link.clone());
                    }
                    ui.label(
                        RichText::new(if self.relay.trim().is_empty() {
                            "same network only"
                        } else {
                            "expires in 5 min"
                        })
                        .color(FAINT)
                        .size(10.0),
                    );
                });
            }
            None if matches!(phase, Some(Phase::Live)) => {
                if let Some(sess) = self.session.clone() {
                    ui.label(
                        RichText::new(format!("{:.0} audio pkt/s", self.rates.audio_pps))
                            .color(FAINT)
                            .size(11.0),
                    );
                    ui.add_space(6.0);
                    app_meter(ui, &sess);
                    ui.add_space(4.0);
                    mic_meter(ui, &sess);
                }
            }
            None => {
                ui.label(RichText::new("appears when you start").color(FAINT).size(11.0));
            }
        }
    }

    fn buttons(&mut self, ui: &mut egui::Ui, phase: Option<&Phase>) {
        let session = self.session.clone();
        let live = matches!(phase, Some(Phase::Live));
        let running = phase.is_some() && !matches!(phase, Some(Phase::Failed(_) | Phase::Ended));

        ui.horizontal(|ui| {
            let mic_on = session.as_ref().is_some_and(|s| s.mic_on());
            let mic_ok = session.as_ref().is_some_and(|s| s.mic_available());
            if pill(ui, "mic", mic_on, live && mic_ok)
                .on_hover_text(format!("microphone - {}", hotkey::DESCRIPTION))
                .clicked()
            {
                if let Some(s) = &session {
                    let now = !s.mic_on();
                    s.set_mic_on(now);
                }
            }

            let paused = session.as_ref().is_some_and(|s| s.paused());
            if pill(ui, if paused { "resume" } else { "pause" }, paused, live)
                .on_hover_text("hold the picture without dropping the viewer")
                .clicked()
            {
                if let Some(s) = &session {
                    s.toggle_pause();
                }
            }

            let ready = self.selected.is_some();
            let label = if running { "stop" } else { "start" };
            if pill(ui, label, !running && ready, running || ready).clicked() {
                if running {
                    self.stop();
                } else {
                    self.start();
                }
            }

            if matches!(phase, Some(Phase::Failed(_) | Phase::Ended))
                && tiny(ui, "clear").clicked()
            {
                self.stop();
            }
        });
    }
}

impl App {
    /// The expanded list, shown only while picking.
    fn source_list(&mut self, ui: &mut egui::Ui) {
        let mut chosen: Option<u32> = None;
        egui::Frame::new()
            .fill(SURFACE)
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.style_mut().spacing.item_spacing.y = 3.0;
                        // Collected first: drawing borrows `self.sources`
                        // while `icon` needs `&mut self`.
                        let rows: Vec<(Source, Option<egui::TextureHandle>)> = self
                            .sources
                            .clone()
                            .into_iter()
                            .map(|s| {
                                let t = self.icon(ui.ctx(), &s.path);
                                (s, t)
                            })
                            .collect();

                        for (s, texture) in &rows {
                            let hit = source_row(
                                ui,
                                s,
                                self.selected == Some(s.pid),
                                texture.as_ref(),
                            );
                            if hit.clicked() {
                                chosen = Some(s.pid);
                            }
                        }
                    });
            });
        if let Some(pid) = chosen {
            self.selected = Some(pid);
            self.picking = false;

            // A running session is told directly. Nothing is torn down: the
            // capture threads pick the change up on their next pass, and the
            // viewer's connection never notices.
            if let Some(s) = &self.session {
                s.select_source(pid);
            }
        }
    }
}

/// What the rate controller is currently asking the encoder for, shown only
/// while that is below full quality.
///
/// At the ceiling this would be a number meaning "normal", which is noise. Below
/// it, it is the answer to the only question a softer picture raises, whether
/// the software is doing something wrong, or the viewer's connection cannot
/// take more.
fn throttle_note(session: &Session) -> Option<String> {
    let (kbps, fps) = session.quality()?;
    if kbps * 1000 >= crate::bwe::MAX_BITRATE {
        return None;
    }
    Some(format!("target {:.1} Mbit/s · {fps} fps", kbps as f32 / 1000.0))
}

/// The mark itself, painted at the size the strip needs it.
///
/// Painted rather than uploaded as a texture: it is five capsules, and a
/// texture would mean carrying a bitmap for every scale factor the window
/// might be dragged onto.
fn glyph(ui: &mut egui::Ui, height: f32) {
    let scale = height / mark::GRID;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(height, height), egui::Sense::hover());
    let amber = Color32::from_rgb(mark::AMBER[0], mark::AMBER[1], mark::AMBER[2]);

    for bar in mark::bars(height.round() as u32) {
        let at = egui::Rect::from_min_size(
            rect.min + egui::vec2(bar.x * scale, bar.y * scale),
            egui::vec2(bar.w * scale, bar.h * scale),
        );
        ui.painter().rect_filled(
            at,
            egui::CornerRadius::same((bar.w * scale / 2.0).round() as u8),
            amber.gamma_multiply(bar.alpha),
        );
    }
}

/// A filled circle, painted rather than typed. Geometric glyphs are not
/// reliably present in the default font set and fall back to an empty box,
/// which reads as a bug rather than a status light.
fn status_dot(ui: &mut egui::Ui, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, colour);
}

impl App {
    /// The renderer texture for an executable's icon, decoding and uploading
    /// it the first time it is asked for.
    fn icon(&mut self, ctx: &egui::Context, path: &str) -> Option<egui::TextureHandle> {
        if let Some(t) = self.textures.get(path) {
            return Some(t.clone());
        }

        let icon = self.icons.get(path)?;
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [icon.width, icon.height],
            &icon.rgba,
        );
        let handle = ctx.load_texture(path, image, egui::TextureOptions::LINEAR);
        self.textures.insert(path.to_owned(), handle.clone());
        Some(handle)
    }
}

/// Draws an icon into a square at `left_center`, or nothing if there is none.
fn draw_icon(ui: &egui::Ui, texture: Option<&egui::TextureHandle>, left_center: egui::Pos2, size: f32) {
    let Some(texture) = texture else { return };
    let rect = egui::Rect::from_center_size(
        egui::pos2(left_center.x + size / 2.0, left_center.y),
        egui::vec2(size, size),
    );
    ui.painter().image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

/// A combo box, drawn rather than assembled from widgets.
///
/// egui's button centres its label and gives no way to left-align it, which is
/// the one thing a field like this must do. Painting it also puts the chevron
/// in its own divided cell, which is what makes it read as a dropdown without
/// needing a caption to say so.
fn combo_field(
    ui: &mut egui::Ui,
    label: &str,
    chosen: bool,
    open: bool,
    icon: Option<&egui::TextureHandle>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 28.0),
        egui::Sense::click(),
    );

    let radius = egui::CornerRadius::same(4);
    let fill = if response.hovered() { LINE } else { SURFACE_HI };
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, if open { ACCENT } else { LINE }),
        egui::StrokeKind::Inside,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let cell = 26.0;
    let split = rect.right() - cell;
    ui.painter().line_segment(
        [
            egui::pos2(split, rect.top() + 1.0),
            egui::pos2(split, rect.bottom() - 1.0),
        ],
        egui::Stroke::new(1.0, LINE),
    );

    let text_left = if icon.is_some() {
        draw_icon(ui, icon, egui::pos2(rect.left() + 8.0, rect.center().y), 16.0);
        rect.left() + 30.0
    } else {
        rect.left() + 10.0
    };
    ui.painter().text(
        egui::pos2(text_left, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        if chosen { INK } else { MUTED },
    );

    // A drawn triangle: the geometric glyphs are not reliably present in the
    // default font set and fall back to an empty box.
    let c = egui::pos2(split + cell / 2.0, rect.center().y);
    let (w, h) = (4.0, 3.0);
    let points = if open {
        vec![
            egui::pos2(c.x - w, c.y + h),
            egui::pos2(c.x + w, c.y + h),
            egui::pos2(c.x, c.y - h),
        ]
    } else {
        vec![
            egui::pos2(c.x - w, c.y - h),
            egui::pos2(c.x + w, c.y - h),
            egui::pos2(c.x, c.y + h),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(points, MUTED, egui::Stroke::NONE));

    response
}

/// A zone of the strip: fixed width, top-aligned, no border of its own.
fn zone<R>(ui: &mut egui::Ui, width: f32, body: impl FnOnce(&mut egui::Ui) -> R) {
    ui.allocate_ui(egui::vec2(width, 62.0), |ui| {
        ui.set_width(width);
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(14, 0))
            .show(ui, |ui| {
                ui.set_width((width - 28.0).max(60.0));
                ui.set_min_height(58.0);
                ui.vertical(body);
            });
    });
}

/// A hairline between zones, rather than a box around each.
fn divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, 52.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, egui::CornerRadius::ZERO, LINE);
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(FAINT).size(9.0).strong());
    ui.add_space(3.0);
}

/// A small filled action. `on` fills it with the accent; disabled fades it.
fn pill(ui: &mut egui::Ui, label: &str, on: bool, enabled: bool) -> egui::Response {
    let (fill, text, stroke) = if !enabled {
        (Color32::TRANSPARENT, FAINT, egui::Stroke::new(1.0, LINE))
    } else if on {
        (ACCENT, BG, egui::Stroke::NONE)
    } else {
        (SURFACE_HI, INK, egui::Stroke::new(1.0, LINE))
    };
    let button = egui::Button::new(RichText::new(label).size(11.0).color(text))
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(4))
        .stroke(stroke);
    ui.add_enabled(enabled, button)
}

/// A borderless text action, for things that sit beside content.
fn tiny(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(10.0).color(MUTED))
            .fill(SURFACE_HI)
            .corner_radius(egui::CornerRadius::same(3))
            .stroke(egui::Stroke::new(1.0, LINE)),
    )
}

/// Dots while hidden, spaced characters while shown. Same character count
/// either way, so revealing the code does not make the strip jump.
fn masked(code: &str, visible: bool) -> String {
    if visible {
        spaced(code)
    } else {
        vec!["\u{2022}"; code.chars().count()].join(" ")
    }
}

/// Drops the scheme so a long relay URL still fits beside the code.
fn shorten_url(url: &str) -> String {
    let bare = url.trim_start_matches("https://").trim_start_matches("http://");
    truncate(bare, 32)
}

/// One row of the expanded list, drawn by hand so the text can be left-aligned
/// and two-line without fighting the button widget's centring.
fn source_row(
    ui: &mut egui::Ui,
    src: &Source,
    selected: bool,
    icon: Option<&egui::TextureHandle>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 40.0),
        egui::Sense::click(),
    );

    let fill = if selected {
        ACCENT.gamma_multiply(0.18)
    } else if response.hovered() {
        SURFACE_HI
    } else {
        BG
    };
    let radius = egui::CornerRadius::same(3);
    ui.painter().rect_filled(rect, radius, fill);
    if selected {
        ui.painter().rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    draw_icon(ui, icon, egui::pos2(rect.left() + 10.0, rect.center().y), 20.0);

    let left = rect.left_top() + egui::vec2(if icon.is_some() { 38.0 } else { 10.0 }, 6.0);
    ui.painter().text(
        left,
        egui::Align2::LEFT_TOP,
        &src.exe,
        egui::FontId::proportional(12.0),
        INK,
    );
    // A packaged application whose real process could not be found will
    // stream a perfect picture and total silence, and nothing downstream can
    // tell that apart from an application that is merely quiet. Saying so
    // here, before it is picked, is the only honest place for it.
    let (subtitle, colour) = if src.frame_hosted {
        ("audio cannot be captured for this app".to_owned(), BAD)
    } else {
        (truncate(&src.title, 70), MUTED)
    };
    ui.painter().text(
        left + egui::vec2(0.0, 17.0),
        egui::Align2::LEFT_TOP,
        subtitle,
        egui::FontId::proportional(10.0),
        colour,
    );

    response
}

/// The shared application's own level.
///
/// The packet counter above it only ever proves the stream is running, never
/// that it carries anything: process loopback is gap-filled with synthesised
/// silence, so a muted application and a loud one send exactly the same number
/// of packets at exactly the same rate. This is the one place that separates
/// them, and it is the first thing to look at when a viewer says they cannot
/// hear the game.
fn app_meter(ui: &mut egui::Ui, session: &Session) {
    if !session.app_audio_ok() {
        ui.label(RichText::new("cannot capture this app's audio").color(BAD).size(10.0));
        return;
    }

    let peak = session.app_peak();
    ui.horizontal(|ui| {
        ui.label(RichText::new("app").color(FAINT).size(10.0));
        meter_bar(ui, peak, 150.0);
    });
}

fn mic_meter(ui: &mut egui::Ui, session: &Session) {
    if !session.mic_available() {
        ui.label(RichText::new("no microphone").color(MUTED).size(10.0));
        return;
    }

    let on = session.mic_on();
    // A level meter rather than just a state: a mic that is on and reading
    // nothing is the failure people otherwise discover mid-conversation.
    let peak = if on { session.mic_peak() } else { 0.0 };
    ui.horizontal(|ui| {
        ui.label(RichText::new("mic").color(FAINT).size(10.0));
        meter_bar(ui, peak, 150.0);
    });
}

fn meter_bar(ui: &mut egui::Ui, peak: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(width), 5.0),
        egui::Sense::hover(),
    );
    let radius = egui::CornerRadius::same(2);
    ui.painter().rect_filled(rect, radius, SURFACE_HI);
    if peak > 0.0 {
        // A floor on the drawn width, not on the value: quiet audio is still
        // audible audio, and a bar too short to see reads as nothing at all,
        // which is the one thing this is here to rule out.
        let mut filled = rect;
        filled.set_width(rect.width() * peak.clamp(0.02, 1.0));
        ui.painter().rect_filled(filled, radius, ACCENT);
    }
}

/// Spaces a code out so it is easy to read aloud over a call.
fn spaced(code: &str) -> String {
    code.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::{masked, shorten_url, spaced, truncate};

    #[test]
    fn hidden_codes_reveal_nothing_but_keep_their_width() {
        let hidden = masked("ABC234", false);
        assert!(!hidden.contains('A') && !hidden.contains('2'));
        // Same character count as the revealed form, so toggling does not
        // shift everything beside it.
        assert_eq!(hidden.chars().count(), masked("ABC234", true).chars().count());
    }

    #[test]
    fn revealed_codes_are_spaced() {
        assert_eq!(masked("ABC234", true), "A B C 2 3 4");
    }

    #[test]
    fn urls_lose_the_scheme_but_keep_the_code() {
        let out = shorten_url("https://sideband.example.workers.dev/ABC234");
        assert!(!out.starts_with("https://"));
        assert!(out.starts_with("sideband."));
    }

    #[test]
    fn codes_are_spaced_for_reading_aloud() {
        assert_eq!(spaced("ABC234"), "A B C 2 3 4");
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        // Byte slicing would panic here, and window titles are full of these.
        let s = "日本語のウィンドウタイトル";
        let out = truncate(s, 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn short_titles_are_left_alone() {
        assert_eq!(truncate("Overwatch", 40), "Overwatch");
    }
}
