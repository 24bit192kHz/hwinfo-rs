//! Root-helper CPU rows from MSRs and powercap (read-only), for CPUs the SMU
//! pm_table path doesn't cover (Intel, AMD Zen 4/5) or as extras:
//!   powercap (intel-rapl, also used for AMD): package / core / uncore / DRAM power
//!   AMD:   per-core energy MSR 0xC001029A (unit MSR 0xC0010299)
//!   Intel: per-thread VID (0x198), core/package C-state residency counters,
//!          thermal status (0x19C / 0x1B1) -> HWiNFO's throttling / critical /
//!          power-limit rows and distance to TjMax (0x1A2), uncore ratio (0x621).
//! Items go into groups titled "CPU [#0]: msr<suffix>"; the collector merges
//! them into the matching CPU group when that row is missing there.

use crate::model::{Group, Item, Kind};
use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::time::Instant;

fn rdmsr(cpu: usize, msr: u64) -> Option<u64> {
    let f = std::fs::File::open(format!("/dev/cpu/{cpu}/msr")).ok()?;
    let mut b = [0u8; 8];
    f.read_exact_at(&mut b, msr).ok()?;
    Some(u64::from_le_bytes(b))
}

#[derive(PartialEq)]
enum Vendor {
    Intel,
    Amd,
    Other,
}

pub struct CpuMsr {
    vendor: Vendor,
    labels: Vec<String>,
    /// First logical CPU of each physical core, in core order.
    core_cpus: Vec<usize>,
    prev: HashMap<String, (Instant, u64)>,
    tjmax: Option<f64>,
}

impl CpuMsr {
    pub fn new() -> Self {
        let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let vendor = if info.contains("GenuineIntel") {
            Vendor::Intel
        } else if info.contains("AuthenticAMD") {
            Vendor::Amd
        } else {
            Vendor::Other
        };
        let cpu = crate::sources::cpu::Cpu::new();
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let labels: Vec<String> = (0..n).map(|i| cpu.thread_label(i)).collect();
        let mut core_cpus: Vec<(usize, usize)> = labels
            .iter()
            .enumerate()
            .filter(|(_, l)| l.ends_with("T0"))
            .filter_map(|(i, l)| Some((l.split_whitespace().nth(1)?.parse().ok()?, i)))
            .collect();
        core_cpus.sort();
        let tjmax = (vendor == Vendor::Intel).then(|| rdmsr(0, 0x1A2).map(|v| ((v >> 16) & 0xFF) as f64)).flatten();
        CpuMsr { vendor, labels, core_cpus: core_cpus.into_iter().map(|(_, i)| i).collect(), prev: HashMap::new(), tjmax }
    }

    /// Rate of a monotonically increasing counter, per second.
    fn rate(&mut self, key: String, now: Instant, v: u64, wrap: u64) -> Option<f64> {
        let prev = self.prev.insert(key, (now, v));
        prev.map(|(t, p)| (v.wrapping_sub(p) & wrap) as f64 / now.duration_since(t).as_secs_f64().max(1e-3))
    }

    pub fn sample(&mut self) -> Vec<Group> {
        let now = Instant::now();
        let mut enh = Vec::new();
        let mut main = Vec::new();
        let mut cst = Vec::new();

        // powercap: every RAPL domain the kernel exposes (energy_uj is root-only).
        if let Ok(rd) = std::fs::read_dir("/sys/class/powercap") {
            let mut zones: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("intel-rapl:"))).collect();
            zones.sort();
            for z in zones {
                let (Some(name), Some(e), Some(max)) = (
                    crate::sources::hwmon::read_trim(z.join("name")),
                    crate::sources::hwmon::read_trim(z.join("energy_uj")).and_then(|s| s.parse::<u64>().ok()),
                    crate::sources::hwmon::read_trim(z.join("max_energy_range_uj")).and_then(|s| s.parse::<u64>().ok()),
                ) else {
                    continue;
                };
                let w = self.rate(format!("rapl {}", z.display()), now, e, u64::MAX).map(|uj| {
                    // Handle the counter wrapping at max_energy_range_uj.
                    if uj > max as f64 * 10.0 { 0.0 } else { uj / 1e6 }
                });
                let label = match name.as_str() {
                    n if n.starts_with("package") => "CPU Package Power".to_string(),
                    // On AMD the powercap "core" zone is only core 0; per-core MSRs cover it.
                    "core" if self.vendor == Vendor::Amd => continue,
                    "core" => "IA Cores Power".to_string(),
                    "uncore" => "GT Cores Power".to_string(),
                    "dram" => "DRAM Power".to_string(),
                    "psys" => "System Agent Power (PSys)".to_string(),
                    other => format!("{other} Power"),
                };
                enh.push(Item::new(label, Kind::Power, 3, w));
            }
        }

        match self.vendor {
            Vendor::Amd => {
                // Per-core energy (Zen 2+): unit = 1 / 2^ESU J.
                if let Some(unit) = rdmsr(0, 0xC001_0299) {
                    let esu = 0.5f64.powi(((unit >> 8) & 0x1F) as i32);
                    let mut kids = Vec::new();
                    for (c, &cpu) in self.core_cpus.clone().iter().enumerate() {
                        let w = rdmsr(cpu, 0xC001_029A).and_then(|e| self.rate(format!("amd core {c}"), now, e & 0xFFFF_FFFF, 0xFFFF_FFFF)).map(|r| r * esu);
                        kids.push(Item::new(format!("Core {c} Power"), Kind::Power, 3, w));
                    }
                    if !kids.is_empty() {
                        enh.push(Item::parent("Core Powers", Kind::Power, 3, kids));
                    }
                }
            }
            Vendor::Intel => {
                // VIDs per thread (IA32_PERF_STATUS[47:32] / 2^13 V).
                let vids: Vec<Item> = (0..self.labels.len())
                    .map(|i| Item::new(format!("{} VID", self.labels[i]), Kind::Volt, 3, rdmsr(i, 0x198).map(|v| ((v >> 32) & 0xFFFF) as f64 / 8192.0).filter(|&v| v > 0.0)))
                    .collect();
                if vids.iter().any(|i| i.value.is_some()) {
                    main.push(Item::parent("Core VIDs", Kind::Volt, 3, vids));
                }
                if let Some(r) = rdmsr(0, 0x621).map(|v| (v & 0x7F) as f64).filter(|&r| r > 0.0) {
                    main.push(Item::new("Uncore Ratio", Kind::Other, 1, Some(r)).unit("x"));
                    main.push(Item::new("Ring/LLC Clock", Kind::Clock, 1, Some(r * 100.0)));
                }
                // Thermal status per core and package.
                let mut thr = Vec::new();
                let mut crit = Vec::new();
                let mut plim = Vec::new();
                let mut dist = Vec::new();
                for (c, &cpu) in self.core_cpus.iter().enumerate() {
                    let Some(s) = rdmsr(cpu, 0x19C) else { continue };
                    thr.push(Item::new(format!("Core {c}"), Kind::Bool, 0, Some((s & 1) as f64)));
                    crit.push(Item::new(format!("Core {c}"), Kind::Bool, 0, Some(((s >> 4) & 1) as f64)));
                    plim.push(Item::new(format!("Core {c}"), Kind::Bool, 0, Some(((s >> 10) & 1) as f64)));
                    if s & (1 << 31) != 0 {
                        dist.push(Item::new(format!("Core {c}"), Kind::Temp, 0, Some(((s >> 16) & 0x7F) as f64)));
                    }
                }
                let any = |v: &[Item]| Some(if v.iter().any(|i| i.value == Some(1.0)) { 1.0 } else { 0.0 });
                if !thr.is_empty() {
                    let mk = |n: &str, kids: Vec<Item>| {
                        let mut p = Item::new(n, Kind::Bool, 0, any(&kids));
                        p.children = kids;
                        p
                    };
                    enh.push(mk("Core Thermal Throttling", thr));
                    enh.push(mk("Core Critical Temperature", crit));
                    enh.push(mk("Core Power Limit Exceeded", plim));
                }
                if !dist.is_empty() {
                    let min = dist.iter().filter_map(|i| i.value).reduce(f64::min);
                    let mut p = Item::new("Core Distance to TjMAX", Kind::Temp, 0, min);
                    p.children = dist;
                    enh.push(p);
                }
                if let Some(s) = rdmsr(0, 0x1B1) {
                    enh.push(Item::new("Package/Ring Thermal Throttling", Kind::Bool, 0, Some((s & 1) as f64)));
                    enh.push(Item::new("Package/Ring Critical Temperature", Kind::Bool, 0, Some(((s >> 4) & 1) as f64)));
                    enh.push(Item::new("Package/Ring Power Limit Exceeded", Kind::Bool, 0, Some(((s >> 10) & 1) as f64)));
                    if let (Some(tj), true) = (self.tjmax, s & (1 << 31) != 0) {
                        enh.push(Item::new("CPU Package", Kind::Temp, 0, Some(tj - ((s >> 16) & 0x7F) as f64)));
                    }
                }
                // C-state residency: counters tick at TSC rate while in the state.
                let core_cpus = self.core_cpus.clone();
                if let Some(tsc) = rdmsr(0, 0x10) {
                    let dt = self.rate("tsc".into(), now, tsc, u64::MAX);
                    let mut pct = |key: &str, cpu: usize, msr: u64| -> Option<f64> {
                        let v = rdmsr(cpu, msr)?;
                        let r = self.rate(format!("{key} {cpu}"), now, v, u64::MAX)?;
                        dt.filter(|&d| d > 0.0).map(|d| (100.0 * r / d).clamp(0.0, 100.0))
                    };
                    for (name, msr) in [("Package C2", 0x60D), ("Package C3", 0x3F8), ("Package C6", 0x3F9), ("Package C7", 0x3FA), ("Package C8", 0x630), ("Package C9", 0x631), ("Package C10", 0x632)] {
                        if let Some(v) = pct(name, 0, msr) {
                            cst.push(Item::new(format!("{name} Residency"), Kind::Usage, 1, Some(v)));
                        }
                    }
                    for (state, msr) in [("C3", 0x3FC), ("C6", 0x3FD), ("C7", 0x3FE)] {
                        let kids: Vec<Item> = core_cpus
                            .iter()
                            .enumerate()
                            .filter_map(|(c, &cpu)| pct(state, cpu, msr).map(|v| Item::new(format!("Core {c} {state} Residency"), Kind::Usage, 1, Some(v))))
                            .collect();
                        if !kids.is_empty() {
                            cst.push(Item::parent(format!("Core {state} Residency"), Kind::Usage, 1, kids));
                        }
                    }
                }
            }
            Vendor::Other => {}
        }
        [("", main), (": C-State Residency", cst), (": Enhanced", enh)]
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(suffix, items)| Group { title: format!("CPU [#0]: msr{suffix}"), items })
            .collect()
    }
}
