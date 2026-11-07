//! GNOME "Layout" tab: per-monitor mode/scale/rotation, positions on a mini-map,
//! mirroring, and safe apply with a 15-second keep/revert confirmation.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, Sense, Stroke};

use crate::mutter::{self, GLogicalIn, GMonitorIn, GState, METHOD_PERSISTENT, METHOD_TEMPORARY};

pub const REVERT_SECS: u64 = 15;
const ROLLBACK_RETRY_SECS: u64 = 2;
const MAX_ROLLBACK_ATTEMPTS: u8 = 3;

#[derive(Clone, PartialEq)]
pub struct MonRef {
    pub connector: String,
    pub mode_idx: usize,
}

#[derive(Clone, PartialEq)]
pub struct Tile {
    pub monitors: Vec<MonRef>,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
}

pub struct PendingApply {
    pub until: Instant,
    pub new_cfg: Vec<GLogicalIn>,
    pub prev_cfg: Vec<GLogicalIn>,
    pub layout_mode: u32,
}

struct Recovery {
    pending_until: Instant,
    prev_layout_mode: u32,
    failures: u8,
    next_retry: Option<Instant>,
}

pub struct LayoutTab {
    pub state: Option<Result<GState, String>>,
    pub tiles: Vec<Tile>,
    pub sel: usize,
    pub mirror: bool,
    pub dirty: bool,
    pub pending: Option<PendingApply>,
    pub last_err: Option<String>,
    pub next_poll: Instant,
    recovery: Option<Recovery>,
}

impl Default for LayoutTab {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutTab {
    pub fn new() -> Self {
        LayoutTab {
            state: None,
            tiles: Vec::new(),
            sel: 0,
            mirror: false,
            dirty: false,
            pending: None,
            last_err: None,
            next_poll: Instant::now(),
            recovery: None,
        }
    }

    pub fn wants_refresh(&mut self) -> bool {
        self.pending.is_none() && !self.dirty && Instant::now() >= self.next_poll
    }

    /// Rebuild editable tiles from a freshly fetched state. Wipes user edits.
    pub fn absorb(&mut self, state: Result<GState, String>) {
        self.next_poll = Instant::now() + Duration::from_secs(2);
        match state {
            Ok(st) => {
                if self.pending.is_none() {
                    self.tiles = build_tiles(&st);
                    self.sync_mirror();
                    self.sel = self.sel.min(self.tiles.len().saturating_sub(1));
                    self.dirty = false;
                }
                self.state = Some(Ok(st));
            }
            Err(e) => {
                self.state = Some(Err(e));
            }
        }
    }

    /// The configuration that is currently active (used as the revert target).
    pub fn prev_config(&self) -> Option<Vec<GLogicalIn>> {
        let st = self.state.as_ref()?.as_ref().ok()?;
        Some(config_from_state(st))
    }

    pub fn build_config(&self) -> Result<Vec<GLogicalIn>, String> {
        let st = self
            .state
            .as_ref()
            .ok_or("GNOME display state not loaded yet")?
            .as_ref()
            .map_err(|e| e.clone())?;
        let mode_id = |connector: &str, idx: usize| -> Result<String, String> {
            let m = st
                .find_monitor(connector)
                .ok_or_else(|| format!("monitor {connector} disappeared"))?;
            m.modes
                .get(idx)
                .map(|m| m.id.clone())
                .ok_or_else(|| format!("mode #{idx} not found for {connector}"))
        };
        self.tiles
            .iter()
            .map(|t| {
                if t.monitors.is_empty() {
                    return Err("empty logical monitor".into());
                }
                let monitors = t
                    .monitors
                    .iter()
                    .map(|m| {
                        Ok((
                            m.connector.clone(),
                            mode_id(&m.connector, m.mode_idx)?,
                            HashMap::new(),
                        ))
                    })
                    .collect::<Result<Vec<GMonitorIn>, String>>()?;
                Ok(GLogicalIn {
                    x: t.x,
                    y: t.y,
                    scale: t.scale,
                    transform: t.transform,
                    primary: t.primary,
                    monitors,
                })
            })
            .collect()
    }

    fn sync_mirror(&mut self) {
        self.mirror = self.tiles.len() == 1 && self.tiles[0].monitors.len() == 2;
    }

    fn can_mirror(&self) -> bool {
        self.tiles.iter().map(|t| t.monitors.len()).sum::<usize>() == 2
    }

    fn set_mirror(&mut self, mirror: bool) {
        if !self.can_mirror() || mirror == self.mirror {
            return;
        }
        if mirror {
            let other = self.tiles.remove(1);
            self.tiles[0].monitors.extend(other.monitors);
            self.tiles[0].primary |= other.primary;
        } else {
            let Some(Ok(st)) = self.state.as_ref() else {
                return;
            };
            let mut other = self.tiles[0].clone();
            let Some(monitor) = self.tiles[0].monitors.pop() else {
                return;
            };
            other.monitors = vec![monitor];
            other.x += tile_dims(st, &self.tiles[0]).0;
            other.primary = false;
            self.tiles.push(other);
        }
        self.sync_mirror();
        self.sel = 0;
        self.dirty = true;
    }

    pub fn overlap_error(&self) -> Option<String> {
        let Some(st) = self.state.as_ref().and_then(|r| r.as_ref().ok()) else {
            return None;
        };
        for i in 0..self.tiles.len() {
            for j in i + 1..self.tiles.len() {
                if positive_overlap(tile_rect(st, &self.tiles[i]), tile_rect(st, &self.tiles[j])) {
                    return Some(format!(
                        "Monitors overlap: {} and {}. Drag them apart on the map.",
                        self.tiles[i]
                            .monitors
                            .first()
                            .map(|m| m.connector.clone())
                            .unwrap_or_default(),
                        self.tiles[j]
                            .monitors
                            .first()
                            .map(|m| m.connector.clone())
                            .unwrap_or_default(),
                    ));
                }
            }
        }
        None
    }

    pub fn tick(&mut self) -> Option<TickAction> {
        if let Some(p) = &self.pending {
            let now = Instant::now();
            if let Some(r) = self
                .recovery
                .as_ref()
                .filter(|r| r.pending_until == p.until)
            {
                if r.failures > 0 {
                    return r
                        .next_retry
                        .filter(|retry| now >= *retry)
                        .map(|_| TickAction::RevertExpired);
                }
            }
            if now >= p.until {
                return Some(TickAction::RevertExpired);
            }
        }
        None
    }
}

pub enum TickAction {
    RevertExpired,
}

fn build_tiles(st: &GState) -> Vec<Tile> {
    let mut tiles = Vec::new();
    for l in &st.logicals {
        let mut monitors = Vec::new();
        for m in &l.monitors {
            let connector = &m[0];
            let mode_idx = st
                .find_monitor(connector)
                .map(|gm| {
                    gm.modes
                        .iter()
                        .position(|mode| mode.is_current)
                        .or_else(|| gm.modes.iter().position(|mode| mode.is_preferred))
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            monitors.push(MonRef {
                connector: connector.clone(),
                mode_idx,
            });
        }
        tiles.push(Tile {
            monitors,
            x: l.x,
            y: l.y,
            scale: l.scale,
            transform: l.transform,
            primary: l.primary,
        });
    }
    tiles
}

pub fn config_from_state(st: &GState) -> Vec<GLogicalIn> {
    st.logicals
        .iter()
        .map(|l| GLogicalIn {
            x: l.x,
            y: l.y,
            scale: l.scale,
            transform: l.transform,
            primary: l.primary,
            monitors: l
                .monitors
                .iter()
                .filter_map(|m| {
                    let gm = st.find_monitor(&m[0])?;
                    let mode = gm.modes.iter().find(|mode| mode.is_current)?;
                    Some((
                        m[0].clone(),
                        mode.id.clone(),
                        HashMap::<String, zbus::zvariant::Value<'static>>::new(),
                    ) as GMonitorIn)
                })
                .collect(),
        })
        .collect()
}

// ---- geometry helpers ----

fn tile_dims(st: &GState, t: &Tile) -> (i32, i32) {
    let Some(m) = t.monitors.first() else {
        return (0, 0);
    };
    let Some(gm) = st.find_monitor(&m.connector) else {
        return (0, 0);
    };
    let Some(mode) = gm.modes.get(m.mode_idx) else {
        return (0, 0);
    };
    let scale = if st.layout_mode == 1 && t.scale.is_finite() && t.scale > 0.0 {
        t.scale
    } else {
        1.0
    };
    let (w, h) = (
        (mode.w as f64 / scale).round() as i32,
        (mode.h as f64 / scale).round() as i32,
    );
    match t.transform {
        1 | 3 | 5 | 7 => (h, w),
        _ => (w, h),
    }
}

fn tile_rect(st: &GState, t: &Tile) -> egui::Rect {
    let (w, h) = tile_dims(st, t);
    egui::Rect::from_min_size(
        egui::pos2(t.x as f32, t.y as f32),
        egui::vec2(w as f32, h as f32),
    )
}

fn positive_overlap(a: egui::Rect, b: egui::Rect) -> bool {
    const ROUNDING_EPSILON: f32 = 1.0;
    a.min.x.max(b.min.x) + ROUNDING_EPSILON < a.max.x.min(b.max.x)
        && a.min.y.max(b.min.y) + ROUNDING_EPSILON < a.max.y.min(b.max.y)
}

fn minimap_point(canvas: egui::Rect, bounds: egui::Rect, scale: f32, p: egui::Pos2) -> egui::Pos2 {
    canvas.center() + (p - bounds.center()) * scale
}

// ---- UI ----

thread_local! {
    static APPLY_REQUEST: std::cell::RefCell<Vec<ApplyRequest>> = const { std::cell::RefCell::new(Vec::new()) };
    static CONFIRM_REQUEST: std::cell::RefCell<Vec<bool>> = const { std::cell::RefCell::new(Vec::new()) };
    static ENABLE_REQUEST: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    static REMOVE_REQUEST: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub enum ApplyRequest {
    Temporary,
    Persistent,
}

/// Drain queued UI actions; the app shell executes them against mutter.
pub fn take_actions() -> (Vec<ApplyRequest>, Vec<bool>, Vec<String>, Vec<usize>) {
    let a = APPLY_REQUEST.with(|c| std::mem::take(&mut *c.borrow_mut()));
    let c = CONFIRM_REQUEST.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    let e = ENABLE_REQUEST.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    let r = REMOVE_REQUEST.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
    (a, c, e, r)
}

pub fn show(ui: &mut egui::Ui, tab: &mut LayoutTab, refresh_clicked: &mut bool) {
    let Some(state_res) = tab.state.as_ref() else {
        ui.spinner();
        ui.label("Loading GNOME display state…");
        return;
    };
    let st = match state_res {
        Ok(st) => st.clone(),
        Err(e) => {
            ui.colored_label(
                Color32::RED,
                format!("Failed to read GNOME display config: {e}"),
            );
            return;
        }
    };

    // Toolbar
    ui.horizontal(|ui| {
        if ui.button("↻ Refresh").clicked() {
            *refresh_clicked = true;
        }
        ui.separator();
        if tab.can_mirror() {
            let mut mirror = tab.mirror;
            if ui
                .checkbox(&mut mirror, "Mirror the two monitors")
                .changed()
            {
                tab.set_mirror(mirror);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let overlap = tab.overlap_error();
            let save = ui.add_enabled(
                overlap.is_none() && tab.pending.is_none(),
                egui::Button::new("Apply & save"),
            );
            if save.clicked() {
                APPLY_REQUEST.with(|cell| cell.borrow_mut().push(ApplyRequest::Persistent));
            }
            save.on_disabled_hover_text(overlap.as_deref().unwrap_or(""));
            let tmp = ui.add_enabled(
                overlap.is_none() && tab.pending.is_none(),
                egui::Button::new("Apply (revert in 15s)"),
            );
            if tmp.clicked() {
                APPLY_REQUEST.with(|cell| cell.borrow_mut().push(ApplyRequest::Temporary));
            }
            tmp.on_disabled_hover_text(overlap.as_deref().unwrap_or(""));
        });
    });
    if let Some(err) = tab.overlap_error() {
        ui.colored_label(Color32::from_rgb(255, 170, 60), err);
    }

    // Keep/revert banner
    if let Some(p) = &tab.pending {
        let left = p.until.saturating_duration_since(Instant::now()).as_secs();
        ui.add_space(4.0);
        egui::Frame::default()
            .fill(Color32::from_rgb(66, 54, 22))
            .inner_margin(8.0)
            .corner_radius(6.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let recovery = tab.recovery.as_ref().filter(|r| r.pending_until == p.until);
                    let message = match recovery {
                        Some(r) if r.failures > 0 && r.next_retry.is_none() => {
                            "Automatic revert failed; use Revert to retry.".to_string()
                        }
                        Some(r) if r.failures > 0 => "Revert failed; retrying shortly…".to_string(),
                        _ => format!("Keep this display layout? Reverting in {left}s…"),
                    };
                    ui.label(egui::RichText::new(message));
                    if ui.button("Keep changes").clicked() {
                        CONFIRM_REQUEST.with(|cell| cell.borrow_mut().push(true));
                    }
                    if ui.button("Revert").clicked() {
                        CONFIRM_REQUEST.with(|cell| cell.borrow_mut().push(false));
                    }
                });
            });
    }

    // Mini-map
    ui.add_space(8.0);
    draw_minimap(ui, tab, &st);

    // Per-tile editors
    ui.add_space(8.0);
    if tab.mirror {
        ui.heading("Mirrored pair");
        ui.label("Both monitors show the same content; scale is shared.");
        edit_card(ui, tab, &st, 0, true);
    } else {
        for i in 0..tab.tiles.len() {
            edit_card(ui, tab, &st, i, false);
        }
    }

    // Connected but switched-off monitors
    let disabled: Vec<&crate::mutter::GMonitor> = st
        .monitors
        .iter()
        .filter(|m| {
            !tab.tiles
                .iter()
                .any(|t| t.monitors.iter().any(|r| r.connector == m.connector))
        })
        .collect();
    if !disabled.is_empty() {
        ui.add_space(4.0);
        ui.separator();
        ui.heading("Off");
        for m in disabled {
            ui.horizontal(|ui| {
                ui.label(format!("{} — {}", m.connector, m.display_name));
                if ui.small_button("Turn on").clicked() {
                    ENABLE_REQUEST.with(|cell| cell.borrow_mut().push(m.connector.clone()));
                }
            });
        }
    }

    if let Some(err) = &tab.last_err {
        ui.add_space(4.0);
        ui.colored_label(Color32::RED, err);
    }
}

fn edit_card(ui: &mut egui::Ui, tab: &mut LayoutTab, st: &GState, i: usize, mirrored: bool) {
    let Some(orig) = tab.tiles.get(i).cloned() else {
        return;
    };
    let mut t = orig.clone();
    let selected = tab.sel == i;

    egui::Frame::default()
        .fill(if selected {
            Color32::from_rgb(44, 50, 64)
        } else {
            Color32::from_rgb(32, 36, 46)
        })
        .inner_margin(10.0)
        .corner_radius(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title = if mirrored && t.monitors.len() == 1 {
                let other = tab
                    .tiles
                    .get(1)
                    .and_then(|ot| ot.monitors.first())
                    .map(|m| m.connector.clone())
                    .unwrap_or_default();
                format!("{} + {} (mirrored)", t.monitors[0].connector, other)
            } else {
                t.monitors
                    .first()
                    .and_then(|m| st.find_monitor(&m.connector))
                    .map(|g| g.display_name.clone())
                    .unwrap_or_else(|| t.monitors[0].connector.clone())
            };
            ui.horizontal(|ui| {
                ui.heading(title.clone());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !selected && ui.small_button("Select").clicked() {
                        tab.sel = i;
                    }
                    if tab.tiles.len() > 1 && ui.small_button("Turn off").clicked() {
                        REMOVE_REQUEST.with(|cell| cell.borrow_mut().push(i));
                    }
                });
            });

            // Resolution for each monitor in this logical monitor.
            for ci in 0..t.monitors.len() {
                let Some(gm) = st.find_monitor(&t.monitors[ci].connector) else {
                    continue;
                };
                let mut mode_idx = t.monitors[ci].mode_idx;
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let label = if ci == 0 {
                        "Resolution".to_string()
                    } else {
                        format!("Resolution ({})", t.monitors[ci].connector)
                    };
                    ui.add_sized([110.0, 20.0], egui::Label::new(label));
                    egui::ComboBox::from_id_salt(("mode", i, ci))
                        .selected_text(
                            gm.modes
                                .get(mode_idx)
                                .map(|m| format!("{}×{} · {:.3} Hz", m.w, m.h, m.refresh))
                                .unwrap_or_default(),
                        )
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for (idx, mode) in gm.modes.iter().enumerate() {
                                let txt = format!(
                                    "{}×{} · {:.3} Hz{}",
                                    mode.w,
                                    mode.h,
                                    mode.refresh,
                                    if mode.is_preferred {
                                        "  (preferred)"
                                    } else {
                                        ""
                                    }
                                );
                                ui.selectable_value(&mut mode_idx, idx, txt);
                            }
                        });
                    t.monitors[ci].mode_idx = mode_idx;
                });
            }

            // Scale
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_sized([110.0, 20.0], egui::Label::new("Scale"));
                let scales = t
                    .monitors
                    .first()
                    .and_then(|m| st.find_monitor(&m.connector))
                    .and_then(|gm| gm.modes.get(t.monitors[0].mode_idx))
                    .map(|mode| mode.scales.clone())
                    .unwrap_or_else(|| vec![1.0]);
                let mut new_scale = t.scale;
                egui::ComboBox::from_id_salt(("scale", i))
                    .selected_text(format!("{}%", (t.scale * 100.0).round() as i32))
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for s in scales {
                            ui.selectable_value(
                                &mut new_scale,
                                s,
                                format!("{}%", (s * 100.0).round() as i32),
                            );
                        }
                    });
                t.scale = new_scale;
            });

            // Rotation
            ui.horizontal(|ui| {
                ui.add_sized([110.0, 20.0], egui::Label::new("Rotation"));
                let mut new_tf = t.transform;
                egui::ComboBox::from_id_salt(("tf", i))
                    .selected_text(transform_text(t.transform))
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for (v, txt) in [
                            (0u32, "Normal"),
                            (1, "Rotate 90°"),
                            (2, "Rotate 180°"),
                            (3, "Rotate 270°"),
                        ] {
                            ui.selectable_value(&mut new_tf, v, txt);
                        }
                    });
                t.transform = new_tf;
            });

            // Position + primary
            ui.horizontal(|ui| {
                ui.add_sized([110.0, 20.0], egui::Label::new("Position"));
                let mut x = t.x;
                let mut y = t.y;
                if mirrored {
                    ui.label("(shared)");
                } else {
                    ui.add(egui::DragValue::new(&mut x).prefix("x: ").speed(4.0));
                    ui.add(egui::DragValue::new(&mut y).prefix("y: ").speed(4.0));
                }
                t.x = x;
                t.y = y;
                let mut prim = t.primary;
                if ui.checkbox(&mut prim, "Primary").changed() && prim {
                    for (j, ot) in tab.tiles.iter_mut().enumerate() {
                        if j != i {
                            ot.primary = false;
                        }
                    }
                }
                t.primary = prim;
            });
        });

    if t != orig {
        if t.scale != orig.scale && mirrored {
            if let Some(other) = tab.tiles.get_mut(1) {
                other.scale = t.scale;
            }
        }
        tab.tiles[i] = t;
        tab.dirty = true;
        tab.sel = i;
    }
}

fn transform_text(v: u32) -> &'static str {
    match v {
        0 => "Normal",
        1 => "Rotate 90°",
        2 => "Rotate 180°",
        3 => "Rotate 270°",
        4 => "Flipped",
        5 => "Flipped 90°",
        6 => "Flipped 180°",
        7 => "Flipped 270°",
        _ => "Normal",
    }
}

fn draw_minimap(ui: &mut egui::Ui, tab: &mut LayoutTab, st: &GState) {
    let mut min = egui::pos2(0.0, 0.0);
    let mut max = egui::pos2(1.0, 1.0);
    for t in &tab.tiles {
        let r = tile_rect(st, t);
        min.x = min.x.min(r.min.x);
        min.y = min.y.min(r.min.y);
        max.x = max.x.max(r.max.x);
        max.y = max.y.max(r.max.y);
    }
    let bounds = egui::Rect::from_min_max(min, max);

    let avail_w = ui.available_width().min(640.0);
    let canvas_h = 170.0;
    let (canvas, resp) =
        ui.allocate_exact_size(egui::vec2(avail_w, canvas_h), Sense::click_and_drag());
    ui.painter()
        .rect_filled(canvas, 6.0, Color32::from_rgb(22, 25, 32));

    if tab.tiles.is_empty() {
        ui.painter().text(
            canvas.center(),
            egui::Align2::CENTER_CENTER,
            "No monitors enabled",
            egui::FontId::proportional(14.0),
            Color32::GRAY,
        );
        return;
    }

    let scale = (canvas.width() / bounds.width()).min(canvas.height() / bounds.height()) * 0.85;
    let content = egui::vec2(bounds.width() * scale, bounds.height() * scale);
    let origin = canvas.center() - content * 0.5 - (bounds.min.to_vec2() * scale);

    let to_screen = |p: egui::Pos2| origin + (p - bounds.min) * scale;

    let n = if tab.mirror && tab.tiles.len() == 2 {
        1
    } else {
        tab.tiles.len()
    };
    let mut rects = Vec::new();
    for i in 0..n {
        let r = tile_rect(st, &tab.tiles[i]);
        let a = to_screen(r.min);
        let b = to_screen(r.max);
        rects.push((i, egui::Rect::from_min_max(a, b)));
    }

    for (i, r) in &rects {
        let selected = tab.sel == *i;
        let fill = if selected {
            Color32::from_rgb(70, 110, 190)
        } else {
            Color32::from_rgb(52, 60, 76)
        };
        ui.painter().rect_filled(*r, 5.0, fill);
        ui.painter().rect_stroke(
            *r,
            5.0,
            Stroke::new(1.5_f32, Color32::from_rgb(140, 160, 200)),
            egui::StrokeKind::Middle,
        );
        let labels: Vec<String> = tab.tiles[*i]
            .monitors
            .iter()
            .map(|m| m.connector.clone())
            .collect();
        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            labels.join(" | "),
            egui::FontId::proportional(12.0),
            Color32::WHITE,
        );
        if tab.tiles[*i].primary {
            ui.painter().text(
                egui::pos2(r.min.x + 8.0, r.min.y + 10.0),
                egui::Align2::LEFT_TOP,
                "★ primary",
                egui::FontId::proportional(11.0),
                Color32::from_rgb(255, 220, 120),
            );
        }
    }

    if let Some(pos) = resp.interact_pointer_pos() {
        if resp.clicked() {
            if let Some((i, _)) = rects.iter().rev().find(|(_, r)| r.contains(pos)) {
                tab.sel = *i;
            }
        }
        if resp.dragged() {
            let d = resp.drag_delta() / scale.max(0.0001);
            let sel = tab.sel.min(tab.tiles.len() - 1);
            if !(tab.mirror && sel == 1) {
                let t = &mut tab.tiles[sel];
                t.x += d.x.round() as i32;
                t.y += d.y.round() as i32;
                tab.dirty = true;
            }
        }
        if resp.drag_stopped() {
            snap(tab, st, scale);
        }
    }
}

/// Snap the selected tile's edges to other tiles' edges and to the origin.
fn snap(tab: &mut LayoutTab, st: &GState, scale: f32) {
    let sel = tab.sel.min(tab.tiles.len().saturating_sub(1));
    let Some(t) = tab.tiles.get(sel) else { return };
    let r = tile_rect(st, t);
    let th = 14.0 / scale.max(0.05); // snap threshold in layout pixels

    let mut targets_x = vec![0.0];
    let mut targets_y = vec![0.0];
    for (j, ot) in tab.tiles.iter().enumerate() {
        if j == sel {
            continue;
        }
        let or = tile_rect(st, ot);
        targets_x.push(or.min.x);
        targets_x.push(or.max.x);
        targets_y.push(or.min.y);
        targets_y.push(or.max.y);
    }

    let mut best_x: Option<(i32, f32)> = None;
    for e in [r.min.x, r.max.x] {
        for t in &targets_x {
            let d = (e - t).abs();
            if d < th && best_x.map(|(_, s)| d < s).unwrap_or(true) {
                best_x = Some(((*t - e) as i32, d));
            }
        }
    }
    let mut best_y: Option<(i32, f32)> = None;
    for e in [r.min.y, r.max.y] {
        for t in &targets_y {
            let d = (e - t).abs();
            if d < th && best_y.map(|(_, s)| d < s).unwrap_or(true) {
                best_y = Some(((*t - e) as i32, d));
            }
        }
    }
    if let Some((dx, _)) = best_x {
        tab.tiles[sel].x += dx;
        tab.dirty = true;
    }
    if let Some((dy, _)) = best_y {
        tab.tiles[sel].y += dy;
        tab.dirty = true;
    }
}

/// Turn a currently-disabled monitor on, placed to the right of everything else.
pub fn enable_monitor(tab: &mut LayoutTab, connector: &str) {
    let Some(Ok(st)) = tab.state.as_ref() else {
        return;
    };
    let Some(gm) = st.find_monitor(connector) else {
        return;
    };
    let mode_idx = gm
        .modes
        .iter()
        .position(|m| m.is_preferred)
        .or_else(|| gm.modes.iter().position(|m| m.is_current))
        .unwrap_or(0);
    let scale = gm
        .modes
        .get(mode_idx)
        .and_then(|m| m.scales.first().copied())
        .unwrap_or(1.0);
    let mut max_x = 0;
    for t in &tab.tiles {
        let r = tile_rect(st, t);
        max_x = max_x.max(r.max.x as i32);
    }
    let primary = tab.tiles.is_empty();
    tab.tiles.push(Tile {
        monitors: vec![MonRef {
            connector: connector.to_string(),
            mode_idx,
        }],
        x: if tab.tiles.is_empty() { 0 } else { max_x },
        y: 0,
        scale,
        transform: 0,
        primary,
    });
    tab.sel = tab.tiles.len() - 1;
    tab.dirty = true;
}

pub fn remove_tile(tab: &mut LayoutTab, i: usize) {
    if i < tab.tiles.len() {
        tab.tiles.remove(i);
        tab.sel = tab.sel.min(tab.tiles.len().saturating_sub(1));
        tab.dirty = true;
        if tab.tiles.len() < 2 {
            tab.mirror = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touching_tiles_are_not_overlapping() {
        let a = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1920.0, 1080.0));
        let b = egui::Rect::from_min_max(egui::pos2(1920.0, 0.0), egui::pos2(3840.0, 1080.0));
        assert!(!positive_overlap(a, b));
    }

    #[test]
    fn subpixel_boundary_rounding_is_not_overlapping() {
        let a = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1920.4, 1080.0));
        let b = egui::Rect::from_min_max(egui::pos2(1920.0, 0.0), egui::pos2(3840.0, 1080.0));
        assert!(!positive_overlap(a, b));
    }

    #[test]
    fn meaningful_overlap_is_rejected() {
        let a = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1920.0, 1080.0));
        let b = egui::Rect::from_min_max(egui::pos2(1918.0, 0.0), egui::pos2(3840.0, 1080.0));
        assert!(positive_overlap(a, b));
    }
}

/// Execute queued apply/confirm/enable/remove requests against mutter.
pub fn process_actions(
    tab: &mut LayoutTab,
    (applies, confirms, enables, removes): (Vec<ApplyRequest>, Vec<bool>, Vec<String>, Vec<usize>),
    status: &mut Option<(String, Instant)>,
) {
    for c in enables {
        enable_monitor(tab, &c);
    }
    for i in removes {
        remove_tile(tab, i);
    }
    for req in applies {
        do_apply(tab, req, status);
    }
    for keep in confirms {
        if keep {
            keep_changes(tab, status);
        } else {
            revert(tab, status);
        }
    }
}

pub fn do_apply(tab: &mut LayoutTab, req: ApplyRequest, status: &mut Option<(String, Instant)>) {
    let method = match req {
        ApplyRequest::Temporary => METHOD_TEMPORARY,
        ApplyRequest::Persistent => METHOD_PERSISTENT,
    };
    let Ok(cfg) = tab.build_config() else {
        tab.last_err = Some("Display state not loaded".into());
        return;
    };
    let prev = tab.prev_config().unwrap_or_default();
    let layout_mode = tab
        .state
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|s| s.layout_mode)
        .unwrap_or(1);
    match mutter::verify(cfg.clone(), layout_mode) {
        Ok(()) => {}
        Err(e) => {
            // Verify (method 0) fails without touching the screen when the
            // configuration is invalid (bad mode, overlap, unsupported scale…).
            tab.last_err = Some(e);
            return;
        }
    }
    match apply_cfg(method, &cfg, layout_mode) {
        Ok(()) => {
            tab.last_err = None;
            if method == METHOD_TEMPORARY {
                tab.pending = Some(PendingApply {
                    until: Instant::now() + Duration::from_secs(REVERT_SECS),
                    new_cfg: cfg.clone(),
                    prev_cfg: prev,
                    layout_mode,
                });
            } else {
                tab.pending = None;
            }
            *status = Some(("Display configuration applied".into(), Instant::now()));
        }
        Err(e) => tab.last_err = Some(e),
    }
}

fn keep_changes(tab: &mut LayoutTab, status: &mut Option<(String, Instant)>) {
    if let Some(p) = tab.pending.take() {
        match apply_cfg(METHOD_PERSISTENT, &p.new_cfg, p.layout_mode) {
            Ok(()) => *status = Some(("Layout saved permanently".into(), Instant::now())),
            Err(e) => tab.last_err = Some(e),
        }
    }
}

pub fn revert(tab: &mut LayoutTab, status: &mut Option<(String, Instant)>) {
    if let Some(p) = tab.pending.take() {
        match apply_cfg(METHOD_PERSISTENT, &p.prev_cfg, p.layout_mode) {
            Ok(()) => *status = Some(("Layout reverted".into(), Instant::now())),
            Err(e) => tab.last_err = Some(e),
        }
    }
}

fn apply_cfg(method: u32, cfg: &[GLogicalIn], layout_mode: u32) -> Result<(), String> {
    let fresh = mutter::get_state()?;
    mutter::apply(
        fresh.serial,
        method,
        cfg.to_vec(),
        layout_mode,
        fresh.supports_changing_layout_mode,
    )
}
