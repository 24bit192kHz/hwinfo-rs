mod config;
mod cpumsr;
mod gui;
mod helper;
mod model;
mod privsrc;
mod sources;
mod summary;

use model::{fmt_value, key, Group, Kind, Stats};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Shared {
    pub groups: Vec<Group>,
    pub stats: Stats,
    pub since: Instant,
    pub interval_ms: u64,
    pub log: Option<CsvLog>,
    pub log_error: Option<String>,
    pub config: config::Config,
    /// Rows currently outside their alert range.
    pub alerting: std::collections::HashSet<String>,
    /// Per-row history for graphs (GUI only), newest last.
    pub history: Option<std::collections::HashMap<String, std::collections::VecDeque<f32>>>,
}

/// Graph history length: 30 minutes at the default 2 s period.
pub const HISTORY_LEN: usize = 900;

/// Every (key, display name, value) row, children included.
pub fn rows(groups: &[Group]) -> impl Iterator<Item = (String, &str, Option<f64>)> {
    groups.iter().flat_map(|g| {
        g.items.iter().flat_map(move |it| {
            std::iter::once((key(&g.title, &it.name, None), it.name.as_str(), it.value))
                .chain(it.children.iter().map(move |c| (key(&g.title, &it.name, Some(&c.name)), c.name.as_str(), c.value)))
        })
    })
}

/// Desktop notification (best effort; also printed for CLI users).
fn notify(summary: &str, body: &str) {
    eprintln!("hwinfo-rs alert: {summary}: {body}");
    let _ = std::process::Command::new("notify-send").args(["-a", "hwinfo-rs", "-u", "critical", summary, body]).stderr(std::process::Stdio::null()).spawn();
}

fn check_alerts(s: &mut Shared) {
    if s.config.alerts.is_empty() {
        s.alerting.clear();
        return;
    }
    let mut now = std::collections::HashSet::new();
    let mut fired = Vec::new();
    for (k, name, v) in rows(&s.groups) {
        let (Some(a), Some(v)) = (s.config.alerts.get(&k), v) else { continue };
        if a.max.is_some_and(|m| v > m) || a.min.is_some_and(|m| v < m) {
            if !s.alerting.contains(&k) {
                fired.push((name.to_string(), v, *a));
            }
            now.insert(k);
        }
    }
    for (name, v, a) in fired {
        let range = match (a.min, a.max) {
            (Some(lo), Some(hi)) => format!("outside {lo}..{hi}"),
            (Some(lo), None) => format!("below {lo}"),
            (None, Some(hi)) => format!("above {hi}"),
            _ => String::new(),
        };
        notify(&format!("{name} = {v:.2}"), &range);
    }
    s.alerting = now;
}

pub type State = Arc<Mutex<Shared>>;

/// HWiNFO-like CSV: Date, Time, then one column per row ("name [unit]").
pub struct CsvLog {
    pub path: std::path::PathBuf,
    file: std::fs::File,
    cols: Option<Vec<(String, String)>>,
}

impl CsvLog {
    pub fn create(path: std::path::PathBuf) -> std::io::Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        Ok(CsvLog { file: std::fs::File::create(&path)?, path, cols: None })
    }

    fn write(&mut self, groups: &[Group]) -> std::io::Result<()> {
        let mut row = std::collections::HashMap::new();
        let mut order = Vec::new();
        for g in groups {
            for it in &g.items {
                let k = key(&g.title, &it.name, None);
                order.push((k.clone(), format!("{} [{}]", it.name, it.unit_str())));
                row.insert(k, it.value);
                for c in &it.children {
                    let k = key(&g.title, &it.name, Some(&c.name));
                    order.push((k.clone(), format!("{} [{}]", c.name, c.unit_str())));
                    row.insert(k, c.value);
                }
            }
        }
        if self.cols.is_none() {
            let hdr: Vec<String> = order.iter().map(|(_, h)| csv_esc(h)).collect();
            writeln!(self.file, "Date,Time,{}", hdr.join(","))?;
            self.cols = Some(order);
        }
        let now = chrono_like_now();
        let vals: Vec<String> = self.cols.as_ref().unwrap().iter().map(|(k, _)| row.get(k).copied().flatten().map(|v| format!("{v:.3}")).unwrap_or_default()).collect();
        writeln!(self.file, "{},{}", now, vals.join(","))?;
        self.file.flush()
    }
}

fn csv_esc(s: &str) -> String {
    if s.contains([',', '"']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// "dd.mm.yyyy,hh:mm:ss.mmm" in local time, without pulling in chrono.
fn chrono_like_now() -> String {
    let out = std::process::Command::new("date").arg("+%d.%m.%Y,%H:%M:%S.%3N").output();
    out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn spawn_sampler(state: State, wake: Option<Box<dyn Fn() + Send>>) {
    std::thread::spawn(move || {
        let mut col = sources::Collector::new();
        loop {
            let t0 = Instant::now();
            let groups = col.sample();
            let interval = {
                let mut s = state.lock().unwrap();
                s.stats.update(&groups);
                if let Some(l) = s.log.as_mut() {
                    if let Err(e) = l.write(&groups) {
                        s.log_error = Some(e.to_string());
                        s.log = None;
                    }
                }
                s.groups = groups;
                check_alerts(&mut s);
                let Shared { history, groups, .. } = &mut *s;
                if let Some(h) = history {
                    for (k, _, v) in rows(groups) {
                        let q = h.entry(k).or_default();
                        if q.len() == HISTORY_LEN {
                            q.pop_front();
                        }
                        q.push_back(v.map(|v| v as f32).unwrap_or(f32::NAN));
                    }
                }
                s.interval_ms
            };
            if let Some(w) = &wake {
                w();
            }
            std::thread::sleep(Duration::from_millis(interval).saturating_sub(t0.elapsed()));
        }
    });
}

fn usage() {
    println!(
        "hwinfo-rs - HWiNFO-style sensor monitor for Linux

USAGE:
  hwinfo-rs                 GUI (Sensors Status window)
  hwinfo-rs --cli           live sensor table in the terminal (q / Ctrl-C to quit)
  hwinfo-rs --once          print one sample and exit
OPTIONS:
  -i, --interval MS         polling interval (default 2000)
  -e, --expand              CLI: show per-core / per-thread rows
  -l, --log FILE.csv        log every sample to CSV (also works with the GUI)
  -f, --filter TEXT         CLI: only groups/rows containing TEXT (case-insensitive)
      --waybar [FORMAT]     JSON lines for a Waybar custom module (exec, no interval);
                            FORMAT uses {{Sensor name}}, e.g. '{{CPU (Tctl/Tdie)}} {{GPU Hot Spot Temperature}}'
      --helper              root helper: write privileged sensors to /run/hwinfo-rs (systemd)
  -h, --help"
    );
}

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let (mut cli, mut once, mut expand, mut helper) = (false, false, false, false);
    let mut waybar: Option<String> = None;
    let mut interval = 2000u64;
    let mut interval_set = false;
    let mut log = None;
    let mut filter = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cli" | "-c" => cli = true,
            "--once" | "-1" => once = true,
            "--expand" | "-e" => expand = true,
            "--interval" | "-i" => {
                interval = args.next().and_then(|v| v.parse().ok()).unwrap_or(2000).max(250);
                interval_set = true;
            }
            "--log" | "-l" => log = args.next().map(std::path::PathBuf::from),
            "--filter" | "-f" => filter = args.next().map(|s| s.to_lowercase()),
            "--helper" => helper = true,
            "--waybar" => {
                waybar = Some(match args.next_if(|f| !f.starts_with('-')) {
                    Some(f) => f,
                    None => "  {CPU (Tctl/Tdie)}   {GPU Hot Spot Temperature}".to_string(),
                });
            }
            "--help" | "-h" => return usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
                std::process::exit(2);
            }
        }
    }

    if helper {
        return helper::run(Duration::from_millis(interval));
    }
    let log = match log.map(CsvLog::create) {
        Some(Ok(l)) => Some(l),
        Some(Err(e)) => {
            eprintln!("cannot create log: {e}");
            std::process::exit(1);
        }
        None => None,
    };
    let config = config::Config::load();
    if !interval_set {
        interval = config.interval_ms;
    }
    let gui = !(cli || once || waybar.is_some());
    let state: State = Arc::new(Mutex::new(Shared {
        groups: Vec::new(),
        stats: Stats::default(),
        since: Instant::now(),
        interval_ms: interval,
        log,
        log_error: None,
        config,
        alerting: Default::default(),
        history: gui.then(Default::default),
    }));

    if once {
        // Two samples so rates and usage have a delta to work with.
        let mut col = sources::Collector::new();
        col.sample();
        std::thread::sleep(Duration::from_millis(500));
        let g = col.sample();
        let mut s = state.lock().unwrap();
        s.stats.update(&g);
        s.groups = g;
        print!("{}", render_text(&s, expand, filter.as_deref(), false, 0));
        return;
    }
    if cli {
        return run_cli(state, expand, filter);
    }
    if let Some(fmt) = waybar {
        return run_waybar(state, &fmt);
    }
    gui::run(state, spawn_sampler);
}

/// Compact value for bars: "62°C", "45%", "1.21V".
fn compact(kind: Kind, unit: &str, dec: usize, v: f64) -> String {
    let d = if matches!(kind, Kind::Temp | Kind::Usage | Kind::Fan | Kind::Memory | Kind::Power) { 0 } else { dec.min(2) };
    format!("{}{}", fmt_value(kind, "", d, Some(v)).trim(), unit)
}

fn run_waybar(state: State, fmt: &str) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    spawn_sampler(state.clone(), Some(Box::new(move || {
        let _ = tx.send(());
    })));
    // Tokens between braces, resolved case-insensitively against row names.
    let tokens: Vec<String> = fmt.split('{').skip(1).filter_map(|t| t.split_once('}').map(|(n, _)| n.to_string())).collect();
    let mut out = std::io::stdout().lock();
    while rx.recv().is_ok() {
        let s = state.lock().unwrap();
        let find = |name: &str| -> Option<(String, String)> {
            let n = name.to_lowercase();
            s.groups.iter().find_map(|g| {
                g.items.iter().chain(g.items.iter().flat_map(|i| i.children.iter())).find(|i| i.name.to_lowercase() == n).and_then(|i| Some((i.name.clone(), compact(i.kind, i.unit_str(), i.decimals, i.value?))))
            })
        };
        let mut text = fmt.to_string();
        let mut tip = Vec::new();
        for t in &tokens {
            let (label, val) = find(t).unwrap_or_else(|| (t.clone(), "?".into()));
            text = text.replace(&format!("{{{t}}}"), &val);
            tip.push(format!("{label}: {val}"));
        }
        for k in &s.alerting {
            tip.push(format!("⚠ {}", k.replace('\u{1f}', " / ")));
        }
        let class = if s.alerting.is_empty() { "normal" } else { "critical" };
        let line = serde_json::json!({ "text": text, "tooltip": tip.join("\n"), "class": class });
        drop(s);
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn run_cli(state: State, expand: bool, filter: Option<String>) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    spawn_sampler(state.clone(), Some(Box::new(move || {
        let _ = tx.send(());
    })));
    // Raw-ish terminal: hide cursor, alternate screen; restore on exit.
    print!("\x1b[?1049h\x1b[?25l");
    let _ = std::process::Command::new("stty").args(["-icanon", "-echo", "min", "0"]).status();
    let restore = || {
        let _ = std::process::Command::new("stty").arg("sane").status();
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
    };
    let quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let quit = quit.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 1];
            loop {
                if let Ok(1) = std::io::Read::read(&mut std::io::stdin(), &mut b) {
                    if b[0] == b'q' || b[0] == 3 {
                        quit.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }
    let _ = ctrlc_hook(quit.clone());
    loop {
        if quit.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        if rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            let (cols, rows) = term_size();
            let s = state.lock().unwrap();
            let text = render_text(&s, expand, filter.as_deref(), true, cols);
            drop(s);
            let mut out = String::from("\x1b[H");
            for l in text.lines().take(rows.saturating_sub(1).max(1)) {
                out.push_str(l);
                out.push_str("\x1b[K\r\n");
            }
            out.push_str("\x1b[J");
            print!("{out}");
            let _ = std::io::stdout().flush();
        }
    }
    restore();
}

/// SIGINT sets the quit flag instead of killing us with the terminal in raw mode.
fn ctrlc_hook(flag: Arc<std::sync::atomic::AtomicBool>) -> std::io::Result<()> {
    static FLAG: std::sync::OnceLock<Arc<std::sync::atomic::AtomicBool>> = std::sync::OnceLock::new();
    let _ = FLAG.set(flag);
    extern "C" fn on_sigint(_: i32) {
        if let Some(f) = FLAG.get() {
            f.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    unsafe extern "C" {
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }
    unsafe { signal(2, on_sigint) };
    Ok(())
}

fn term_size() -> (usize, usize) {
    #[repr(C)]
    struct WinSize {
        row: u16,
        col: u16,
        x: u16,
        y: u16,
    }
    unsafe extern "C" {
        fn ioctl(fd: i32, req: u64, ...) -> i32;
    }
    let mut ws = WinSize { row: 0, col: 0, x: 0, y: 0 };
    if unsafe { ioctl(1, 0x5413, &mut ws) } == 0 && ws.col > 0 {
        (ws.col as usize, ws.row as usize)
    } else {
        (120, 50)
    }
}

/// The same table as the GUI, as text. `color` adds HWiNFO-like ANSI styling.
pub fn render_text(s: &Shared, expand: bool, filter: Option<&str>, color: bool, width: usize) -> String {
    let vw = 14;
    let name_w = if width > 0 { width.saturating_sub(4 * (vw + 1) + 1).clamp(24, 60) } else { 44 };
    let (rst, hdr, grp, yes, dim) = if color {
        ("\x1b[0m", "\x1b[1;97;100m", "\x1b[3;96m", "\x1b[91m", "\x1b[90m")
    } else {
        ("", "", "", "", "")
    };
    let mut out = String::new();
    let el = s.since.elapsed().as_secs();
    let log = s.log.as_ref().map(|l| format!("  logging → {}", l.path.display())).unwrap_or_default();
    out.push_str(&format!("{hdr}{:<name_w$} {:>vw$} {:>vw$} {:>vw$} {:>vw$}{rst}\n", "Sensor", "Current", "Minimum", "Maximum", "Average"));
    let cut = |t: &str, w: usize| -> String {
        if t.chars().count() > w { format!("{}…", t.chars().take(w - 1).collect::<String>()) } else { t.to_string() }
    };
    for g in &s.groups {
        let gmatch = filter.is_none_or(|f| g.title.to_lowercase().contains(f));
        let rows: Vec<_> = g
            .items
            .iter()
            .filter(|it| gmatch || filter.is_some_and(|f| it.name.to_lowercase().contains(f) || it.children.iter().any(|c| c.name.to_lowercase().contains(f))))
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("{grp}▾ {}{rst}\n", cut(&g.title, name_w + 4 * (vw + 1) - 2)));
        for it in rows {
            let mut line = |name: &str, k: String, kind: Kind, unit: &str, dec: usize, v: Option<f64>, indent: &str| {
                let st = s.stats.get(&k);
                let f = |x: Option<f64>| fmt_value(kind, unit, dec, x);
                let cur = f(v);
                let (mn, mx) = (f(st.map(|s| s.min)), f(st.map(|s| s.max)));
                let av = if kind == Kind::Bool { String::new() } else { f(st.map(|s| s.avg())) };
                let paint = |t: String| if color && t == "Yes" && gui::is_alarm_flag(name) { format!("{yes}{t:>vw$}{rst}") } else { format!("{t:>vw$}") };
                out.push_str(&format!("{:<name_w$} {} {} {} {}\n", cut(&format!("{indent}{name}"), name_w), paint(cur), paint(mn), paint(mx), paint(av)));
            };
            let marker = if it.children.is_empty() { "  " } else if expand { "▾ " } else { "▸ " };
            line(&it.name, key(&g.title, &it.name, None), it.kind, it.unit_str(), it.decimals, it.value, marker);
            if expand {
                for c in &it.children {
                    line(&c.name, key(&g.title, &it.name, Some(&c.name)), c.kind, c.unit_str(), c.decimals, c.value, "      ");
                }
            }
        }
        out.push('\n');
    }
    out.push_str(&format!("{dim}{:02}:{:02}:{:02}  interval {} ms{log}   q = quit{rst}\n", el / 3600, el / 60 % 60, el % 60, s.interval_ms));
    out
}
