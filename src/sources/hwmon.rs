//! Every hwmon chip through libsensors, so /etc/sensors.d labels, compute
//! lines (voltage dividers) and ignores apply exactly as in `sensors`.

use crate::model::{Item, Kind};
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::path::{Path, PathBuf};

#[repr(C)]
struct BusId {
    kind: i16,
    nr: i16,
}

#[repr(C)]
struct ChipName {
    prefix: *mut c_char,
    bus: BusId,
    addr: c_int,
    path: *mut c_char,
}

#[repr(C)]
struct Feature {
    name: *mut c_char,
    number: c_int,
    kind: c_int,
    first_subfeature: c_int,
    padding1: c_int,
}

#[repr(C)]
struct Subfeature {
    name: *mut c_char,
    number: c_int,
    kind: c_int,
    mapping: c_int,
    flags: c_uint,
}

#[link(name = "sensors")]
unsafe extern "C" {
    fn sensors_init(input: *mut c_void) -> c_int;
    fn sensors_get_detected_chips(m: *const ChipName, nr: *mut c_int) -> *const ChipName;
    fn sensors_get_features(name: *const ChipName, nr: *mut c_int) -> *const Feature;
    fn sensors_get_subfeature(name: *const ChipName, f: *const Feature, t: c_int) -> *const Subfeature;
    fn sensors_get_label(name: *const ChipName, f: *const Feature) -> *mut c_char;
    fn sensors_get_value(name: *const ChipName, subfeat_nr: c_int, value: *mut f64) -> c_int;
    fn free(p: *mut c_void);
}

const F_IN: c_int = 0x00;
const F_FAN: c_int = 0x01;
const F_TEMP: c_int = 0x02;
const F_POWER: c_int = 0x03;
const F_ENERGY: c_int = 0x04;
const F_CURR: c_int = 0x05;
const F_PWM: c_int = 0x07;

/// One chip's readings, plus where it lives in sysfs so callers can route it.
pub struct Chip {
    pub prefix: String,
    pub addr: i32,
    pub path: PathBuf,
    pub items: Vec<Item>,
}

/// Chips whose reads are slow device commands (NVMe admin / ATA SMART, 25-50 ms
/// each) and whose values change over minutes: read every SLOW_EVERY, cached between.
const SLOW_CHIPS: [&str; 2] = ["nvme", "drivetemp"];
const SLOW_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

pub struct Hwmon {
    ok: bool,
    slow: std::collections::HashMap<PathBuf, (std::time::Instant, Vec<Item>)>,
}

impl Hwmon {
    pub fn new() -> Self {
        Hwmon { ok: unsafe { sensors_init(std::ptr::null_mut()) } == 0, slow: Default::default() }
    }

    pub fn sample(&mut self) -> Vec<Chip> {
        let mut out = Vec::new();
        if !self.ok {
            return out;
        }
        unsafe {
            let mut cn = 0;
            loop {
                let chip = sensors_get_detected_chips(std::ptr::null(), &mut cn);
                if chip.is_null() {
                    break;
                }
                let c = &*chip;
                let prefix = CStr::from_ptr(c.prefix).to_string_lossy().into_owned();
                let path = PathBuf::from(CStr::from_ptr(c.path).to_string_lossy().into_owned());
                let slow = SLOW_CHIPS.contains(&prefix.as_str());
                if slow {
                    if let Some((t, items)) = self.slow.get(&path) {
                        if t.elapsed() < SLOW_EVERY {
                            out.push(Chip { prefix, addr: c.addr, path, items: items.clone() });
                            continue;
                        }
                    }
                }
                let mut items = Vec::new();
                let mut fn_ = 0;
                loop {
                    let feat = sensors_get_features(chip, &mut fn_);
                    if feat.is_null() {
                        break;
                    }
                    let f = &*feat;
                    let (kind, sub, dec, unit): (Kind, c_int, usize, Option<&'static str>) = match f.kind {
                        F_IN => (Kind::Volt, F_IN << 8, 3, None),
                        F_FAN => (Kind::Fan, F_FAN << 8, 0, None),
                        F_TEMP => (Kind::Temp, F_TEMP << 8, 1, None),
                        F_POWER => (Kind::Power, F_POWER << 8, 3, None),
                        F_ENERGY => (Kind::Other, F_ENERGY << 8, 1, Some("J")),
                        F_CURR => (Kind::Current, F_CURR << 8, 3, None),
                        F_PWM => (Kind::Usage, F_PWM << 8, 0, None),
                        _ => continue,
                    };
                    // Power: prefer powerN_input (SUBFEATURE_POWER_INPUT = 0x303), else average.
                    let mut sf = if f.kind == F_POWER { sensors_get_subfeature(chip, feat, (F_POWER << 8) | 3) } else { std::ptr::null() };
                    if sf.is_null() {
                        sf = sensors_get_subfeature(chip, feat, sub);
                    }
                    if sf.is_null() {
                        continue;
                    }
                    let mut v = 0.0;
                    let val = (sensors_get_value(chip, (*sf).number, &mut v) == 0).then_some(v);
                    let lp = sensors_get_label(chip, feat);
                    let label = if lp.is_null() {
                        CStr::from_ptr(f.name).to_string_lossy().into_owned()
                    } else {
                        let s = CStr::from_ptr(lp).to_string_lossy().into_owned();
                        free(lp as *mut c_void);
                        s
                    };
                    let mut it = Item::new(label, kind, dec, val);
                    it.unit = unit;
                    items.push(it);
                }
                if slow {
                    self.slow.insert(path.clone(), (std::time::Instant::now(), items.clone()));
                }
                out.push(Chip { prefix, addr: c.addr, path, items });
            }
        }
        out
    }
}

pub fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}
