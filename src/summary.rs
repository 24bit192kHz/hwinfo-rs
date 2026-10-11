//! "System Summary" window data (HWiNFO's summary): board/BIOS, OS, CPU,
//! memory modules, GPUs, drives, network. Unprivileged sysfs/procfs reads,
//! plus memory-module details the root helper writes from dmidecode.

use crate::sources::hwmon::read_trim;
use std::collections::BTreeMap;

pub const MODULES_FILE: &str = "/run/hwinfo-rs/modules.tsv";

type Section = (String, Vec<(String, String)>);

fn kv(rows: &mut Vec<(String, String)>, k: &str, v: Option<String>) {
    if let Some(v) = v.filter(|v| !v.trim().is_empty() && v != "Default string" && v != "To be filled by O.E.M.") {
        rows.push((k.to_string(), v.trim().to_string()));
    }
}

fn human(bytes: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", units[u])
}

pub fn collect() -> Vec<Section> {
    let dmi = |f: &str| read_trim(format!("/sys/class/dmi/id/{f}"));
    let mut out = Vec::new();

    let mut sys = Vec::new();
    kv(&mut sys, "Manufacturer", dmi("sys_vendor"));
    kv(&mut sys, "Product", dmi("product_name"));
    kv(&mut sys, "Motherboard", dmi("board_vendor").zip(dmi("board_name")).map(|(a, b)| format!("{a} {b}")));
    kv(&mut sys, "Board revision", dmi("board_version"));
    kv(&mut sys, "BIOS", dmi("bios_vendor").map(|v| format!("{v} {}", dmi("bios_version").unwrap_or_default())));
    kv(&mut sys, "BIOS date", dmi("bios_date"));
    out.push(("System".to_string(), sys));

    let mut os = Vec::new();
    let osr = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    kv(&mut os, "Distribution", osr.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|s| s.trim_matches('"').to_string()));
    kv(&mut os, "Kernel", read_trim("/proc/sys/kernel/osrelease"));
    kv(&mut os, "Hostname", read_trim("/proc/sys/kernel/hostname"));
    kv(&mut os, "Uptime", read_trim("/proc/uptime").and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok()).map(|s| format!("{}d {}h {}m", (s / 86400.0) as u64, (s / 3600.0) as u64 % 24, (s / 60.0) as u64 % 60)));
    kv(&mut os, "Session", std::env::var("XDG_SESSION_TYPE").ok());
    out.push(("Operating System".to_string(), os));

    let mut cpu = Vec::new();
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |k: &str| info.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string());
    kv(&mut cpu, "Processor", field("model name"));
    kv(&mut cpu, "Vendor", field("vendor_id"));
    kv(&mut cpu, "Family / Model / Stepping", field("cpu family").map(|f| format!("{f} / {} / {}", field("model\t").unwrap_or_default(), field("stepping").unwrap_or_default())));
    kv(&mut cpu, "Microcode", field("microcode"));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    kv(&mut cpu, "Cores / Threads", field("cpu cores").map(|c| format!("{c} / {threads}")));
    kv(&mut cpu, "Base clock", read_trim("/sys/devices/system/cpu/cpu0/acpi_cppc/nominal_freq").map(|m| format!("{m} MHz")));
    kv(&mut cpu, "Max boost", read_trim("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").and_then(|k| k.parse::<f64>().ok()).map(|k| format!("{:.0} MHz", k / 1000.0)));
    kv(&mut cpu, "Frequency driver", read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver"));
    let mut caches = Vec::new();
    for i in 0..8 {
        let b = format!("/sys/devices/system/cpu/cpu0/cache/index{i}");
        let (Some(l), Some(t), Some(sz)) = (read_trim(format!("{b}/level")), read_trim(format!("{b}/type")), read_trim(format!("{b}/size"))) else { break };
        let shared = read_trim(format!("{b}/shared_cpu_list")).unwrap_or_default();
        caches.push(format!("L{l} {} {sz} (cpus {shared})", if t == "Unified" { "" } else { &t[..1] }));
    }
    kv(&mut cpu, "Caches (per instance)", (!caches.is_empty()).then(|| caches.join(", ")));
    let flags = field("flags").unwrap_or_default();
    let ext: Vec<&str> = ["sse4_2", "avx", "avx2", "fma", "avx512f", "avx512_vnni", "sha_ni", "aes", "vaes", "svm", "vmx"].into_iter().filter(|f| flags.split_whitespace().any(|x| x == *f)).collect();
    kv(&mut cpu, "Instruction sets", Some(ext.join(" ")));
    out.push(("CPU".to_string(), cpu));

    let mut mem = Vec::new();
    let mi = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    kv(&mut mem, "Total", mi.lines().find_map(|l| l.strip_prefix("MemTotal:")).and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok()).map(|k| format!("{:.1} GiB", k / 1048576.0)));
    match std::fs::read_to_string(MODULES_FILE) {
        Ok(s) => {
            for l in s.lines() {
                if let Some((k, v)) = l.split_once('\t') {
                    mem.push((k.to_string(), v.to_string()));
                }
            }
        }
        Err(_) => mem.push(("Modules".into(), "(run hwinfo-rs-helper for module details)".into())),
    }
    out.push(("Memory".to_string(), mem));

    let mut gpu = Vec::new();
    if std::env::var_os("HWINFO_RS_NO_NVIDIA").is_none() {
        if let Ok(nvml) = nvml_wrapper::Nvml::init() {
            kv(&mut gpu, "NVIDIA driver", nvml.sys_driver_version().ok());
            for i in 0..nvml.device_count().unwrap_or(0) {
                let Ok(d) = nvml.device_by_index(i) else { continue };
                let p = format!("GPU #{i}");
                kv(&mut gpu, &p, d.name().ok());
                kv(&mut gpu, &format!("{p} VBIOS"), d.vbios_version().ok());
                kv(&mut gpu, &format!("{p} VRAM"), d.memory_info().ok().map(|m| format!("{:.0} MiB", m.total as f64 / 1048576.0)));
                kv(&mut gpu, &format!("{p} PCIe"), d.max_pcie_link_gen().ok().zip(d.max_pcie_link_width().ok()).map(|(g, w)| format!("Gen {g} x{w} max")));
                kv(&mut gpu, &format!("{p} Serial"), d.serial().ok());
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
        let mut cards: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("card") && !n.to_string_lossy().contains('-'))).collect();
        cards.sort();
        for c in cards {
            let dev = c.join("device");
            let hex = |f: &str| read_trim(dev.join(f)).and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            let drv = std::fs::canonicalize(dev.join("driver")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
            if drv.as_deref() == Some("nvidia") {
                continue;
            }
            let name = hex("vendor").zip(hex("device")).and_then(|(v, d)| crate::sources::gpu_other::pci_name(v, d));
            kv(&mut gpu, &c.file_name().unwrap().to_string_lossy(), name.map(|n| format!("{n} ({})", drv.unwrap_or_default())));
        }
    }
    out.push(("Graphics".to_string(), gpu));

    let mut drives = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/block") {
        let mut devs: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| std::path::Path::new(&format!("/sys/block/{n}/device")).exists()).collect();
        devs.sort();
        for d in devs {
            let b = format!("/sys/block/{d}");
            let model = read_trim(format!("{b}/device/model")).unwrap_or_else(|| d.clone());
            let size = read_trim(format!("{b}/size")).and_then(|s| s.parse::<f64>().ok()).map(|s| human(s * 512.0));
            let rot = read_trim(format!("{b}/queue/rotational")).map(|r| if r == "1" { "HDD" } else { "SSD" });
            let fw = read_trim(format!("{b}/device/firmware_rev")).or_else(|| read_trim(format!("{b}/device/rev")));
            drives.push((d.clone(), format!("{model}, {}, {}{}", size.unwrap_or_default(), rot.unwrap_or(""), fw.map(|f| format!(", fw {f}")).unwrap_or_default())));
            kv(&mut drives, &format!("{d} Serial"), read_trim(format!("{b}/device/serial")));
        }
    }
    out.push(("Drives".to_string(), drives));

    let mut net = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        let mut nics: BTreeMap<String, String> = BTreeMap::new();
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            if !p.join("device").exists() {
                continue;
            }
            let drv = std::fs::canonicalize(p.join("device/driver")).ok().and_then(|x| x.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
            let speed = read_trim(p.join("speed")).filter(|s| !s.starts_with('-')).map(|s| format!(", {s} Mbps")).unwrap_or_default();
            nics.insert(n.clone(), format!("{drv}{speed}"));
            nics.insert(format!("{n} MAC"), read_trim(p.join("address")).unwrap_or_default());
        }
        net.extend(nics);
    }
    out.push(("Network".to_string(), net));
    out
}

/// Root helper: memory modules from dmidecode (type 17), written once at start.
pub fn write_modules() {
    let Ok(o) = std::process::Command::new("dmidecode").args(["-t", "17"]).output() else { return };
    let text = String::from_utf8_lossy(&o.stdout);
    let mut lines = Vec::new();
    for dev in text.split("\n\n").filter(|d| d.contains("Memory Device")) {
        let f = |k: &str| dev.lines().find_map(|l| l.trim().strip_prefix(&format!("{k}: "))).map(str::trim).filter(|v| !v.is_empty() && *v != "Unknown" && *v != "Not Specified");
        let Some(size) = f("Size").filter(|s| !s.starts_with("No Module")) else { continue };
        let slot = [f("Bank Locator"), f("Locator")].into_iter().flatten().collect::<Vec<_>>().join(" / ");
        let desc = [Some(size), f("Type"), f("Configured Memory Speed").or(f("Speed")), f("Manufacturer"), f("Part Number"), f("Rank").map(|r| if r == "1" { "1 rank" } else { "2 ranks" })]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("{slot}\t{desc}"));
        if let Some(sn) = f("Serial Number").filter(|s| s.trim_start_matches('0').len() > 0) {
            lines.push(format!("{slot} Serial\t{sn}"));
        }
    }
    let _ = std::fs::create_dir_all("/run/hwinfo-rs");
    let _ = std::fs::write(MODULES_FILE, lines.join("\n"));
}
