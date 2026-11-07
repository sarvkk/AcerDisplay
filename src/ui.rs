//! Main application shell: sidebar with the monitor list, the Image (DDC/CI) tab,
//! and glue for the Layout and Profiles tabs.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::Color32;

use crate::ddc::{self, Caps, Ev, MonitorId, Worker};
use crate::layout::{self, LayoutTab};
use crate::mutter::{self, GState};
use crate::profiles::{MonitorProfile, Profile, Store};

pub enum JobResult {
    Ddc(Vec<(MonitorId, Worker)>),
    Gnome(Result<GState, String>),
    BuiltinBrightness {
        monitor_key: String,
        result: Result<i32, String>,
    },
    BuiltinBrightnessSet {
        monitor_key: String,
        result: Result<(), String>,
    },
    ProfileLayout {
        name: String,
        logicals: Vec<mutter::GLogicalIn>,
        prev: Vec<mutter::GLogicalIn>,
        layout_mode: u32,
        result: Result<(), String>,
    },
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    Image,
    Layout,
    Profiles,
}

pub struct MonUi {
    pub id: MonitorId,
    pub worker: Worker,
    /// code -> (current, max) as last reported by the monitor.
    pub values: BTreeMap<u8, (u16, u16)>,
    /// Local slider/dropdown state, synced from `values` when idle.
    pub edits: BTreeMap<u8, f64>,
    pub failures: BTreeMap<u8, String>,
    pub caps: Option<Result<Caps, String>>,
    /// Code whose slider is being dragged this frame.
    pub dragging: Option<u8>,
    /// Debounced slider commits: code -> (value, when to send).
    pub pending_set: BTreeMap<u8, (f64, Instant)>,
    /// Deferred reads, e.g. re-reading image values after a mode switch:
    /// (when to send, code).
    pub pending_gets: Vec<(Instant, u8)>,
    pub builtin_brightness: Option<i32>,
    pub builtin_tried: bool,
    pub builtin_failure: Option<String>,
}

pub struct ProfilesTab {
    pub store: Store,
    pub new_name: String,
}

pub struct App {
    pub tab: Tab,
    pub sel: usize,
    pub monitors: Option<Vec<MonUi>>,
    pub perm_error: Option<String>,
    pub rx: Receiver<JobResult>,
    pub tx: Sender<JobResult>,
    pub layout: LayoutTab,
    pub profiles: ProfilesTab,
    pub status: Option<(String, Instant)>,
    pub repaint_context: Option<egui::Context>,
    /// Whether the initial monitor selection has been adjusted once the GNOME
    /// state (needed to spot built-in panels) was first loaded.
    pub auto_select_done: bool,
}

impl App {
    pub fn new() -> Self {
        Self::with_tab(None)
    }

    pub fn with_tab(tab: Option<Tab>) -> Self {
        let (tx, rx) = channel::<JobResult>();
        spawn_ddc_enum(tx.clone());
        let perm_error = ddc::i2c_permission_error();
        let (store, profile_error) = match Store::load() {
            Ok(store) => (store, None),
            Err(err) => (
                Store::default(),
                Some(format!("Loading profiles failed: {err}")),
            ),
        };
        App {
            tab: tab.unwrap_or(Tab::Image),
            sel: 0,
            monitors: None,
            perm_error,
            rx,
            tx,
            layout: LayoutTab::new(),
            profiles: ProfilesTab {
                store,
                new_name: String::new(),
            },
            status: profile_error.map(|message| (message, Instant::now())),
            repaint_context: None,
            auto_select_done: false,
        }
    }

    pub fn spawn_gnome(&self) {
        let tx = self.tx.clone();
        let repaint = self.repaint_context.clone();
        thread::spawn(move || {
            if tx.send(JobResult::Gnome(mutter::get_state())).is_ok() {
                if let Some(ctx) = repaint {
                    ctx.request_repaint();
                }
            }
        });
    }
}

fn spawn_ddc_enum(tx: Sender<JobResult>) {
    thread::spawn(move || {
        let _ = tx.send(JobResult::Ddc(ddc::enumerate()));
    });
}

/// True when an EDID identity string carries no usable value (missing, or
/// mutter's hex fallback like "0x15e7").
fn identity_placeholder(s: &str) -> bool {
    let s = s.trim();
    s.is_empty() || s.eq_ignore_ascii_case("unknown") || s == "-" || s.starts_with("0x")
}

/// Whether this GNOME monitor describes the same physical monitor as `id`.
/// Built-in panels report no model/serial strings in EDID, so identity falls
/// back to the manufacturer with placeholder-tolerant model/serial matching.
fn gnome_matches_ddc(gm: &mutter::GMonitor, id: &MonitorId) -> bool {
    if gm.key() == id.key {
        return true;
    }
    let compatible = |a: &str, b: &str| {
        a.eq_ignore_ascii_case(b) || identity_placeholder(a) || identity_placeholder(b)
    };
    gm.vendor.eq_ignore_ascii_case(&id.manufacturer)
        && compatible(&gm.product, &id.model)
        && compatible(&gm.serial, &id.serial)
}

/// The GNOME monitor describing the same physical monitor as `id`, if known.
fn gnome_monitor_for<'a>(
    state: Option<&'a GState>,
    id: &MonitorId,
) -> Option<&'a mutter::GMonitor> {
    state?.monitors.iter().find(|gm| gnome_matches_ddc(gm, id))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.repaint_context = Some(ctx.clone());
        // Tab shortcuts (1/2/3), ignored while typing in a text field.
        let typing = ctx.wants_keyboard_input();
        if !typing {
            ctx.input(|i| {
                if i.key_pressed(egui::Key::Num1) {
                    self.tab = Tab::Image;
                }
                if i.key_pressed(egui::Key::Num2) {
                    self.tab = Tab::Layout;
                }
                if i.key_pressed(egui::Key::Num3) {
                    self.tab = Tab::Profiles;
                }
            });
        }

        self.process_jobs(ctx);
        self.process_monitors();

        // GNOME layout tab bookkeeping.
        let mut refresh_gnome = false;
        if self.tab == Tab::Layout && self.layout.wants_refresh() {
            refresh_gnome = true;
        }
        if let Some(layout::TickAction::RevertExpired) = self.layout.tick() {
            layout::revert(&mut self.layout, &mut self.status);
            refresh_gnome = true;
        }
        let actions = layout::take_actions();
        let had_apply = !actions.0.is_empty() || !actions.1.is_empty();
        layout::process_actions(&mut self.layout, actions, &mut self.status);
        if had_apply {
            refresh_gnome = true;
        }
        if refresh_gnome {
            self.spawn_gnome();
        }

        // Top bar with tabs.
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("🖥 Display Widget");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Image, "Image (DDC)");
                ui.selectable_value(&mut self.tab, Tab::Layout, "Layout (GNOME)");
                ui.selectable_value(&mut self.tab, Tab::Profiles, "Profiles");
            });
            ui.add_space(4.0);
        });

        // Sidebar with the monitor list.
        egui::SidePanel::left("monitors_panel")
            .default_width(250.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.heading("Monitors");
                ui.add_space(4.0);
                match &mut self.monitors {
                    None => {
                        ui.spinner();
                        ui.label("Detecting monitors…");
                    }
                    Some(mons) if mons.is_empty() => {
                        ui.label("No DDC-capable monitors found.");
                        if let Some(err) = &self.perm_error {
                            ui.add_space(6.0);
                            permission_help(ui, err);
                        }
                    }
                    Some(mons) => {
                        let state = self.layout.state.as_ref().and_then(|r| r.as_ref().ok());
                        for (i, m) in mons.iter().enumerate() {
                            let name = gnome_monitor_for(state, &m.id)
                                .filter(|gm| gm.is_builtin)
                                .map(|gm| gm.display_name.clone())
                                .unwrap_or_else(|| m.id.model.clone());
                            let label =
                                format!("{}\n{} · serial {}", name, m.id.manufacturer, m.id.serial);
                            if ui
                                .add(
                                    egui::Button::new(egui::RichText::new(&label).small())
                                        .selected(self.sel == i)
                                        .min_size(egui::vec2(ui.available_width(), 0.0)),
                                )
                                .clicked()
                            {
                                self.sel = i;
                            }
                        }
                        ui.add_space(6.0);
                        if ui.button("↻ Rescan").clicked() {
                            spawn_ddc_enum(self.tx.clone());
                            self.monitors = None;
                            self.spawn_gnome();
                        }
                    }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.separator();
                    ui.label(
                        egui::RichText::new("DDC/CI + mutter DisplayConfig")
                            .small()
                            .weak(),
                    );
                });
            });

        // Main content.
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Image => {
                let Some(mons) = &mut self.monitors else {
                    ui.spinner();
                    return;
                };
                if mons.is_empty() {
                    ui.heading("Image settings");
                    ui.label(
                        "Connect a monitor that supports DDC/CI (most external monitors do), \
                             then click Rescan.",
                    );
                    return;
                }
                let sel = self.sel.min(mons.len() - 1);
                let state = self.layout.state.as_ref().and_then(|r| r.as_ref().ok());
                let gm = gnome_monitor_for(state, &mons[sel].id);
                let is_builtin = gm.is_some_and(|gm| gm.is_builtin);
                let title = match gm {
                    Some(gm) if gm.is_builtin => gm.display_name.clone(),
                    _ => mons[sel].id.model.clone(),
                };
                let dragging = show_image(ui, &mut mons[sel], is_builtin, &title, &self.tx);
                mons[sel].dragging = dragging;
            }
            Tab::Layout => {
                let mut manual_refresh = false;
                layout::show(ui, &mut self.layout, &mut manual_refresh);
                if manual_refresh {
                    self.layout.dirty = false;
                    self.spawn_gnome();
                }
            }
            Tab::Profiles => self.show_profiles(ui),
        });

        // Status toast.
        if let Some((msg, t)) = &self.status {
            if Instant::now() - *t < Duration::from_secs(4) {
                egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
                    ui.label(
                        egui::RichText::new(format!("✓ {msg}"))
                            .color(Color32::from_rgb(140, 220, 140)),
                    );
                });
            } else {
                self.status = None;
            }
        }

        // Keep repainting while anything is in flight.
        let busy = self.monitors.as_ref().is_some_and(|ms| {
            ms.iter().any(|m| {
                !m.pending_set.is_empty() || !m.pending_gets.is_empty() || m.caps.is_none()
            })
        }) || self.layout.pending.is_some()
            || self.monitors.is_none();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }
}

/// Find the GNOME connector matching the selected DDC monitor (by serial, then model).
fn permission_help(ui: &mut egui::Ui, err: &str) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_max_width(ui.available_width());
        ui.label(egui::RichText::new("i2c permission needed").strong());
        ui.label(
            egui::RichText::new(
                "DDC/CI talks to the monitor through /dev/i2c-*, which is restricted. Fix:",
            )
            .small(),
        );
        ui.monospace("  sudo usermod -aG i2c $USER");
        ui.label(
            egui::RichText::new("then log out and back in (or run the bundled installer:").small(),
        );
        ui.monospace("  sudo ./install.sh");
        ui.label(egui::RichText::new("After that, click Rescan.").small());
        ui.label(egui::RichText::new(err).small().weak());
    });
}

impl App {
    fn process_jobs(&mut self, ctx: &egui::Context) {
        while let Ok(job) = self.rx.try_recv() {
            match job {
                JobResult::Ddc(list) => {
                    self.perm_error = ddc::i2c_permission_error();
                    let mut mons = Vec::new();
                    for (id, worker) in list {
                        worker.set_repaint_context(&ctx);
                        worker.get(ddc::BRIGHTNESS);
                        worker.get(ddc::CONTRAST);
                        worker.caps();
                        mons.push(MonUi {
                            id,
                            worker,
                            values: BTreeMap::new(),
                            edits: BTreeMap::new(),
                            failures: BTreeMap::new(),
                            caps: None,
                            dragging: None,
                            pending_set: BTreeMap::new(),
                            pending_gets: Vec::new(),
                            builtin_brightness: None,
                            builtin_tried: false,
                            builtin_failure: None,
                        });
                    }
                    // Auto-apply profiles marked auto.
                    let autos: Vec<Profile> = self
                        .profiles
                        .store
                        .profiles
                        .iter()
                        .filter(|p| p.auto)
                        .cloned()
                        .collect();
                    for m in &mut mons {
                        for p in &autos {
                            if let Some(mp) = p.monitors.get(&m.id.key) {
                                for (code, val) in &mp.vcp {
                                    if *code == ddc::POWER_MODE {
                                        continue;
                                    }
                                    m.worker.set(*code, *val);
                                }
                            }
                        }
                    }
                    self.monitors = Some(mons);
                    self.sel = 0;
                    self.auto_select_done = false;
                    self.spawn_gnome();
                }
                JobResult::Gnome(res) => {
                    self.layout.absorb(res);
                    // Once the GNOME state is known, start on a monitor that
                    // actually speaks DDC/CI rather than the built-in panel,
                    // whose Image tab is GNOME-driven.
                    if !self.auto_select_done {
                        if let Some(mons) = &self.monitors {
                            let state = self.layout.state.as_ref().and_then(|r| r.as_ref().ok());
                            if let Some(idx) = mons.iter().position(|m| {
                                !gnome_monitor_for(state, &m.id).is_some_and(|gm| gm.is_builtin)
                            }) {
                                self.sel = idx;
                            }
                            self.auto_select_done = true;
                        }
                    }
                }
                JobResult::BuiltinBrightness {
                    monitor_key,
                    result,
                } => {
                    if let Some(mon) = self
                        .monitors
                        .as_mut()
                        .and_then(|mons| mons.iter_mut().find(|m| m.id.key == monitor_key))
                    {
                        match result {
                            Ok(value) => mon.builtin_brightness = Some(value),
                            Err(err) => mon.builtin_failure = Some(err),
                        }
                    }
                }
                JobResult::BuiltinBrightnessSet {
                    monitor_key,
                    result,
                } => {
                    if let Some(mon) = self
                        .monitors
                        .as_mut()
                        .and_then(|mons| mons.iter_mut().find(|m| m.id.key == monitor_key))
                    {
                        if let Err(err) = result {
                            mon.builtin_failure = Some(err);
                        } else {
                            mon.builtin_failure = None;
                        }
                    }
                }
                JobResult::ProfileLayout {
                    name,
                    logicals,
                    prev,
                    layout_mode,
                    result,
                } => match result {
                    Ok(()) => {
                        self.layout.pending = Some(layout::PendingApply {
                            until: Instant::now() + Duration::from_secs(layout::REVERT_SECS),
                            new_cfg: logicals,
                            prev_cfg: prev,
                            layout_mode,
                        });
                        self.spawn_gnome();
                        self.status = Some((
                            format!("Applied profile “{name}” — confirm in Layout tab"),
                            Instant::now(),
                        ));
                    }
                    Err(e) => {
                        self.status = Some((format!("Profile layout failed: {e}"), Instant::now()));
                    }
                },
            }
        }
    }

    fn process_monitors(&mut self) {
        let Some(mons) = &mut self.monitors else {
            return;
        };
        for mon in mons.iter_mut() {
            while let Some(ev) = mon.worker.try_recv() {
                match ev {
                    Ev::Value {
                        code, value, max, ..
                    } => {
                        mon.values.insert(code, (value, max));
                        mon.failures.remove(&code);
                        if mon.dragging != Some(code) && !mon.pending_set.contains_key(&code) {
                            mon.edits.insert(code, value as f64);
                        }
                    }
                    Ev::SetFinished { code, result, .. } => {
                        match result {
                            crate::ddc::WriteResult::Verified { value, max } => {
                                mon.values.insert(code, (value, max));
                                mon.edits.insert(code, value as f64);
                                mon.failures.remove(&code);
                            }
                            crate::ddc::WriteResult::Unverified(err)
                            | crate::ddc::WriteResult::Failed(err) => {
                                mon.failures.insert(code, err);
                            }
                        }
                        if code == ddc::ACER_MODE {
                            // Brightness/contrast are stored per mode by the
                            // display; give it a moment to settle, then re-read
                            // the curated set (its stored values just changed).
                            let at = Instant::now() + Duration::from_millis(700);
                            for c in ddc::CURATED {
                                mon.pending_gets.push((at, c));
                            }
                        }
                    }
                    Ev::Failed { code, err, .. } => {
                        mon.failures.insert(code, err);
                    }
                    Ev::Caps(res) => {
                        let mut to_get: Vec<u8> = ddc::CURATED.to_vec();
                        to_get.push(ddc::POWER_MODE);
                        to_get.push(ddc::ACER_MODE);
                        mon.caps = Some(res);
                        if let Ok(caps) = mon.caps.as_ref().unwrap() {
                            for c in to_get {
                                if caps.codes.contains(&c) {
                                    mon.worker.get(c);
                                }
                            }
                        }
                    }
                }
            }
            // Fire debounced slider commits.
            let now = Instant::now();
            let due: Vec<(u8, f64)> = mon
                .pending_set
                .iter()
                .filter(|(_, (_, t))| now >= *t)
                .map(|(c, (v, _))| (*c, *v))
                .collect();
            for (code, v) in due {
                mon.pending_set.remove(&code);
                mon.worker.set(code, v as u16);
            }
            // Fire deferred reads (e.g. re-reading image values after a mode switch).
            let due_gets: Vec<u8> = mon
                .pending_gets
                .iter()
                .filter(|(t, _)| now >= *t)
                .map(|(_, c)| *c)
                .collect();
            mon.pending_gets.retain(|(t, _)| now < *t);
            for code in due_gets {
                mon.worker.get(code);
            }
        }
    }

    fn show_profiles(&mut self, ui: &mut egui::Ui) {
        ui.heading("Profiles");
        ui.label(
            egui::RichText::new(
                "A profile remembers image settings (brightness/contrast/…) per monitor and, \
                 optionally, its GNOME layout (mode, scale, rotation, position).",
            )
            .weak(),
        );
        ui.add_space(8.0);

        // Built-in monitor modes: the display's own picture profiles (the same
        // ones behind its Mode button), switched over the vendor VCP. Shown
        // for monitors whose capabilities advertise that control.
        let mode_mons: Vec<(usize, String, Option<u16>, Option<String>)> = self
            .monitors
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, m)| {
                m.caps
                    .as_ref()
                    .and_then(|c| c.as_ref().ok())
                    .is_some_and(|c| c.codes.contains(&ddc::ACER_MODE))
            })
            .map(|(i, m)| {
                (
                    i,
                    m.id.model.clone(),
                    m.values.get(&ddc::ACER_MODE).map(|(v, _)| *v),
                    m.failures.get(&ddc::ACER_MODE).cloned(),
                )
            })
            .collect();
        if !mode_mons.is_empty() {
            ui.label(egui::RichText::new("Monitor modes").strong());
            ui.label(
                egui::RichText::new(
                    "Picture presets built into the display itself — the same profiles as \
                     the monitor's Mode button. Each mode remembers its own brightness.",
                )
                .small()
                .weak(),
            );
            ui.add_space(4.0);
            let mut clicks: Vec<(usize, u16, &'static str)> = Vec::new();
            for (idx, model, current, failure) in &mode_mons {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(format!("{model}:")).strong());
                    for (value, name) in ddc::ACER_MODES {
                        let selected = *current == Some(value);
                        if ui.selectable_label(selected, name).clicked() && !selected {
                            clicks.push((*idx, value, name));
                        }
                    }
                    let current_text = match current {
                        Some(v) => match ddc::acer_mode_name(*v) {
                            Some(name) => format!("current: {name}"),
                            // Known mode values are all below 0x10; anything else
                            // (e.g. a 0xFF placeholder before the first write)
                            // means "no idea which mode".
                            None if *v > 0x0f => "current: unknown (pick one to sync)".to_string(),
                            None => format!("current: 0x{v:02X}"),
                        },
                        None => "current: unknown (pick one to sync)".to_string(),
                    };
                    ui.label(egui::RichText::new(current_text).small().weak());
                });
                if let Some(err) = failure {
                    ui.label(
                        egui::RichText::new(err)
                            .small()
                            .color(egui::Color32::LIGHT_RED),
                    );
                }
            }
            for (idx, value, name) in clicks {
                let model = if let Some(mon) = self.monitors.as_mut().and_then(|m| m.get_mut(idx)) {
                    mon.failures.remove(&ddc::ACER_MODE);
                    mon.worker.set(ddc::ACER_MODE, value);
                    Some(mon.id.model.clone())
                } else {
                    None
                };
                if let Some(model) = model {
                    self.status = Some((format!("Switching {model} to “{name}”…"), Instant::now()));
                }
            }
            ui.add_space(8.0);
            ui.separator();
        }

        // Save current state as a new profile.
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.add(
                egui::TextEdit::singleline(&mut self.profiles.new_name)
                    .desired_width(200.0)
                    .hint_text("e.g. Work"),
            );
            let name = if self.profiles.new_name.trim().is_empty() {
                format!("Profile {}", self.profiles.store.profiles.len() + 1)
            } else {
                self.profiles.new_name.trim().to_string()
            };
            if ui.button("💾 Save current").clicked() {
                let profile = self.capture_profile(&name);
                self.profiles.store.upsert(profile);
                if let Err(e) = self.profiles.store.save() {
                    self.status = Some((format!("Saving profiles failed: {e}"), Instant::now()));
                } else {
                    self.status = Some((format!("Saved profile “{name}”"), Instant::now()));
                }
                self.profiles.new_name.clear();
            }
        });

        ui.add_space(8.0);
        ui.separator();

        if self.profiles.store.profiles.is_empty() {
            ui.label("No profiles yet.");
            return;
        }

        let profiles: Vec<Profile> = self.profiles.store.profiles.iter().cloned().collect();
        let mut save_after: bool = false;
        for profile in profiles {
            let name = profile.name.clone();
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&name).strong());
                    let mut auto = profile.auto;
                    if ui.checkbox(&mut auto, "apply at startup").changed() {
                        if let Some(current) = self
                            .profiles
                            .store
                            .profiles
                            .iter_mut()
                            .find(|p| p.name == name)
                        {
                            current.auto = auto;
                        }
                        save_after = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("🗑")
                            .on_hover_text("Delete profile")
                            .clicked()
                        {
                            self.profiles.store.remove(&name);
                            save_after = true;
                        }
                        if ui
                            .small_button("Apply full")
                            .on_hover_text("Image settings + GNOME mode/scale/rotation/position")
                            .clicked()
                        {
                            self.apply_profile_full(&name);
                        }
                        if ui
                            .small_button("Apply image")
                            .on_hover_text("Only DDC image settings (brightness, contrast, …)")
                            .clicked()
                        {
                            self.apply_profile_image(&name);
                        }
                    });
                });
                let mons: Vec<String> = profile.monitors.keys().cloned().collect();
                ui.label(egui::RichText::new(mons.join(",  ")).small().weak());
            });
            ui.add_space(4.0);
        }
        if save_after {
            if let Err(e) = self.profiles.store.save() {
                self.status = Some((format!("Saving profiles failed: {e}"), Instant::now()));
            }
        }
    }

    fn capture_profile(&self, name: &str) -> Profile {
        let mut profile = Profile {
            name: name.to_string(),
            auto: false,
            monitors: BTreeMap::new(),
        };
        if let Some(mons) = &self.monitors {
            for m in mons {
                let mp = MonitorProfile {
                    vcp: m
                        .values
                        .iter()
                        .filter(|(code, _)| **code != ddc::POWER_MODE)
                        .map(|(c, (v, _))| (*c, *v))
                        .collect(),
                    ..Default::default()
                };
                profile.monitors.insert(m.id.key.clone(), mp);
            }
        }
        if let Some(Ok(st)) = &self.layout.state {
            for tile in &self.layout.tiles {
                for r in &tile.monitors {
                    if let Some(gm) = st.find_monitor(&r.connector) {
                        let entry = profile.monitors.entry(gm.key()).or_default();
                        if let Some(mode) = gm.modes.get(r.mode_idx) {
                            entry.mode = Some(mode.id.clone());
                        }
                        entry.scale = Some(tile.scale);
                        entry.transform = Some(tile.transform);
                        entry.x = Some(tile.x);
                        entry.y = Some(tile.y);
                    }
                }
            }
        }
        profile
    }

    fn apply_profile_image(&mut self, name: &str) {
        let Some(prof) = self.profiles.store.named(name).cloned() else {
            return;
        };
        let Some(mons) = &mut self.monitors else {
            return;
        };
        let mut matched = 0;
        for m in mons {
            if let Some(mp) = prof.monitors.get(&m.id.key) {
                matched += 1;
                // Switch the display's picture mode first: the monitor then
                // loads its own per-mode stored values, so everything we
                // restore afterwards (brightness, …) wins.
                let mut ordered: Vec<(&u8, &u16)> = mp.vcp.iter().collect();
                ordered.sort_by_key(|(code, _)| u8::from(**code != ddc::ACER_MODE));
                for (code, val) in ordered {
                    if *code == ddc::POWER_MODE {
                        continue;
                    }
                    m.pending_set.remove(code);
                    m.edits.insert(*code, *val as f64);
                    m.worker.set(*code, *val);
                }
            }
        }
        self.status = Some((
            if matched == 0 {
                format!("No monitors matched profile “{name}”")
            } else {
                format!("Applied image settings from “{name}”")
            },
            Instant::now(),
        ));
    }

    fn apply_profile_full(&mut self, name: &str) {
        self.apply_profile_image(name);
        let Some(prof) = self.profiles.store.named(name).cloned() else {
            return;
        };
        let Some(Ok(st)) = self.layout.state.as_ref() else {
            self.status = Some((
                "GNOME state not loaded — only image settings applied".into(),
                Instant::now(),
            ));
            return;
        };
        let st = st.clone();
        let prev = crate::layout::config_from_state(&st);
        let mut logicals = prev.clone();
        for l in &mut logicals {
            for m in &mut l.monitors {
                let key = st.find_monitor(&m.0).map(|gm| gm.key()).unwrap_or_default();
                if let Some(mp) = prof.monitors.get(&key) {
                    if let Some(gm) = st.find_monitor(&m.0) {
                        if let Some(mode_id) = &mp.mode {
                            if let Some(mode) = gm.modes.iter().find(|mm| &mm.id == mode_id) {
                                m.1 = mode.id.clone();
                            }
                        }
                    }
                    if let Some(s) = mp.scale {
                        l.scale = s;
                    }
                    if let Some(t) = mp.transform {
                        l.transform = t;
                    }
                    if let (Some(x), Some(y)) = (mp.x, mp.y) {
                        l.x = x;
                        l.y = y;
                    }
                }
            }
        }
        let tx = self.tx.clone();
        let name = name.to_string();
        let layout_mode = st.layout_mode;
        thread::spawn(move || {
            let result = mutter::get_state().and_then(|fresh| {
                mutter::apply(
                    fresh.serial,
                    mutter::METHOD_TEMPORARY,
                    logicals.clone(),
                    layout_mode,
                    fresh.supports_changing_layout_mode,
                )
            });
            let _ = tx.send(JobResult::ProfileLayout {
                name,
                logicals,
                prev,
                layout_mode,
                result,
            });
        });
        self.status = Some(("Applying profile layout…".into(), Instant::now()));
    }
}

// ---------- Image (DDC) tab ----------

fn show_image(
    ui: &mut egui::Ui,
    mon: &mut MonUi,
    is_builtin: bool,
    title: &str,
    tx: &Sender<JobResult>,
) -> Option<u8> {
    ui.heading(title);
    ui.label(
        egui::RichText::new(format!(
            "{} · serial {} · i2c id {}",
            mon.id.manufacturer, mon.id.serial, mon.id.backend_id
        ))
        .weak(),
    );
    ui.add_space(8.0);

    if is_builtin {
        ui.label("Built-in panel — brightness is handled by GNOME:");
        if !mon.builtin_tried {
            mon.builtin_tried = true;
            let tx = tx.clone();
            let monitor_key = mon.id.key.clone();
            thread::spawn(move || {
                let _ = tx.send(JobResult::BuiltinBrightness {
                    monitor_key,
                    result: mutter::get_builtin_brightness(),
                });
            });
        }
        if let Some(b) = &mut mon.builtin_brightness {
            let mut v = *b;
            let resp = ui.add(egui::Slider::new(&mut v, 0..=100).text("Brightness"));
            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                let tx = tx.clone();
                let monitor_key = mon.id.key.clone();
                let value = v as i32;
                thread::spawn(move || {
                    let _ = tx.send(JobResult::BuiltinBrightnessSet {
                        monitor_key,
                        result: mutter::set_builtin_brightness(value),
                    });
                });
            }
            *b = v;
            if let Some(err) = &mon.builtin_failure {
                ui.colored_label(Color32::from_rgb(255, 170, 60), err);
            }
        } else {
            ui.colored_label(
                Color32::from_rgb(255, 170, 60),
                mon.builtin_failure
                    .as_deref()
                    .unwrap_or("Panel brightness unavailable."),
            );
        }
        return None;
    }

    match &mon.caps {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading monitor capabilities…");
            });
        }
        Some(Err(e)) => {
            ui.colored_label(
                Color32::from_rgb(255, 170, 60),
                format!("Could not read capabilities: {e}"),
            );
            ui.label("Controls below use standard defaults.");
        }
        Some(Ok(caps)) => {
            let extra = caps
                .model
                .as_deref()
                .map(|m| format!(" · caps model: {m}"))
                .unwrap_or_default();
            ui.label(
                egui::RichText::new(format!(
                    "{} VCP features supported · MCCS {}{}",
                    caps.codes.len(),
                    caps.mccs_version.as_deref().unwrap_or("?"),
                    extra
                ))
                .weak(),
            );
        }
    }
    ui.add_space(8.0);

    let mut dragging: Option<u8> = None;

    // Curated rows.
    slider_row(ui, mon, ddc::BRIGHTNESS, "Brightness", &mut dragging);
    slider_row(ui, mon, ddc::CONTRAST, "Contrast", &mut dragging);

    let color_entries = entries_for(mon, ddc::COLOR_PRESET, ddc::color_name);
    combo_row(ui, mon, ddc::COLOR_PRESET, "Color preset", &color_entries);

    let input_entries = entries_for(mon, ddc::INPUT_SOURCE, ddc::input_name);
    combo_row(ui, mon, ddc::INPUT_SOURCE, "Input source", &input_entries);

    slider_row(ui, mon, ddc::AUDIO_VOLUME, "Audio volume", &mut dragging);

    // Power.
    if row_supported(mon, ddc::POWER_MODE) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_sized([110.0, 18.0], egui::Label::new("Power mode"));
            let set = |ui: &mut egui::Ui, mon: &mut MonUi, val: u16, label: &str, tip: &str| {
                let b = ui.small_button(label).on_hover_text(tip);
                if b.clicked() {
                    mon.worker.set(ddc::POWER_MODE, val);
                    mon.edits.insert(ddc::POWER_MODE, val as f64);
                }
            };
            set(ui, mon, 0x01, "On", "Normal operation");
            set(
                ui,
                mon,
                0x02,
                "Standby",
                "Low-power standby (DDC stays reachable)",
            );
            set(
                ui,
                mon,
                0x04,
                "Off",
                "Power off — the monitor may not answer DDC until woken by its power button",
            );
            if let Some((v, _)) = mon.values.get(&ddc::POWER_MODE) {
                ui.label(format!("current: {}", power_text(*v)));
            }
        });
    }

    // Advanced.
    ui.add_space(8.0);
    let advanced = mon
        .caps
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|c| c.codes.clone());
    if let Some(codes) = advanced {
        egui::CollapsingHeader::new("Advanced — all supported VCP features")
            .default_open(false)
            .show(ui, |ui| {
                let mut get_requests: Vec<u8> = Vec::new();
                let mut sets: Vec<(u8, f64)> = Vec::new();
                egui::Grid::new(("advanced_grid", &mon.id.key))
                    .num_columns(5)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        for code in codes {
                            if ddc::CURATED.contains(&code) || code == ddc::POWER_MODE {
                                continue;
                            }
                            ui.monospace(format!("0x{code:02X}"));
                            ui.label(ddc::vcp_name(code));
                            match mon.values.get(&code) {
                                Some((v, max)) => ui.label(format!("{v} / {max}")),
                                None => ui.label("—"),
                            };
                            if ui.small_button("Read").clicked() {
                                get_requests.push(code);
                            }
                            let values = mon
                                .caps
                                .as_ref()
                                .and_then(|r| r.as_ref().ok())
                                .and_then(|c| c.values.get(&code))
                                .cloned()
                                .unwrap_or_default();
                            if !values.is_empty() {
                                let mut chosen: Option<u16> = None;
                                let cur = mon.values.get(&code).map(|(v, _)| *v);
                                egui::ComboBox::from_id_salt(("adv_combo", &mon.id.key, code))
                                    .selected_text(match cur {
                                        Some(v) => value_label(v, &values),
                                        None => "—".into(),
                                    })
                                    .width(150.0)
                                    .show_ui(ui, |ui| {
                                        for (val, _) in &values {
                                            ui.selectable_value(
                                                &mut chosen,
                                                Some(*val),
                                                value_label(*val, &values),
                                            );
                                        }
                                    });
                                if let Some(v) = chosen {
                                    sets.push((code, v as f64));
                                }
                                ui.end_row();
                                continue;
                            }
                            let max =
                                mon.values.get(&code).map(|(_, m)| *m).unwrap_or(65535) as f64;
                            let default_v =
                                mon.values.get(&code).map(|(v, _)| *v as f64).unwrap_or(0.0);
                            let cur = mon.edits.entry(code).or_insert(default_v);
                            ui.add(
                                egui::DragValue::new(cur)
                                    .range(0.0..=max)
                                    .speed(1.0)
                                    .suffix(if max > 100.0 { "" } else { "" }),
                            );
                            if ui.small_button("Set").clicked() {
                                sets.push((code, *cur));
                            }
                            ui.end_row();
                        }
                    });
                for c in get_requests {
                    mon.worker.get(c);
                }
                for (c, v) in sets {
                    mon.pending_set.remove(&c);
                    mon.edits.insert(c, v);
                    mon.worker.set(c, v as u16);
                }
            });
    }

    // Re-read everything.
    ui.add_space(8.0);
    if ui.button("↻ Re-read values").clicked() {
        mon.worker.get(ddc::BRIGHTNESS);
        mon.worker.get(ddc::CONTRAST);
        mon.worker.caps();
        for code in mon.values.keys().copied().collect::<Vec<_>>() {
            mon.worker.get(code);
        }
    }

    dragging
}

fn power_text(v: u16) -> &'static str {
    match v {
        0x01 => "on",
        0x02 => "standby",
        0x03 => "suspend",
        0x04 => "off",
        _ => "unknown",
    }
}

/// Whether the monitor supports `code`, as far as we know.
/// None = capabilities not read yet; Some(true) if listed or capabilities unreadable.
fn row_supported(mon: &MonUi, code: u8) -> bool {
    match &mon.caps {
        None => true, // still reading; be optimistic (rows show a spinner)
        Some(Err(_)) => true,
        Some(Ok(c)) => c.codes.contains(&code),
    }
}

fn entries_for(
    mon: &MonUi,
    code: u8,
    fallback: fn(u16) -> Option<&'static str>,
) -> Vec<(u16, String)> {
    let from_caps = mon
        .caps
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .and_then(|c| c.values.get(&code))
        .map(|m| {
            m.iter()
                .map(|(v, name)| {
                    (
                        *v,
                        name.clone().unwrap_or_else(|| fallback_name(*v, fallback)),
                    )
                })
                .collect::<Vec<_>>()
        });
    match from_caps {
        Some(v) if !v.is_empty() => v,
        _ => {
            // Reasonable defaults when the capabilities string gave us nothing.
            match code {
                ddc::INPUT_SOURCE => [0x01u16, 0x03, 0x0f, 0x10, 0x11, 0x12]
                    .into_iter()
                    .map(|v| (v, fallback_name(v, fallback)))
                    .collect(),
                ddc::COLOR_PRESET => [0x01u16, 0x02, 0x05, 0x08, 0x0b, 0x0c]
                    .into_iter()
                    .map(|v| (v, fallback_name(v, fallback)))
                    .collect(),
                _ => Vec::new(),
            }
        }
    }
}

fn fallback_name(v: u16, fallback: fn(u16) -> Option<&'static str>) -> String {
    fallback(v)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("0x{v:02X}"))
}

fn value_label(v: u16, values: &BTreeMap<u16, Option<String>>) -> String {
    values
        .get(&v)
        .and_then(|o| o.clone())
        .unwrap_or_else(|| format!("0x{v:02X}"))
}

fn slider_row(
    ui: &mut egui::Ui,
    mon: &mut MonUi,
    code: u8,
    label: &str,
    dragging: &mut Option<u8>,
) {
    if !row_supported(mon, code) {
        return; // monitor does not list this feature
    }
    if let Some(err) = mon.failures.get(&code) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [110.0, 18.0],
                egui::Label::new(egui::RichText::new(label).weak()),
            );
            ui.label(
                egui::RichText::new(format!("not available — {err}"))
                    .small()
                    .weak(),
            );
        });
        return;
    }
    let Some(&(cur, max)) = mon.values.get(&code) else {
        ui.horizontal(|ui| {
            ui.add_sized(
                [110.0, 18.0],
                egui::Label::new(egui::RichText::new(label).weak()),
            );
            ui.spinner();
        });
        return;
    };
    let max = (max.max(1)) as f64;
    let v = mon.edits.entry(code).or_insert(cur as f64);
    let mut commit: Option<(u8, f64, bool)> = None; // code, value, immediate
    ui.horizontal(|ui| {
        ui.add_sized([110.0, 18.0], egui::Label::new(label));
        let resp = ui.add(egui::Slider::new(v, 0.0..=max).show_value(false));
        ui.monospace(format!("{:>4.0} / {}", v, max));
        if resp.changed() {
            commit = Some((code, *v, resp.drag_stopped()));
        }
        if resp.dragged() {
            *dragging = Some(code);
        }
    });
    if let Some((c, val, immediate)) = commit {
        let when = if immediate {
            Instant::now()
        } else {
            Instant::now() + Duration::from_millis(250)
        };
        mon.pending_set.insert(c, (val, when));
    }
}

fn combo_row(ui: &mut egui::Ui, mon: &mut MonUi, code: u8, label: &str, entries: &[(u16, String)]) {
    if !row_supported(mon, code) {
        return;
    }
    if let Some(err) = mon.failures.get(&code) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [110.0, 18.0],
                egui::Label::new(egui::RichText::new(label).weak()),
            );
            ui.label(
                egui::RichText::new(format!("not available — {err}"))
                    .small()
                    .weak(),
            );
        });
        return;
    }
    let Some(&(cur, _)) = mon.values.get(&code) else {
        if !entries.is_empty() {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [110.0, 18.0],
                    egui::Label::new(egui::RichText::new(label).weak()),
                );
                ui.spinner();
            });
        }
        return;
    };
    let mut chosen: u16 = cur;
    ui.horizontal(|ui| {
        ui.add_sized([110.0, 18.0], egui::Label::new(label));
        let cur_label = entries
            .iter()
            .find(|(v, _)| *v == cur)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| format!("0x{cur:02X}"));
        egui::ComboBox::from_id_salt(("combo", &mon.id.key, code))
            .selected_text(cur_label)
            .width(200.0)
            .show_ui(ui, |ui| {
                for (val, name) in entries {
                    ui.selectable_value(&mut chosen, *val, name);
                }
            });
    });
    if chosen != cur {
        mon.pending_set.remove(&code);
        mon.edits.insert(code, chosen as f64);
        mon.worker.set(code, chosen);
    }
}
