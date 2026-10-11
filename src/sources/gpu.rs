//! NVIDIA GPUs through NVML (libnvidia-ml, loaded at runtime), plus the
//! VRAM junction temperature from the nvvram hwmon driver (kmod/nvvram).

use crate::model::{Item, Kind};
use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{Clock, PcieUtilCounter, TemperatureSensor, TemperatureThreshold};
use nvml_wrapper::enums::device::SampleValue;
use nvml_wrapper::structs::device::FieldId;
use nvml_wrapper::Nvml;
use std::collections::HashMap;

// NVML field IDs (nvml.h); all read-only.
const FI_POWER_MIN_LIMIT: u32 = 187;
const FI_POWER_MAX_LIMIT: u32 = 188;
const FI_POWER_DEFAULT_LIMIT: u32 = 189;

/// HWiNFO's "PCI Express Error Counters", in its order.
const PCIE_COUNTERS: [(u32, &str); 12] = [
    (175, "Receiver Errors"),
    (94, "Replay Count"),
    (95, "Replay Rollover Count"),
    (178, "Bad DLLP Count"),
    (176, "Bad TLP Count"),
    (182, "LCRC Error Count"),
    (177, "NAKs Sent Count"),
    (174, "NAKs Received Count"),
    (169, "Recovery Count"),
    (173, "Correctable Error Count"),
    (179, "Non-Fatal Error Count"),
    (180, "Fatal Error Count"),
];
const FI_PCIE_UNSUPPORTED_REQ: u32 = 181;
/// Cumulative PCIe byte counters (u32, wrap): rates from deltas instead of
/// nvmlDeviceGetPcieThroughput, which blocks ~20 ms per call.
const FI_PCIE_TX_BYTES: u32 = 197;
const FI_PCIE_RX_BYTES: u32 = 198;

/// NVML_FI_DEV_PCIE_COUNT_LANE_ERROR (183) needs a per-lane scopeId, which
/// nvml-wrapper's field API doesn't set, so call nvmlDeviceGetFieldValues raw.
fn lane_errors(d: &nvml_wrapper::Device, lanes: u32) -> Vec<Option<f64>> {
    #[repr(C)]
    struct FieldValue {
        field_id: u32,
        scope_id: u32,
        timestamp: i64,
        latency_usec: i64,
        value_type: u32,
        nvml_return: u32,
        value: u64,
    }
    type GetFieldValues = unsafe extern "C" fn(*mut std::ffi::c_void, i32, *mut FieldValue) -> u32;
    static LIB: std::sync::OnceLock<Option<libloading::Library>> = std::sync::OnceLock::new();
    let Some(lib) = LIB.get_or_init(|| unsafe { libloading::Library::new("libnvidia-ml.so.1").ok() }) else { return Vec::new() };
    let Ok(f) = (unsafe { lib.get::<GetFieldValues>(b"nvmlDeviceGetFieldValues") }) else { return Vec::new() };
    let mut vals: Vec<FieldValue> = (0..lanes)
        .map(|l| FieldValue { field_id: 183, scope_id: l, timestamp: 0, latency_usec: 0, value_type: 0, nvml_return: 0, value: 0 })
        .collect();
    let h = unsafe { d.handle() } as *mut std::ffi::c_void;
    if unsafe { f(h, vals.len() as i32, vals.as_mut_ptr()) } != 0 {
        return Vec::new();
    }
    vals.iter().map(|v| (v.nvml_return == 0).then_some(v.value as u32 as f64)).collect()
}

pub struct Gpu {
    nvml: Option<Nvml>,
    nvapi: Option<crate::sources::nvapi::NvApi>,
    /// Previous (time, tx, rx) PCIe byte counters per GPU.
    pcie_prev: HashMap<u32, (std::time::Instant, u32, u32)>,
    /// Last process-utilization timestamp per GPU (NVML returns samples newer than it).
    proc_ts: HashMap<u32, u64>,
}

impl Gpu {
    pub fn new() -> Self {
        // HWINFO_RS_NO_NVIDIA: never load NVML/NVAPI (e.g. hybrid laptops where
        // initializing them would power up a runtime-suspended dGPU).
        if std::env::var_os("HWINFO_RS_NO_NVIDIA").is_some() {
            return Gpu { nvml: None, nvapi: None, pcie_prev: HashMap::new(), proc_ts: HashMap::new() };
        }
        Gpu { nvml: Nvml::init().ok(), nvapi: crate::sources::nvapi::NvApi::new(), pcie_prev: HashMap::new(), proc_ts: HashMap::new() }
    }

    /// One (title, items) per GPU. `vram_temp`/`hotspot` come from the nvvram driver.
    pub fn sample(&mut self, vram_temp: Option<f64>, hotspot: Option<f64>) -> Vec<(String, Vec<Item>)> {
        let Some(nvml) = &self.nvml else { return Vec::new() };
        let n = nvml.device_count().unwrap_or(0);
        let mut out = Vec::new();
        for idx in 0..n {
            let Ok(d) = nvml.device_by_index(idx) else { continue };
            let ids: Vec<FieldId> = [FI_PCIE_TX_BYTES, FI_PCIE_RX_BYTES, FI_POWER_MIN_LIMIT, FI_POWER_MAX_LIMIT, FI_POWER_DEFAULT_LIMIT, FI_PCIE_UNSUPPORTED_REQ]
                .into_iter()
                .chain(PCIE_COUNTERS.iter().map(|&(id, _)| id))
                .map(FieldId)
                .collect();
            let mut fields: HashMap<u32, f64> = HashMap::new();
            if let Ok(vals) = d.field_values_for(&ids) {
                for v in vals.into_iter().flatten() {
                    let x = match v.value {
                        Ok(SampleValue::U32(x)) => x as f64,
                        Ok(SampleValue::U64(x)) => x as f64,
                        Ok(SampleValue::I64(x)) => x as f64,
                        Ok(SampleValue::F64(x)) => x,
                        Err(_) => continue,
                    };
                    fields.insert(v.field.0, x);
                }
            }
            let fv = |id: u32| fields.get(&id).copied();
            let bdf = d.pci_info().ok().map(|p| p.bus_id.to_lowercase());
            let sysfs = |f: &str| -> Option<String> {
                let b = bdf.as_ref()?;
                // NVML bus ids are 00000000:06:00.0; sysfs uses 0000:06:00.0.
                let b = if b.len() > 12 { &b[b.len() - 12..] } else { b.as_str() };
                crate::sources::hwmon::read_trim(format!("/sys/bus/pci/devices/{b}/{f}"))
            };
            let name = d.name().unwrap_or_else(|_| "NVIDIA GPU".into());
            let mut v = Vec::new();
            let ok = |r: Result<u32, _>| r.ok().map(|x| x as f64);

            v.push(Item::new("GPU Temperature", Kind::Temp, 1, ok(d.temperature(TemperatureSensor::Gpu))));
            if idx == 0 {
                v.push(Item::new("GPU Memory Junction Temperature", Kind::Temp, 1, vram_temp));
            }
            v.push(Item::new("GPU Thermal Limit", Kind::Temp, 1, ok(d.temperature_threshold(TemperatureThreshold::Slowdown))));

            let power = d.power_usage().ok().map(|mw| mw as f64 / 1000.0);
            let limit = d.enforced_power_limit().ok().map(|mw| mw as f64 / 1000.0);
            v.push(Item::new("GPU Power", Kind::Power, 3, power));
            v.push(Item::new("GPU Power Limit", Kind::Power, 0, limit));
            let tdp = fv(FI_POWER_DEFAULT_LIMIT).map(|mw| mw / 1000.0);
            v.push(Item::new("Total GPU Power [% of TDP]", Kind::Usage, 1, power.zip(tdp).filter(|&(_, l)| l > 0.0).map(|(p, l)| 100.0 * p / l)));
            v.push(Item::new("Total GPU Power [% of limit]", Kind::Usage, 1, power.zip(limit).filter(|&(_, l)| l > 0.0).map(|(p, l)| 100.0 * p / l)));
            v.push(Item::new("GPU Power Limit (default / TDP)", Kind::Power, 0, tdp));
            v.push(Item::new("GPU Power Limit (min)", Kind::Power, 0, fv(FI_POWER_MIN_LIMIT).map(|mw| mw / 1000.0)));
            v.push(Item::new("GPU Power Limit (max)", Kind::Power, 0, fv(FI_POWER_MAX_LIMIT).map(|mw| mw / 1000.0)));
            v.push(Item::new("GPU Energy (since driver load)", Kind::Other, 3, d.total_energy_consumption().ok().map(|mj| mj as f64 / 3.6e9)).unit("kWh"));

            v.push(Item::new("GPU Clock", Kind::Clock, 1, ok(d.clock_info(Clock::Graphics))));
            v.push(Item::new("GPU Memory Clock", Kind::Clock, 1, ok(d.clock_info(Clock::Memory))));
            v.push(Item::new("GPU SM Clock", Kind::Clock, 1, ok(d.clock_info(Clock::SM))));
            v.push(Item::new("GPU Video Clock", Kind::Clock, 1, ok(d.clock_info(Clock::Video))));

            let util = d.utilization_rates().ok();
            v.push(Item::new("GPU Core Load", Kind::Usage, 1, util.as_ref().map(|u| u.gpu as f64)));
            v.push(Item::new("GPU Memory Controller Load", Kind::Usage, 1, util.as_ref().map(|u| u.memory as f64)));
            let enc = d.encoder_utilization().ok().map(|u| u.utilization as f64);
            let dec = d.decoder_utilization().ok().map(|u| u.utilization as f64);
            v.push(Item::new("GPU Video Engine Load", Kind::Usage, 1, enc.into_iter().chain(dec).reduce(f64::max)));
            v.push(Item::new("GPU Video Decode 0 Usage", Kind::Usage, 1, dec));
            v.push(Item::new("GPU Video Encode 0 Usage", Kind::Usage, 1, enc));

            if let Ok(m) = d.memory_info() {
                let mb = |b: u64| b as f64 / 1048576.0;
                v.push(Item::new("GPU Memory Allocated", Kind::Memory, 0, Some(mb(m.used))));
                v.push(Item::new("GPU Memory Available", Kind::Memory, 0, Some(mb(m.free))));
                v.push(Item::new("GPU Memory Usage", Kind::Usage, 1, Some(100.0 * m.used as f64 / m.total.max(1) as f64)));
            }

            let fans = d.num_fans().unwrap_or(0);
            for f in 0..fans {
                v.push(Item::new(format!("GPU Fan{}", f + 1), Kind::Usage, 0, ok(d.fan_speed(f))));
                if let Ok(rpm) = d.fan_speed_rpm(f) {
                    v.push(Item::new(format!("GPU Fan{} Speed", f + 1), Kind::Fan, 0, Some(rpm as f64)));
                }
            }

            let gts = sysfs("current_link_speed").and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok());
            let width = ok(d.current_pcie_link_width());
            let now = std::time::Instant::now();
            let (rx, tx) = match (fv(FI_PCIE_TX_BYTES), fv(FI_PCIE_RX_BYTES)) {
                (Some(t), Some(r)) => {
                    let (t, r) = (t as u32, r as u32);
                    let rates = self.pcie_prev.get(&idx).map(|&(pt, ptx, prx)| {
                        let dt = now.duration_since(pt).as_secs_f64().max(1e-3);
                        (r.wrapping_sub(prx) as f64 / dt / 1048576.0, t.wrapping_sub(ptx) as f64 / dt / 1048576.0)
                    });
                    self.pcie_prev.insert(idx, (now, t, r));
                    (rates.map(|x| x.0), rates.map(|x| x.1))
                }
                _ => (
                    d.pcie_throughput(PcieUtilCounter::Receive).ok().map(|kb| kb as f64 / 1024.0),
                    d.pcie_throughput(PcieUtilCounter::Send).ok().map(|kb| kb as f64 / 1024.0),
                ),
            };
            // Bus load: traffic vs. one direction's raw link bandwidth (128b/130b for >= 8 GT/s).
            let link_mbs = gts.zip(width).map(|(g, w)| g * 1000.0 / 8.0 * w * if g >= 8.0 { 128.0 / 130.0 } else { 0.8 });
            let bus = rx.zip(tx).zip(link_mbs).filter(|&(_, l)| l > 0.0).map(|((r, t), l)| (100.0 * r.max(t) / l).min(100.0));
            v.push(Item::new("GPU Bus Load", Kind::Usage, 1, bus));
            v.push(Item::new("PCIe Link Speed", Kind::Other, 1, gts).unit("GT/s"));
            v.push(Item::new("PCIe Link Width", Kind::Other, 0, width).unit("x"));
            v.push(Item::new("PCIe Link Generation", Kind::Other, 0, ok(d.current_pcie_link_gen())));
            v.push(Item::new("GPU PCIe Rx", Kind::Rate, 3, rx));
            v.push(Item::new("GPU PCIe Tx", Kind::Rate, 3, tx));
            v.push(Item::new("GPU Performance State (P-state)", Kind::Other, 0, d.performance_state().ok().map(|p| p.as_c() as f64)));

            if !PCIE_COUNTERS.iter().all(|&(id, _)| fv(id).is_none()) {
                let mut kids: Vec<Item> = PCIE_COUNTERS.iter().map(|&(id, n)| Item::new(n, Kind::Other, 0, fv(id))).collect();
                kids.push(Item::new("Unsupported Request Count", Kind::Other, 0, fv(FI_PCIE_UNSUPPORTED_REQ)));
                let lanes = d.current_pcie_link_width().unwrap_or(16).min(16);
                for (lane, n) in lane_errors(&d, lanes).into_iter().enumerate() {
                    kids.push(Item::new(format!("PCIe Lane {lane} Errors"), Kind::Other, 0, n));
                }
                let total: f64 = kids.iter().filter_map(|k| k.value).sum();
                let mut parent = Item::new("PCI Express Error Counters", Kind::Other, 0, Some(total));
                parent.children = kids;
                v.push(parent);
            }
            if let Ok(r) = d.current_throttle_reasons() {
                let b = |m: ThrottleReasons| Some(if r.intersects(m) { 1.0 } else { 0.0 });
                // HWiNFO's limiter rows from NVML clock-event reasons. Reliability /
                // Max Operating Voltage have no NVML reason, so they are not shown.
                let kids = vec![
                    Item::new("Performance Limit - Power", Kind::Bool, 0, b(ThrottleReasons::SW_POWER_CAP | ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN)),
                    Item::new("Performance Limit - Thermal", Kind::Bool, 0, b(ThrottleReasons::SW_THERMAL_SLOWDOWN | ThrottleReasons::HW_THERMAL_SLOWDOWN)),
                    Item::new("Performance Limit - HW Slowdown", Kind::Bool, 0, b(ThrottleReasons::HW_SLOWDOWN)),
                    Item::new("Performance Limit - Utilization", Kind::Bool, 0, b(ThrottleReasons::GPU_IDLE)),
                    Item::new("Performance Limit - Clock Setting", Kind::Bool, 0, b(ThrottleReasons::APPLICATIONS_CLOCKS_SETTING | ThrottleReasons::DISPLAY_CLOCK_SETTING)),
                    Item::new("Performance Limit - Sync Boost", Kind::Bool, 0, b(ThrottleReasons::SYNC_BOOST)),
                ];
                let any = kids.iter().filter_map(|k| k.value).fold(0.0f64, f64::max);
                let mut parent = Item::new("GPU Performance Limiters", Kind::Bool, 0, Some(any));
                parent.children = kids;
                v.push(parent);
            }
            // NVAPI rows (hot spot, core voltage, rails, % of TDP) slot in after the temperatures.
            let extra = self.nvapi.as_ref().map(|n| n.sample(idx as usize)).unwrap_or_default();
            if !extra.is_empty() {
                let names: Vec<String> = extra.iter().map(|i| i.name.clone()).collect();
                v.retain(|i| !names.contains(&i.name));
                let at = v.iter().position(|i| i.name == "GPU Thermal Limit").unwrap_or(0);
                let (temps, rest): (Vec<Item>, Vec<Item>) = extra.into_iter().partition(|i| i.kind == Kind::Temp);
                v.splice(at..at, temps);
                let (clocks, rest): (Vec<Item>, Vec<Item>) = rest.into_iter().partition(|i| i.kind == Kind::Clock);
                let (limiters, rest): (Vec<Item>, Vec<Item>) = rest.into_iter().partition(|i| i.name == "GPU Performance Limiters");
                let at = v.iter().position(|i| i.name == "GPU Video Clock").map(|p| p + 1).unwrap_or(v.len());
                v.splice(at..at, clocks);
                v.extend(limiters);
                let at = v.iter().position(|i| i.name == "GPU Power").map(|p| p + 1).unwrap_or(v.len());
                v.splice(at..at, rest);
            }
            if !v.iter().any(|i| i.name == "GPU Hot Spot Temperature") && idx == 0 && hotspot.is_some() {
                let at = v.iter().position(|i| i.name == "GPU Thermal Limit").unwrap_or(0);
                v.insert(at, Item::new("GPU Hot Spot Temperature", Kind::Temp, 1, hotspot));
            }
            out.push((format!("GPU [#{idx}]: {name}"), v));

            // Per-process VRAM and engine load (nvtop-style), heaviest first.
            let mut procs: HashMap<u32, (u64, Option<(u32, u32, u32)>)> = HashMap::new();
            for p in d.running_graphics_processes().unwrap_or_default().into_iter().chain(d.running_compute_processes().unwrap_or_default()) {
                let mem = match p.used_gpu_memory {
                    nvml_wrapper::enums::device::UsedGpuMemory::Used(b) => b,
                    _ => 0,
                };
                let e = procs.entry(p.pid).or_default();
                e.0 = e.0.max(mem);
            }
            let since = self.proc_ts.get(&idx).copied();
            if let Ok(samples) = d.process_utilization_stats(since) {
                for s in samples {
                    self.proc_ts.insert(idx, s.timestamp.max(*self.proc_ts.get(&idx).unwrap_or(&0)));
                    if let Some(e) = procs.get_mut(&s.pid) {
                        e.1 = Some((s.sm_util, s.enc_util, s.dec_util));
                    }
                }
            }
            let mut rows: Vec<(u32, u64, Option<(u32, u32, u32)>)> = procs.into_iter().map(|(pid, (m, u))| (pid, m, u)).collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1));
            let items: Vec<Item> = rows
                .iter()
                .take(24)
                .map(|&(pid, mem, util)| {
                    let comm = crate::sources::hwmon::read_trim(format!("/proc/{pid}/comm")).unwrap_or_else(|| "?".into());
                    let mut it = Item::new(format!("{comm} ({pid})"), Kind::Memory, 0, Some(mem as f64 / 1048576.0));
                    if let Some((sm, enc, dec)) = util {
                        it.children = vec![
                            Item::new("GPU Load", Kind::Usage, 0, Some(sm as f64)),
                            Item::new("Video Encode", Kind::Usage, 0, Some(enc as f64)),
                            Item::new("Video Decode", Kind::Usage, 0, Some(dec as f64)),
                        ];
                    }
                    it
                })
                .collect();
            if !items.is_empty() {
                out.push((format!("GPU [#{idx}]: Processes"), items));
            }
        }
        out
    }
}
