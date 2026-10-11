//! `hwinfo-rs --helper`: the root-only readings HWiNFO gets as Administrator.
//! Runs as a systemd service and writes /run/hwinfo-rs/priv.tsv (0644), which
//! the unprivileged GUI/CLI merge in. Each line:
//!   group \t name \t kind \t unit \t decimals \t value
//! Child rows use "parent\u{1f}child" as the name.
//! Read-only: smartctl queries (no self-tests, no settings).

use crate::model::{Group, Item, Kind};
use std::time::{Duration, Instant};

pub const PRIV_FILE: &str = "/run/hwinfo-rs/priv.tsv";

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Temp => "temp",
        Kind::Volt => "volt",
        Kind::Clock => "clock",
        Kind::Power => "power",
        Kind::Current => "current",
        Kind::Usage => "usage",
        Kind::Fan => "fan",
        Kind::Memory => "memory",
        Kind::Rate => "rate",
        Kind::Bool => "bool",
        Kind::Other => "other",
    }
}

fn kind_from(s: &str) -> Kind {
    match s {
        "temp" => Kind::Temp,
        "volt" => Kind::Volt,
        "clock" => Kind::Clock,
        "power" => Kind::Power,
        "current" => Kind::Current,
        "usage" => Kind::Usage,
        "fan" => Kind::Fan,
        "memory" => Kind::Memory,
        "rate" => Kind::Rate,
        "bool" => Kind::Bool,
        _ => Kind::Other,
    }
}

/// Unprivileged side: read the helper's snapshot if it is fresh.
pub fn read_priv() -> Vec<Group> {
    let p = std::path::Path::new(PRIV_FILE);
    let fresh = p.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e < Duration::from_secs(180));
    if !fresh {
        return Vec::new();
    }
    let mut groups: Vec<Group> = Vec::new();
    for l in std::fs::read_to_string(p).unwrap_or_default().lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() != 6 {
            continue;
        }
        let unit: &'static str = Box::leak(f[3].to_string().into_boxed_str());
        let (parent, name) = match f[1].split_once('\u{1f}') {
            Some((p, c)) => (Some(p), c),
            None => (None, f[1]),
        };
        let mut it = Item::new(name, kind_from(f[2]), f[4].parse().unwrap_or(0), f[5].parse().ok());
        if !unit.is_empty() {
            it.unit = Some(unit);
        }
        let g = match groups.iter().position(|g| g.title == f[0]) {
            Some(i) => &mut groups[i],
            None => {
                groups.push(Group { title: f[0].to_string(), items: Vec::new() });
                groups.last_mut().unwrap()
            }
        };
        match parent.and_then(|p| g.items.iter_mut().rev().find(|i| i.name == p)) {
            Some(p) => p.children.push(it),
            None => g.items.push(it),
        }
    }
    groups
}

fn smart(dev: &str) -> Option<(String, Vec<Item>)> {
    let out = std::process::Command::new("smartctl").args(["-j", "-a", dev]).output().ok()?;
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let model = j["model_name"].as_str()?.to_string();
    let serial = j["serial_number"].as_str().unwrap_or("").to_string();
    let gib = 1024.0 * 1024.0 * 1024.0;
    let mut v = Vec::new();
    let passed = j["smart_status"]["passed"].as_bool();
    if let Some(n) = j.get("nvme_smart_health_information_log").filter(|n| n.is_object()) {
        let num = |k: &str| n[k].as_f64();
        v.push(Item::new("Drive Remaining Life", Kind::Usage, 1, num("percentage_used").map(|u| (100.0 - u).max(0.0))));
        v.push(Item::new("Drive Available Spare", Kind::Usage, 1, num("available_spare")));
        v.push(Item::new("Drive Failure", Kind::Bool, 0, passed.map(|p| if p { 0.0 } else { 1.0 })));
        v.push(Item::new("Drive Warning", Kind::Bool, 0, num("critical_warning").map(|w| if w > 0.0 { 1.0 } else { 0.0 })));
        v.push(Item::new("Total Host Writes", Kind::Other, 0, num("data_units_written").map(|u| u * 512000.0 / gib)).unit("GB"));
        v.push(Item::new("Total Host Reads", Kind::Other, 0, num("data_units_read").map(|u| u * 512000.0 / gib)).unit("GB"));
        v.push(Item::new("Power-On Hours", Kind::Other, 0, num("power_on_hours")).unit("h"));
        v.push(Item::new("Unsafe Shutdowns", Kind::Other, 0, num("unsafe_shutdowns")));
        v.push(Item::new("Media Errors", Kind::Other, 0, num("media_errors")));
    } else if let Some(tab) = j["ata_smart_attributes"]["table"].as_array() {
        let attr = |id: u64| tab.iter().find(|a| a["id"].as_u64() == Some(id));
        let raw = |id: u64| attr(id).and_then(|a| a["raw"]["value"].as_f64());
        let name = |id: u64| attr(id).and_then(|a| a["name"].as_str()).unwrap_or("");
        // Remaining life: SSD_Life_Left (231) on Phison-based drives, 169 on some SMI ones.
        let life = raw(231).or_else(|| raw(169).filter(|&x| x <= 100.0));
        v.push(Item::new("Drive Remaining Life", Kind::Usage, 1, life));
        let failing = tab.iter().any(|a| a["when_failed"].as_str().is_some_and(|w| !w.is_empty()));
        v.push(Item::new("Drive Failure", Kind::Bool, 0, passed.map(|p| if p { 0.0 } else { 1.0 })));
        v.push(Item::new("Drive Warning", Kind::Bool, 0, Some(if failing { 1.0 } else { 0.0 })));
        // Writes/reads: GiB-named attributes are GiB. "Total_LBAs" is nominally 512 B
        // sectors, but some SMI controllers count 32 MiB units: if 512 B units would
        // mean under 1 GiB over 100+ power-on hours, assume 32 MiB units.
        let hours = raw(9).unwrap_or(0.0);
        let to_gb = |id: u64| -> Option<f64> {
            let r = raw(id)?;
            let n = name(id);
            Some(if n.contains("GiB") || n.contains("GB") {
                r
            } else if n.contains("LBAs") && r * 512.0 < gib && hours > 100.0 {
                r * 32.0 / 1024.0
            } else {
                r * 512.0 / gib
            })
        };
        v.push(Item::new("Total Host Writes", Kind::Other, 0, to_gb(241)).unit("GB"));
        if attr(233).is_some_and(|a| a["name"].as_str().is_some_and(|n| n.contains("Flash") || n.contains("NAND"))) {
            v.push(Item::new("Total NAND Writes", Kind::Other, 0, to_gb(233)).unit("GB"));
        }
        v.push(Item::new("Total Host Reads", Kind::Other, 0, to_gb(242)).unit("GB"));
        v.push(Item::new("Power-On Hours", Kind::Other, 0, raw(9)).unit("h"));
        if let Some(crc) = raw(199) {
            v.push(Item::new("SATA CRC Errors", Kind::Other, 0, Some(crc)));
        }
    }
    Some((format!("{model} ({serial})"), v))
}

fn block_devices() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/block")
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.retain(|n| (n.starts_with("sd") || n.starts_with("nvme")) && std::path::Path::new(&format!("/sys/block/{n}/device")).exists());
    v.sort();
    v
}

fn write_atomic(groups: &[Group]) -> std::io::Result<()> {
    let mut s = String::new();
    let line = |s: &mut String, g: &str, name: &str, it: &Item| {
        let val = it.value.map(|x| x.to_string()).unwrap_or_default();
        s.push_str(&format!("{g}\t{name}\t{}\t{}\t{}\t{val}\n", kind_name(it.kind), it.unit.unwrap_or(""), it.decimals));
    };
    for g in groups {
        for it in &g.items {
            line(&mut s, &g.title, &it.name, it);
            for c in &it.children {
                line(&mut s, &g.title, &format!("{}\u{1f}{}", it.name, c.name), c);
            }
        }
    }
    std::fs::create_dir_all("/run/hwinfo-rs")?;
    let tmp = format!("{PRIV_FILE}.tmp");
    std::fs::write(&tmp, s)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(tmp, PRIV_FILE)
}

pub fn run(interval: Duration) {
    // MSR access for per-thread clocks, VIDs, C-states (no-op if built in).
    let _ = std::process::Command::new("modprobe").arg("msr").status();
    crate::summary::write_modules();
    let mut smart_cache: Vec<Group> = Vec::new();
    let mut last_smart: Option<Instant> = None;
    let mut msr = crate::privsrc::Msr::new();
    let mut cpumsr = crate::cpumsr::CpuMsr::new();
    loop {
        // SMART data changes slowly; poll it once a minute.
        if last_smart.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
            smart_cache = block_devices()
                .iter()
                .filter_map(|d| {
                    let dev = if d.starts_with("nvme") { format!("/dev/{}", d.trim_end_matches(|c: char| c.is_ascii_digit()).trim_end_matches('n')) } else { format!("/dev/{d}") };
                    smart(&dev).map(|(title, items)| Group { title: format!("S.M.A.R.T.: {title} ({d})"), items })
                })
                .collect();
            last_smart = Some(Instant::now());
        }
        let mut groups = smart_cache.clone();
        groups.extend(crate::privsrc::sample(&mut msr));
        groups.extend(cpumsr.sample());
        if let Err(e) = write_atomic(&groups) {
            eprintln!("hwinfo-rs helper: {e}");
        }
        std::thread::sleep(interval);
    }
}
