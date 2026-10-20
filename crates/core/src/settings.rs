//! Server-wide defaults for new VMs, kept in `settings.json`. Clients such as Runo
//! show and change them through `GET` and `PUT /v1/settings`, so a server can be set
//! up once instead of passing the same flags to every `cove up`.

use crate::model::Network;
use crate::paths::Paths;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Every field is optional: what is left out falls back to Cove's own defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    /// Allowlist rules for VMs with the `allowlist` network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_pause_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop_minutes: Option<u64>,
    /// New VMs start from this snapshot unless they name an image or a snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

impl Settings {
    pub fn load(paths: &Paths) -> Settings {
        std::fs::read_to_string(paths.settings_file())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        self.check()?;
        paths.ensure()?;
        let tmp = paths.settings_file().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, paths.settings_file())?;
        Ok(())
    }

    pub fn check(&self) -> Result<()> {
        if self.cpus == Some(0) {
            bail!("cpus must be at least 1");
        }
        if self.memory_mb.is_some_and(|m| m < 256) {
            bail!("memory_mb must be at least 256");
        }
        if self.disk_gb == Some(0) {
            bail!("disk_gb must be at least 1");
        }
        if self.auto_pause_minutes == Some(0) || self.idle_stop_minutes == Some(0) {
            bail!("idle timers take at least 1 minute (leave them out for none)");
        }
        if let Some(b) = &self.backend {
            crate::backend::resolve(b)?;
        }
        if let Some(s) = &self.snapshot {
            if !crate::valid_name(s) {
                bail!("{s:?} is not a snapshot name");
            }
        }
        Ok(())
    }

    /// Fill the fields a create request left out. `body` is the request's JSON
    /// object; fields it has, even as `null`, are left alone.
    pub fn apply(&self, body: &mut serde_json::Value) {
        let Some(obj) = body.as_object_mut() else {
            return;
        };
        let defaults = serde_json::to_value(self).unwrap_or_default();
        for (key, value) in defaults.as_object().into_iter().flatten() {
            if key == "snapshot" {
                // An explicit image means "not the default snapshot".
                if obj.contains_key("image") || obj.contains_key("snapshot") {
                    continue;
                }
            }
            if !obj.contains_key(key) {
                obj.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_only_what_is_missing() {
        let settings = Settings {
            cpus: Some(4),
            memory_mb: Some(8192),
            network: Some(Network::Allowlist),
            auto_pause_minutes: Some(30),
            snapshot: Some("universal".into()),
            ..Default::default()
        };
        let mut body = serde_json::json!({ "name": "a", "cpus": 2, "idle_stop_minutes": null });
        settings.apply(&mut body);
        assert_eq!(body["cpus"], 2);
        assert_eq!(body["memory_mb"], 8192);
        assert_eq!(body["network"], "allowlist");
        assert_eq!(body["auto_pause_minutes"], 30);
        assert_eq!(body["snapshot"], "universal");
        assert!(body["idle_stop_minutes"].is_null());
        // Naming an image skips the default snapshot.
        let mut body = serde_json::json!({ "name": "a", "image": "debian-13" });
        settings.apply(&mut body);
        assert!(body.get("snapshot").is_none());
    }

    #[test]
    fn saves_checks_and_loads() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path());
        assert_eq!(Settings::load(&paths), Settings::default());
        let s = Settings {
            cpus: Some(0),
            ..Default::default()
        };
        assert!(s.save(&paths).is_err());
        let s = Settings {
            cpus: Some(4),
            auto_pause_minutes: Some(30),
            ..Default::default()
        };
        s.save(&paths).unwrap();
        assert_eq!(Settings::load(&paths), s);
        assert!(serde_json::from_str::<Settings>(r#"{"cpu":4}"#).is_err());
    }
}
