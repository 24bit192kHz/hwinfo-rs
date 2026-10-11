//! User settings and layout, saved to $XDG_CONFIG_HOME/hwinfo-rs/config.json.
//! Rows are addressed by their stats key (group + row + child), so renames
//! and hides survive restarts and don't disturb Min/Max/Average tracking.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Clone, Copy, Default, PartialEq)]
pub struct Alert {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Clone)]
pub struct Config {
    pub hidden: BTreeSet<String>,
    pub renames: BTreeMap<String, String>,
    pub group_order: Vec<String>,
    pub alerts: BTreeMap<String, Alert>,
    pub hide_serials: bool,
    pub show_hidden: bool,
    pub interval_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hidden: BTreeSet::new(),
            renames: BTreeMap::new(),
            group_order: Vec::new(),
            alerts: BTreeMap::new(),
            hide_serials: false,
            show_hidden: false,
            interval_ms: 2000,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("hwinfo-rs").join("config.json"))
}

impl Config {
    pub fn load() -> Self {
        let mut c = Config::default();
        let Some(v) = path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return c };
        let strs = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
        c.hidden = strs("hidden").into_iter().collect();
        c.group_order = strs("group_order");
        if let Some(m) = v["renames"].as_object() {
            c.renames = m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
        }
        if let Some(m) = v["alerts"].as_object() {
            c.alerts = m.iter().map(|(k, a)| (k.clone(), Alert { min: a["min"].as_f64(), max: a["max"].as_f64() })).filter(|(_, a)| a.min.is_some() || a.max.is_some()).collect();
        }
        c.hide_serials = v["hide_serials"].as_bool().unwrap_or(false);
        c.show_hidden = v["show_hidden"].as_bool().unwrap_or(false);
        c.interval_ms = v["interval_ms"].as_u64().unwrap_or(2000).clamp(250, 60000);
        c
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        let alerts: Map<String, Value> = self.alerts.iter().map(|(k, a)| (k.clone(), json!({ "min": a.min, "max": a.max }))).collect();
        let v = json!({
            "hidden": self.hidden,
            "renames": self.renames,
            "group_order": self.group_order,
            "alerts": alerts,
            "hide_serials": self.hide_serials,
            "show_hidden": self.show_hidden,
            "interval_ms": self.interval_ms,
        });
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }

    /// Display title for a group (rename, then serial hiding).
    pub fn group_title(&self, title: &str) -> String {
        let t = self.renames.get(title).cloned().unwrap_or_else(|| title.to_string());
        if self.hide_serials { strip_serial(&t) } else { t }
    }

    /// Groups in the user's order; unknown groups keep their natural position after known ones.
    pub fn order<T>(&self, mut groups: Vec<T>, title: impl Fn(&T) -> &str) -> Vec<T> {
        if self.group_order.is_empty() {
            return groups;
        }
        groups.sort_by_key(|g| self.group_order.iter().position(|o| o == title(g)).unwrap_or(usize::MAX));
        groups
    }
}

/// "S.M.A.R.T.: MODEL (SERIAL) (dev)" -> "S.M.A.R.T.: MODEL (dev)".
pub fn strip_serial(t: &str) -> String {
    if !(t.starts_with("S.M.A.R.T.:") || t.starts_with("Drive:")) {
        return t.to_string();
    }
    let parts: Vec<&str> = t.split(" (").collect();
    if parts.len() >= 3 {
        format!("{} ({}", parts[..parts.len() - 2].join(" ("), parts[parts.len() - 1])
    } else {
        t.to_string()
    }
}
