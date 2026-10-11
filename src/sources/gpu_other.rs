//! AMD (amdgpu) and Intel (i915 / xe) GPUs from sysfs + their hwmon chips,
//! and laptop batteries from power_supply. All unprivileged reads.

use crate::model::{Item, Kind};
use crate::sources::hwmon::{read_trim, Chip};
use std::path::{Path, PathBuf};

/// Vendor/device name from the system pci.ids (hwdata), like lspci.
pub fn pci_name(vendor: u16, device: u16) -> Option<String> {
    static DB: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let db = DB.get_or_init(|| {
        ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    });
    let v = format!("{vendor:04x}  ");
    let d = format!("\t{device:04x}  ");
    let mut lines = db.lines().skip_while(|l| !l.starts_with(&v));
    let vname = lines.next()?.get(6..)?.trim().to_string();
    let dname = lines.take_while(|l| l.starts_with('\t') || l.starts_with('#') || l.is_empty()).find(|l| l.starts_with(&d))?.get(7..)?.trim().to_string();
    // "Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]" -> prefer the bracketed marketing name.
    let pretty = match (dname.find('['), dname.rfind(']')) {
        (Some(a), Some(b)) if b > a => dname[a + 1..b].to_string(),
        _ => dname,
    };
    let vshort = vname.split_whitespace().next().unwrap_or("").replace(",", "");
    Some(format!("{vshort} {pretty}"))
}

fn hex(p: &Path) -> Option<u16> {
    u16::from_str_radix(read_trim(p)?.trim_start_matches("0x"), 16).ok()
}

fn num(p: impl AsRef<Path>) -> Option<f64> {
    read_trim(p)?.parse().ok()
}

/// "* " marks the active level in pp_dpm_* tables: "1: 1500Mhz *".
fn dpm_current(p: &Path) -> Option<f64> {
    let s = std::fs::read_to_string(p).ok()?;
    let l = s.lines().find(|l| l.trim_end().ends_with('*'))?;
    l.split_whitespace().nth(1)?.trim_end_matches(|c: char| c.is_alphabetic()).parse().ok()
}

/// One group per non-NVIDIA DRM GPU: (title, items).
pub fn sample(chips: &[Chip]) -> Vec<(String, Vec<Item>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/drm") else { return out };
    let mut cards: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("card") && !n.to_string_lossy().contains('-')))
        .collect();
    cards.sort();
    for card in cards {
        let dev = card.join("device");
        let Some(vendor) = hex(&dev.join("vendor")) else { continue };
        let driver = std::fs::canonicalize(dev.join("driver")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
        if !matches!(driver.as_str(), "amdgpu" | "i915" | "xe") {
            continue;
        }
        let device = hex(&dev.join("device")).unwrap_or(0);
        let name = pci_name(vendor, device).unwrap_or_else(|| format!("{driver} GPU"));
        let devpath = std::fs::canonicalize(&dev).ok();
        let mut v = Vec::new();
        // The card's own hwmon chip (temps, power, fans, voltages), matched by device path.
        for c in chips.iter().filter(|c| devpath.is_some() && std::fs::canonicalize(c.path.join("device")).ok() == devpath) {
            for it in &c.items {
                let mut it = it.clone();
                it.name = match (driver.as_str(), it.name.as_str()) {
                    (_, "edge") => "GPU Temperature".into(),
                    (_, "junction") => "GPU Hot Spot Temperature".into(),
                    (_, "mem") => "GPU Memory Temperature".into(),
                    (_, "vddgfx") => "GPU Core Voltage".into(),
                    (_, "vddnb") => "GPU SoC Voltage".into(),
                    (_, "PPT") | (_, "power1") => "GPU Power".into(),
                    (_, "fan1") => "GPU Fan".into(),
                    (_, n) => format!("GPU {n}"),
                };
                v.push(it);
            }
            for (f, label) in [("freq1_input", "GPU Clock"), ("freq2_input", "GPU Memory Clock")] {
                if let Some(hz) = num(c.path.join(f)) {
                    v.push(Item::new(label, Kind::Clock, 1, Some(hz / 1e6)));
                }
            }
        }
        match driver.as_str() {
            "amdgpu" => {
                if !v.iter().any(|i| i.name == "GPU Clock") {
                    v.push(Item::new("GPU Clock", Kind::Clock, 1, dpm_current(&dev.join("pp_dpm_sclk"))));
                    v.push(Item::new("GPU Memory Clock", Kind::Clock, 1, dpm_current(&dev.join("pp_dpm_mclk"))));
                }
                v.push(Item::new("GPU Core Load", Kind::Usage, 1, num(dev.join("gpu_busy_percent"))));
                v.push(Item::new("GPU Memory Controller Load", Kind::Usage, 1, num(dev.join("mem_busy_percent"))));
                let used = num(dev.join("mem_info_vram_used"));
                let total = num(dev.join("mem_info_vram_total"));
                v.push(Item::new("GPU Memory Allocated", Kind::Memory, 0, used.map(|b| b / 1048576.0)));
                v.push(Item::new("GPU Memory Usage", Kind::Usage, 1, used.zip(total).filter(|&(_, t)| t > 0.0).map(|(u, t)| 100.0 * u / t)));
            }
            "i915" => {
                v.push(Item::new("GPU Clock", Kind::Clock, 1, num(card.join("gt_act_freq_mhz"))));
                v.push(Item::new("GPU Requested Clock", Kind::Clock, 1, num(card.join("gt_cur_freq_mhz"))));
                v.push(Item::new("GPU Max Clock", Kind::Clock, 1, num(card.join("gt_max_freq_mhz"))));
            }
            "xe" => {
                let f = dev.join("tile0/gt0/freq0");
                v.push(Item::new("GPU Clock", Kind::Clock, 1, num(f.join("act_freq"))));
                v.push(Item::new("GPU Requested Clock", Kind::Clock, 1, num(f.join("cur_freq"))));
            }
            _ => {}
        }
        v.push(Item::new("PCIe Link Speed", Kind::Other, 1, read_trim(dev.join("current_link_speed")).and_then(|s| s.split_whitespace().next()?.parse().ok())).unit("GT/s"));
        v.retain(|i| i.value.is_some());
        if !v.is_empty() {
            let n = out.len();
            out.push((format!("GPU [#{n}]: {name}"), v));
        }
    }
    out
}

/// Laptop batteries (HWiNFO's "Battery" group).
pub fn batteries() -> Vec<(String, Vec<Item>)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") else { return out };
    let mut bats: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| read_trim(p.join("type")).as_deref() == Some("Battery") && read_trim(p.join("scope")).as_deref() != Some("Device")).collect();
    bats.sort();
    for b in bats {
        let n = |f: &str| num(b.join(f));
        // energy_* (uWh) or charge_* (uAh, then x voltage).
        let volt = n("voltage_now").map(|uv| uv / 1e6);
        let wh = |e: &str, c: &str| n(e).map(|x| x / 1e6).or_else(|| n(c).zip(n("voltage_min_design").or(n("voltage_now"))).map(|(c, v)| c / 1e6 * v / 1e6));
        let (now, full, design) = (wh("energy_now", "charge_now"), wh("energy_full", "charge_full"), wh("energy_full_design", "charge_full_design"));
        let rate = n("power_now").map(|p| p / 1e6).or_else(|| n("current_now").zip(volt).map(|(i, v)| i / 1e6 * v));
        let charging = read_trim(b.join("status")).map(|s| s == "Charging");
        let mut v = vec![
            Item::new("Charge Level", Kind::Usage, 1, n("capacity")),
            Item::new("Remaining Capacity", Kind::Other, 2, now).unit("Wh"),
            Item::new("Full Charged Capacity", Kind::Other, 2, full).unit("Wh"),
            Item::new("Designed Capacity", Kind::Other, 2, design).unit("Wh"),
            Item::new("Wear Level", Kind::Usage, 1, full.zip(design).filter(|&(_, d)| d > 0.0).map(|(f, d)| (100.0 * (1.0 - f / d)).max(0.0))),
            Item::new("Battery Voltage", Kind::Volt, 3, volt),
            Item::new(if charging == Some(true) { "Charge Rate" } else { "Discharge Rate" }, Kind::Power, 3, rate),
            Item::new("Charging", Kind::Bool, 0, charging.map(|c| c as u8 as f64)),
            Item::new("Charge Cycles", Kind::Other, 0, n("cycle_count")),
        ];
        v.retain(|i| i.value.is_some());
        let model = [read_trim(b.join("manufacturer")), read_trim(b.join("model_name"))].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let id = b.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        out.push((format!("Battery: {} ({id})", if model.is_empty() { "Battery".into() } else { model }), v));
    }
    out
}
