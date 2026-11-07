//! DDC/CI backend: enumerate monitors over i2c-dev and talk VCP to them.
//! Each monitor gets a worker thread so the UI never blocks on the (slow) bus.
//!
//! We talk i2c directly (ddc-i2c) instead of going through ddcutil, and we
//! enumerate /dev/i2c-* ourselves so no libudev/system package is needed.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use eframe::egui;
use std::thread;
use std::time::Duration;

use ddc::{Ddc, Edid};
use ddc_hi::{Backend, DisplayInfo, FeatureCode, VcpValue};
use ddc_i2c::I2cDeviceDdc;

pub const BRIGHTNESS: FeatureCode = 0x10;
pub const CONTRAST: FeatureCode = 0x12;
pub const COLOR_PRESET: FeatureCode = 0x14;
pub const INPUT_SOURCE: FeatureCode = 0x60;
pub const AUDIO_VOLUME: FeatureCode = 0x62;
pub const POWER_MODE: FeatureCode = 0xd6;

/// Vendor VCP carrying the monitor's built-in picture modes — the profiles
/// behind the display's own Mode button (and Acer's Windows tools).
pub const ACER_MODE: FeatureCode = 0xe2;

/// Picture-mode values an Acer EK251Q G accepts, in the monitor's display
/// order. Other values in this range are rejected by the firmware.
pub const ACER_MODES: [(u16, &str); 8] = [
    (0x00, "sRGB"),
    (0x01, "Reading"),
    (0x02, "Darkroom"),
    (0x03, "User"),
    (0x04, "Standard"),
    (0x09, "ECO"),
    (0x0c, "Graphics"),
    (0x0d, "Movie"),
];

/// Name of an Acer built-in picture mode, if the value is a known one.
pub fn acer_mode_name(v: u16) -> Option<&'static str> {
    ACER_MODES.iter().find(|(m, _)| *m == v).map(|(_, n)| *n)
}

/// Codes polled automatically for the curated UI section.
pub const CURATED: [FeatureCode; 5] = [
    BRIGHTNESS,
    CONTRAST,
    COLOR_PRESET,
    INPUT_SOURCE,
    AUDIO_VOLUME,
];

#[derive(Clone, Debug)]
pub struct MonitorId {
    /// Stable identity used for profiles: "MFR|MODEL|SERIAL".
    pub key: String,
    /// Backend id (i2c rdev number as string).
    pub backend_id: String,
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
}

#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub codes: Vec<FeatureCode>,
    /// Non-continuous features: code -> supported raw values (with optional names)
    /// as reported by the monitor's capabilities string.
    pub values: BTreeMap<FeatureCode, BTreeMap<u16, Option<String>>>,
    pub model: Option<String>,
    pub mccs_version: Option<String>,
}

pub enum Cmd {
    Get(FeatureCode),
    Set(FeatureCode, u16),
    Caps,
}

pub enum WriteResult {
    Verified { value: u16, max: u16 },
    Unverified(String),
    Failed(String),
}

pub enum Ev {
    Value {
        request: u64,
        code: FeatureCode,
        value: u16,
        max: u16,
    },
    SetFinished {
        request: u64,
        code: FeatureCode,
        desired: u16,
        result: WriteResult,
    },
    Failed {
        request: u64,
        code: FeatureCode,
        err: String,
    },
    Caps(Result<Caps, String>),
}

pub struct Worker {
    tx: Sender<(u64, Cmd)>,
    rx: Receiver<Ev>,
    next_request: AtomicU64,
    repaint: Arc<Mutex<Option<egui::Context>>>,
}

fn publish(tx: &Sender<Ev>, repaint: &Mutex<Option<egui::Context>>, ev: Ev) {
    if tx.send(ev).is_ok() {
        if let Some(ctx) = repaint.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }
}

/// Returns Some(explanation) when i2c devices exist but are not accessible
/// for the current user (missing `i2c` group / udev rule).
pub fn i2c_permission_error() -> Option<String> {
    let mut found = 0usize;
    let mut denied = 0usize;
    let mut denied_first = String::new();
    for i in 0..32 {
        let path = format!("/dev/i2c-{i}");
        if std::fs::metadata(&path).is_ok() {
            found += 1;
            if std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .is_err()
            {
                denied += 1;
                if denied_first.is_empty() {
                    denied_first = path;
                }
            }
        }
    }
    if found > 0 && denied == found {
        Some(format!(
            "Found {found} i2c devices but none are accessible (tried {denied_first}). \
             Add yourself to the `i2c` group or install the udev rule — see README."
        ))
    } else {
        None
    }
}

pub fn enumerate() -> Vec<(MonitorId, Worker)> {
    // Demo mode for trying the UI without hardware/permissions:
    //   DISPLAY_WIDGET_DEMO=1 display-widget
    if std::env::var("DISPLAY_WIDGET_DEMO").as_deref() == Ok("1") {
        return vec![demo_monitor()];
    }

    let mut out = Vec::new();
    for i in 0..32 {
        let path = format!("/dev/i2c-{i}");
        if !Path::new(&path).exists() {
            continue;
        }
        if ignorable_adapter(i) {
            continue;
        }
        let Ok(dev) = ddc_i2c::from_i2c_device(&path) else {
            continue; // no permission or adapter unavailable
        };
        let mut dev = BackendDev::Real(dev);
        let Some(id) = probe_monitor(&mut dev, i) else {
            continue; // no monitor / not a DDC bus
        };
        out.push((id, Worker::spawn(dev)));
    }
    out
}

/// Read the EDID of the monitor on this bus (if any) and build its identity.
fn probe_monitor(dev: &mut BackendDev, i: u32) -> Option<MonitorId> {
    let mut edid = vec![0u8; 128];
    match dev {
        BackendDev::Real(d) => {
            d.read_edid(0, &mut edid).ok()?;
        }
        BackendDev::Fake(_) => unreachable!("demo monitor needs no probing"),
    }
    let info = DisplayInfo::from_edid(Backend::I2cDevice, i.to_string(), edid).ok()?;
    Some(MonitorId {
        key: format!(
            "{}|{}|{}",
            info.manufacturer_id.clone().unwrap_or_default(),
            info.model_name.clone().unwrap_or_default(),
            info.serial_number.clone().unwrap_or_default(),
        ),
        backend_id: info.id.clone(),
        manufacturer: info
            .manufacturer_id
            .clone()
            .unwrap_or_else(|| "Unknown".into()),
        model: info.model_name.clone().unwrap_or_else(|| "Unknown".into()),
        serial: info.serial_number.clone().unwrap_or_else(|| "-".into()),
    })
}

/// A simulated monitor so the UI can be exercised without i2c access.
fn demo_monitor() -> (MonitorId, Worker) {
    let mut values: BTreeMap<u8, (u16, u16)> = BTreeMap::new();
    for (code, v, m) in [
        (BRIGHTNESS, 60u16, 100u16),
        (CONTRAST, 50, 100),
        (AUDIO_VOLUME, 40, 100),
        (0x87u8, 5, 100), // sharpness
        (0x73u8, 2, 4),   // gamma
        (COLOR_PRESET, 0x05, 0),
        (INPUT_SOURCE, 0x11, 0),
        (POWER_MODE, 1, 0),
    ] {
        values.insert(code, (v, m));
    }
    let mut vcp_features = BTreeMap::new();
    for (code, name, vals) in [
        (0x10u8, Some("Brightness".to_string()), BTreeMap::new()),
        (0x12, Some("Contrast".to_string()), BTreeMap::new()),
        (
            COLOR_PRESET,
            Some("Color preset".to_string()),
            BTreeMap::from([
                (0x01u8, Some("sRGB".to_string())),
                (0x05, Some("6500K".to_string())),
                (0x08, Some("9300K".to_string())),
                (0x0b, Some("User 1".to_string())),
            ]),
        ),
        (
            INPUT_SOURCE,
            Some("Input source".to_string()),
            BTreeMap::from([
                (0x0fu8, Some("DisplayPort-1".to_string())),
                (0x11, Some("HDMI-1".to_string())),
                (0x12, Some("HDMI-2".to_string())),
            ]),
        ),
        (
            0x62,
            Some("Audio speaker volume".to_string()),
            BTreeMap::new(),
        ),
        (
            0x73,
            Some("Gamma".to_string()),
            BTreeMap::from([
                (1u8, Some("1.8".to_string())),
                (2u8, Some("2.2".to_string())),
                (4u8, Some("4.0".to_string())),
            ]),
        ),
        (0x87, Some("Sharpness".to_string()), BTreeMap::new()),
        (0xd6, Some("Power mode".to_string()), BTreeMap::new()),
    ] {
        vcp_features.insert(code, mccs::VcpDescriptor { name, values: vals });
    }
    let caps = mccs::Capabilities {
        model: Some("EK251Q G (demo)".to_string()),
        mccs_version: Some(mccs::Version { major: 2, minor: 1 }),
        vcp_features,
        ..Default::default()
    };
    let id = MonitorId {
        key: "ACR|EK251Q G|DEMO0001".into(),
        backend_id: "demo".into(),
        manufacturer: "ACR".into(),
        model: "EK251Q G (demo)".into(),
        serial: "DEMO0001".into(),
    };
    (
        id,
        Worker::spawn(BackendDev::Fake(FakeDev { values, caps })),
    )
}

enum BackendDev {
    Real(I2cDeviceDdc),
    Fake(FakeDev),
}

struct FakeDev {
    values: BTreeMap<u8, (u16, u16)>,
    caps: mccs::Capabilities,
}

impl FakeDev {
    fn get(&self, code: u8) -> Result<VcpValue, String> {
        match self.values.get(&code) {
            Some(&(v, m)) => Ok(vcp_value(v, m)),
            None => Err(format!("unsupported feature 0x{code:02X}")),
        }
    }

    fn set(&mut self, code: u8, val: u16) -> Result<(), String> {
        match self.values.get_mut(&code) {
            Some(e) => {
                e.0 = if e.1 == 0 { val } else { val.min(e.1) };
                Ok(())
            }
            None => Err(format!("unsupported feature 0x{code:02X}")),
        }
    }
}

fn vcp_value(v: u16, max: u16) -> VcpValue {
    VcpValue {
        ty: 0,
        mh: (max >> 8) as u8,
        ml: max as u8,
        sh: (v >> 8) as u8,
        sl: v as u8,
    }
}

/// Adapters ddcutil considers incapable of DDC (SMBus controllers etc.).
fn ignorable_adapter(i: u32) -> bool {
    let Ok(name) = std::fs::read_to_string(format!("/sys/bus/i2c/devices/i2c-{i}/name")) else {
        return false;
    };
    let name = name.trim();
    ["SMBus", "soc:i2cdsi", "smu", "mac-io", "u4"]
        .iter()
        .any(|p| name.starts_with(p))
}

impl Worker {
    fn spawn(mut display: BackendDev) -> Worker {
        let (cmd_tx, cmd_rx) = channel::<(u64, Cmd)>();
        let (ev_tx, ev_rx) = channel::<Ev>();
        let repaint = Arc::new(Mutex::new(None));
        let worker_repaint = repaint.clone();
        thread::Builder::new()
            .name("ddc-worker".into())
            .spawn(move || {
                while let Ok((request, cmd)) = cmd_rx.recv() {
                    match cmd {
                        Cmd::Get(code) => {
                            let res = match &mut display {
                                BackendDev::Real(d) => retry(3, || d.get_vcp_feature(code)),
                                BackendDev::Fake(f) => f.get(code),
                            };
                            match res {
                                Ok(v) => {
                                    publish(
                                        &ev_tx,
                                        &worker_repaint,
                                        Ev::Value {
                                            request,
                                            code,
                                            value: v.value(),
                                            max: v.maximum(),
                                        },
                                    );
                                }
                                Err(e) => {
                                    publish(
                                        &ev_tx,
                                        &worker_repaint,
                                        Ev::Failed {
                                            request,
                                            code,
                                            err: e,
                                        },
                                    );
                                }
                            }
                        }
                        Cmd::Set(code, val) => {
                            let res = match &mut display {
                                BackendDev::Real(d) => retry(3, || d.set_vcp_feature(code, val)),
                                BackendDev::Fake(f) => f.set(code, val),
                            };
                            let result = match res {
                                Ok(()) => {
                                    thread::sleep(Duration::from_millis(40));
                                    let res = match &mut display {
                                        BackendDev::Real(d) => retry(2, || d.get_vcp_feature(code)),
                                        BackendDev::Fake(f) => f.get(code),
                                    };
                                    match res {
                                        Ok(v) => WriteResult::Verified {
                                            value: v.value(),
                                            max: v.maximum(),
                                        },
                                        Err(e) => WriteResult::Unverified(e),
                                    }
                                }
                                Err(e) => WriteResult::Failed(e),
                            };
                            publish(
                                &ev_tx,
                                &worker_repaint,
                                Ev::SetFinished {
                                    request,
                                    code,
                                    desired: val,
                                    result,
                                },
                            );
                        }
                        Cmd::Caps => {
                            let ev = match &mut display {
                                BackendDev::Real(d) => read_caps(d),
                                BackendDev::Fake(f) => Ok(f.caps.clone()),
                            };
                            let ev = match ev {
                                Ok(caps) => {
                                    let mut values = BTreeMap::new();
                                    for (code, desc) in &caps.vcp_features {
                                        if !desc.values.is_empty() {
                                            values.insert(
                                                *code,
                                                desc.values
                                                    .iter()
                                                    .map(|(k, v)| (*k as u16, v.clone()))
                                                    .collect(),
                                            );
                                        }
                                    }
                                    Ev::Caps(Ok(Caps {
                                        codes: caps.vcp_features.keys().copied().collect(),
                                        values,
                                        model: caps.model,
                                        mccs_version: caps.mccs_version.map(|v| v.to_string()),
                                    }))
                                }
                                Err(e) => Ev::Caps(Err(e)),
                            };
                            publish(&ev_tx, &worker_repaint, ev);
                        }
                    }
                }
            })
            .expect("failed to spawn ddc worker thread");
        Worker {
            tx: cmd_tx,
            rx: ev_rx,
            next_request: AtomicU64::new(1),
            repaint,
        }
    }

    pub fn set_repaint_context(&self, ctx: &egui::Context) {
        *self.repaint.lock().unwrap() = Some(ctx.clone());
        ctx.request_repaint();
    }

    fn send(&self, cmd: Cmd) -> u64 {
        let request = self.next_request.fetch_add(1, Ordering::Relaxed);
        if self.tx.send((request, cmd)).is_err() {
            log::error!("DDC worker disconnected");
        }
        request
    }

    pub fn get(&self, code: FeatureCode) {
        self.send(Cmd::Get(code));
    }

    pub fn set(&self, code: FeatureCode, value: u16) -> u64 {
        self.send(Cmd::Set(code, value))
    }

    pub fn caps(&self) {
        self.send(Cmd::Caps);
    }

    pub fn try_recv(&self) -> Option<Ev> {
        self.rx.try_recv().ok()
    }
}

/// Read and parse the capabilities string from a display.
fn read_caps(display: &mut I2cDeviceDdc) -> Result<mccs::Capabilities, String> {
    let raw = display
        .capabilities_string()
        .map_err(|e| format!("read capabilities string: {e}"))?;
    match mccs_caps::parse_capabilities(&raw) {
        Ok(caps) => Ok(caps),
        Err(strict) => match parse_caps_lenient(&raw) {
            Ok(caps) => {
                log::debug!("strict capabilities parse failed ({strict}); parsed leniently");
                Ok(caps)
            }
            Err(lenient) => {
                log::debug!(
                    "unparsed capabilities string: {:?}",
                    String::from_utf8_lossy(&raw)
                );
                Err(format!("parse MCCS capabilities: {lenient}"))
            }
        },
    }
}

/// Tolerant fallback for monitors whose capabilities string does not satisfy
/// the strict grammar `mccs-caps` expects. Acer is a repeat offender: its
/// string carries an extra `)` after `type(...)` and again at the end, which
/// makes the strict parser reject the whole (otherwise valid) string.
/// This scans for the `vcp(...)` section and extracts the feature codes and
/// their value lists, ignoring malformed bracketing elsewhere.
fn parse_caps_lenient(raw: &[u8]) -> Result<mccs::Capabilities, String> {
    let text = String::from_utf8_lossy(raw);
    let lower = text.to_ascii_lowercase();

    let vcp_body = tag_body(&text, &lower, "vcp(").ok_or("no vcp(...) section found")?;

    let mut codes: Vec<u8> = Vec::new();
    let mut values: BTreeMap<u8, BTreeMap<u8, Option<String>>> = BTreeMap::new();
    let mut current: Option<u8> = None;
    let mut in_list = false;

    let bytes = vcp_body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                in_list = true;
                i += 1;
            }
            b')' => {
                in_list = false;
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < bytes.len() && i - start < 2 && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
                if i - start == 2 {
                    let v = u8::from_str_radix(&vcp_body[start..i], 16)
                        .map_err(|e| format!("bad hex value: {e}"))?;
                    if in_list {
                        if let Some(code) = current {
                            values.entry(code).or_default().insert(v, None);
                        }
                    } else if !codes.contains(&v) {
                        codes.push(v);
                        current = Some(v);
                    }
                } else if i == start {
                    i += 1; // skip non-hex junk
                }
            }
        }
    }

    if codes.is_empty() {
        return Err("vcp(...) section contained no feature codes".into());
    }

    let mut vcp_features = BTreeMap::new();
    for code in codes {
        vcp_features.insert(
            code,
            mccs::VcpDescriptor {
                name: None,
                values: values.remove(&code).unwrap_or_default(),
            },
        );
    }

    Ok(mccs::Capabilities {
        model: tag_value(&text, &lower, "model("),
        mccs_version: tag_version(&text, &lower),
        vcp_features,
        ..Default::default()
    })
}

/// Body of the first `<tag>...(...)` occurrence, up to its matching `)`.
fn tag_body<'a>(text: &'a str, lower: &str, tag: &str) -> Option<&'a str> {
    let start = lower.find(tag)? + tag.len();
    let rest = &text[start..];
    let mut depth = 1i32;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..i]);
                }
            }
            _ => {}
        }
    }
    Some(rest) // unterminated: take the remainder
}

/// Value of `<tag>(value)` as a trimmed string, if non-empty.
fn tag_value(text: &str, lower: &str, tag: &str) -> Option<String> {
    let value = tag_body(text, lower, tag)?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `mccs_ver(major.minor)` as a parsed version.
fn tag_version(text: &str, lower: &str) -> Option<mccs::Version> {
    let value = tag_body(text, lower, "mccs_ver(")?;
    let mut parts = value.trim().split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    Some(mccs::Version { major, minor })
}
fn retry<T, E: std::fmt::Display>(
    attempts: u32,
    mut f: impl FnMut() -> Result<T, E>,
) -> Result<T, String> {
    let mut last = String::new();
    for i in 0..attempts {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                last = e.to_string();
                if i + 1 < attempts {
                    thread::sleep(Duration::from_millis(60));
                }
            }
        }
    }
    Err(last)
}

/// Well-known MCCS value names for input source (0x60).
pub fn input_name(v: u16) -> Option<&'static str> {
    Some(match v {
        0x01 => "VGA-1",
        0x02 => "VGA-2",
        0x03 => "DVI-1",
        0x04 => "DVI-2",
        0x0f => "DisplayPort-1",
        0x10 => "DisplayPort-2",
        0x11 => "HDMI-1",
        0x12 => "HDMI-2",
        0x13 => "HDMI-3",
        0x90 => "USB-C 1",
        0x91 => "USB-C 2",
        _ => return None,
    })
}

/// Well-known MCCS value names for color preset (0x14).
pub fn color_name(v: u16) -> Option<&'static str> {
    Some(match v {
        0x01 => "sRGB",
        0x02 => "Display Native",
        0x03 => "4000K",
        0x04 => "5000K",
        0x05 => "6500K",
        0x06 => "7500K",
        0x07 => "8200K",
        0x08 => "9300K",
        0x09 => "10000K",
        0x0a => "11500K",
        0x0b => "User 1",
        0x0c => "User 2",
        0x0d => "User 3",
        _ => return None,
    })
}

/// Human names for common VCP codes (used in the Advanced list).
pub fn vcp_name(code: FeatureCode) -> String {
    let s = match code {
        0x10 => "Brightness",
        0x12 => "Contrast",
        0x13 => "Brightness area select",
        0x14 => "Color preset",
        0x16 => "Red gain",
        0x18 => "Green gain",
        0x1a => "Blue gain",
        0x1e => "Auto setup",
        0x20 => "Horizontal position",
        0x30 => "Vertical position",
        0x52 => "Active control",
        0x54 => "Performance preservation",
        0x60 => "Input source",
        0x62 => "Audio speaker volume",
        0x6c => "Video black level: red",
        0x6e => "Video black level: green",
        0x70 => "Video black level: blue",
        0x73 => "Gamma",
        0x74 => "Power mode (0xd6 alias)",
        0x82 => "New control definition",
        0x85 => "DPMS wake",
        0x87 => "Sharpness",
        0x88 => "Overdrive / OD",
        0x8d => "Audio: mute",
        0x91 => "Flush panel self-test",
        0x9b => "6-axis saturations: red",
        0xac => "Horizontal frequency",
        0xae => "Vertical frequency",
        0xb2 => "Flat-panel sub-pixel layout",
        0xb6 => "Display technology type",
        0xc0 => "Display usage time",
        0xc6 => "Application enable key",
        0xc8 => "Display controller type",
        0xc9 => "Display firmware level",
        0xca => "OSD/button control",
        0xd6 => "Power mode",
        0xdf => "VCP version",
        0xe2 => "Acer monitor mode",
        _ => return format!("0x{code:02x}"),
    };
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As reported by an Acer EK251Q G: extra `)` after type(...), a double
    /// space inside vcp(...), and a trailing `)`.
    const ACER_RAW: &str = "(prot(monitor)type(LCD))model(ACER)cmds(01 02 03 07 0C E3 F3)vcp(04 10 12 14(05 06 08 0B) 16 18 1A 59 5A 5B 5C 5D 5E 60(01 11)  62 8D 9B 9C 9D 9E 9F A0 CC(01 02 03 04 05 06 07 08 09 0a 0c 0d 0e 14 16 1E 24) D6(01 02 04 05) E0(00 04 05) E1(01 05 06) E2(00 01 02 03 04 09 0C 0D 14 15 16) E3 E4 E5 E7(00 01 02) E8(00 01 02 03 04)) mswhql(1)asset_eep(40)mccs_ver(2.2))";

    #[test]
    fn strict_parser_rejects_acer_quirk() {
        assert!(mccs_caps::parse_capabilities(ACER_RAW).is_err());
    }

    #[test]
    fn lenient_parser_handles_acer_quirks() {
        let caps = parse_caps_lenient(ACER_RAW.as_bytes()).unwrap();
        assert_eq!(caps.model.as_deref(), Some("ACER"));
        assert_eq!(
            caps.mccs_version,
            Some(mccs::Version { major: 2, minor: 2 })
        );
        for code in CURATED {
            assert!(
                caps.vcp_features.contains_key(&code),
                "missing 0x{code:02X}"
            );
        }
        let color = &caps.vcp_features[&COLOR_PRESET].values;
        assert!(color.contains_key(&0x05) && color.contains_key(&0x0b));
        let input = &caps.vcp_features[&INPUT_SOURCE].values;
        assert!(input.contains_key(&0x01) && input.contains_key(&0x11));
        // Continuous features must not pick up a bogus value list.
        assert!(caps.vcp_features[&BRIGHTNESS].values.is_empty());
    }

    #[test]
    fn lenient_parser_accepts_well_formed_strings() {
        let raw =
            "(prot(monitor)type(LCD)model(S27R650)mccs_ver(2.1)vcp(10 12 14(05 08) 60(0F 11)))";
        let caps = parse_caps_lenient(raw.as_bytes()).unwrap();
        assert_eq!(caps.model.as_deref(), Some("S27R650"));
        assert_eq!(
            caps.mccs_version,
            Some(mccs::Version { major: 2, minor: 1 })
        );
        assert_eq!(caps.vcp_features.len(), 4);
        assert_eq!(caps.vcp_features[&COLOR_PRESET].values.len(), 2);
        assert!(caps.vcp_features[&BRIGHTNESS].values.is_empty());
    }

    #[test]
    fn acer_mode_names_round_trip() {
        for (value, name) in ACER_MODES {
            assert_eq!(acer_mode_name(value), Some(name));
        }
        assert_eq!(acer_mode_name(0x05), None);
        assert_eq!(acer_mode_name(0x0e), None);
    }
}
