//! Saved monitor profiles, persisted as JSON in $XDG_CONFIG_HOME/display-widget.

use std::collections::BTreeMap;
use std::fs::{File, rename};
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MonitorProfile {
    /// VCP code (as u8) -> desired raw value.
    #[serde(default)]
    pub vcp: BTreeMap<u8, u16>,
    /// Mutter mode id, e.g. "1920x1080@60.000".
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub transform: Option<u32>,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    /// Apply automatically when the app starts.
    #[serde(default)]
    pub auto: bool,
    /// Keyed by monitor identity "MFR|MODEL|SERIAL".
    pub monitors: BTreeMap<String, MonitorProfile>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Store {
    pub profiles: Vec<Profile>,
}

impl Store {
    pub fn path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("display-widget").join("profiles.json")
    }

    pub fn load() -> Result<Store, String> {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("parse {}: {e}", Self::path().display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
            Err(e) => Err(format!("read {}: {e}", Self::path().display())),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let temp = path.with_extension("json.tmp");
        let result = (|| {
            let mut file =
                File::create(&temp).map_err(|e| format!("create {}: {e}", temp.display()))?;
            file.write_all(text.as_bytes())
                .map_err(|e| format!("write {}: {e}", temp.display()))?;
            file.sync_all()
                .map_err(|e| format!("sync {}: {e}", temp.display()))?;
            rename(&temp, &path).map_err(|e| format!("replace {}: {e}", path.display()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    pub fn named(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.name == profile.name) {
            Some(p) => *p = profile,
            None => self.profiles.push(profile),
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
    }
}
