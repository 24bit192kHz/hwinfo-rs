//! CPU: per-thread usage from /proc/stat, plus the AMD SMU power table
//! (ryzen_smu pm_table; layouts for Zen 1-3 desktop/APU parts generated from
//! ryzen_monitor_ng into pm_tables.rs) for HWiNFO's "Enhanced" / C-state rows.
//! Without a known pm_table (Intel, other AMD) the CPU temperature driver's
//! readings are shown and the root helper adds MSR-based rows.

use crate::model::{Item, Kind};
use crate::sources::hwmon::read_trim;
use crate::sources::pm_tables::{Layout, LAYOUTS};

const PM_TABLE: &str = "/sys/kernel/ryzen_smu_drv/pm_table";
const PM_VERSION: &str = "/sys/kernel/ryzen_smu_drv/pm_table_version";

pub struct Cpu {
    pub model: String,
    /// Logical CPU -> physical core index (0..cores), and SMT thread number.
    threads: Vec<(usize, usize)>,
    cores: usize,
    prev: Vec<(u64, u64)>,
    layout: Option<&'static Layout>,
    /// pm_table core slots that are populated (fused-off cores read 0).
    slots: Vec<usize>,
    /// Nominal (base) clock in MHz, the 100% point of Windows' "utility".
    nominal: f64,
}

/// One pm_table snapshot, read by field name through the version's layout.
struct Pm<'a> {
    t: Vec<f32>,
    layout: &'a Layout,
    slots: &'a [usize],
}

impl Pm<'_> {
    fn idx(&self, name: &str) -> Option<&'static [u16]> {
        self.layout.fields.iter().find(|(n, _)| *n == name).map(|(_, i)| *i)
    }

    fn get(&self, name: &str) -> Option<f64> {
        let i = *self.idx(name)?.first()? as usize;
        self.t.get(i).map(|&v| v as f64).filter(|v| v.is_finite())
    }

    /// Per-core field for the populated slots, in Linux core order.
    fn cores(&self, name: &str, scale: f64) -> Vec<Option<f64>> {
        let Some(ix) = self.idx(name) else { return Vec::new() };
        self.slots.iter().map(|&s| ix.get(s).and_then(|&i| self.t.get(i as usize)).map(|&v| v as f64 * scale)).collect()
    }

    fn l3(&self, name: &str, scale: f64) -> Vec<Option<f64>> {
        let Some(ix) = self.idx(name) else { return Vec::new() };
        ix.iter().map(|&i| self.t.get(i as usize).map(|&v| v as f64 * scale)).filter(|v| v.is_some_and(|x| x != 0.0)).collect()
    }
}

impl Cpu {
    pub fn new() -> Self {
        let model = std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
            .unwrap_or_else(|| "CPU".into());
        // HWiNFO drops AMD's " 16-Core Processor" suffix and Intel's "(R)/(TM)".
        let model = match model.rfind("-Core Processor") {
            Some(i) => model[..i].rsplit_once(' ').map(|(a, _)| a.to_string()).unwrap_or(model),
            None => model,
        };
        let model = model.replace("(R)", "").replace("(TM)", "").replace(" CPU ", " ").split(" @ ").next().unwrap_or("").trim().to_string();
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let ids: Vec<(u32, u32)> = (0..n)
            .map(|i| {
                let t = format!("/sys/devices/system/cpu/cpu{i}/topology");
                let core = read_trim(format!("{t}/core_id")).and_then(|s| s.parse().ok()).unwrap_or(i as u32);
                let die = read_trim(format!("{t}/die_id")).and_then(|s| s.parse().ok()).unwrap_or(0);
                (die, core)
            })
            .collect();
        let mut uniq = ids.clone();
        uniq.sort();
        uniq.dedup();
        let mut seen = vec![0usize; uniq.len()];
        let threads = ids
            .iter()
            .map(|dc| {
                let core = uniq.iter().position(|u| u == dc).unwrap();
                let t = seen[core];
                seen[core] += 1;
                (core, t)
            })
            .collect();
        let cores = uniq.len();
        let layout = read_pm_version().and_then(|v| LAYOUTS.iter().find(|l| l.version == v));
        // Populated slots: cores with a non-zero temperature in the first sample.
        let slots = layout
            .and_then(|l| {
                let t = read_pm()?;
                let ix = l.fields.iter().find(|(n, _)| *n == "CORE_TEMP")?.1;
                let s: Vec<usize> = (0..l.max_cores).filter(|&c| ix.get(c).and_then(|&i| t.get(i as usize)).is_some_and(|&v| v > 0.0)).collect();
                Some(if s.len() >= cores { s[..cores].to_vec() } else { s })
            })
            .unwrap_or_default();
        let nominal = read_trim("/sys/devices/system/cpu/cpu0/acpi_cppc/nominal_freq")
            .or_else(|| read_trim("/sys/devices/system/cpu/cpu0/cpufreq/base_frequency").map(|k| (k.parse::<f64>().unwrap_or(0.0) / 1000.0).to_string()))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        Cpu { model, threads, cores, prev: Vec::new(), layout, slots, nominal }
    }


    /// "Core C Tt" for logical CPU `i` (same naming as HWiNFO).
    pub fn thread_label(&self, i: usize) -> String {
        let (c, t) = self.threads.get(i).copied().unwrap_or((i, 0));
        format!("Core {c} T{t}")
    }

    fn usage(&mut self) -> Vec<Option<f64>> {
        let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let mut cur = Vec::new();
        for l in stat.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")) {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            let total: u64 = v.iter().take(8).sum();
            cur.push((total, idle));
        }
        let out = cur
            .iter()
            .enumerate()
            .map(|(i, &(t, id))| {
                let &(pt, pid) = self.prev.get(i)?;
                let dt = t.saturating_sub(pt);
                (dt > 0).then(|| 100.0 * (1.0 - id.saturating_sub(pid) as f64 / dt as f64))
            })
            .collect();
        self.prev = cur;
        out
    }

    /// Returns the items for HWiNFO's three CPU groups:
    /// (clocks/usage, C-State Residency, Enhanced). `temps` are the CPU
    /// temperature driver's rows (k10temp / coretemp / zenpower).
    pub fn sample(&mut self, temps: &[Item]) -> (Vec<Item>, Vec<Item>, Vec<Item>) {
        let usage = self.usage();
        let pm = self.layout.and_then(|l| Some(Pm { t: read_pm()?, layout: l, slots: &self.slots }));
        let kids = |vals: Vec<Option<f64>>, kind: Kind, dec: usize, suffix: &str| -> Vec<Item> {
            vals.into_iter().enumerate().map(|(c, v)| Item::new(format!("Core {c}{suffix}"), kind, dec, v)).collect()
        };
        let ghz = 1000.0;

        let mut main = Vec::new();
        let mut cstate = Vec::new();
        let mut enh = Vec::new();

        if let Some(p) = &pm {
            main.push(Item::parent("Core VIDs", Kind::Volt, 3, kids(p.cores("CORE_VOLTAGE", 1.0), Kind::Volt, 3, " VID")));
            main.push(Item::parent("Core Clocks", Kind::Clock, 1, kids(p.cores("CORE_FREQ", ghz), Kind::Clock, 1, " Clock")));
            main.push(Item::new("Bus Clock", Kind::Clock, 1, Some(100.0)));
            let eff = kids(p.cores("CORE_FREQEFF", ghz), Kind::Clock, 1, " Effective Clock");
            main.push(Item::parent("Core Effective Clocks", Kind::Clock, 1, eff.clone()));
            main.push(Item::parent("Average Effective Clock", Kind::Clock, 1, eff).with_no_children());
        } else {
            // Generic: cpufreq's current clock per core (first thread of each core).
            let mut per_core = vec![None; self.cores];
            for (i, &(c, t)) in self.threads.iter().enumerate() {
                if t == 0 {
                    per_core[c] = read_trim(format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")).and_then(|k| k.parse::<f64>().ok()).map(|k| k / 1000.0);
                }
            }
            main.push(Item::parent("Core Clocks", Kind::Clock, 1, kids(per_core, Kind::Clock, 1, " Clock")));
        }
        let thread_usage: Vec<Item> = usage.iter().enumerate().map(|(i, &u)| Item::new(format!("{} Usage", self.thread_label(i)), Kind::Usage, 1, u)).collect();
        let max_thread = usage.iter().flatten().copied().reduce(f64::max);
        main.push(Item::parent("Core Usage", Kind::Usage, 1, thread_usage.clone()));
        main.push(Item::new("Max CPU/Thread Usage", Kind::Usage, 1, max_thread));
        main.push(Item::parent("Total CPU Usage", Kind::Usage, 1, thread_usage).with_no_children());

        if let Some(p) = &pm {
            if self.nominal > 0.0 {
                let util = p.cores("CORE_FREQEFF", ghz).into_iter().map(|v| v.map(|c| 100.0 * c / self.nominal)).collect();
                let k = kids(util, Kind::Usage, 1, " Utility");
                main.push(Item::parent("Core Utility", Kind::Usage, 1, k.clone()));
                main.push(Item::parent("Total CPU Utility", Kind::Usage, 1, k).with_no_children());
            }
            let ratios = kids(p.cores("CORE_FREQ", 10.0), Kind::Other, 1, " Ratio").into_iter().map(|i| i.unit("x")).collect();
            main.push(Item::parent("Core Ratios", Kind::Other, 1, ratios));

            cstate.push(Item::new("Package C6 Residency", Kind::Usage, 1, p.get("PC6")));
            cstate.push(Item::parent("Core C0 Residency", Kind::Usage, 1, kids(p.cores("CORE_C0", 1.0), Kind::Usage, 1, " C0 Residency")));
            cstate.push(Item::parent("Core C1 Residency", Kind::Usage, 1, kids(p.cores("CORE_CC1", 1.0), Kind::Usage, 1, " C1 Residency")));
            cstate.push(Item::parent("Core C6 Residency", Kind::Usage, 1, kids(p.cores("CORE_CC6", 1.0), Kind::Usage, 1, " C6 Residency")));
        }

        let t = |label: &str| temps.iter().find(|i| i.name == label).and_then(|i| i.value);
        if let Some(v) = t("Tctl").or(t("Tdie")) {
            enh.push(Item::new("CPU (Tctl/Tdie)", Kind::Temp, 1, Some(v)));
        }
        if let Some(p) = &pm {
            enh.push(Item::new("CPU Die (average)", Kind::Temp, 1, p.get("THM_VALUE")));
            for i in temps.iter().filter(|i| i.name.starts_with("Tccd")) {
                enh.push(Item::new(format!("CPU CCD{} (Tdie)", &i.name[4..]), Kind::Temp, 1, i.value));
            }
            let ccds = temps.iter().filter(|i| i.name.starts_with("Tccd")).count().max(1);
            let per = (self.slots.len() / ccds).max(1);
            let core_t: Vec<Item> = p.cores("CORE_TEMP", 1.0).into_iter().enumerate().map(|(c, v)| Item::new(format!("Core{c} (CCD{})", c / per + 1), Kind::Temp, 1, v)).collect();
            let max = core_t.iter().filter_map(|i| i.value).reduce(f64::max);
            enh.push(Item::parent("Core Temperatures", Kind::Temp, 1, core_t));
            enh.push(Item::new("Core Max", Kind::Temp, 1, max));
            let l3: Vec<Item> = p.l3("L3_TEMP", 1.0).into_iter().enumerate().map(|(i, v)| Item::new(format!("L3 Cache (CCD{})", i + 1), Kind::Temp, 1, v)).collect();
            if !l3.is_empty() {
                enh.push(Item::parent("L3 Temperatures", Kind::Temp, 1, l3));
            }
            enh.push(Item::new("CPU IOD Hotspot", Kind::Temp, 1, p.get("SOC_TEMP")));
            enh.push(Item::new("CPU Core Voltage (SVI2 TFN)", Kind::Volt, 3, p.get("CPU_TELEMETRY_VOLTAGE")));
            enh.push(Item::new("SoC Voltage (SVI2 TFN)", Kind::Volt, 3, p.get("SOC_TELEMETRY_VOLTAGE")));
            enh.push(Item::new("CPU Core VID (Effective)", Kind::Volt, 3, p.get("CPU_SET_VOLTAGE")));
            enh.push(Item::new("CPU Core Current (SVI2 TFN)", Kind::Current, 3, p.get("CPU_TELEMETRY_CURRENT")));
            enh.push(Item::new("SoC Current (SVI2 TFN)", Kind::Current, 3, p.get("SOC_TELEMETRY_CURRENT")));
            enh.push(Item::new("CPU TDC", Kind::Current, 3, p.get("TDC_VALUE")));
            enh.push(Item::new("CPU EDC", Kind::Current, 3, p.get("EDC_VALUE")));
            enh.push(Item::new("CPU Package Power", Kind::Power, 3, p.get("SOCKET_POWER")));
            let core_p = p.cores("CORE_POWER", 1.0);
            let core_sum: f64 = core_p.iter().flatten().sum();
            enh.push(Item::parent("Core Powers", Kind::Power, 3, kids(core_p, Kind::Power, 3, " Power")));
            let (cpu_w, soc_w) = (p.get("CPU_TELEMETRY_POWER"), p.get("SOC_TELEMETRY_POWER"));
            enh.push(Item::new("CPU Core Power (SVI2 TFN)", Kind::Power, 3, cpu_w));
            enh.push(Item::new("CPU SoC Power (SVI2 TFN)", Kind::Power, 3, soc_w));
            enh.push(Item::new("Core+SoC Power (SVI2 TFN)", Kind::Power, 3, cpu_w.zip(soc_w).map(|(a, b)| a + b)));
            enh.push(Item::new("CPU PPT", Kind::Power, 3, p.get("PPT_VALUE")));
            // HWiNFO (RE round 3): PRD = 100 * SVI2 core power / sum of per-core powers.
            enh.push(Item::new("Power Reporting Deviation (Accuracy)", Kind::Usage, 1, cpu_w.filter(|_| core_sum > 0.0).map(|w| 100.0 * w / core_sum)));
            let pct = |v: &str, l: &str| p.get(v).zip(p.get(l)).filter(|&(_, l)| l > 0.0).map(|(v, l)| 100.0 * v / l);
            enh.push(Item::new("Infinity Fabric Clock (FCLK)", Kind::Clock, 1, p.get("FCLK_FREQ")));
            enh.push(Item::new("Memory Controller Clock (UCLK)", Kind::Clock, 1, p.get("UCLK_FREQ")));
            let l3c: Vec<Item> = p.l3("L3_FREQ", ghz).into_iter().enumerate().map(|(i, v)| Item::new(format!("L3 Cache (CCD{}) Clock", i + 1), Kind::Clock, 1, v)).collect();
            if !l3c.is_empty() {
                enh.push(Item::parent("L3 Clocks", Kind::Clock, 1, l3c));
            }
            enh.push(Item::new("Frequency Limit - Global", Kind::Clock, 1, p.get("CCLK_LIMIT").map(|v| v * ghz)));
            enh.push(Item::new("CPU PPT Limit", Kind::Usage, 1, pct("PPT_VALUE", "PPT_LIMIT")));
            enh.push(Item::new("CPU TDC Limit", Kind::Usage, 1, pct("TDC_VALUE", "TDC_LIMIT")));
            enh.push(Item::new("CPU EDC Limit", Kind::Usage, 1, pct("EDC_VALUE", "EDC_LIMIT")));
            enh.push(Item::new("Thermal Limit", Kind::Usage, 1, pct("THM_VALUE", "THM_LIMIT")));
            enh.push(Item::new("Thermal Throttling (PROCHOT CPU)", Kind::Bool, 0, p.get("PROCHOT").map(|v| if v > 0.0 { 1.0 } else { 0.0 })));
            enh.push(Item::new("DRAM Read Bandwidth", Kind::Other, 3, p.get("CS_UMC_READS")).unit("GB/s"));
            enh.push(Item::new("DRAM Write Bandwidth", Kind::Other, 3, p.get("CS_UMC_WRITES")).unit("GB/s"));
            enh.push(Item::new("Average Active Core Count", Kind::Other, 1, p.get("AVG_CORE_COUNT")));
            enh.push(Item::new("CPU VDDP Voltage", Kind::Volt, 3, p.get("V_VDDP")));
            enh.push(Item::new("CPU VDDG CCD Voltage", Kind::Volt, 3, p.get("V_VDDG_CCD")));
            enh.push(Item::new("CPU VDDG IOD Voltage", Kind::Volt, 3, p.get("V_VDDG_IOD")));
            enh.push(Item::new("PPT Limit (setting)", Kind::Power, 0, p.get("PPT_LIMIT")));
            enh.push(Item::new("TDC Limit (setting)", Kind::Current, 0, p.get("TDC_LIMIT")));
            enh.push(Item::new("EDC Limit (setting)", Kind::Current, 0, p.get("EDC_LIMIT")));
            enh.retain(|i| i.value.is_some() || !i.children.is_empty());
        } else {
            // No SMU table: whatever the CPU temperature driver reports.
            for i in temps.iter().filter(|i| i.name != "Tctl" && i.name != "Tdie") {
                let mut i = i.clone();
                if i.name.starts_with("Package id") {
                    i.name = "CPU Package".into();
                }
                if let Some(rest) = i.name.strip_prefix("Tccd") {
                    i.name = format!("CPU CCD{rest} (Tdie)");
                }
                enh.push(i);
            }
            let core: Vec<Item> = enh.iter().filter(|i| i.name.starts_with("Core ")).cloned().collect();
            if !core.is_empty() {
                enh.retain(|i| !i.name.starts_with("Core "));
                let max = core.iter().filter_map(|i| i.value).reduce(f64::max);
                enh.push(Item::parent("Core Temperatures", Kind::Temp, 1, core));
                enh.push(Item::new("Core Max", Kind::Temp, 1, max));
            }
        }
        (main, cstate, enh)
    }
}

impl Item {
    /// Keep the averaged value but drop the per-core rows (summary lines).
    pub fn with_no_children(mut self) -> Self {
        self.children.clear();
        self
    }
}

fn read_pm_version() -> Option<u32> {
    let b = std::fs::read(PM_VERSION).ok()?;
    Some(u32::from_le_bytes(b.get(0..4)?.try_into().ok()?))
}

fn read_pm() -> Option<Vec<f32>> {
    let b = std::fs::read(PM_TABLE).ok()?;
    Some(b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
}
