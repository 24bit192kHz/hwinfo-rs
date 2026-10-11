//! Root-only sensors for the helper beyond SMART.
//!
//! ITE IT8915FN on an NVIDIA card's board I2C bus (nvidia exposes it as
//! "NVIDIA i2c adapter 1 at <bdf>"; seen on ASUS ROG STRIX cards): detected by
//! chip id 0x15/0x89 at regs 0x20/0x21, GPU VRM temperature in reg 0x15 (whole
//! °C). HWiNFO reads the same register (analysis/RE-REPORT.md, id 0xE0002100).
//! SMBus read-byte only, through i2c-dev.
//!
//! Per-thread APERF/MPERF (MSR 0xE8/0xE7, rdmsr via /dev/cpu/N/msr): HWiNFO's
//! "Core N Tx Effective Clock" = dAPERF/dt and "Utility" = that / nominal clock.

use crate::model::{Group, Item, Kind};
use std::os::fd::AsRawFd;

const I2C_SLAVE: u64 = 0x0703;
const I2C_SMBUS: u64 = 0x0720;
const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_BYTE_DATA: u32 = 2;

#[repr(C)]
struct SmbusIoctl {
    read_write: u8,
    command: u8,
    size: u32,
    data: *mut [u8; 34],
}

unsafe extern "C" {
    fn ioctl(fd: i32, req: u64, ...) -> i32;
}

fn read_byte(f: &std::fs::File, addr: u16, reg: u8) -> Option<u8> {
    let mut data = [0u8; 34];
    let mut args = SmbusIoctl { read_write: I2C_SMBUS_READ, command: reg, size: I2C_SMBUS_BYTE_DATA, data: &mut data };
    unsafe {
        if ioctl(f.as_raw_fd(), I2C_SLAVE, addr as u64) < 0 || ioctl(f.as_raw_fd(), I2C_SMBUS, &mut args) < 0 {
            return None;
        }
    }
    Some(data[0])
}

/// GPU board bus: the nvidia adapter numbered "1".
fn gpu_board_bus() -> Option<String> {
    let rd = std::fs::read_dir("/sys/bus/i2c/devices").ok()?;
    for e in rd.flatten() {
        let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default();
        if name.starts_with("NVIDIA i2c adapter 1 ") {
            return Some(format!("/dev/{}", e.file_name().to_string_lossy()));
        }
    }
    None
}

fn it8915() -> Option<Group> {
    let f = std::fs::File::open(gpu_board_bus()?).ok()?;
    let mut items = Vec::new();
    for (n, addr) in [0x2Bu16, 0x68].iter().enumerate() {
        if read_byte(&f, *addr, 0x20) != Some(0x15) || read_byte(&f, *addr, 0x21) != Some(0x89) {
            continue;
        }
        let t = read_byte(&f, *addr, 0x15).filter(|&t| t > 0 && t < 0xC8).map(|t| t as f64);
        let name = if n == 0 { "GPU VRM Temperature".to_string() } else { format!("GPU VRM Temperature (0x{addr:02X})") };
        items.push(Item::new(name, Kind::Temp, 0, t));
    }
    (!items.is_empty()).then(|| Group { title: "GPU [#0]: ITE IT8915FN".into(), items })
}

pub struct Msr {
    prev: Vec<Option<(std::time::Instant, u64)>>,
    nominal: f64,
}

impl Msr {
    pub fn new() -> Self {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let nominal = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/acpi_cppc/nominal_freq").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0);
        Msr { prev: vec![None; n], nominal }
    }

    /// Items named "cpuN Effective Clock" / "cpuN Utility"; the collector renames them.
    fn sample(&mut self) -> Option<Group> {
        use std::os::unix::fs::FileExt;
        let mut items = Vec::new();
        for (cpu, prev) in self.prev.iter_mut().enumerate() {
            let Ok(f) = std::fs::File::open(format!("/dev/cpu/{cpu}/msr")) else { return None };
            let mut b = [0u8; 8];
            if f.read_exact_at(&mut b, 0xE8).is_err() {
                return None;
            }
            let now = std::time::Instant::now();
            let aperf = u64::from_le_bytes(b);
            let eff = prev.map(|(t, a)| aperf.wrapping_sub(a) as f64 / now.duration_since(t).as_secs_f64() / 1e6);
            *prev = Some((now, aperf));
            items.push(Item::new(format!("cpu{cpu} Effective Clock"), Kind::Clock, 1, eff));
            if self.nominal > 0.0 {
                items.push(Item::new(format!("cpu{cpu} Utility"), Kind::Usage, 1, eff.map(|e| 100.0 * e / self.nominal)));
            }
        }
        Some(Group { title: "CPU [#0]: threads".into(), items })
    }
}

/// WDDM "D3D Memory Dynamic" counterpart: system RAM that GPU clients have mapped
/// through the NVIDIA device files (pinned/shared host allocations), summed over
/// every process holding a /dev/nvidia* handle. Root sees all processes.
pub struct GpuShared {
    cache: Option<(std::time::Instant, f64)>,
}

impl GpuShared {
    pub fn new() -> Self {
        GpuShared { cache: None }
    }

    fn sample(&mut self) -> Option<Group> {
        if !std::path::Path::new("/dev/nvidiactl").exists() {
            return None;
        }
        if self.cache.is_none_or(|(t, _)| t.elapsed() > std::time::Duration::from_secs(10)) {
            let mut kb = 0u64;
            for e in std::fs::read_dir("/proc").ok()?.flatten() {
                let pid = e.file_name();
                if !pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                let has = std::fs::read_dir(e.path().join("fd")).ok().is_some_and(|fds| {
                    fds.flatten().any(|f| std::fs::read_link(f.path()).ok().is_some_and(|t| t.to_string_lossy().starts_with("/dev/nvidia")))
                });
                if !has {
                    continue;
                }
                let Ok(smaps) = std::fs::read_to_string(e.path().join("smaps")) else { continue };
                let mut dev = false;
                for l in smaps.lines() {
                    let first = l.split_whitespace().next().unwrap_or("");
                    if first.contains('-') && first.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                        dev = l.split_whitespace().nth(5).is_some_and(|p| p.starts_with("/dev/nvidia"));
                    } else if dev && first == "Rss:" {
                        kb += l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                    }
                }
            }
            self.cache = Some((std::time::Instant::now(), kb as f64 / 1024.0));
        }
        let mb = self.cache.map(|c| c.1);
        Some(Group { title: "GPU [#0]: d3d".into(), items: vec![Item::new("GPU D3D Memory Dynamic", Kind::Memory, 0, mb)] })
    }
}

pub fn sample(msr: &mut Msr, shared: &mut GpuShared) -> Vec<Group> {
    it8915().into_iter().chain(msr.sample()).chain(shared.sample()).collect()
}
