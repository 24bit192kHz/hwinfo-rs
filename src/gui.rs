//! "Sensors Status" window drawn to look like HWiNFO64's dark theme:
//! Sensor / Current / Minimum / Maximum / Average, italic group rows with a
//! chip icon, alternating black/grey rows with a navy Current column, red
//! "Yes" flags, and the bottom toolbar with the elapsed timer.

use crate::model::{fmt_value, key, Group, Kind};
use crate::{CsvLog, State};
use egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, Vec2,
};
use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::window::{Window, WindowId};

const ROW_H: f32 = 22.0;
const FONT: f32 = 14.5;
const BAR_H: f32 = 54.0;

const BG: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
const ROW_A: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
const ROW_B: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
const CUR_A: Color32 = Color32::from_rgb(0x0b, 0x17, 0x2e);
const CUR_B: Color32 = Color32::from_rgb(0x12, 0x21, 0x3d);
const GROUP_BG: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);
const HEADER_BG: Color32 = Color32::from_rgb(0x2b, 0x2b, 0x2b);
const GRID: Color32 = Color32::from_rgb(0x38, 0x38, 0x38);
const TEXT: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf2);
const GROUP_TEXT: Color32 = Color32::from_rgb(0xe0, 0xe0, 0xe0);
const RED: Color32 = Color32::from_rgb(0xff, 0x4d, 0x4d);
const SELECT: Color32 = Color32::from_rgb(0x26, 0x4f, 0x78);
const DIM: Color32 = Color32::from_rgb(0x70, 0x70, 0x70);
const GRAPH_LINE: Color32 = Color32::from_rgb(0xff, 0xd0, 0x30);
const GRAPH_GRID: Color32 = Color32::from_rgb(0x10, 0x40, 0x10);

#[derive(Clone, Copy)]
enum Row {
    Group(usize),
    Item(usize, usize),
    Child(usize, usize, usize),
    Blank,
}

struct App {
    state: State,
    collapsed: HashSet<String>,
    expanded: HashSet<String>,
    selected: Option<String>,
    show_settings: bool,
    show_filter: bool,
    filter: String,
    copied_at: Option<Instant>,
    /// Open graph windows (row keys).
    graphs: Vec<String>,
    /// Rename dialog: (key or group title, is_group, text).
    rename: Option<(String, bool, String)>,
    /// Alert dialog: (key, row name, min text, max text).
    alert_edit: Option<(String, String, String, String)>,
    show_summary: bool,
    summary: Option<Vec<(String, Vec<(String, String)>)>>,
}

type Sampler = fn(State, Option<Box<dyn Fn() + Send>>);

/// Wakes the event loop when the sampler has new data.
struct DataReady;

struct Gfx {
    window: Rc<Window>,
    surface: softbuffer::Surface<OwnedDisplayHandle, Rc<Window>>,
    egui_winit: egui_winit::State,
}

/// winit + softbuffer + CPU rasterizer instead of eframe/GL: no GL driver in
/// the process (~70 MB less), no GPU context, and presenting never blocks, so
/// the compositor's pings are always answered even when the window is hidden.
struct Runner {
    app: App,
    ctx: egui::Context,
    softbuffer: softbuffer::Context<OwnedDisplayHandle>,
    renderer: EguiSoftwareRender,
    gfx: Option<Gfx>,
    occluded: bool,
    next_repaint: Option<Instant>,
}

pub fn run(state: State, spawn: Sampler) {
    let event_loop = match EventLoop::<DataReady>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("GUI failed: {e}\n(try --cli)");
            std::process::exit(1);
        }
    };
    let proxy = event_loop.create_proxy();
    spawn(state.clone(), Some(Box::new(move || {
        let _ = proxy.send_event(DataReady);
    })));
    let ctx = egui::Context::default();
    setup_style(&ctx);
    let softbuffer = softbuffer::Context::new(event_loop.owned_display_handle()).expect("softbuffer context");
    let mut runner = Runner {
        app: App {
            state,
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            selected: None,
            show_settings: false,
            show_filter: false,
            filter: String::new(),
            copied_at: None,
            graphs: Vec::new(),
            rename: None,
            alert_edit: None,
            show_summary: false,
            summary: None,
        },
        ctx,
        softbuffer,
        renderer: EguiSoftwareRender::new(ColorFieldOrder::Bgra).with_caching(false),
        gfx: None,
        occluded: false,
        next_repaint: None,
    };
    if let Err(e) = event_loop.run_app(&mut runner) {
        eprintln!("GUI failed: {e}\n(try --cli)");
        std::process::exit(1);
    }
}

impl Runner {
    fn redraw(&self) {
        if let Some(g) = &self.gfx {
            if !self.occluded {
                g.window.request_redraw();
            }
        }
    }

    fn paint(&mut self, el: &ActiveEventLoop) {
        let Some(g) = self.gfx.as_mut() else { return };
        let size = g.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        let t0 = Instant::now();
        let raw = g.egui_winit.take_egui_input(&g.window);
        let app = &mut self.app;
        let out = self.ctx.run_ui(raw, |ui| app.ui(ui));
        let t1 = Instant::now();
        g.egui_winit.handle_platform_output(&g.window, out.platform_output);
        if let Some(vp) = out.viewport_output.get(&egui::ViewportId::ROOT) {
            if vp.commands.iter().any(|c| matches!(c, egui::ViewportCommand::Close)) {
                el.exit();
                return;
            }
            self.next_repaint = (vp.repaint_delay < Duration::from_secs(3600)).then(|| Instant::now() + vp.repaint_delay);
        }
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let t2 = Instant::now();
        if g.surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = g.surface.buffer_mut() else { return };
        let mut target = BufferMutRef::new(bytemuck::cast_slice_mut(&mut buf), w.get() as usize, h.get() as usize);
        self.renderer.render(&mut target, &prims, &out.textures_delta, out.pixels_per_point);
        let t3 = Instant::now();
        // Throttle to the compositor's frame callbacks: a hidden window gets none,
        // so it stops repainting instead of rasterizing frames nobody sees.
        g.window.pre_present_notify();
        let _ = buf.present();
        if std::env::var_os("HWINFO_RS_PROFILE").is_some() {
            let ms = |a: Instant, b: Instant| b.duration_since(a).as_secs_f64() * 1000.0;
            eprintln!("frame: ui {:.1} ms, tessellate {:.1} ms, raster {:.1} ms, present {:.1} ms, {} prims", ms(t0, t1), ms(t1, t2), ms(t2, t3), ms(t3, Instant::now()), prims.len());
        }
    }
}

impl ApplicationHandler<DataReady> for Runner {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        use winit::platform::wayland::WindowAttributesExtWayland;
        let attrs = Window::default_attributes()
            .with_title("HWiNFO-rs - Sensors Status")
            .with_inner_size(LogicalSize::new(820.0, 960.0))
            .with_min_inner_size(LogicalSize::new(520.0, 300.0));
        let attrs = WindowAttributesExtWayland::with_name(attrs, "hwinfo-rs", "hwinfo-rs");
        let attrs = winit::platform::x11::WindowAttributesExtX11::with_name(attrs, "hwinfo-rs", "hwinfo-rs");
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("GUI failed: {e}\n(try --cli)");
                el.exit();
                return;
            }
        };
        let surface = softbuffer::Surface::new(&self.softbuffer, window.clone()).expect("softbuffer surface");
        let egui_winit = egui_winit::State::new(self.ctx.clone(), egui::ViewportId::ROOT, &*window, Some(window.scale_factor() as f32), None, None);
        window.request_redraw();
        self.gfx = Some(Gfx { window, surface, egui_winit });
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: DataReady) {
        self.redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(g) = self.gfx.as_mut() else { return };
        let resp = g.egui_winit.on_window_event(&g.window, &event);
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.paint(el),
            WindowEvent::Occluded(o) => {
                self.occluded = o;
                self.redraw();
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.redraw(),
            _ if resp.repaint => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        match self.next_repaint {
            Some(t) if t <= Instant::now() => {
                self.next_repaint = None;
                self.redraw();
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Segoe UI (regular + italic) from the system if present, like HWiNFO on Windows.
fn setup_style(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let find = |names: &[&str]| -> Option<Vec<u8>> {
        let home = std::env::var("HOME").unwrap_or_default();
        for n in names {
            for dir in ["/usr/share/fonts", &format!("{home}/.local/share/fonts"), "/usr/share/fonts/TTF", "/usr/share/fonts/noto"] {
                if let Ok(b) = std::fs::read(format!("{dir}/{n}")) {
                    return Some(b);
                }
            }
        }
        None
    };
    let mut prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut italic = prop.clone();
    if let Some(b) = find(&["segoeui.ttf", "NotoSans-Regular.ttf"]) {
        fonts.font_data.insert("ui".into(), Arc::new(FontData::from_owned(b)));
        prop.insert(0, "ui".into());
        italic.insert(0, "ui".into());
    }
    if let Some(b) = find(&["segoeuii.ttf", "NotoSans-Italic.ttf"]) {
        fonts.font_data.insert("ui-italic".into(), Arc::new(FontData::from_owned(b)));
        italic.insert(0, "ui-italic".into());
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Name("italic".into()), italic);
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = Color32::from_rgb(0x24, 0x24, 0x24);
    v.extreme_bg_color = Color32::from_rgb(0x17, 0x17, 0x17);
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x4d, 0x4d, 0x4d);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x6a, 0x6a, 0x6a);
    v.widgets.active.bg_fill = Color32::from_rgb(0x80, 0x80, 0x80);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.scroll = egui::style::ScrollStyle::solid();
        s.spacing.scroll.bar_width = 14.0;
    });
}

fn font() -> FontId {
    FontId::proportional(FONT)
}
fn italic() -> FontId {
    FontId::new(FONT, FontFamily::Name("italic".into()))
}

impl App {
    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, BG);

        let (groups, since, logging, log_err, interval, cfg, alerting) = {
            let s = self.state.lock().unwrap();
            let groups = s.config.order(s.groups.clone(), |g: &Group| g.title.as_str());
            (groups, s.since, s.log.as_ref().map(|l| l.path.clone()), s.log_error.clone(), s.interval_ms, s.config.clone(), s.alerting.clone())
        };
        let mut cfg_changed: Option<crate::config::Config> = None;

        // Layout: header row, table, toolbar.
        let scroll_w = 14.0;
        let table = Rect::from_min_max(full.min, Pos2::new(full.max.x, full.max.y - BAR_H));
        let val_w = ((table.width() - scroll_w) * 0.15).clamp(96.0, 170.0);
        let name_w = table.width() - scroll_w - 4.0 * val_w;
        let col_x = |c: usize| table.min.x + name_w + val_w * c as f32;

        let filter_h = if self.show_filter { 30.0 } else { 0.0 };
        let header = Rect::from_min_size(table.min + Vec2::new(0.0, filter_h), Vec2::new(table.width(), ROW_H + 4.0));
        if self.show_filter {
            let r = Rect::from_min_size(table.min, Vec2::new(table.width(), filter_h));
            ui.painter().rect_filled(r, 0.0, HEADER_BG);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r.shrink2(Vec2::new(8.0, 4.0))));
            child.horizontal(|ui| {
                ui.label(egui::RichText::new("Filter:").color(TEXT).font(font()));
                let te = ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(f32::INFINITY).font(font()));
                if !te.has_focus() && self.filter.is_empty() {
                    te.request_focus();
                }
            });
        }
        self.paint_header(ui, header, &col_x, val_w);

        // Flatten rows.
        let flt = self.filter.to_lowercase();
        let mut rows = Vec::new();
        for (gi, g) in groups.iter().enumerate() {
            let gmatch = flt.is_empty() || g.title.to_lowercase().contains(&flt);
            let items: Vec<usize> = (0..g.items.len())
                .filter(|&ii| gmatch || g.items[ii].name.to_lowercase().contains(&flt) || g.items[ii].children.iter().any(|c| c.name.to_lowercase().contains(&flt)))
                .collect();
            if items.is_empty() {
                continue;
            }
            if cfg.hidden.contains(&g.title) && !cfg.show_hidden {
                continue;
            }
            rows.push(Row::Group(gi));
            if !self.collapsed.contains(&g.title) {
                for ii in items {
                    let it = &g.items[ii];
                    let k = key(&g.title, &it.name, None);
                    if cfg.hidden.contains(&k) && !cfg.show_hidden {
                        continue;
                    }
                    rows.push(Row::Item(gi, ii));
                    if self.expanded.contains(&k) {
                        for ci in 0..it.children.len() {
                            if cfg.show_hidden || !cfg.hidden.contains(&key(&g.title, &it.name, Some(&it.children[ci].name))) {
                                rows.push(Row::Child(gi, ii, ci));
                            }
                        }
                    }
                }
            }
            rows.push(Row::Blank);
        }

        let body = Rect::from_min_max(Pos2::new(table.min.x, header.max.y), table.max);
        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body));
        body_ui.painter().rect_filled(body, 0.0, ROW_A);
        // show_rows sizes its visible range with item_spacing, so zero it first.
        body_ui.spacing_mut().item_spacing = Vec2::ZERO;
        let mut toggle_group = None;
        let mut toggle_item = None;
        let mut select = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(body.height()).show_rows(&mut body_ui, ROW_H, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            for i in range {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(table.width() - scroll_w, ROW_H), Sense::click());
                let p = ui.painter();
                let zebra = i % 2 == 0;
                match rows[i] {
                    Row::Group(gi) => {
                        let g = &groups[gi];
                        p.rect_filled(rect, 0.0, GROUP_BG);
                        for c in 0..4 {
                            p.line_segment([Pos2::new(col_x(c), rect.min.y), Pos2::new(col_x(c), rect.max.y)], Stroke::new(1.0, GRID));
                        }
                        let open = !self.collapsed.contains(&g.title);
                        chevron(p, Pos2::new(rect.min.x + 16.0, rect.center().y), open);
                        chip_icon(p, Pos2::new(rect.min.x + 40.0, rect.center().y));
                        let dim = cfg.hidden.contains(&g.title);
                        clip_text(p, Rect::from_min_max(Pos2::new(rect.min.x + 52.0, rect.min.y), Pos2::new(col_x(0) - 4.0, rect.max.y)), &cfg.group_title(&g.title), italic(), if dim { DIM } else { GROUP_TEXT });
                        if resp.clicked() {
                            toggle_group = Some(g.title.clone());
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Rename group…").clicked() {
                                self.rename = Some((g.title.clone(), true, cfg.group_title(&g.title)));
                                ui.close();
                            }
                            for (label, delta) in [("Move up", -1isize), ("Move down", 1)] {
                                if ui.button(label).clicked() {
                                    let mut c = cfg.clone();
                                    let mut order: Vec<String> = groups.iter().map(|g| g.title.clone()).collect();
                                    if let Some(i) = order.iter().position(|t| t == &g.title) {
                                        let j = (i as isize + delta).clamp(0, order.len() as isize - 1) as usize;
                                        order.swap(i, j);
                                    }
                                    c.group_order = order;
                                    cfg_changed = Some(c);
                                    ui.close();
                                }
                            }
                            let hidden = cfg.hidden.contains(&g.title);
                            if ui.button(if hidden { "Unhide group" } else { "Hide group" }).clicked() {
                                let mut c = cfg.clone();
                                if !c.hidden.remove(&g.title) {
                                    c.hidden.insert(g.title.clone());
                                }
                                cfg_changed = Some(c);
                                ui.close();
                            }
                        });
                    }
                    Row::Item(gi, ii) | Row::Child(gi, ii, _) => {
                        let g = &groups[gi];
                        let parent = &g.items[ii];
                        let (it, k, indent) = match rows[i] {
                            Row::Child(_, _, ci) => (&parent.children[ci], key(&g.title, &parent.name, Some(&parent.children[ci].name)), 40.0),
                            _ => (parent, key(&g.title, &parent.name, None), 0.0),
                        };
                        let selected = self.selected.as_deref() == Some(k.as_str());
                        let (bg, cur_bg) = if selected { (SELECT, SELECT) } else if zebra { (ROW_A, CUR_A) } else { (ROW_B, CUR_B) };
                        p.rect_filled(rect, 0.0, bg);
                        p.rect_filled(Rect::from_min_max(Pos2::new(col_x(0), rect.min.y), Pos2::new(col_x(1), rect.max.y)), 0.0, cur_bg);
                        for c in 0..4 {
                            p.line_segment([Pos2::new(col_x(c), rect.min.y), Pos2::new(col_x(c), rect.max.y)], Stroke::new(1.0, GRID));
                        }
                        let mut x = rect.min.x + 16.0 + indent;
                        let is_parent = matches!(rows[i], Row::Item(..)) && !it.children.is_empty();
                        if is_parent {
                            let open = self.expanded.contains(&k);
                            chevron(p, Pos2::new(x, rect.center().y), open);
                            x += 24.0;
                        } else if matches!(rows[i], Row::Child(..)) {
                            x += 24.0;
                        }
                        kind_icon(p, Pos2::new(x + 8.0, rect.center().y), it.kind);
                        let shown = cfg.renames.get(&k).cloned().unwrap_or_else(|| it.name.clone());
                        let dim = cfg.hidden.contains(&k);
                        clip_text(p, Rect::from_min_max(Pos2::new(x + 20.0, rect.min.y), Pos2::new(col_x(0) - 4.0, rect.max.y)), &shown, font(), if dim { DIM } else { TEXT });

                        let st = self.state.lock().unwrap().stats.get(&k).copied();
                        let unit = it.unit_str();
                        let f = |v: Option<f64>| fmt_value(it.kind, unit, it.decimals, v);
                        let vals = [
                            f(it.value),
                            f(st.map(|s| s.min)),
                            f(st.map(|s| s.max)),
                            if it.kind == Kind::Bool { String::new() } else { f(st.map(|s| s.avg())) },
                        ];
                        let alarm = alerting.contains(&k);
                        for (c, v) in vals.iter().enumerate() {
                            let color = if (v == "Yes" && is_alarm_flag(&it.name)) || (c == 0 && alarm) { RED } else if dim { DIM } else { TEXT };
                            p.text(Pos2::new(col_x(c + 1) - 7.0, rect.center().y), Align2::RIGHT_CENTER, v, font(), color);
                        }
                        if resp.clicked() {
                            if is_parent && resp.interact_pointer_pos().is_some_and(|pp| pp.x < rect.min.x + 32.0 + indent) {
                                toggle_item = Some(k.clone());
                            } else {
                                select = Some(k.clone());
                            }
                        }
                        if resp.double_clicked() {
                            if is_parent {
                                toggle_item = Some(k.clone());
                            } else if !self.graphs.contains(&k) {
                                self.graphs.push(k.clone());
                            }
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Show graph").clicked() {
                                if !self.graphs.contains(&k) {
                                    self.graphs.push(k.clone());
                                }
                                ui.close();
                            }
                            if ui.button("Rename…").clicked() {
                                self.rename = Some((k.clone(), false, shown.clone()));
                                ui.close();
                            }
                            if ui.button(if dim { "Unhide" } else { "Hide" }).clicked() {
                                let mut c = cfg.clone();
                                if !c.hidden.remove(&k) {
                                    c.hidden.insert(k.clone());
                                }
                                cfg_changed = Some(c);
                                ui.close();
                            }
                            if it.kind != Kind::Bool && ui.button("Set alert…").clicked() {
                                let a = cfg.alerts.get(&k).copied().unwrap_or_default();
                                let f = |v: Option<f64>| v.map(|x| x.to_string()).unwrap_or_default();
                                self.alert_edit = Some((k.clone(), shown.clone(), f(a.min), f(a.max)));
                                ui.close();
                            }
                        });
                    }
                    Row::Blank => {
                        p.rect_filled(rect, 0.0, if zebra { ROW_A } else { ROW_B });
                        for c in 0..4 {
                            p.line_segment([Pos2::new(col_x(c), rect.min.y), Pos2::new(col_x(c), rect.max.y)], Stroke::new(1.0, GRID));
                        }
                    }
                }
            }
        });
        if let Some(t) = toggle_group {
            if !self.collapsed.remove(&t) {
                self.collapsed.insert(t);
            }
        }
        if let Some(k) = toggle_item {
            if !self.expanded.remove(&k) {
                self.expanded.insert(k);
            }
        }
        if let Some(k) = select {
            self.selected = if self.selected.as_deref() == Some(k.as_str()) { None } else { Some(k) };
        }

        self.dialogs(&ctx, &cfg, &groups, &mut cfg_changed);
        if let Some(c) = cfg_changed {
            c.save();
            self.state.lock().unwrap().config = c;
        }

        let bar = Rect::from_min_max(Pos2::new(full.min.x, full.max.y - BAR_H), full.max);
        self.paint_toolbar(ui, &ctx, bar, &groups, since, logging, log_err, interval);
    }
}

impl App {
    /// Rename / alert dialogs, graph windows and the summary window.
    fn dialogs(&mut self, ctx: &egui::Context, cfg: &crate::config::Config, groups: &[Group], changed: &mut Option<crate::config::Config>) {
        if let Some((target, is_group, mut text)) = self.rename.take() {
            let mut open = true;
            let mut done = None;
            egui::Window::new("Rename").collapsible(false).resizable(false).open(&mut open).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(300.0));
                r.request_focus();
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        done = Some(true);
                    }
                    if ui.button("Reset").clicked() {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(set) => {
                    let mut c = changed.take().unwrap_or_else(|| cfg.clone());
                    if set && !text.trim().is_empty() {
                        c.renames.insert(target, text.trim().to_string());
                    } else {
                        c.renames.remove(&target);
                    }
                    let _ = is_group;
                    *changed = Some(c);
                }
                None if open => self.rename = Some((target, is_group, text)),
                None => {}
            }
        }

        if let Some((k, name, mut lo, mut hi)) = self.alert_edit.take() {
            let mut open = true;
            let mut done = None;
            egui::Window::new(format!("Alert: {name}")).collapsible(false).resizable(false).open(&mut open).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label("Notify when the value leaves this range (leave a side empty to ignore it).");
                ui.horizontal(|ui| {
                    ui.label("Min:");
                    ui.add(egui::TextEdit::singleline(&mut lo).desired_width(90.0));
                    ui.label("Max:");
                    ui.add(egui::TextEdit::singleline(&mut hi).desired_width(90.0));
                });
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Remove alert").clicked() {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(set) => {
                    let mut c = changed.take().unwrap_or_else(|| cfg.clone());
                    let p = |s: &str| s.trim().replace(',', ".").parse::<f64>().ok();
                    let a = crate::config::Alert { min: p(&lo), max: p(&hi) };
                    if set && (a.min.is_some() || a.max.is_some()) {
                        c.alerts.insert(k, a);
                    } else {
                        c.alerts.remove(&k);
                    }
                    *changed = Some(c);
                }
                None if open => self.alert_edit = Some((k, name, lo, hi)),
                None => {}
            }
        }

        // Graph windows (HWiNFO style: black, green grid, yellow trace).
        let mut close = Vec::new();
        for k in &self.graphs {
            let Some((name, kind, unit, dec)) = groups.iter().find_map(|g| {
                g.items.iter().find_map(|it| {
                    if key(&g.title, &it.name, None) == *k {
                        return Some((it.name.clone(), it.kind, it.unit_str(), it.decimals));
                    }
                    it.children.iter().find(|c| key(&g.title, &it.name, Some(&c.name)) == *k).map(|c| (c.name.clone(), c.kind, c.unit_str(), c.decimals))
                })
            }) else {
                continue;
            };
            let name = cfg.renames.get(k).cloned().unwrap_or(name);
            let hist: Vec<f32> = self.state.lock().unwrap().history.as_ref().and_then(|h| h.get(k)).map(|q| q.iter().copied().collect()).unwrap_or_default();
            let mut open = true;
            egui::Window::new(&name).id(egui::Id::new(("graph", k))).open(&mut open).default_size([420.0, 200.0]).resizable(true).show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(ui.available_size().max(Vec2::new(200.0, 100.0)), Sense::hover());
                graph(ui.painter(), rect, &hist, kind, unit, dec);
            });
            if !open {
                close.push(k.clone());
            }
        }
        self.graphs.retain(|k| !close.contains(k));

        if self.show_summary {
            let data = self.summary.get_or_insert_with(crate::summary::collect);
            let mut open = true;
            egui::Window::new("System Summary").open(&mut open).default_size([520.0, 640.0]).resizable(true).show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (section, rows) in data.iter() {
                        ui.label(egui::RichText::new(section).font(italic()).color(GROUP_TEXT));
                        egui::Grid::new(section).num_columns(2).striped(true).spacing([16.0, 2.0]).show(ui, |ui| {
                            for (k, v) in rows {
                                let v = if cfg.hide_serials && (k.to_lowercase().contains("serial") || k.ends_with("MAC") || k == "Hostname") { "(hidden)".to_string() } else { v.clone() };
                                ui.label(egui::RichText::new(k).color(DIM));
                                ui.label(egui::RichText::new(v).color(TEXT));
                                ui.end_row();
                            }
                        });
                        ui.add_space(8.0);
                    }
                });
            });
            if !open {
                self.show_summary = false;
                self.summary = None;
            }
        }
    }

    fn paint_header(&self, ui: &egui::Ui, r: Rect, col_x: &dyn Fn(usize) -> f32, val_w: f32) {
        let p = ui.painter();
        p.rect_filled(r, 0.0, HEADER_BG);
        p.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, GRID));
        p.text(Pos2::new(r.min.x + 8.0, r.center().y), Align2::LEFT_CENTER, "Sensor", font(), TEXT);
        for (c, t) in ["Current", "Minimum", "Maximum", "Average"].iter().enumerate() {
            let x = col_x(c);
            p.line_segment([Pos2::new(x, r.min.y), Pos2::new(x, r.max.y)], Stroke::new(1.0, GRID));
            p.text(Pos2::new(x + val_w - 7.0, r.center().y), Align2::RIGHT_CENTER, *t, font(), TEXT);
        }
        let x = col_x(4);
        p.line_segment([Pos2::new(x, r.min.y), Pos2::new(x, r.max.y)], Stroke::new(1.0, GRID));
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        bar: Rect,
        groups: &[Group],
        since: Instant,
        logging: Option<std::path::PathBuf>,
        log_err: Option<String>,
        interval: u64,
    ) {
        ui.painter().rect_filled(bar, 0.0, Color32::from_rgb(0x12, 0x12, 0x12));
        let bw = 52.0;
        let bh = 44.0;
        let y = bar.center().y;
        let btn = |ui: &mut egui::Ui, x: f32, id: &str, tip: &str| -> (Rect, egui::Response) {
            let r = Rect::from_center_size(Pos2::new(x + bw / 2.0, y), Vec2::new(bw, bh));
            let resp = ui.interact(r, ui.id().with(id), Sense::click()).on_hover_text(tip);
            let fill = if resp.hovered() { Color32::from_rgb(0x3a, 0x3a, 0x3a) } else { Color32::from_rgb(0x26, 0x26, 0x26) };
            ui.painter().rect_filled(r, CornerRadius::same(3), fill);
            ui.painter().rect_stroke(r, CornerRadius::same(3), Stroke::new(1.0, Color32::from_rgb(0x44, 0x44, 0x44)), StrokeKind::Inside);
            (r, resp)
        };
        let x0 = bar.min.x + 8.0;

        // ⇄ expand / collapse every multi-row sensor
        let (r, resp) = btn(ui, x0, "expand", "Expand / collapse all per-core rows");
        arrows_icon(ui.painter(), r.center(), Color32::from_rgb(0x3d, 0x8e, 0xe8));
        if resp.clicked() {
            if self.expanded.is_empty() {
                for g in groups {
                    for it in g.items.iter().filter(|i| !i.children.is_empty()) {
                        self.expanded.insert(key(&g.title, &it.name, None));
                    }
                }
            } else {
                self.expanded.clear();
            }
        }
        // →← collapse / expand all groups
        let (r, resp) = btn(ui, x0 + bw + 6.0, "collapse", "Collapse / expand all groups");
        collapse_icon(ui.painter(), r.center(), Color32::from_rgb(0x2f, 0x5f, 0x9e));
        if resp.clicked() {
            if self.collapsed.is_empty() {
                self.collapsed = groups.iter().map(|g| g.title.clone()).collect();
            } else {
                self.collapsed.clear();
            }
        }

        let mid = bar.center().x;
        // monitor + magnifier: system summary
        let (r, resp) = btn(ui, mid - 2.0 * bw - 70.0, "summary", "System summary");
        monitor_icon(ui.painter(), r.center(), true);
        if resp.clicked() {
            self.show_summary = !self.show_summary;
        }
        // two monitors: filter box (also Ctrl+F)
        let (r, resp) = btn(ui, mid - bw - 60.0, "filter", "Filter sensors (Ctrl+F)");
        monitor_icon(ui.painter(), r.center(), false);
        if resp.clicked() || ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.show_filter = !self.show_filter;
            if !self.show_filter {
                self.filter.clear();
            }
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) && !ctx.egui_wants_keyboard_input() {
            let s = self.state.lock().unwrap();
            ctx.copy_text(crate::render_text(&s, true, None, false, 0));
            self.copied_at = Some(Instant::now());
        }

        let el = since.elapsed().as_secs();
        let timer = if self.copied_at.is_some_and(|t| t.elapsed().as_secs() < 2) {
            "copied".to_string()
        } else {
            format!("{}:{:02}:{:02}", el / 3600, el / 60 % 60, el % 60)
        };
        ui.painter().text(Pos2::new(mid + 12.0, y), Align2::CENTER_CENTER, timer, FontId::proportional(17.0), TEXT);

        let right = bar.max.x - 8.0;
        // clock: reset min/max/avg
        let (r, resp) = btn(ui, right - 4.0 * bw - 18.0, "reset", "Reset Min/Max/Average and timer");
        clock_icon(ui.painter(), r.center());
        if resp.clicked() {
            let mut s = self.state.lock().unwrap();
            s.stats.reset();
            s.since = Instant::now();
        }
        // document+: CSV logging
        let tip = match (&logging, &log_err) {
            (Some(p), _) => format!("Stop logging ({})", p.display()),
            (None, Some(e)) => format!("Start logging (last error: {e})"),
            _ => "Start logging to CSV".into(),
        };
        let (r, resp) = btn(ui, right - 3.0 * bw - 12.0, "log", &tip);
        log_icon(ui.painter(), r.center(), logging.is_some());
        if resp.clicked() {
            let mut s = self.state.lock().unwrap();
            if s.log.is_some() {
                s.log = None;
            } else {
                let stamp = std::process::Command::new("date").arg("+%Y%m%d-%H%M%S").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
                let dir = std::env::var("HOME").map(|h| format!("{h}/test/hwinfo-rs/logs")).unwrap_or_else(|_| "logs".into());
                match CsvLog::create(format!("{dir}/hwinfo-{stamp}.csv").into()) {
                    Ok(l) => {
                        s.log = Some(l);
                        s.log_error = None;
                    }
                    Err(e) => s.log_error = Some(e.to_string()),
                }
            }
        }
        // gear: settings
        let (r, resp) = btn(ui, right - 2.0 * bw - 6.0, "settings", "Settings");
        gear_icon(ui.painter(), r.center());
        if resp.clicked() {
            self.show_settings = !self.show_settings;
        }
        // X: close
        let (r, resp) = btn(ui, right - bw, "close", "Close");
        close_icon(ui.painter(), r.center());
        if resp.clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.show_settings {
            let mut open = true;
            let mut iv = interval;
            egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(false).anchor(Align2::RIGHT_BOTTOM, [-12.0, -BAR_H - 8.0]).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Polling period [ms]:");
                    ui.add(egui::DragValue::new(&mut iv).range(250..=60000).speed(50));
                });
                ui.horizontal(|ui| {
                    for v in [500u64, 1000, 2000, 5000] {
                        if ui.selectable_label(iv == v, format!("{v}")).clicked() {
                            iv = v;
                        }
                    }
                });
                if let Some(p) = &logging {
                    ui.label(format!("Logging to {}", p.display()));
                }
                ui.separator();
                let mut c = self.state.lock().unwrap().config.clone();
                let before = (c.hide_serials, c.show_hidden);
                ui.checkbox(&mut c.hide_serials, "Hide serial numbers");
                ui.checkbox(&mut c.show_hidden, "Show hidden rows (dimmed)");
                let mut reset = false;
                ui.horizontal(|ui| {
                    if ui.button("Reset layout").on_hover_text("Clear renames, hidden rows and group order").clicked() {
                        reset = true;
                    }
                    if ui.button("Copy all values").on_hover_text("Ctrl+C").clicked() {
                        let s = self.state.lock().unwrap();
                        ctx.copy_text(crate::render_text(&s, true, None, false, 0));
                        self.copied_at = Some(Instant::now());
                    }
                });
                if reset {
                    c.renames.clear();
                    c.hidden.clear();
                    c.group_order.clear();
                }
                if reset || before != (c.hide_serials, c.show_hidden) {
                    c.save();
                    self.state.lock().unwrap().config = c;
                }
                ui.label(egui::RichText::new("Right-click a row or group: graph, rename, hide, alert, order.").color(DIM));
            });
            if iv != interval {
                let mut s = self.state.lock().unwrap();
                s.interval_ms = iv;
                s.config.interval_ms = iv;
                s.config.save();
            }
            self.show_settings = open;
        }
        // Keep the timer ticking even between samples.
        ctx.request_repaint_after(Duration::from_millis(1000));
    }
}

fn clip_text(p: &egui::Painter, r: Rect, text: &str, font: FontId, color: Color32) {
    let mut t = text.to_string();
    let w = |s: &str| p.layout_no_wrap(s.to_string(), font.clone(), color).size().x;
    if w(&t) > r.width() {
        while !t.is_empty() && w(&format!("{t}...")) > r.width() {
            t.pop();
        }
        t = format!("{}...", t.trim_end());
    }
    p.with_clip_rect(r).text(Pos2::new(r.min.x, r.center().y), Align2::LEFT_CENTER, t, font, color);
}

fn chevron(p: &egui::Painter, c: Pos2, open: bool) {
    let s = Stroke::new(2.0, Color32::from_rgb(0xc8, 0xc8, 0xc8));
    let pts = if open {
        vec![c + Vec2::new(-6.0, -3.0), c + Vec2::new(0.0, 3.0), c + Vec2::new(6.0, -3.0)]
    } else {
        vec![c + Vec2::new(-3.0, -6.0), c + Vec2::new(3.0, 0.0), c + Vec2::new(-3.0, 6.0)]
    };
    p.add(Shape::line(pts, s));
}

fn chip_icon(p: &egui::Painter, c: Pos2) {
    let body = Rect::from_center_size(c, Vec2::new(10.0, 16.0));
    p.rect_filled(body, 1.0, Color32::from_rgb(0x9a, 0x9a, 0x9a));
    for i in 0..4 {
        let yy = body.min.y + 2.5 + i as f32 * 3.7;
        p.line_segment([Pos2::new(body.min.x - 2.5, yy), Pos2::new(body.min.x, yy)], Stroke::new(1.5, Color32::from_rgb(0x70, 0x70, 0x70)));
        p.line_segment([Pos2::new(body.max.x, yy), Pos2::new(body.max.x + 2.5, yy)], Stroke::new(1.5, Color32::from_rgb(0x70, 0x70, 0x70)));
        p.line_segment([Pos2::new(body.min.x + 2.0, yy), Pos2::new(body.max.x - 2.0, yy)], Stroke::new(1.0, Color32::from_rgb(0x55, 0x55, 0x55)));
    }
}

fn kind_icon(p: &egui::Painter, c: Pos2, kind: Kind) {
    match kind {
        Kind::Temp => {
            p.rect_filled(Rect::from_center_size(c + Vec2::new(0.0, -2.0), Vec2::new(3.5, 13.0)), 1.5, Color32::from_rgb(0xe8, 0x1c, 0x1c));
            p.circle_filled(c + Vec2::new(0.0, 6.0), 3.2, Color32::from_rgb(0xe8, 0x1c, 0x1c));
            p.rect_filled(Rect::from_center_size(c + Vec2::new(0.0, -5.0), Vec2::new(1.5, 5.0)), 0.5, Color32::WHITE);
        }
        Kind::Volt | Kind::Current | Kind::Power => {
            let col = match kind {
                Kind::Volt => Color32::from_rgb(0xff, 0xc8, 0x1e),
                Kind::Power => Color32::from_rgb(0xff, 0xa0, 0x1e),
                _ => Color32::from_rgb(0xff, 0xe0, 0x60),
            };
            let pts = vec![
                c + Vec2::new(1.5, -7.5),
                c + Vec2::new(-4.5, 1.0),
                c + Vec2::new(-0.5, 1.0),
                c + Vec2::new(-2.0, 7.5),
                c + Vec2::new(4.5, -1.5),
                c + Vec2::new(0.5, -1.5),
            ];
            p.add(Shape::convex_polygon(pts[..3].to_vec(), col, Stroke::NONE));
            p.add(Shape::convex_polygon(vec![pts[0], pts[2], pts[5]], col, Stroke::NONE));
            p.add(Shape::convex_polygon(pts[2..6].to_vec(), col, Stroke::NONE));
        }
        _ => {
            // gauge: light disc, dark rim, needle
            p.circle_filled(c, 7.5, Color32::from_rgb(0xdc, 0xdc, 0xdc));
            p.circle_stroke(c, 7.5, Stroke::new(1.0, Color32::from_rgb(0x70, 0x70, 0x70)));
            p.circle_filled(c, 5.0, Color32::from_rgb(0xf4, 0xf4, 0xf4));
            p.line_segment([c, c + Vec2::new(4.0, -3.0)], Stroke::new(1.6, Color32::from_rgb(0x30, 0x30, 0x30)));
            p.circle_filled(c, 1.3, Color32::from_rgb(0x30, 0x30, 0x30));
        }
    }
}

fn arrows_icon(p: &egui::Painter, c: Pos2, col: Color32) {
    let tri = |tip: Pos2, dir: f32| {
        p.add(Shape::convex_polygon(vec![tip, tip + Vec2::new(-9.0 * dir, -8.0), tip + Vec2::new(-9.0 * dir, 8.0)], col, Stroke::NONE));
    };
    p.rect_filled(Rect::from_center_size(c, Vec2::new(16.0, 6.0)), 0.0, col);
    tri(c + Vec2::new(-17.0, 0.0), -1.0);
    tri(c + Vec2::new(17.0, 0.0), 1.0);
}

fn collapse_icon(p: &egui::Painter, c: Pos2, col: Color32) {
    for dir in [-1.0f32, 1.0] {
        let tip = c + Vec2::new(-2.0 * dir, 0.0);
        p.add(Shape::convex_polygon(vec![tip, tip + Vec2::new(-9.0 * dir, -8.0), tip + Vec2::new(-9.0 * dir, 8.0)], col, Stroke::NONE));
        p.rect_filled(Rect::from_center_size(c + Vec2::new(-15.0 * dir, 0.0), Vec2::new(8.0, 6.0)), 0.0, col);
    }
}

fn monitor_icon(p: &egui::Painter, c: Pos2, magnifier: bool) {
    let screen = |o: Vec2, s: f32| {
        let r = Rect::from_center_size(c + o, Vec2::new(22.0 * s, 15.0 * s));
        p.rect_filled(r, 1.5, Color32::from_rgb(0x9e, 0xc4, 0xe8));
        p.rect_stroke(r, 1.5, Stroke::new(1.0, Color32::from_rgb(0x3a, 0x6c, 0xb0)), StrokeKind::Inside);
        p.rect_filled(Rect::from_center_size(r.center_bottom() + Vec2::new(0.0, 3.0), Vec2::new(8.0 * s, 3.0)), 0.0, Color32::from_rgb(0x88, 0x88, 0x88));
    };
    if magnifier {
        screen(Vec2::new(2.0, -3.0), 1.0);
        p.circle_stroke(c + Vec2::new(-6.0, 5.0), 5.5, Stroke::new(2.0, Color32::from_rgb(0x40, 0x40, 0x40)));
        p.line_segment([c + Vec2::new(-2.0, 9.0), c + Vec2::new(4.0, 15.0)], Stroke::new(3.0, Color32::from_rgb(0x9a, 0x6a, 0x2a)));
    } else {
        screen(Vec2::new(-6.0, -6.0), 0.75);
        screen(Vec2::new(6.0, 5.0), 0.75);
    }
}

fn clock_icon(p: &egui::Painter, c: Pos2) {
    p.circle_filled(c, 16.0, Color32::WHITE);
    p.circle_stroke(c, 16.0, Stroke::new(2.5, Color32::from_rgb(0x20, 0x20, 0x20)));
    for i in 0..12 {
        let a = i as f32 / 12.0 * std::f32::consts::TAU;
        let d = Vec2::new(a.sin(), -a.cos());
        p.line_segment([c + d * 12.5, c + d * 14.5], Stroke::new(1.5, Color32::from_rgb(0x20, 0x20, 0x20)));
    }
    p.line_segment([c, c + Vec2::new(0.0, -10.0)], Stroke::new(2.5, Color32::from_rgb(0x20, 0x20, 0x20)));
    p.line_segment([c, c + Vec2::new(7.0, 0.0)], Stroke::new(2.5, Color32::from_rgb(0x20, 0x20, 0x20)));
}

fn log_icon(p: &egui::Painter, c: Pos2, active: bool) {
    let r = Rect::from_center_size(c + Vec2::new(-3.0, -1.0), Vec2::new(22.0, 28.0));
    p.rect_filled(r, 1.0, Color32::from_rgb(0xf0, 0xf0, 0xf0));
    p.rect_stroke(r, 1.0, Stroke::new(1.0, Color32::from_rgb(0x90, 0x90, 0x90)), StrokeKind::Inside);
    for i in 0..5 {
        let yy = r.min.y + 6.0 + i as f32 * 4.0;
        p.line_segment([Pos2::new(r.min.x + 4.0, yy), Pos2::new(r.max.x - 4.0, yy)], Stroke::new(1.0, Color32::from_rgb(0xa0, 0xa0, 0xa0)));
    }
    let g = if active { Color32::from_rgb(0xe0, 0x30, 0x30) } else { Color32::from_rgb(0x4c, 0xb8, 0x3c) };
    let pc = c + Vec2::new(8.0, 9.0);
    if active {
        p.rect_filled(Rect::from_center_size(pc, Vec2::new(11.0, 11.0)), 1.0, g);
    } else {
        p.rect_filled(Rect::from_center_size(pc, Vec2::new(14.0, 4.5)), 0.0, g);
        p.rect_filled(Rect::from_center_size(pc, Vec2::new(4.5, 14.0)), 0.0, g);
    }
}

fn gear_icon(p: &egui::Painter, c: Pos2) {
    let col = Color32::from_rgb(0xd0, 0xd0, 0xd0);
    for i in 0..8 {
        let a = i as f32 / 8.0 * std::f32::consts::TAU;
        let d = Vec2::new(a.cos(), a.sin());
        p.line_segment([c + d * 9.0, c + d * 16.0], Stroke::new(6.0, col));
    }
    p.circle_filled(c, 11.5, col);
    p.circle_filled(c, 5.0, Color32::from_rgb(0x26, 0x26, 0x26));
}

fn close_icon(p: &egui::Painter, c: Pos2) {
    let col = Color32::from_rgb(0x2a, 0x8c, 0xe8);
    p.line_segment([c + Vec2::new(-12.0, -12.0), c + Vec2::new(12.0, 12.0)], Stroke::new(7.0, col));
    p.line_segment([c + Vec2::new(12.0, -12.0), c + Vec2::new(-12.0, 12.0)], Stroke::new(7.0, col));
}

fn graph(p: &egui::Painter, r: Rect, hist: &[f32], kind: Kind, unit: &str, dec: usize) {
    p.rect_filled(r, 0.0, Color32::BLACK);
    for i in 1..4 {
        let y = r.min.y + r.height() * i as f32 / 4.0;
        p.line_segment([Pos2::new(r.min.x, y), Pos2::new(r.max.x, y)], Stroke::new(1.0, GRAPH_GRID));
    }
    for i in 1..8 {
        let x = r.min.x + r.width() * i as f32 / 8.0;
        p.line_segment([Pos2::new(x, r.min.y), Pos2::new(x, r.max.y)], Stroke::new(1.0, GRAPH_GRID));
    }
    let vals: Vec<f32> = hist.iter().copied().filter(|v| v.is_finite()).collect();
    if vals.is_empty() {
        return;
    }
    let (mut lo, mut hi) = vals.iter().fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    if kind == Kind::Usage || kind == Kind::Bool {
        lo = lo.min(0.0);
        hi = hi.max(if kind == Kind::Bool { 1.0 } else { 100.0 });
    }
    if (hi - lo).abs() < 1e-6 {
        hi = lo + 1.0;
    }
    let span = hi - lo;
    let (lo, hi) = (lo - span * 0.05, hi + span * 0.05);
    let n = hist.len().max(2);
    let pts: Vec<Pos2> = hist
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_finite())
        .map(|(i, &v)| Pos2::new(r.max.x - r.width() * (n - 1 - i) as f32 / (crate::HISTORY_LEN - 1) as f32, r.max.y - r.height() * (v - lo) / (hi - lo)))
        .collect();
    p.with_clip_rect(r).add(Shape::line(pts, Stroke::new(1.5, GRAPH_LINE)));
    let f = |v: f32| crate::model::fmt_value(kind, unit, dec, Some(v as f64));
    let cur = *vals.last().unwrap();
    p.text(r.min + Vec2::new(6.0, 4.0), Align2::LEFT_TOP, format!("{}  (max {})", f(cur), f(vals.iter().copied().fold(f32::MIN, f32::max))), FontId::proportional(13.0), TEXT);
    p.text(Pos2::new(r.min.x + 6.0, r.max.y - 4.0), Align2::LEFT_BOTTOM, format!("min {}  ·  30 min", f(vals.iter().copied().fold(f32::MAX, f32::min))), FontId::proportional(12.0), DIM);
}

/// HWiNFO paints "Yes" red only for alarm-type flags (throttling, limits, failures...).
pub fn is_alarm_flag(name: &str) -> bool {
    let n = name.to_lowercase();
    ["throttl", "critical", "failure", "warning", "limit", "intrusion", "error", "prochot", "htc"].iter().any(|w| n.contains(w))
}
