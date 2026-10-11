//! Sensor tree shared by every source and both front ends.

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Temp,
    Volt,
    Clock,
    Power,
    Current,
    Usage,
    Fan,
    Memory,
    Rate,
    Bool,
    Other,
}

impl Kind {
    pub fn unit(self) -> &'static str {
        match self {
            Kind::Temp => "°C",
            Kind::Volt => "V",
            Kind::Clock => "MHz",
            Kind::Power => "W",
            Kind::Current => "A",
            Kind::Usage => "%",
            Kind::Fan => "RPM",
            Kind::Memory => "MB",
            Kind::Rate => "MB/s",
            Kind::Bool | Kind::Other => "",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub kind: Kind,
    /// Overrides `kind.unit()` (e.g. "x", "KB/s", "GT/s").
    pub unit: Option<&'static str>,
    pub decimals: usize,
    pub value: Option<f64>,
    pub children: Vec<Item>,
}

impl Item {
    pub fn new(name: impl Into<String>, kind: Kind, decimals: usize, value: Option<f64>) -> Self {
        Item { name: name.into(), kind, unit: None, decimals, value, children: Vec::new() }
    }

    pub fn unit(mut self, u: &'static str) -> Self {
        self.unit = Some(u);
        self
    }

    /// A parent row whose value is the average of its children (HWiNFO style).
    pub fn parent(name: impl Into<String>, kind: Kind, decimals: usize, children: Vec<Item>) -> Self {
        let vals: Vec<f64> = children.iter().filter_map(|c| c.value).collect();
        let value = (!vals.is_empty()).then(|| vals.iter().sum::<f64>() / vals.len() as f64);
        let unit = children.first().and_then(|c| c.unit);
        Item { name: name.into(), kind, unit, decimals, value, children }
    }

    pub fn unit_str(&self) -> &'static str {
        self.unit.unwrap_or_else(|| self.kind.unit())
    }
}

#[derive(Clone, Debug)]
pub struct Group {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub n: u64,
}

impl Stat {
    pub fn avg(&self) -> f64 {
        self.sum / self.n.max(1) as f64
    }
}

/// Min/max/average per row, keyed by "group\u{1f}item\u{1f}child".
#[derive(Default)]
pub struct Stats {
    map: HashMap<String, Stat>,
}

pub fn key(group: &str, item: &str, child: Option<&str>) -> String {
    match child {
        Some(c) => format!("{group}\u{1f}{item}\u{1f}{c}"),
        None => format!("{group}\u{1f}{item}"),
    }
}

impl Stats {
    pub fn update(&mut self, groups: &[Group]) {
        for g in groups {
            for it in &g.items {
                self.push(key(&g.title, &it.name, None), it.value);
                for c in &it.children {
                    self.push(key(&g.title, &it.name, Some(&c.name)), c.value);
                }
            }
        }
    }

    fn push(&mut self, k: String, v: Option<f64>) {
        let Some(v) = v.filter(|v| v.is_finite()) else { return };
        let s = self.map.entry(k).or_insert(Stat { min: v, max: v, sum: 0.0, n: 0 });
        s.min = s.min.min(v);
        s.max = s.max.max(v);
        s.sum += v;
        s.n += 1;
    }

    pub fn get(&self, k: &str) -> Option<&Stat> {
        self.map.get(k)
    }

    pub fn reset(&mut self) {
        self.map.clear();
    }
}

/// HWiNFO-style number: thousands separators, fixed decimals, unit suffix.
pub fn fmt_value(kind: Kind, unit: &str, decimals: usize, v: Option<f64>) -> String {
    let Some(v) = v else { return String::new() };
    if kind == Kind::Bool {
        return if v >= 0.5 { "Yes".into() } else { "No".into() };
    }
    let s = format!("{:.*}", decimals, v.abs());
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (s, None),
    };
    let mut grouped = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let sign = if v < 0.0 && v.abs() >= 0.5 * 10f64.powi(-(decimals as i32)) { "-" } else { "" };
    let num = match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    };
    if unit.is_empty() { num } else { format!("{num} {unit}") }
}
