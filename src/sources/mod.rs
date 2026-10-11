pub mod cpu;
pub mod gpu;
pub mod gpu_other;
pub mod hwmon;
pub mod nvapi;
pub mod pm_tables;
pub mod system;

use crate::model::{Group, Item, Kind};
use hwmon::{read_trim, Chip};
use std::path::{Path, PathBuf};

pub struct Collector {
    board: String,
    hwmon: hwmon::Hwmon,
    cpu: cpu::Cpu,
    gpu: gpu::Gpu,
    disks: system::Disks,
    net: system::Net,
}

fn canon(p: impl AsRef<Path>) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

impl Collector {
    pub fn new() -> Self {
        let dmi = |f| read_trim(format!("/sys/class/dmi/id/{f}")).unwrap_or_default();
        let mut vendor = dmi("board_vendor");
        for suffix in [" Technology Co., Ltd.", " COMPUTER INC.", " Co., Ltd.", " Corporation", " Inc."] {
            vendor = vendor.replace(suffix, "");
        }
        Collector {
            board: format!("{vendor} {}", dmi("board_name")).trim().to_string(),
            hwmon: hwmon::Hwmon::new(),
            cpu: cpu::Cpu::new(),
            gpu: gpu::Gpu::new(),
            disks: system::Disks::new(),
            net: system::Net::new(),
        }
    }

    pub fn sample(&mut self) -> Vec<Group> {
        let prof = std::env::var_os("HWINFO_RS_PROFILE").is_some();
        let mut tick = std::time::Instant::now();
        let mut lap = |what: &str| {
            if prof {
                eprintln!("  {what}: {:.2} ms", tick.elapsed().as_secs_f64() * 1000.0);
            }
            tick = std::time::Instant::now();
        };
        let chips = self.hwmon.sample();
        lap("hwmon");
        let mut privileged = crate::helper::read_priv();
        let by = |p: &'static str| chips.iter().filter(move |c| c.prefix == p);

        let mut groups = vec![Group { title: format!("System: {}", self.board), items: system::memory() }];

        // CPU temperature driver: k10temp / zenpower (AMD) or coretemp (Intel).
        let k10: Vec<Item> = chips
            .iter()
            .filter(|c| matches!(c.prefix.as_str(), "k10temp" | "zenpower" | "coretemp"))
            .flat_map(|c| c.items.clone())
            .collect();
        lap("helper+memory");
        let (mut main, mut cstate, mut enh) = self.cpu.sample(&k10);
        lap("cpu");
        // Helper's per-thread APERF rows replace the per-core SMU estimates.
        if let Some(i) = privileged.iter().position(|g| g.title == "CPU [#0]: threads") {
            let g = privileged.remove(i);
            let rows = |suffix: &str| -> Vec<Item> {
                let mut v: Vec<(String, Item)> = g
                    .items
                    .iter()
                    .filter_map(|it| {
                        let n: usize = it.name.strip_prefix("cpu")?.strip_suffix(suffix)?.parse().ok()?;
                        let label = self.cpu.thread_label(n);
                        let mut it = it.clone();
                        it.name = format!("{label}{suffix}");
                        Some((label, it))
                    })
                    .collect();
                // Order like HWiNFO: by core, then thread.
                v.sort_by_key(|(l, _)| {
                    let mut p = l.split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse::<usize>().ok());
                    (p.next().unwrap_or(0), p.next().unwrap_or(0))
                });
                v.into_iter().map(|(_, i)| i).collect()
            };
            for (parent, total, suffix, kind) in [("Core Effective Clocks", "Average Effective Clock", " Effective Clock", Kind::Clock), ("Core Utility", "Total CPU Utility", " Utility", Kind::Usage)] {
                let kids = rows(suffix);
                if kids.is_empty() {
                    continue;
                }
                for (name, children) in [(parent, true), (total, false)] {
                    let mut row = Item::parent(name, kind, 1, kids.clone());
                    if !children {
                        row = row.with_no_children();
                    }
                    // Replace the SMU estimate if present, else add (Intel, Zen 4/5).
                    match main.iter_mut().find(|i| i.name == name) {
                        Some(it) => *it = row,
                        None => main.push(row),
                    }
                }
            }
        }
        // Helper's MSR/powercap rows fill in whatever the SMU path didn't provide.
        for (suffix, target) in [("", &mut main), (": C-State Residency", &mut cstate), (": Enhanced", &mut enh)] {
            let title = format!("CPU [#0]: msr{suffix}");
            if let Some(i) = privileged.iter().position(|g| g.title == title) {
                for it in privileged.remove(i).items {
                    if !target.iter().any(|t| t.name == it.name) {
                        target.push(it);
                    }
                }
            }
        }
        groups.push(Group { title: format!("CPU [#0]: {}", self.cpu.model), items: main });
        groups.push(Group { title: format!("CPU [#0]: {}: C-State Residency", self.cpu.model), items: cstate });

        // zen_extras driver: IOD temps, throttle flags, DRAM timings (SMN, read-only).
        if let Some(z) = chips.iter().find(|c| c.prefix == "zen_extras") {
            let rd = |f: &str| read_trim(z.path.join(f)).and_then(|s| s.parse::<f64>().ok());
            let mut t = Vec::new();
            if let Some(r) = rd("mem_ratio_x3") {
                t.push(Item::new("Memory Clock", Kind::Clock, 1, Some(r / 3.0 * 100.0)));
                t.push(Item::new("Memory Clock Ratio", Kind::Other, 2, Some(r / 3.0)).unit("x"));
            }
            let (rcd_rd, rcd_wr) = (rd("mem_trcdrd"), rd("mem_trcdwr"));
            t.push(Item::new("Tcas", Kind::Other, 0, rd("mem_tcl")).unit("T"));
            if rcd_rd == rcd_wr {
                t.push(Item::new("Trcd", Kind::Other, 0, rcd_rd).unit("T"));
            } else {
                t.push(Item::new("Trcd (read)", Kind::Other, 0, rcd_rd).unit("T"));
                t.push(Item::new("Trcd (write)", Kind::Other, 0, rcd_wr).unit("T"));
            }
            for (name, f) in [("Trp", "mem_trp"), ("Tras", "mem_tras"), ("Trc", "mem_trc"), ("Trfc", "mem_trfc")] {
                t.push(Item::new(name, Kind::Other, 0, rd(f)).unit("T"));
            }
            t.push(Item::new("Command Rate", Kind::Other, 0, rd("mem_cmd2t").map(|v| v + 1.0)).unit("T"));
            t.push(Item::new("Gear Down Mode", Kind::Bool, 0, rd("mem_gdm")));
            groups.push(Group { title: "Memory Timings".into(), items: t });

            let iod = |label: &str| z.items.iter().find(|i| i.name == label).and_then(|i| i.value);
            let at = enh.iter().position(|i| i.name == "CPU IOD Hotspot").map(|p| p + 1).unwrap_or(enh.len());
            enh.retain(|i| i.name != "CPU IOD Hotspot");
            let at = at.saturating_sub(1).min(enh.len());
            enh.insert(at, Item::new("CPU IOD Average", Kind::Temp, 1, iod("IOD Average")));
            enh.insert(at, Item::new("CPU IOD Hotspot", Kind::Temp, 1, iod("IOD Hotspot")));
            enh.retain(|i| i.name != "Thermal Throttling (PROCHOT CPU)");
            let at = enh.iter().position(|i| i.name == "DRAM Read Bandwidth").unwrap_or(enh.len());
            for (name, f) in [("Thermal Throttling (PROCHOT EXT)", "throttle_prochot_ext"), ("Thermal Throttling (PROCHOT CPU)", "throttle_prochot_cpu"), ("Thermal Throttling (HTC)", "throttle_htc")] {
                enh.insert(at, Item::new(name, Kind::Bool, 0, rd(f)));
            }
        }
        groups.push(Group { title: format!("CPU [#0]: {}: Enhanced", self.cpu.model), items: enh });

        for c in chips.iter().filter(|c| c.prefix.starts_with("it8") || c.prefix.starts_with("nct") || c.prefix.starts_with("f71")) {
            let model = c.prefix.split('_').next().unwrap_or(&c.prefix).to_uppercase();
            let chip = match &model[..2] {
                "IT" => format!("ITE {model}"),
                "NC" => format!("Nuvoton {model}"),
                "F7" => format!("Fintek {model}"),
                _ => model,
            };
            let mut items = c.items.clone();
            // sensors.d ignores intrusion0 (libsensors hides it); HWiNFO lists it.
            if let Some(v) = read_trim(c.path.join("intrusion0_alarm")).and_then(|s| s.parse::<f64>().ok()) {
                items.push(Item::new("Chassis Intrusion", Kind::Bool, 0, Some(v)));
            }
            groups.push(Group { title: format!("{} ({chip})", self.board), items });
        }

        // CPU VRM through gsa1_smbus: HWiNFO shows one RAA229004 group per PMBus page.
        if let Some(c) = by("raa229004").next() {
            let w = |n: &str| c.items.iter().find(|i| i.name == n).and_then(|i| i.value);
            for (rail, suffix) in [("VDDCR_CPU", ""), ("VDDCR_SOC", " [VDDCR_SOC]")] {
                let pout = w(&format!("POUT ({rail})"));
                let pin = w(&format!("PIN ({rail})"));
                let items = vec![
                    Item::new("VR Loop1", Kind::Temp, 1, w("VR Loop1")),
                    Item::new("VR Loop2", Kind::Temp, 1, w("VR Loop2")),
                    Item::new("VR VOUT", Kind::Volt, 3, w(&format!("VR VOUT ({rail})"))),
                    Item::new("VR VIN", Kind::Volt, 3, w("VR VIN")),
                    Item::new("Current (IOUT)", Kind::Current, 3, w(&format!("IOUT ({rail})"))),
                    Item::new("Current (IIN)", Kind::Current, 3, w(&format!("IIN ({rail})"))),
                    Item::new("Power (POUT)", Kind::Power, 3, pout),
                    Item::new("Power (Input)", Kind::Power, 3, pin),
                    Item::new("VRM Efficiency", Kind::Usage, 1, pout.zip(pin).filter(|&(_, i)| i > 0.0).map(|(o, i)| (100.0 * o / i).min(100.0))),
                ];
                groups.push(Group { title: format!("{} (Renesas RAA229004){suffix}", self.board), items });
            }
        }

        let mut dimms: Vec<&Chip> = chips.iter().filter(|c| c.prefix == "jc42" || c.prefix == "spd5118").collect();
        dimms.sort_by_key(|c| c.addr);
        if !dimms.is_empty() {
            let items = dimms
                .iter()
                .enumerate()
                .flat_map(|(n, c)| c.items.iter().map(move |i| {
                    let mut i = i.clone();
                    // Unlabelled jc42 -> HWiNFO-style name; TSOD 0x18+n pairs with SPD 0x50+n.
                    if i.name == "temp1" {
                        // jc42 (DDR4 TSOD) sits at 0x18+n, spd5118 (DDR5 hub) at the SPD address itself.
                        let spd = if c.prefix == "jc42" { c.addr + 0x38 } else { c.addr };
                        i.name = format!("DIMM[{n}] Temperature (SPD 0x{spd:02X})");
                    }
                    i
                }))
                .collect();
            groups.push(Group { title: "DIMM Temperature Sensor".into(), items });
        }

        // Drives: S.M.A.R.T. temperatures (nvme/drivetemp hwmon) then activity.
        lap("cpu-extras/board/dimm");
        let disks = self.disks.sample();
        for (dev, model, _) in &disks {
            let ctrl = canon(format!("/sys/block/{dev}/device"));
            let temps: Vec<Item> = chips
                .iter()
                .filter(|c| c.prefix == "nvme" || c.prefix == "drivetemp")
                .filter(|c| ctrl.is_some() && canon(c.path.join("device")) == ctrl)
                .flat_map(|c| c.items.iter().cloned())
                .enumerate()
                .map(|(n, mut i)| {
                    i.name = if n == 0 { "Drive Temperature".into() } else { format!("Drive Temperature {}", n + 1) };
                    i
                })
                .collect();
            // Root helper's SMART data for this disk (title carries the serial, like HWiNFO).
            let suffix = format!("({dev})");
            let (title, extra) = match privileged.iter().position(|g| g.title.starts_with("S.M.A.R.T.:") && g.title.ends_with(&suffix)) {
                Some(i) => {
                    let g = privileged.remove(i);
                    (g.title, g.items)
                }
                None => (format!("S.M.A.R.T.: {model} ({dev})"), Vec::new()),
            };
            let mut items = temps;
            items.extend(extra);
            if !items.is_empty() {
                groups.push(Group { title, items });
            }
        }
        for (dev, model, items) in disks {
            groups.push(Group { title: format!("Drive: {model} ({dev})"), items });
        }

        let nv = |label: &str| by("nvvram").flat_map(|c| c.items.iter()).find(|i| i.name == label).and_then(|i| i.value);
        lap("disks");
        let gpus = self.gpu.sample(nv("VRAM Junction"), nv("GPU Hot Spot"));
        lap("gpu");
        for (i, (title, items)) in gpus.into_iter().enumerate() {
            // Helper's GPU-board chips follow their GPU, titled like HWiNFO.
            let tag = format!("GPU [#{i}]: ");
            let mut extra = Vec::new();
            let mut items = items;
            privileged.retain(|g| match g.title.strip_prefix(&tag) {
                // Rows that belong inside the GPU group itself (D3D memory dynamic).
                Some("d3d") => {
                    let at = items.iter().position(|i| i.name == "GPU D3D Memory Dedicated").map(|p| p + 1).unwrap_or(items.len());
                    for (n, it) in g.items.iter().enumerate() {
                        items.insert(at + n, it.clone());
                    }
                    false
                }
                Some(chip) => {
                    extra.push(Group { title: format!("{title}: {chip}"), items: g.items.clone() });
                    false
                }
                None => true,
            });
            groups.push(Group { title, items });
            groups.extend(extra);
        }

        let nv_count = groups.iter().filter(|g| g.title.starts_with("GPU [#") && !g.title.contains(": ITE")).count();
        for (title, items) in gpu_other::sample(&chips) {
            let n: usize = title[6..].split(']').next().and_then(|x| x.parse().ok()).unwrap_or(0);
            groups.push(Group { title: format!("GPU [#{}]{}", n + nv_count, &title[title.find(']').unwrap() + 1..]), items });
        }

        let phy: Vec<Item> = chips
            .iter()
            .filter(|c| c.prefix.starts_with("r8169") || c.prefix.ends_with("_phy"))
            .flat_map(|c| c.items.iter().map(|i| {
                let mut i = i.clone();
                if i.name == "temp1" {
                    i.name = "PHY Temperature".into();
                }
                i
            }))
            .collect();
        for (i, (nic, mut items)) in self.net.sample().into_iter().enumerate() {
            let drv = canon(format!("/sys/class/net/{nic}/device/driver"))
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default();
            if i == 0 {
                items.extend(phy.iter().cloned());
            }
            groups.push(Group { title: format!("Network: {nic} ({drv})"), items });
        }

        for (title, items) in gpu_other::batteries() {
            groups.push(Group { title, items });
        }

        let known = ["k10temp", "zenpower", "coretemp", "jc42", "spd5118", "nvme", "drivetemp", "nvvram", "zen_extras", "raa229004", "amdgpu", "i915", "xe", "nouveau"];
        for c in chips.iter().filter(|c| {
            !known.contains(&c.prefix.as_str()) && !c.prefix.starts_with("r8169") && !c.prefix.starts_with("it8") && !c.prefix.starts_with("nct") && !c.prefix.starts_with("f71") && !c.prefix.ends_with("_phy")
        }) {
            // power_supply hwmon twins (BAT0, AC, ADP1, ucsi) duplicate the Battery group.
            if ["BAT", "AC", "ADP", "ucsi", "hidpp_battery"].iter().any(|p| c.prefix.starts_with(p)) {
                continue;
            }
            let title = chip_title(&c.prefix, &self.board);
            let what = match c.prefix.split('_').next().unwrap_or("") {
                "iwlwifi" | "mt7921" | "mt7921e" | "ath11k" | "ath12k" => "Wi-Fi",
                "pch" => "PCH",
                "acpitz" => "Thermal Zone",
                _ => "",
            };
            let items = c
                .items
                .iter()
                // Unpopulated EC slots read nothing, 0 °C or -128 °C.
                .filter(|i| i.value.is_some() && !(i.kind == Kind::Temp && (i.value == Some(0.0) || i.value.unwrap_or(0.0) <= -100.0)))
                .map(|i| {
                    let mut i = i.clone();
                    if !what.is_empty() && i.name.starts_with("temp") {
                        i.name = format!("{what} Temperature{}", if i.name == "temp1" { String::new() } else { format!(" {}", &i.name[4..]) });
                    }
                    i
                })
                .collect();
            groups.push(Group { title, items });
        }

        // Any other helper groups (memory timings, VRM, ...) not placed above.
        groups.extend(privileged.into_iter().filter(|g| !g.title.starts_with("S.M.A.R.T.:")));

        lap("net");
        let mut errs = system::hw_errors();
        lap("errors");
        if !errs.is_empty() {
            let total = errs.iter().filter_map(|i| i.value).sum::<f64>();
            errs.insert(0, Item::new("Total Errors", Kind::Other, 0, Some(total)));
            groups.push(Group { title: "Hardware Errors (PCIe AER / EDAC)".into(), items: errs });
        }
        groups.retain(|g| !g.items.is_empty());
        groups
    }
}

/// HWiNFO-style group title for a hwmon chip without a dedicated source.
fn chip_title(prefix: &str, board: &str) -> String {
    let p = prefix.split('_').next().unwrap_or(prefix);
    match p {
        "acpitz" => "ACPI Thermal Zone".into(),
        "asus" if prefix.starts_with("asus_ec_sensors") => format!("{board} (ASUS EC)"),
        "asusec" | "asus-ec-sensors" => format!("{board} (ASUS EC)"),
        "gigabyte" => format!("{board} (Gigabyte WMI)"),
        "dell" => "Dell SMM".into(),
        "thinkpad" => format!("{board} (ThinkPad EC)"),
        "iwlwifi" => "Network: Wi-Fi (iwlwifi)".into(),
        "mt7921" | "mt7921e" | "ath11k" | "ath12k" => format!("Network: Wi-Fi ({p})"),
        "pch" => format!("{board} (Intel PCH)"),
        "corsairpsu" | "corsair" => "PSU (Corsair)".into(),
        "nzxt" | "kraken" => "Cooler (NZXT)".into(),
        "zenpower" => "CPU (zenpower)".into(),
        _ => prefix.to_string(),
    }
}
