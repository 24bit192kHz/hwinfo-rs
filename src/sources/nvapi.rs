//! NVIDIA's private NVAPI on Linux (libnvidia-api.so.1, unprivileged, Get calls
//! only): GPU hot spot, core voltage, per-rail power/voltage/current and the
//! "% of TDP" topology — what HWiNFO reads through NVAPI on Windows.
//! IDs from the HWiNFO64 RE (analysis/RE-REPORT.md); struct layouts from the
//! RTSS NVAPI header, confirmed by size-scanning on this driver.

use crate::model::{Item, Kind};
use std::ffi::c_void;

type QueryInterface = unsafe extern "C" fn(u32) -> *const c_void;
type GetFn = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;

const ID_INITIALIZE: u32 = 0x0150_E828;
const ID_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
const ID_THERM_CHANNEL_GET_STATUS: u32 = 0x65FE_3AAD;
const ID_CLIENT_VOLT_RAILS_GET_STATUS: u32 = 0x465F_9BCF;
const ID_POWER_MONITOR_GET_INFO: u32 = 0xC12E_B19E;
const ID_POWER_MONITOR_GET_STATUS: u32 = 0xF402_38EF;
const ID_CLIENT_POWER_TOPOLOGY_GET_STATUS: u32 = 0xEDCF_624E;
const ID_PERF_POLICIES_GET_STATUS: u32 = 0x3D35_8A0C;
const ID_GET_ALL_CLOCKS: u32 = 0x1BD6_9F49;

const fn ver(size: u32, v: u32) -> u32 {
    size | (v << 16)
}

/// NV_GPU_POWER_CHANNEL_POWER_RAIL -> HWiNFO-style label.
fn rail_name(rail: u32) -> &'static str {
    match rail {
        1 => "GPU Core (NVVDD) Output",
        2 => "GPU FBVDD Output",
        3 | 4 => "GPU FBVDDQ Output",
        5 => "GPU PEXVDD Output",
        8 => "GPU Total Output",
        11 => "GPU SRAM Output",
        222 => "GPU PCIe +12V #2 Input",
        223 | 245 => "GPU Total Board Input",
        224 => "GPU High Voltage #0 Input",
        225 => "GPU High Voltage #1 Input",
        226 => "GPU Core (NVVDD1) Input",
        227 => "GPU Core (NVVDD2) Input",
        228 => "GPU 8-pin #3 Input",
        229 => "GPU 8-pin #4 Input",
        232 => "GPU Misc0 Input",
        233 => "GPU Misc1 Input",
        234 => "GPU Misc2 Input",
        235 => "GPU Misc3 Input",
        240 => "GPU SRAM Input",
        241 => "GPU Input PP Source",
        242 => "GPU 3.3V PP Input",
        243 => "GPU 3.3V Main Input",
        246 => "GPU Core (NVVDD) Input",
        247 => "GPU FBVDD Input",
        248 | 249 => "GPU FBVDDQ Input",
        250 => "GPU 8-pin #1 Input",
        251 => "GPU 8-pin #2 Input",
        252 => "GPU 6-pin #1 Input",
        253 => "GPU 6-pin #2 Input",
        254 => "GPU PCIe +3.3V Input",
        255 => "GPU PCIe +12V Input",
        _ => "GPU Rail",
    }
}

struct Channel {
    idx: usize,
    name: String,
    /// type 1 = summation, 2 = estimation (fixed voltage), others measured.
    kind: u32,
}

pub struct NvApi {
    _lib: libloading::Library,
    gpus: Vec<*mut c_void>,
    therm: Option<GetFn>,
    volt: Option<GetFn>,
    pm_status: Option<GetFn>,
    topo: Option<GetFn>,
    perf: Option<GetFn>,
    clocks: Option<GetFn>,
    channels: Vec<Vec<Channel>>,
    masks: Vec<u32>,
    /// ClientPowerTopologyGetStatus busy-waits ~37 ms per call, so its one
    /// unique row ("normalized" % of TDP) is refreshed every TOPO_EVERY.
    topo_cache: std::cell::RefCell<std::collections::HashMap<usize, (std::time::Instant, Option<f64>)>>,
}

const TOPO_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

// The handles are opaque driver cookies; we only use them from the sampler thread.
unsafe impl Send for NvApi {}

impl NvApi {
    pub fn new() -> Option<Self> {
        if std::env::var_os("HWINFO_RS_NO_NVAPI").is_some() {
            return None;
        }
        unsafe {
            let lib = libloading::Library::new("libnvidia-api.so.1").ok()?;
            let qi: QueryInterface = *lib.get::<QueryInterface>(b"nvapi_QueryInterface").ok()?;
            let get = |id: u32| -> Option<*const c_void> {
                let p = qi(id);
                (!p.is_null()).then_some(p)
            };
            let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(get(ID_INITIALIZE)?);
            if init() != 0 {
                return None;
            }
            let enum_gpus: unsafe extern "C" fn(*mut [*mut c_void; 64], *mut u32) -> i32 = std::mem::transmute(get(ID_ENUM_PHYSICAL_GPUS)?);
            let mut h = [std::ptr::null_mut(); 64];
            let mut n = 0u32;
            if enum_gpus(&mut h, &mut n) != 0 {
                return None;
            }
            let gpus: Vec<*mut c_void> = h[..n as usize].to_vec();
            let f = |id| get(id).map(|p| std::mem::transmute::<*const c_void, GetFn>(p));

            // Power-monitor channel table (static): NV_GPU_POWER_MONITOR_GET_INFO_V2.
            let mut channels = Vec::new();
            let mut masks = Vec::new();
            let info = f(ID_POWER_MONITOR_GET_INFO);
            for &g in &gpus {
                let mut chans = Vec::new();
                let mut mask = 0;
                if let Some(info) = info {
                    let mut b = vec![0u32; 0xaa8 / 4];
                    b[0] = ver(0xaa8, 1);
                    if info(g, b.as_mut_ptr()) == 0 && b[1] & 0xff != 0 {
                        mask = b[4];
                        let mut seen = std::collections::HashMap::<&str, usize>::new();
                        for i in 0..32 {
                            if mask & (1 << i) == 0 {
                                continue;
                            }
                            let base = (0x28 + 0x3c * i) / 4;
                            let kind = b[base + 3];
                            let rail = b[base + 4];
                            let n = rail_name(rail);
                            let c = seen.entry(n).or_insert(0);
                            *c += 1;
                            let name = if *c == 1 { n.to_string() } else { format!("{n} #{c}") };
                            chans.push(Channel { idx: i, name, kind });
                        }
                    }
                }
                channels.push(chans);
                masks.push(mask);
            }
            Some(NvApi {
                therm: f(ID_THERM_CHANNEL_GET_STATUS),
                volt: f(ID_CLIENT_VOLT_RAILS_GET_STATUS),
                pm_status: f(ID_POWER_MONITOR_GET_STATUS),
                topo: f(ID_CLIENT_POWER_TOPOLOGY_GET_STATUS),
                perf: f(ID_PERF_POLICIES_GET_STATUS),
                clocks: f(ID_GET_ALL_CLOCKS),
                _lib: lib,
                topo_cache: Default::default(),
                gpus,
                channels,
                masks,
            })
        }
    }

    /// Items for GPU `idx` (NVAPI and NVML enumerate the single card identically here).
    pub fn sample(&self, idx: usize) -> Vec<Item> {
        let Some(&g) = self.gpus.get(idx) else { return Vec::new() };
        let mut v = Vec::new();
        unsafe {
            // ThermChannelGetStatus v2: temps (s24.8) from word 10; channel 1 = hot spot.
            if let Some(therm) = self.therm {
                let mut b = [0u32; 42];
                b[0] = ver(168, 2);
                b[1] = 0x3;
                if therm(g, b.as_mut_ptr()) == 0 {
                    v.push(Item::new("GPU Hot Spot Temperature", Kind::Temp, 1, Some(b[11] as i32 as f64 / 256.0)));
                }
            }
            // ClientVoltRailsGetStatus v1: core voltage in uV at word 10.
            if let Some(volt) = self.volt {
                let mut b = [0u32; 0x4c / 4];
                b[0] = ver(0x4c, 1);
                if volt(g, b.as_mut_ptr()) == 0 && b[10] > 0 {
                    v.push(Item::new("GPU Core Voltage", Kind::Volt, 3, Some(b[10] as f64 / 1e6)));
                }
            }
            // ClientPowerTopologyGetStatus v1: entries {domain, ?, power (1/1000 %), ?}.
            // Domain 0 (% of TDP) equals NVML power / default limit, which the NVML
            // side shows live; only domain 1 (normalized) is taken from here.
            if let Some(topo) = self.topo {
                let mut cache = self.topo_cache.borrow_mut();
                let fresh = cache.get(&idx).filter(|(t, _)| t.elapsed() < TOPO_EVERY).map(|&(_, v)| v);
                let norm = fresh.unwrap_or_else(|| {
                    let mut b = [0u32; 0x48 / 4];
                    b[0] = ver(0x48, 1);
                    let v = (topo(g, b.as_mut_ptr()) == 0)
                        .then(|| (0..(b[1] as usize).min(4)).find(|&e| b[2 + 4 * e] == 1).map(|e| b[4 + 4 * e] as f64 / 1000.0))
                        .flatten();
                    cache.insert(idx, (std::time::Instant::now(), v));
                    v
                });
                if norm.is_some() {
                    v.push(Item::new("Total GPU Power (normalized) [% of TDP]", Kind::Usage, 1, norm));
                }
            }
            // GetAllClocks v2 (0x484 bytes): crossbar +0x0C, system +0x14 (kHz); effective
            // clock +0x104, or +0x2A8 x0.5 when byte +0x2AC has bit 0x20 (HWiNFO RE, round 3).
            if let Some(clk) = self.clocks {
                let mut b = [0u32; 0x484 / 4];
                b[0] = ver(0x484, 2);
                if clk(g, b.as_mut_ptr()) == 0 {
                    let flag = (b[0x2AC / 4] & 0xff) as u8;
                    let eff = if flag & 0x20 != 0 { b[0x2A8 / 4] as f64 / 2000.0 } else { b[0x104 / 4] as f64 / 1000.0 };
                    v.push(Item::new("GPU Effective Clock", Kind::Clock, 1, (eff > 0.0).then_some(eff)));
                    v.push(Item::new("GPU Crossbar Clock", Kind::Clock, 1, Some(b[0x0C / 4] as f64 / 1000.0)));
                    v.push(Item::new("GPU System Clock", Kind::Clock, 1, Some(b[0x14 / 4] as f64 / 1000.0)));
                }
            }
            // PerfPoliciesGetStatus v1 (0x550 bytes): input mask 0x1F at +4, limit bits at +0x10.
            if let Some(perf) = self.perf {
                let mut b = [0u32; 0x550 / 4];
                b[0] = ver(0x550, 1);
                b[1] = 0x1F;
                if perf(g, b.as_mut_ptr()) == 0 && (b[2] | b[3]) != 0 {
                    let m = b[4];
                    let names = ["Power", "Thermal", "Reliability Voltage", "Max Operating Voltage", "Utilization", "SLI GPUBoost Sync"];
                    let kids: Vec<Item> = names
                        .iter()
                        .enumerate()
                        .map(|(i, n)| Item::new(format!("Performance Limit - {n}"), Kind::Bool, 0, Some(((m >> i) & 1) as f64)))
                        .collect();
                    let mut parent = Item::new("GPU Performance Limiters", Kind::Bool, 0, Some(if m & 0x3F != 0 { 1.0 } else { 0.0 }));
                    parent.children = kids;
                    v.push(parent);
                }
            }
            // PowerMonitorGetStatus v1 (= STATUS_V2 layout): per-channel avg power, current, voltage.
            if let (Some(st), Some(chans)) = (self.pm_status, self.channels.get(idx)) {
                let mut b = vec![0u32; 0x59c / 4];
                b[0] = ver(0x59c, 1);
                b[1] = self.masks[idx];
                if !chans.is_empty() && st(g, b.as_mut_ptr()) == 0 {
                    let rd = |i: usize, off: usize| b[(0x1c + 0x2c * i + off) / 4];
                    let mut powers = Vec::new();
                    let mut volts = Vec::new();
                    let mut currents = Vec::new();
                    for c in chans {
                        let (p, a, u) = (rd(c.idx, 0), rd(c.idx, 0xc), rd(c.idx, 0x10));
                        let sum = if c.kind == 1 { " (sum)" } else { "" };
                        powers.push(Item::new(format!("{} Power{sum}", c.name), Kind::Power, 3, Some(p as f64 / 1000.0)));
                        // Estimated channels report a fixed voltage and no current.
                        if c.kind != 2 && u > 0 {
                            volts.push(Item::new(format!("{} Voltage", c.name), Kind::Volt, 3, Some(u as f64 / 1e6)));
                        }
                        if c.kind != 2 && a > 0 {
                            currents.push(Item::new(format!("{} Current", c.name), Kind::Current, 3, Some(a as f64 / 1000.0)));
                        }
                    }
                    v.push(Item::parent("GPU Rail Powers", Kind::Power, 3, powers));
                    v.push(Item::parent("GPU Rail Voltages", Kind::Volt, 3, volts));
                    v.push(Item::parent("GPU Rail Currents", Kind::Current, 3, currents));
                }
            }
        }
        v
    }
}
