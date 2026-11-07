//! GNOME display configuration via mutter's org.gnome.Mutter.DisplayConfig DBus API.
//!
//! GetCurrentState(out u serial,
//!                 out a((ssss)a(siiddada{sv})a{sv}) monitors,
//!                 out a(iiduba(ssss)a{sv}) logical_monitors,
//!                 out a{sv} properties);
//! ApplyMonitorsConfig(in u serial, in u method,
//!                     in a(iiduba(ssa{sv})) logical_monitors,
//!                     in a{sv} properties);
//!
//! method: 0 = verify, 1 = temporary, 2 = persistent.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Type, Value};

pub const METHOD_VERIFY: u32 = 0;
pub const METHOD_TEMPORARY: u32 = 1;
pub const METHOD_PERSISTENT: u32 = 2;

pub const DEST: &str = "org.gnome.Mutter.DisplayConfig";
pub const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
pub const IFACE: &str = "org.gnome.Mutter.DisplayConfig";

#[derive(Clone, Debug)]
pub struct GMode {
    pub id: String,
    pub w: u32,
    pub h: u32,
    pub refresh: f64,
    pub scales: Vec<f64>,
    pub is_current: bool,
    pub is_preferred: bool,
}

#[derive(Clone, Debug)]
pub struct GMonitor {
    pub connector: String,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub display_name: String,
    pub is_builtin: bool,
    pub modes: Vec<GMode>,
}

impl GMonitor {
    /// Same identity scheme as the DDC side: "MFR|MODEL|SERIAL".
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.vendor, self.product, self.serial)
    }
}

#[derive(Clone, Debug)]
pub struct GLogical {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    /// (connector, vendor, product, serial) per monitor in this logical monitor.
    pub monitors: Vec<[String; 4]>,
}

#[derive(Clone, Debug)]
pub struct GState {
    pub serial: u32,
    pub monitors: Vec<GMonitor>,
    pub logicals: Vec<GLogical>,
    pub layout_mode: u32,
    pub supports_changing_layout_mode: bool,
}

impl GState {
    pub fn find_monitor(&self, connector: &str) -> Option<&GMonitor> {
        self.monitors.iter().find(|m| m.connector == connector)
    }
}

/// One monitor entry in an ApplyMonitorsConfig logical monitor: (connector, mode_id, props).
pub type GMonitorIn = (String, String, HashMap<String, Value<'static>>);

/// One logical monitor in ApplyMonitorsConfig (no logical-monitor properties field).
#[derive(Clone, Debug, Serialize, Type)]
pub struct GLogicalIn {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub monitors: Vec<GMonitorIn>,
}

// ---- typed reply of GetCurrentState ----

#[derive(Deserialize, Type)]
#[zvariant(signature = "(ssss)")]
struct MonIds(String, String, String, String);

#[derive(Deserialize, Type)]
#[zvariant(signature = "(siiddada{sv})")]
struct ModeEntry {
    id: String,
    w: i32,
    h: i32,
    refresh: f64,
    #[allow(dead_code)]
    preferred_scale: f64,
    scales: Vec<f64>,
    props: HashMap<String, OwnedValue>,
}

#[derive(Deserialize, Type)]
#[zvariant(signature = "((ssss)a(siiddada{sv})a{sv})")]
struct MonitorEntry {
    ids: MonIds,
    modes: Vec<ModeEntry>,
    props: HashMap<String, OwnedValue>,
}

#[derive(Deserialize, Type)]
#[zvariant(signature = "(iiduba(ssss)a{sv})")]
struct LogicalEntry {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    monitors: Vec<MonIds>,
    #[allow(dead_code)]
    props: HashMap<String, OwnedValue>,
}

#[derive(Deserialize, Type)]
#[zvariant(signature = "(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})")]
struct GetStateReply {
    serial: u32,
    monitors: Vec<MonitorEntry>,
    logicals: Vec<LogicalEntry>,
    properties: HashMap<String, OwnedValue>,
}

pub fn connection() -> Result<Connection, String> {
    Connection::session().map_err(|e| format!("connect to session bus: {e:#}"))
}

pub fn get_state() -> Result<GState, String> {
    let conn = connection()?;
    let msg = conn
        .call_method(Some(DEST), PATH, Some(IFACE), "GetCurrentState", &())
        .map_err(|e| format!("GetCurrentState: {e:#}"))?;
    let body = msg.body();
    let reply: GetStateReply = body
        .deserialize()
        .map_err(|e| format!("parse GetCurrentState reply: {e:#}"))?;

    let monitors = reply
        .monitors
        .into_iter()
        .map(|m| {
            let (connector, vendor, product, serial) = (
                m.ids.0.clone(),
                m.ids.1.clone(),
                m.ids.2.clone(),
                m.ids.3.clone(),
            );
            GMonitor {
                display_name: as_str(m.props.get("display-name")),
                is_builtin: as_bool(m.props.get("is-builtin")),
                connector,
                vendor,
                product,
                serial,
                modes: m
                    .modes
                    .into_iter()
                    .map(|mode| GMode {
                        id: mode.id,
                        w: mode.w.max(0) as u32,
                        h: mode.h.max(0) as u32,
                        refresh: mode.refresh,
                        scales: mode.scales,
                        is_current: as_bool(mode.props.get("is-current")),
                        is_preferred: as_bool(mode.props.get("is-preferred")),
                    })
                    .collect(),
            }
        })
        .collect();

    let logicals = reply
        .logicals
        .into_iter()
        .map(|l| GLogical {
            x: l.x,
            y: l.y,
            scale: l.scale,
            transform: l.transform,
            primary: l.primary,
            monitors: l
                .monitors
                .into_iter()
                .map(|m| [m.0, m.1, m.2, m.3])
                .collect(),
        })
        .collect();

    let layout_mode = as_u32(reply.properties.get("layout-mode")).unwrap_or(1);
    let supports_changing_layout_mode =
        as_bool(reply.properties.get("supports-changing-layout-mode"));
    Ok(GState {
        serial: reply.serial,
        monitors,
        logicals,
        layout_mode,
        supports_changing_layout_mode,
    })
}

pub fn apply(
    serial: u32,
    method: u32,
    logicals: Vec<GLogicalIn>,
    layout_mode: u32,
    supports_changing_layout_mode: bool,
) -> Result<(), String> {
    let conn = connection()?;
    let mut props: HashMap<String, Value<'static>> = HashMap::new();
    if supports_changing_layout_mode {
        props.insert("layout-mode".into(), Value::U32(layout_mode));
    }
    conn.call_method(
        Some(DEST),
        PATH,
        Some(IFACE),
        "ApplyMonitorsConfig",
        &(serial, method, logicals, props),
    )
    .map_err(|e| format!("ApplyMonitorsConfig: {e:#}"))?;
    Ok(())
}

/// Verify a configuration without applying it (method 0): fails without touching
/// the screen when the configuration is invalid.
pub fn verify(logicals: Vec<GLogicalIn>, layout_mode: u32) -> Result<(), String> {
    let fresh = get_state()?;
    apply(
        fresh.serial,
        METHOD_VERIFY,
        logicals,
        layout_mode,
        fresh.supports_changing_layout_mode,
    )
}

/// Built-in panel brightness via gnome-settings-daemon (0..100).
pub fn get_builtin_brightness() -> Result<i32, String> {
    let conn = connection()?;
    let msg = conn
        .call_method(
            Some("org.gnome.SettingsDaemon.Power"),
            "/org/gnome/SettingsDaemon/Power",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.gnome.SettingsDaemon.Power.Screen", "Brightness"),
        )
        .map_err(|e| format!("gsd Get Brightness: {e:#}"))?;
    let body = msg.body();
    let v: Value = body.deserialize().map_err(|e| e.to_string())?;
    match as_i32_v(Some(&v)) {
        Some(percent) if percent >= 0 => Ok(percent),
        _ => Err("GNOME's power daemon reports no controllable backlight for this panel".into()),
    }
}

pub fn set_builtin_brightness(percent: i32) -> Result<(), String> {
    let conn = connection()?;
    conn.call_method(
        Some("org.gnome.SettingsDaemon.Power"),
        "/org/gnome/SettingsDaemon/Power",
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(
            "org.gnome.SettingsDaemon.Power.Screen",
            "Brightness",
            Value::Value(Box::new(Value::I32(percent.clamp(0, 100)))),
        ),
    )
    .map_err(|e| format!("gsd Set Brightness: {e:#}"))?;
    Ok(())
}

// ---- OwnedValue -> helpers (variant-unwrapping) ----

fn conv(v: Option<&OwnedValue>) -> Option<Value<'static>> {
    v.map(|o| o.clone().into())
}

fn as_str(v: Option<&OwnedValue>) -> String {
    as_str_v(conv(v).as_ref())
}

fn as_bool(v: Option<&OwnedValue>) -> bool {
    as_bool_v(conv(v).as_ref())
}

fn as_u32(v: Option<&OwnedValue>) -> Option<u32> {
    as_u32_v(conv(v).as_ref())
}

fn as_str_v<'v>(v: Option<&'v Value<'v>>) -> String {
    match v {
        Some(Value::Str(s)) => s.as_str().to_string(),
        Some(Value::Value(inner)) => as_str_v(Some(inner)),
        _ => String::new(),
    }
}

fn as_bool_v<'v>(v: Option<&'v Value<'v>>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Value(inner)) => as_bool_v(Some(inner)),
        _ => false,
    }
}

fn as_u32_v<'v>(v: Option<&'v Value<'v>>) -> Option<u32> {
    match v {
        Some(Value::U32(u)) => Some(*u),
        Some(Value::I32(i)) if *i >= 0 => Some(*i as u32),
        Some(Value::Value(inner)) => as_u32_v(Some(inner)),
        _ => None,
    }
}

fn as_i32_v<'v>(v: Option<&'v Value<'v>>) -> Option<i32> {
    match v {
        Some(Value::I32(i)) => Some(*i),
        Some(Value::U32(u)) => Some(*u as i32),
        Some(Value::Value(inner)) => as_i32_v(Some(inner)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_monitors_config_has_mutter_signature() {
        let logicals = vec![GLogicalIn {
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: true,
            monitors: vec![("HDMI-1".into(), "1920x1080@60".into(), HashMap::new())],
        }];
        let props = HashMap::from([("layout-mode".to_string(), Value::U32(1))]);
        let message = zbus::Message::method_call(PATH, "ApplyMonitorsConfig")
            .unwrap()
            .interface(IFACE)
            .unwrap()
            .build(&(1_u32, METHOD_VERIFY, logicals, props))
            .unwrap();

        assert_eq!(
            message.body().signature().to_string(),
            "(uua(iiduba(ssa{sv}))a{sv})"
        );
    }
}
