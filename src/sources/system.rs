//! Memory, drives, network and hardware-error counters from procfs/sysfs.

use crate::model::{Item, Kind};
use crate::sources::hwmon::read_trim;
use std::collections::HashMap;
use std::time::Instant;

pub fn memory() -> Vec<Item> {
    let s = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let m: HashMap<&str, f64> = s
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k, v.split_whitespace().next()?.parse::<f64>().ok()? / 1024.0))
        })
        .collect();
    let g = |k| m.get(k).copied();
    let mut v = Vec::new();
    // Windows "virtual memory" = RAM + page file. Linux's Committed_AS counts
    // overcommitted reservations (often > 100%), so use what is actually backed:
    // RAM in use + swap in use, against RAM + swap.
    if let (Some(t), Some(a), Some(st), Some(sf)) = (g("MemTotal"), g("MemAvailable"), g("SwapTotal"), g("SwapFree")) {
        let (used, limit) = ((t - a) + (st - sf), t + st);
        v.push(Item::new("Virtual Memory Committed", Kind::Memory, 0, Some(used)));
        v.push(Item::new("Virtual Memory Available", Kind::Memory, 0, Some(limit - used)));
        v.push(Item::new("Virtual Memory Load", Kind::Usage, 1, Some(100.0 * used / limit)));
    }
    if let (Some(t), Some(a)) = (g("MemTotal"), g("MemAvailable")) {
        v.push(Item::new("Physical Memory Used", Kind::Memory, 0, Some(t - a)));
        v.push(Item::new("Physical Memory Available", Kind::Memory, 0, Some(a)));
        v.push(Item::new("Physical Memory Load", Kind::Usage, 1, Some(100.0 * (t - a) / t)));
    }
    if let (Some(t), Some(f)) = (g("SwapTotal"), g("SwapFree")) {
        v.push(Item::new("Page File Usage", Kind::Usage, 1, Some(if t > 0.0 { 100.0 * (t - f) / t } else { 0.0 })));
    }
    v
}

struct DiskPrev {
    t: Instant,
    rd_sec: u64,
    wr_sec: u64,
    rd_ticks: u64,
    wr_ticks: u64,
    io_ticks: u64,
}

/// Drive activity like HWiNFO's "Drive: <model>" groups.
pub struct Disks {
    prev: HashMap<String, DiskPrev>,
    first: HashMap<String, (u64, u64)>,
}

impl Disks {
    pub fn new() -> Self {
        Disks { prev: HashMap::new(), first: HashMap::new() }
    }

    /// Returns (block device name, model, items) for each whole disk.
    pub fn sample(&mut self) -> Vec<(String, String, Vec<Item>)> {
        let now = Instant::now();
        let s = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
        let mut out = Vec::new();
        for l in s.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 14 {
                continue;
            }
            let name = f[2].to_string();
            if !std::path::Path::new(&format!("/sys/block/{name}/device")).exists() {
                continue;
            }
            let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            let cur = DiskPrev { t: now, rd_sec: n(5), wr_sec: n(9), rd_ticks: n(6), wr_ticks: n(10), io_ticks: n(12) };
            let first = *self.first.entry(name.clone()).or_insert((cur.rd_sec, cur.wr_sec));
            let model = read_trim(format!("/sys/block/{name}/device/model")).unwrap_or_else(|| name.clone());
            let mut items = Vec::new();
            if let Some(p) = self.prev.get(&name) {
                let dt = now.duration_since(p.t).as_secs_f64().max(1e-3);
                let ms = dt * 1000.0;
                let pct = |d: u64| Some((100.0 * d as f64 / ms).min(100.0));
                items.push(Item::new("Read Activity", Kind::Usage, 1, pct(cur.rd_ticks.saturating_sub(p.rd_ticks))));
                items.push(Item::new("Write Activity", Kind::Usage, 1, pct(cur.wr_ticks.saturating_sub(p.wr_ticks))));
                items.push(Item::new("Total Activity", Kind::Usage, 1, pct(cur.io_ticks.saturating_sub(p.io_ticks))));
                items.push(Item::new("Read Rate", Kind::Rate, 3, Some((cur.rd_sec.saturating_sub(p.rd_sec)) as f64 * 512.0 / 1e6 / dt)));
                items.push(Item::new("Write Rate", Kind::Rate, 3, Some((cur.wr_sec.saturating_sub(p.wr_sec)) as f64 * 512.0 / 1e6 / dt)));
            }
            items.push(Item::new("Read Total", Kind::Memory, 0, Some(cur.rd_sec.saturating_sub(first.0) as f64 * 512.0 / 1048576.0)));
            items.push(Item::new("Write Total", Kind::Memory, 0, Some(cur.wr_sec.saturating_sub(first.1) as f64 * 512.0 / 1048576.0)));
            self.prev.insert(name.clone(), cur);
            out.push((name, model, items));
        }
        out
    }
}

/// Physical NICs only (those with a /device link).
pub struct Net {
    prev: HashMap<String, (Instant, u64, u64)>,
    first: HashMap<String, (u64, u64)>,
}

impl Net {
    pub fn new() -> Self {
        Net { prev: HashMap::new(), first: HashMap::new() }
    }

    pub fn sample(&mut self) -> Vec<(String, Vec<Item>)> {
        let now = Instant::now();
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir("/sys/class/net") else { return out };
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for n in names {
            let base = format!("/sys/class/net/{n}");
            if !std::path::Path::new(&format!("{base}/device")).exists() {
                continue;
            }
            let rx = read_trim(format!("{base}/statistics/rx_bytes")).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let tx = read_trim(format!("{base}/statistics/tx_bytes")).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let first = *self.first.entry(n.clone()).or_insert((rx, tx));
            let mut items = vec![
                Item::new("Total DL", Kind::Memory, 0, Some(rx.saturating_sub(first.0) as f64 / 1048576.0)),
                Item::new("Total UP", Kind::Memory, 0, Some(tx.saturating_sub(first.1) as f64 / 1048576.0)),
            ];
            if let Some(&(t, prx, ptx)) = self.prev.get(&n) {
                let dt = now.duration_since(t).as_secs_f64().max(1e-3);
                items.push(Item::new("Current DL rate", Kind::Rate, 3, Some(rx.saturating_sub(prx) as f64 / 1024.0 / dt)).unit("KB/s"));
                items.push(Item::new("Current UP rate", Kind::Rate, 3, Some(tx.saturating_sub(ptx) as f64 / 1024.0 / dt)).unit("KB/s"));
            }
            if let Some(sp) = read_trim(format!("{base}/speed")).and_then(|s| s.parse::<f64>().ok()).filter(|&s| s > 0.0) {
                items.push(Item::new("Link Speed", Kind::Other, 0, Some(sp)).unit("Mbps"));
            }
            self.prev.insert(n.clone(), (now, rx, tx));
            out.push((n, items));
        }
        out
    }
}

/// Linux stand-in for "Windows Hardware Errors (WHEA)": PCIe AER and EDAC counters.
pub fn hw_errors() -> Vec<Item> {
    let mut corr = 0u64;
    let mut nonfatal = 0u64;
    let mut fatal = 0u64;
    let mut have_aer = false;
    if let Ok(rd) = std::fs::read_dir("/sys/bus/pci/devices") {
        for e in rd.flatten() {
            let p = e.path();
            for (file, acc) in [("aer_dev_correctable", &mut corr), ("aer_dev_nonfatal", &mut nonfatal), ("aer_dev_fatal", &mut fatal)] {
                if let Ok(s) = std::fs::read_to_string(p.join(file)) {
                    have_aer = true;
                    *acc += s.lines().find(|l| l.starts_with("TOTAL_")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0);
                }
            }
        }
    }
    let mut v = Vec::new();
    if have_aer {
        v.push(Item::new("PCIe Correctable Errors", Kind::Other, 0, Some(corr as f64)));
        v.push(Item::new("PCIe Non-Fatal Errors", Kind::Other, 0, Some(nonfatal as f64)));
        v.push(Item::new("PCIe Fatal Errors", Kind::Other, 0, Some(fatal as f64)));
    }
    let mut ce = None::<u64>;
    let mut ue = None::<u64>;
    if let Ok(rd) = std::fs::read_dir("/sys/devices/system/edac/mc") {
        for e in rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("mc")) {
            if let Some(c) = read_trim(e.path().join("ce_count")).and_then(|s| s.parse::<u64>().ok()) {
                *ce.get_or_insert(0) += c;
            }
            if let Some(c) = read_trim(e.path().join("ue_count")).and_then(|s| s.parse::<u64>().ok()) {
                *ue.get_or_insert(0) += c;
            }
        }
    }
    if let Some(c) = ce {
        v.push(Item::new("Memory Corrected Errors (EDAC)", Kind::Other, 0, Some(c as f64)));
    }
    if let Some(u) = ue {
        v.push(Item::new("Memory Uncorrected Errors (EDAC)", Kind::Other, 0, Some(u as f64)));
    }
    v
}
