//! The HUD: a live hologram of every session, its agents and workflows, and the MCP servers and
//! skills the sessions share, drawn in Braille dots. Sessions orbit the core on a tilted floor,
//! their agents and workflows circle them like moons, shared servers and skills hang on a crown
//! around the edge, and far things fade. Packets run along the links as work happens: prompts
//! arrive from the core, calls go out to agents and servers, results come back, and token
//! traffic streams between each session and the core. Mouse: drag turns the view, the wheel
//! zooms around the pointer, click locks a target, double-click opens a session. `v` folds the
//! view flat, `d` hands the camera to the director.

mod director;

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::f64::consts::{PI, TAU};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Painter, Points, Shape};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::insight::{self, Alert, Collision};
use crate::model::{Feed, Kind, Session};
use crate::parse::{self, clip, server_label, short_model};
use crate::source::World;
use crate::theme::{Theme, fog, mix, rgb};
use crate::time::{fmt_dur, fmt_n};
use crate::ui::vitals;
use director::Director;

const DOUBLE_CLICK: Duration = Duration::from_millis(450);
const MAX_PACKETS: usize = 320;
/// Camera tilt above the floor in 3D, about 32 degrees.
const PITCH_3D: f64 = 0.56;
/// How long the HUD stays untouched before the director takes the camera.
const IDLE_DIRECTOR: Duration = Duration::from_secs(120);

type P3 = (f64, f64, f64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NodeKind {
    Core,
    Session,
    Agent,
    Workflow,
    Mcp,
    Skill,
}

struct Node {
    id: String,
    kind: NodeKind,
    label: String,
    sub: String,
    parent: Option<String>,
    session: Option<usize>,
    /// Where the node belongs, in world units: x right, y up, z toward the viewer.
    target: P3,
    active: bool,
    alive: bool,
    needs: bool,
    color: Color,
    r: f64,
    /// A warning on what the node is doing: a call far slower than usual, a loop.
    warn: Option<String>,
}

struct Edge {
    a: usize,
    b: usize,
    active: bool,
    color: Color,
    /// A session's link to a shared MCP server or skill: shown only when it matters.
    shared: bool,
    /// Two sessions changing the same file: the file's name.
    alarm: Option<String>,
}

struct Packet {
    from: String,
    to: String,
    born: Instant,
    dur: f64,
    color: Color,
}

struct Spark {
    node: String,
    label: String,
    born: Instant,
    color: Color,
}

/// A node as drawn in the last frame, for hit tests, keyboard focus and the target panel.
#[derive(Clone)]
struct Hit {
    id: String,
    kind: NodeKind,
    session: Option<usize>,
    x: f64,
    y: f64,
    r: f64,
}

pub enum HudAction {
    None,
    Open(usize),
}

/// What turns world points into canvas points: a turn around the vertical axis, a tilt, some
/// perspective, and in the flat view a stretch across wide screens.
#[derive(Clone, Copy)]
struct Cam {
    yaw: f64,
    pitch: f64,
    persp: f64,
    sx: f64,
    dist: f64,
}

impl Cam {
    /// Canvas point, depth (toward the viewer is positive) and scale of a world point.
    fn project(&self, (x, y, z): P3) -> (f64, f64, f64, f64) {
        let (sy, cy) = self.yaw.sin_cos();
        let (x1, z1) = (x * cy + z * sy, -x * sy + z * cy);
        let (sp, cp) = self.pitch.sin_cos();
        let up = y * cp - z1 * sp;
        let depth = z1 * cp + y * sp;
        let k = 1.0 + self.persp * (self.dist / (self.dist - depth).max(self.dist * 0.3) - 1.0);
        (x1 * k * self.sx, up * k, depth, k)
    }

    fn xy(&self, p: P3) -> (f64, f64) {
        let (x, y, ..) = self.project(p);
        (x, y)
    }
}

pub struct HudState {
    pos: HashMap<String, P3>,
    born: HashMap<String, Instant>,
    packets: Vec<Packet>,
    sparks: Vec<Spark>,
    waves: Vec<Instant>,
    seen: HashMap<String, i64>,
    flares: HashMap<String, i64>,
    credit: HashMap<String, (f64, f64)>,
    selected: Option<String>,
    hover: Option<String>,
    mouse: Option<(f64, f64)>,
    zoom: f64,
    pan: (f64, f64),
    /// Where the camera glides to; zoom, pan and turn ease toward these every frame.
    goal_zoom: f64,
    goal_pan: (f64, f64),
    yaw: f64,
    pitch: f64,
    goal_yaw: f64,
    goal_pitch: f64,
    /// 1 for the 3D view, 0 for the flat one; the fold between them is animated.
    depth: f64,
    goal_depth: f64,
    /// Button pressed, where, the pan and turn at that moment, and whether it moved.
    drag: Option<(MouseButton, u16, u16, (f64, f64), (f64, f64), bool)>,
    last: Option<Instant>,
    last_input: Instant,
    started: Instant,
    area: Rect,
    bounds: [f64; 4],
    hits: Vec<Hit>,
    last_click: Option<(Instant, String)>,
    director: Director,
    /// Place nodes at their targets on the next frame instead of flying them in.
    pub settle: bool,
}

impl HudState {
    pub fn new() -> HudState {
        HudState {
            pos: HashMap::new(),
            born: HashMap::new(),
            packets: Vec::new(),
            sparks: Vec::new(),
            waves: Vec::new(),
            seen: HashMap::new(),
            flares: HashMap::new(),
            credit: HashMap::new(),
            selected: None,
            hover: None,
            mouse: None,
            zoom: 1.0,
            pan: (0.0, 0.0),
            goal_zoom: 1.0,
            goal_pan: (0.0, 0.0),
            yaw: 0.0,
            pitch: PITCH_3D,
            goal_yaw: 0.0,
            goal_pitch: PITCH_3D,
            depth: 1.0,
            goal_depth: 1.0,
            drag: None,
            last: None,
            last_input: Instant::now(),
            started: Instant::now(),
            area: Rect::default(),
            bounds: [0.0; 4],
            hits: Vec::new(),
            last_click: None,
            director: Director::new(),
            settle: false,
        }
    }

    /// The flat view (the graph seen from above) or the 3D one.
    pub fn set_flat(&mut self, flat: bool) {
        self.goal_depth = if flat { 0.0 } else { 1.0 };
    }

    pub fn director_on(&mut self, on: bool) {
        self.director.set(on);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        f: &mut Frame,
        area: Rect,
        t: &Theme,
        fx: bool,
        world: &World,
        order: &[usize],
        names: &HashMap<usize, String>,
        alerts: &[Alert],
        collisions: &[Collision],
    ) {
        if area.width < 30 || area.height < 12 {
            return;
        }
        let now = Instant::now();
        let dt = self.last.map_or(0.0, |l| now.duration_since(l).as_secs_f64()).min(0.25);
        self.last = Some(now);
        let time = now.duration_since(self.started).as_secs_f64();
        if !self.director.on && self.last_input.elapsed() > IDLE_DIRECTOR && fx {
            self.director.set(true);
        }
        let ease = |rate: f64| if self.settle { 1.0 } else { 1.0 - (-dt * rate).exp() };
        let (k_cam, k_fold) = (ease(7.0), ease(3.5));
        self.depth += (self.goal_depth - self.depth) * k_fold;
        if self.drag.is_none_or(|d| !d.5) {
            self.zoom += (self.goal_zoom - self.zoom) * k_cam;
            self.pan.0 += (self.goal_pan.0 - self.pan.0) * k_cam;
            self.pan.1 += (self.goal_pan.1 - self.pan.1) * k_cam;
            self.yaw += (self.goal_yaw - self.yaw) * k_cam;
            self.pitch += (self.goal_pitch - self.pitch) * k_cam;
        }

        // Canvas units are Braille dots: two per cell across, four per cell down, so circles are round.
        let hw = area.width as f64;
        let hh = area.height as f64 * 2.0;
        let m = self.depth;
        let pitch = PI / 2.0 + (self.pitch - PI / 2.0) * m;
        // The flat view fits the graph to the screen and stretches orbits across wide screens;
        // the tilted view fits the floor's ellipse, near side included.
        let flat_r = (hw * 0.92).min(hh * 0.84);
        let tilt_r = (hw * 0.9).min(hh * 0.9 / (pitch.sin() * 1.35 + 0.18)) / 0.92;
        let maxr = flat_r + (tilt_r - flat_r) * m;
        let sx_flat = (hw * 0.92 / (hh * 0.84)).clamp(1.0, 1.8);
        let cam = Cam { yaw: self.yaw * m, pitch, persp: m, sx: sx_flat + (1.0 - sx_flat) * m, dist: maxr * 3.2 };
        let (x0, x1) = (self.pan.0 - hw / self.zoom, self.pan.0 + hw / self.zoom);
        let (y0, y1) = (self.pan.1 - hh / self.zoom, self.pan.1 + hh / self.zoom);
        self.area = area;
        self.bounds = [x0, x1, y0, y1];
        let (cw, ch) = ((x1 - x0) / area.width as f64, (y1 - y0) / area.height as f64);

        let orbit = if fx { time * m } else { 0.0 };
        let (nodes, edges) = build(world, order, names, maxr, orbit, m, t, alerts, collisions);
        self.place(&nodes, dt, now);
        let world_at: Vec<P3> = nodes.iter().map(|n| self.pos.get(&n.id).copied().unwrap_or(n.target)).collect();
        let proj: Vec<(f64, f64, f64, f64)> = world_at.iter().map(|&p| cam.project(p)).collect();
        let at: Vec<(f64, f64)> = proj.iter().map(|p| (p.0, p.1)).collect();
        self.spawn(world, order, &nodes, dt, now, t);
        self.packets.retain(|p| now.duration_since(p.born).as_secs_f64() < p.dur);
        self.sparks.retain(|s| now.duration_since(s.born) < Duration::from_millis(1500));
        self.waves.retain(|w| now.duration_since(*w) < Duration::from_millis(1700));
        self.hits = nodes
            .iter()
            .zip(&proj)
            .map(|(n, &(x, y, _, k))| Hit { id: n.id.clone(), kind: n.kind, session: n.session, x, y, r: n.r * k })
            .collect();
        if self.selected.as_ref().is_some_and(|s| !self.hits.iter().any(|h| &h.id == s)) {
            self.selected = None;
        }
        let index: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();

        // The director picks what to look at; the camera then follows that node as it moves.
        if self.director.on {
            let ids: HashSet<&str> = index.keys().copied().collect();
            let aim = self.director.update(world, order, names, alerts, collisions, &ids, t);
            match aim.and_then(|id| index.get(id.as_str()).copied()) {
                Some(i) => {
                    self.goal_pan = at[i];
                    self.goal_zoom = 2.1;
                    self.goal_yaw += dt * 0.06 * m;
                }
                None => {
                    self.goal_pan = (0.0, 0.0);
                    self.goal_zoom = 1.0;
                    self.goal_yaw += dt * 0.045 * m;
                }
            }
        }

        let r1 = maxr * 0.46;
        let r2 = maxr * 0.92;
        // Far things fade toward the background.
        let fade = |depth: f64| ((-depth / r2).clamp(0.0, 1.0) * 0.6 + (depth / r2).clamp(0.0, 1.0) * -0.0) * m;
        let fogged = |c: Color, depth: f64| fog(c, t.bg, fade(depth));
        let mut dots: Vec<((f64, f64), Color)> = Vec::new();
        for p in &self.packets {
            let (Some(&a), Some(&b)) = (index.get(p.from.as_str()), index.get(p.to.as_str())) else { continue };
            let u = (now.duration_since(p.born).as_secs_f64() / p.dur).clamp(0.0, 1.0);
            let e = u * u * (3.0 - 2.0 * u);
            for (k, back) in [0.0, 0.03, 0.06, 0.09].into_iter().enumerate() {
                let v = (e - back).max(0.0);
                let (pa, pb) = (world_at[a], world_at[b]);
                let w = (pa.0 + (pb.0 - pa.0) * v, pa.1 + (pb.1 - pa.1) * v, pa.2 + (pb.2 - pa.2) * v);
                let (x, y, d, _) = cam.project(w);
                let c = match k {
                    0 => p.color,
                    1 => t.hi,
                    _ => t.dim,
                };
                dots.push(((x, y), fogged(c, d)));
            }
        }
        let born: Vec<f64> = nodes
            .iter()
            .map(|n| self.born.get(&n.id).map_or(1.0, |b| now.duration_since(*b).as_secs_f64() / 0.9))
            .collect();
        let sparks: Vec<(usize, f64, String, Color)> = self
            .sparks
            .iter()
            .filter_map(|s| {
                let i = *index.get(s.node.as_str())?;
                Some((i, now.duration_since(s.born).as_secs_f64() / 1.5, s.label.clone(), s.color))
            })
            .collect();
        let waves: Vec<f64> = self.waves.iter().map(|w| now.duration_since(*w).as_secs_f64() / 1.7).collect();
        let sel = self.selected.as_ref().and_then(|s| self.hits.iter().find(|h| &h.id == s)).cloned();
        let hover = self.hover.as_ref().and_then(|s| self.hits.iter().find(|h| &h.id == s)).cloned();
        let callout = sel.as_ref().filter(|_| !self.director.on).map(|_| callout_rect(area));
        let leader_end = callout.map(|r| {
            let (cx, cy) = (r.x.saturating_sub(1), r.y + r.height / 2);
            (x0 + (cx - area.x) as f64 * cw + cw / 2.0, y1 - (cy - area.y) as f64 * ch - ch / 2.0)
        });
        let mouse = self.mouse;
        let sweep = time * 0.9;
        let ids_sel = sel.as_ref().map(|h| h.id.clone());
        let ids_hover = hover.as_ref().map(|h| h.id.clone());
        // A locked target dims everything it is not linked to; pointing at a node reveals its links.
        let sel_ix = ids_sel.as_deref().and_then(|id| index.get(id).copied());
        let hover_ix = ids_hover.as_deref().and_then(|id| index.get(id).copied());
        let focus: Option<HashSet<usize>> = sel_ix.map(|i| {
            let mut set: HashSet<usize> = [0, i].into_iter().collect();
            for e in &edges {
                if e.a == i {
                    set.insert(e.b);
                }
                if e.b == i {
                    set.insert(e.a);
                }
            }
            set
        });
        let lit = |i: usize| focus.as_ref().is_none_or(|f| f.contains(&i));
        let touches = |e: &Edge, i: Option<usize>| i.is_some_and(|i| e.a == i || e.b == i);
        // Labels sit on whole rows, on a plate of background so the dots never cross the text.
        let (lw, lh) = ((x1 - x0) / (area.width.max(2) - 1) as f64, (y1 - y0) / (area.height.max(2) - 1) as f64);
        let row_of = |y: f64| ((y1 - y) / lh).floor();
        let at_row = |r: f64| y1 - (r + 0.5) * lh;
        let col_of = |x: f64| ((x - x0) / lw).floor();
        let at_col = |c: f64| x0 + (c + 0.5) * lw;
        let hover_tip = hover.as_ref().filter(|h| Some(&h.id) != ids_sel.as_ref()).and_then(|h| {
            let n = &nodes[*index.get(h.id.as_str())?];
            Some((n.label.clone(), n.warn.clone().unwrap_or_else(|| n.sub.clone()), n.color))
        });
        // Far nodes first, so the near ones are drawn over them.
        let mut by_depth: Vec<usize> = (0..nodes.len()).collect();
        by_depth.sort_by(|&a, &b| proj[a].2.total_cmp(&proj[b].2));
        let floor = |r: f64, gap: usize, base: Color| Ring3 { cam, r, y: 0.0, gap, base, bg: t.bg, fade: 0.65 * m, r2 };

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .background_color(t.bg)
            .x_bounds([x0, x1])
            .y_bounds([y0, y1])
            .paint(|ctx| {
                // The floor: two orbits, compass ticks, a radar sweep, faint spokes in 3D.
                ctx.draw(&floor(r1, 4, t.faint));
                ctx.draw(&floor(r2, 3, t.faint));
                for deg in (0..360).step_by(10) {
                    let a = (deg as f64).to_radians();
                    let len = if deg % 30 == 0 { maxr * 0.045 } else { maxr * 0.02 };
                    let color = if deg % 90 == 0 { t.dim } else { t.faint };
                    let (p, q) = ((r2 + 2.0) * a.cos(), (r2 + 2.0 + len) * a.cos());
                    let (pz, qz) = (-(r2 + 2.0) * a.sin(), -(r2 + 2.0 + len) * a.sin());
                    let d = cam.project((p, 0.0, pz)).2;
                    ctx.draw(&Seg { a: cam.xy((p, 0.0, pz)), b: cam.xy((q, 0.0, qz)), color: fogged(color, d) });
                }
                if m > 0.3 {
                    for k in 0..12 {
                        let a = TAU * k as f64 / 12.0;
                        let (p, q) = ((r1 * 0.3 * a.cos(), 0.0, -r1 * 0.3 * a.sin()), (r2 * a.cos(), 0.0, -r2 * a.sin()));
                        let d = cam.project(q).2;
                        ctx.draw(&Dashed { a: cam.xy(p), b: cam.xy(q), on: 0.6, off: 7.0, phase: 0.0, color: fogged(t.faint, d) });
                    }
                }
                if fx {
                    // On a tilted floor the sweep's trail would close into a solid band: there it is
                    // a light fan of dotted lines.
                    let trail = if m > 0.5 { 6 } else { 10 };
                    for k in 0..trail {
                        let a = sweep - k as f64 * if m > 0.5 { 0.05 } else { 0.025 };
                        let tip = (r2 * a.cos(), 0.0, -r2 * a.sin());
                        let color = fogged(rgb(mix(t.g0, (24, 24, 24), k as f64 / trail as f64)), cam.project(tip).2);
                        let (from, to) = (cam.xy((0.0, 0.0, 0.0)), cam.xy(tip));
                        if m > 0.5 && k > 0 {
                            ctx.draw(&Dashed { a: from, b: to, on: 1.0, off: 2.0 + k as f64, phase: 0.0, color });
                        } else {
                            ctx.draw(&Seg { a: from, b: to, color });
                        }
                    }
                }
                ctx.layer();

                // Stalks under raised nodes, the orbits of working moons, then the links.
                for (i, n) in nodes.iter().enumerate() {
                    let p = world_at[i];
                    if matches!(n.kind, NodeKind::Mcp | NodeKind::Skill) && p.1 > 1.0 {
                        let d = proj[i].2;
                        ctx.draw(&Dashed { a: at[i], b: cam.xy((p.0, 0.0, p.2)), on: 0.8, off: 2.2, phase: 0.0, color: fogged(t.faint, d) });
                    }
                }
                for e in &edges {
                    let (a, b) = (at[e.a], at[e.b]);
                    let d = (proj[e.a].2 + proj[e.b].2) / 2.0;
                    if e.alarm.is_some() {
                        let phase = if fx { -time * 24.0 } else { 0.0 };
                        ctx.draw(&Dashed { a, b, on: 3.0, off: 1.5, phase, color: if (time * 3.0) as i64 % 2 == 0 { t.err } else { t.warn } });
                        continue;
                    }
                    let pointed = touches(e, sel_ix) || touches(e, hover_ix);
                    if e.shared && !e.active && !pointed {
                        continue;
                    }
                    let in_focus = lit(e.a) && lit(e.b);
                    if e.active && in_focus {
                        let phase = if fx { -time * 18.0 } else { 0.0 };
                        ctx.draw(&Dashed { a, b, on: 2.5, off: 2.0, phase, color: fogged(e.color, d) });
                    } else if pointed {
                        ctx.draw(&Dashed { a, b, on: 1.5, off: 1.5, phase: 0.0, color: e.color });
                    } else {
                        ctx.draw(&Dashed { a, b, on: 1.0, off: 5.0, phase: 0.0, color: fogged(t.faint, d) });
                    }
                }
                ctx.layer();

                for w in &waves {
                    ctx.draw(&Ring3 { cam, r: (r2 * w).max(1.0), y: 0.0, gap: 1, base: t.grad(*w), bg: t.bg, fade: 0.5 * m, r2 });
                }
                for &i in &by_depth {
                    let n = &nodes[i];
                    let (x, y, d, k) = proj[i];
                    let dim = !lit(i);
                    let color = if dim { t.faint } else { fogged(n.color, d) };
                    node_shape(ctx, n, (x, y), n.r * k, color, time, born[i], fx, dim, t);
                    if n.warn.is_some() && !dim {
                        let on = !fx || (time * 2.5) as i64 % 2 == 0;
                        ctx.draw(&Ring { c: (x, y), sx: 1.0, r: n.r * k * 2.3, gap: 2, color: if on { t.warn } else { t.err } });
                    }
                }
                for (i, u, _, color) in &sparks {
                    let r = nodes[*i].r * proj[*i].3;
                    ctx.draw(&Ring { c: at[*i], sx: 1.0, r: r * (1.4 + 2.2 * u), gap: 2, color: *color });
                }
                if let Some(h) = &hover {
                    ctx.draw(&Ring { c: (h.x, h.y), sx: 1.0, r: h.r * 2.0 + 1.0, gap: 2, color: t.hi });
                }
                if let Some(h) = &sel {
                    reticle(ctx, (h.x, h.y), h.r, time, t);
                    if let Some(end) = leader_end {
                        let elbow = (end.0 - 6.0 * cw, end.1);
                        ctx.draw(&Seg { a: polar((h.x, h.y), h.r * 2.6 + 3.0, (elbow.1 - h.y).atan2(elbow.0 - h.x)), b: elbow, color: t.hi });
                        ctx.draw(&Seg { a: elbow, b: end, color: t.hi });
                    }
                }
                if let Some(m) = mouse {
                    ctx.draw(&Seg { a: (m.0 - 2.0 * cw, m.1), b: (m.0 + 2.0 * cw, m.1), color: t.dim });
                    ctx.draw(&Seg { a: (m.0, m.1 - ch), b: (m.0, m.1 + ch), color: t.dim });
                }
                ctx.layer();

                for (pt, color) in &dots {
                    ctx.draw(&Points { coords: std::slice::from_ref(pt), color: *color });
                }

                for &i in &by_depth {
                    let n = &nodes[i];
                    let (x, y, d, k) = proj[i];
                    let hot = ids_sel.as_deref() == Some(n.id.as_str()) || ids_hover.as_deref() == Some(n.id.as_str());
                    let major = matches!(n.kind, NodeKind::Session | NodeKind::Core);
                    // Finished agents and workflows stay quiet until pointed at; so does whatever a
                    // locked target is not linked to.
                    if (matches!(n.kind, NodeKind::Agent | NodeKind::Workflow) && !n.active && !hot && n.warn.is_none()) || (!lit(i) && !major) {
                        continue;
                    }
                    let shown = ((born[i] * 1.6).min(1.0) * n.label.chars().count() as f64).round() as usize;
                    let label: String = n.label.chars().take(shown).collect();
                    let color = if !n.alive || !lit(i) {
                        t.faint
                    } else if n.needs {
                        t.warn
                    } else if hot {
                        t.hi
                    } else {
                        fogged(n.color, d)
                    };
                    let mut style = Style::new().fg(color).bg(t.bg);
                    if hot || n.active || major {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    let reach = n.r * k * if n.kind == NodeKind::Core { 2.1 } else { 1.25 };
                    let row = row_of(y - reach) + 1.0;
                    let len = label.chars().count() as f64;
                    let deco = if n.needs { "⚑ " } else { "" };
                    let text = format!(" {deco}{label} ");
                    ctx.print(at_col(col_of(x) - (len + 2.0 + deco.chars().count() as f64) / 2.0), at_row(row), Line::from(Span::styled(text, style)));
                    let sub = if n.needs {
                        Some(("needs you".to_string(), t.warn))
                    } else if let Some(w) = n.warn.as_ref().filter(|_| lit(i)) {
                        Some((clip(w, 24), t.warn))
                    } else if !n.sub.is_empty() && (hot || major) && lit(i) {
                        Some((clip(&n.sub, 22), if n.active { t.accent } else { t.dim }))
                    } else {
                        None
                    };
                    if let Some((sub, c)) = sub {
                        let len = sub.chars().count() as f64;
                        ctx.print(at_col(col_of(x) - (len + 2.0) / 2.0), at_row(row + 1.0), Line::from(Span::styled(format!(" {sub} "), Style::new().fg(c).bg(t.bg))));
                    }
                }
                for e in edges.iter().filter(|e| e.alarm.is_some()) {
                    let (a, b) = (at[e.a], at[e.b]);
                    let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
                    let text = format!(" ✖ {} ", e.alarm.as_deref().unwrap_or(""));
                    let len = text.chars().count() as f64;
                    ctx.print(at_col(col_of(mid.0) - len / 2.0), at_row(row_of(mid.1)), Line::from(Span::styled(text, Style::new().fg(t.bg).bg(t.err).add_modifier(Modifier::BOLD))));
                }
                for (i, u, label, color) in &sparks {
                    let (x, y) = at[*i];
                    let fade = if *u < 0.6 { *color } else { t.dim };
                    let r = nodes[*i].r * proj[*i].3;
                    let row = row_of(y + r) - 1.0 - (u * 2.0).floor();
                    ctx.print(at_col(col_of(x + r) + 2.0), at_row(row), Line::from(Span::styled(clip(label, 18), Style::new().fg(fade).bg(t.bg))));
                }
                if let (Some((label, sub, color)), Some(m)) = (&hover_tip, mouse) {
                    let (c, r) = (col_of(m.0) + 2.0, row_of(m.1) - 2.0);
                    ctx.print(at_col(c), at_row(r), Line::from(Span::styled(format!(" {label} "), Style::new().fg(*color).bg(t.surface).add_modifier(Modifier::BOLD))));
                    if !sub.is_empty() {
                        ctx.print(at_col(c), at_row(r + 1.0), Line::from(Span::styled(format!(" {} ", clip(sub, 30)), Style::new().fg(t.dim).bg(t.surface))));
                    }
                }
            });
        f.render_widget(canvas, area);
        self.overlays(f, area, t, world, order, names, sel.as_ref(), callout);
        self.settle = false;
    }

    /// Eases every node toward its target; new nodes fly out from their parent.
    fn place(&mut self, nodes: &[Node], dt: f64, now: Instant) {
        let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        self.pos.retain(|k, _| ids.contains(k.as_str()));
        self.born.retain(|k, _| ids.contains(k.as_str()));
        let settle = self.settle;
        let k = if settle { 1.0 } else { 1.0 - (-dt * 5.0).exp() };
        for n in nodes {
            if !self.pos.contains_key(&n.id) {
                let start = if settle {
                    n.target
                } else {
                    n.parent.as_ref().and_then(|p| self.pos.get(p).copied()).unwrap_or((0.0, 0.0, 0.0))
                };
                self.pos.insert(n.id.clone(), start);
                let born = if settle { now.checked_sub(Duration::from_secs(5)).unwrap_or(now) } else { now };
                self.born.insert(n.id.clone(), born);
            }
            if let Some(p) = self.pos.get_mut(&n.id) {
                p.0 += (n.target.0 - p.0) * k;
                p.1 += (n.target.1 - p.1) * k;
                p.2 += (n.target.2 - p.2) * k;
            }
        }
    }

    /// Turns new feed events into packets and sparks, token rates into packet streams, and a
    /// rebuilt prompt cache into a flare on its session.
    fn spawn(&mut self, world: &World, order: &[usize], nodes: &[Node], dt: f64, now: Instant, t: &Theme) {
        let ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        for &si in order {
            let s = &world.sessions[si];
            let sid = format!("s:{}", s.id);
            let latest = s.feed.back().map_or(0, |f| f.at);
            // History is already on screen: only events after the first frame make packets.
            let seen = *self.seen.entry(s.id.clone()).or_insert(latest);
            for it in s.feed.iter().filter(|it| it.at > seen) {
                let target = it
                    .tool
                    .as_deref()
                    .and_then(|tool| s.step_by_tool(tool))
                    .and_then(|st| match st.kind {
                        Kind::Agent => st.child.map(|c| format!("a:{}:{c}", s.id)),
                        Kind::Workflow => st.workflow.map(|w| format!("w:{}:{w}", s.id)),
                        Kind::Mcp => Some(format!("m:{}", parse::mcp_parts(&st.name).0)),
                        Kind::Skill => st.summary.split_whitespace().next().map(|k| format!("k:{k}")),
                        _ => None,
                    })
                    .filter(|id| ids.contains(id.as_str()));
                let color = it.kind.map_or(t.prompt, |k| t.kind(k));
                let packet = |from: &str, to: &str, color: Color| Packet { from: from.into(), to: to.into(), born: now, dur: 0.9, color };
                match (it.feed, target) {
                    (Feed::Prompt, _) => self.packets.push(packet("core", &sid, t.prompt)),
                    (Feed::Start, Some(id)) => self.packets.push(packet(&sid, &id, color)),
                    (Feed::Done, Some(id)) => self.packets.push(packet(&id, &sid, t.ok)),
                    (Feed::Failed, Some(id)) => self.packets.push(packet(&id, &sid, t.err)),
                    (Feed::Failed, None) => self.spark(&sid, format!("✗ {}", it.label), t.err, now),
                    (_, None) => self.spark(&sid, it.label.clone(), color, now),
                }
            }
            self.seen.insert(s.id.clone(), latest.max(seen));
            let flare = s.flares.back().map_or(0, |f| f.0);
            let seen_flare = *self.flares.entry(s.id.clone()).or_insert(flare);
            if flare > seen_flare {
                let n = s.flares.back().map_or(0, |f| f.1);
                self.spark(&sid, format!("✺ cache rebuilt {}", fmt_n(n)), t.accent, now);
                self.flares.insert(s.id.clone(), flare);
            }
            if !s.alive {
                continue;
            }
            let (up, down) = (s.tok_up.per_min(world.now), s.tok_down.per_min(world.now));
            let c = self.credit.entry(s.id.clone()).or_insert((0.0, 0.0));
            c.0 = (c.0 + dt * (up as f64 / 60.0) / 4000.0).min(3.0);
            c.1 = (c.1 + dt * (down as f64 / 60.0) / 35.0).min(3.0);
            while c.0 >= 1.0 {
                c.0 -= 1.0;
                self.packets.push(Packet { from: sid.clone(), to: "core".into(), born: now, dur: 1.2, color: t.accent });
            }
            while c.1 >= 1.0 {
                c.1 -= 1.0;
                self.packets.push(Packet { from: "core".into(), to: sid.clone(), born: now, dur: 1.2, color: t.hi });
            }
        }
        if self.packets.len() > MAX_PACKETS {
            let extra = self.packets.len() - MAX_PACKETS;
            self.packets.drain(..extra);
        }
    }

    fn spark(&mut self, node: &str, label: String, color: Color, now: Instant) {
        self.sparks.retain(|s| s.node != node);
        self.sparks.push(Spark { node: node.into(), label, born: now, color });
    }

    #[allow(clippy::too_many_arguments)]
    fn overlays(&self, f: &mut Frame, area: Rect, t: &Theme, world: &World, order: &[usize], names: &HashMap<usize, String>, sel: Option<&Hit>, callout: Option<Rect>) {
        let now = world.now;
        let live = world.sessions.iter().filter(|s| s.alive).count();
        let busy = world.sessions.iter().filter(|s| s.busy()).count();
        let agents: usize = world.sessions.iter().map(Session::running_agents).sum();
        let up: u64 = world.sessions.iter().map(|s| s.tok_up.per_min(now)).sum();
        let down: u64 = world.sessions.iter().map(|s| s.tok_down.per_min(now)).sum();
        let buf = f.buffer_mut();

        let (l, r, top, bottom) = (area.x, area.right() - 1, area.y, area.bottom() - 1);
        for (x, y, s, g) in [(l, top, "┏━━", 0.0), (r - 2, top, "━━┓", 1.0), (l, bottom, "┗━━", 0.0), (r - 2, bottom, "━━┛", 1.0)] {
            buf.set_string(x, y, s, Style::new().fg(t.grad(g)));
        }
        for (x, y, g) in [(l, top + 1, 0.0), (r, top + 1, 1.0), (l, bottom - 1, 0.0), (r, bottom - 1, 1.0)] {
            buf.set_string(x, y, "┃", Style::new().fg(t.grad(g)));
        }

        let kv = |k: &str, v: String, c: Color| {
            Line::from(vec![Span::styled(format!("{k:<9}"), Style::new().fg(t.faint)), Span::styled(v, Style::new().fg(c))])
        };
        let status = if busy > 0 { ("ACTIVE", t.ok) } else if live > 0 { ("STANDBY", t.dim) } else { ("NO SIGNAL", t.err) };
        let rate: u64 = world.sessions.iter().filter(|s| s.alive).map(|s| vitals::money_rate(s, now)).sum();
        let view = if self.goal_depth > 0.5 { format!("3D · {:.0}° · {:.1}×", self.pitch.to_degrees(), self.zoom) } else { format!("flat · {:.1}×", self.zoom) };
        let lines = [
            Line::from(vec![
                Span::styled("◤ SYSTEM ", Style::new().fg(t.hi).add_modifier(Modifier::BOLD)),
                Span::styled(status.0, Style::new().fg(status.1).add_modifier(Modifier::BOLD)),
            ]),
            kv("sessions", format!("{live} live · {busy} busy"), t.fg),
            kv("agents", format!("{agents} running"), t.agent),
            kv("uplink", format!("⇡ {}/m", fmt_n(up)), t.accent),
            kv("downlink", format!("⇣ {}/m", fmt_n(down)), t.hi),
            kv("burn", format!("{}/h API-equiv.", vitals::money(rate)), t.dim),
            kv("view", view, t.dim),
        ];
        for (i, line) in lines.iter().enumerate() {
            let y = top + 1 + i as u16;
            if y < bottom {
                buf.set_line(l + 2, y, line, area.width.saturating_sub(4).min(34));
            }
        }

        if self.director.on {
            self.director.draw(f, area, t, world, order, names);
            return;
        }

        // The latest events of every session, bottom left.
        let mut items: Vec<(&Session, &crate::model::FeedItem)> =
            order.iter().flat_map(|&si| world.sessions[si].feed.iter().map(move |it| (&world.sessions[si], it))).collect();
        items.sort_by_key(|(_, it)| Reverse(it.at));
        let width = (area.width / 2).saturating_sub(4);
        for (k, (s, it)) in items.iter().take(4).enumerate() {
            let y = bottom.saturating_sub(1 + k as u16);
            if y <= top + lines.len() as u16 {
                break;
            }
            let color = it.kind.map_or(t.prompt, |k| t.kind(k));
            let line = Line::from(vec![
                Span::styled(format!("▸ {:>6} ", fmt_dur(now - it.at)), Style::new().fg(t.faint)),
                Span::styled(format!("{:<12} ", clip(s.project(), 12)), Style::new().fg(t.dim)),
                Span::styled(format!("{} ", clip(if it.label.is_empty() { "prompt" } else { &it.label }, 10)), Style::new().fg(color)),
                Span::styled(it.text.clone(), Style::new().fg(if k == 0 { t.fg } else { t.dim })),
            ]);
            buf.set_line(l + 2, y, &line, width);
        }

        let hint = if self.goal_depth > 0.5 {
            "drag turn · wheel zoom · click lock · v flat · d director · space ping · 0 reset"
        } else {
            "drag pan · wheel zoom · click lock · v 3D · d director · space ping · 0 reset"
        };
        let hint_w = hint.chars().count() as u16;
        if area.width > hint_w + 6 && area.width / 2 > hint_w / 2 {
            buf.set_string(r.saturating_sub(hint_w + 1), bottom - 1, hint, Style::new().fg(t.faint));
        }

        if let (Some(h), Some(rect)) = (sel, callout) {
            f.render_widget(Clear, rect);
            let block = Block::bordered()
                .border_type(BorderType::Double)
                .border_style(Style::new().fg(t.accent))
                .style(Style::new().bg(t.surface))
                .title_top(Line::from(Span::styled(" ◢ TARGET LOCK ", Style::new().fg(t.accent).add_modifier(Modifier::BOLD))));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            f.render_widget(Paragraph::new(target_lines(h, world, t)).wrap(Wrap { trim: true }), inner);
        }
    }

    fn touched(&mut self) {
        self.last_input = Instant::now();
        self.director.set(false);
    }

    pub fn on_mouse(&mut self, m: MouseEvent) -> HudAction {
        let p = self.to_canvas(m.column, m.row);
        match m.kind {
            MouseEventKind::Moved => {
                // A small move does not wake the director's screen.
                if self.mouse.is_some() || !self.director.on {
                    self.touched();
                }
                self.mouse = p;
                self.hover = p.and_then(|p| self.hit(p)).map(|h| h.id);
            }
            MouseEventKind::Down(button) => {
                self.touched();
                self.drag = Some((button, m.column, m.row, self.pan, (self.yaw, self.pitch), false));
                if button != MouseButton::Left {
                    return HudAction::None;
                }
                if let Some(h) = p.and_then(|p| self.hit(p)) {
                    let double = self.last_click.as_ref().is_some_and(|(at, id)| *id == h.id && at.elapsed() < DOUBLE_CLICK);
                    self.last_click = Some((Instant::now(), h.id.clone()));
                    self.selected = Some(h.id.clone());
                    if double {
                        match (h.kind, h.session) {
                            (NodeKind::Session, Some(si)) => return HudAction::Open(si),
                            _ => self.fly_to(&h),
                        }
                    }
                }
            }
            MouseEventKind::Drag(_) => {
                if let Some((button, c0, r0, pan0, (yaw0, pitch0), _)) = self.drag {
                    let (dx, dy) = (m.column as f64 - c0 as f64, m.row as f64 - r0 as f64);
                    if button == MouseButton::Left && self.goal_depth > 0.5 {
                        // Turning: across spins the floor, down and up tilt it.
                        self.yaw = yaw0 - dx * 0.025;
                        self.pitch = (pitch0 + dy * 0.05).clamp(0.15, PI / 2.0);
                        self.goal_yaw = self.yaw;
                        self.goal_pitch = self.pitch;
                    } else {
                        let [x0, x1, y0, y1] = self.bounds;
                        let cw = (x1 - x0) / self.area.width.max(1) as f64;
                        let ch = (y1 - y0) / self.area.height.max(1) as f64;
                        self.pan = (pan0.0 - dx * cw, pan0.1 + dy * ch);
                        self.goal_pan = self.pan;
                    }
                    self.drag = Some((button, c0, r0, pan0, (yaw0, pitch0), true));
                }
            }
            MouseEventKind::Up(_) => {
                let dragged = self.drag.is_some_and(|d| d.5);
                if !dragged && p.and_then(|p| self.hit(p)).is_none() {
                    self.selected = None;
                }
                self.drag = None;
            }
            MouseEventKind::ScrollUp => {
                self.touched();
                self.zoom_at(p, 1.2);
            }
            MouseEventKind::ScrollDown => {
                self.touched();
                self.zoom_at(p, 1.0 / 1.2);
            }
            _ => {}
        }
        HudAction::None
    }

    pub fn on_key(&mut self, code: KeyCode) -> HudAction {
        if code == KeyCode::Char('d') {
            self.last_input = Instant::now();
            let on = !self.director.on;
            self.director.set(on);
            return HudAction::None;
        }
        self.touched();
        let n = self.hits.len();
        let pos = self.selected.as_ref().and_then(|s| self.hits.iter().position(|h| &h.id == s));
        match code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Down | KeyCode::Char('l') | KeyCode::Char('j') if n > 0 => {
                self.selected = Some(self.hits[pos.map_or(0, |p| (p + 1) % n)].id.clone());
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') if n > 0 => {
                self.selected = Some(self.hits[pos.map_or(n - 1, |p| (p + n - 1) % n)].id.clone());
            }
            KeyCode::Enter => {
                if let Some(si) = pos.and_then(|p| self.hits[p].session) {
                    return HudAction::Open(si);
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom_at(None, 1.25),
            KeyCode::Char('-') => self.zoom_at(None, 1.0 / 1.25),
            KeyCode::Char('0') => {
                self.goal_zoom = 1.0;
                self.goal_pan = (0.0, 0.0);
                self.goal_yaw = 0.0;
                self.goal_pitch = PITCH_3D;
            }
            KeyCode::Char('v') => self.set_flat(self.goal_depth > 0.5),
            KeyCode::Char('f') => {
                if let Some(h) = pos.map(|p| self.hits[p].clone()) {
                    self.fly_to(&h);
                }
            }
            KeyCode::Char(' ') => self.waves.push(Instant::now()),
            KeyCode::Esc => self.selected = None,
            _ => {}
        }
        HudAction::None
    }

    pub fn has_target(&self) -> bool {
        self.selected.is_some()
    }

    /// The session of the locked target, if it belongs to one.
    pub fn target_session(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.hits.iter().find(|h| &h.id == id)?.session
    }

    /// Zooms keeping the point under the pointer (or the view center) in place; the camera
    /// glides there over the next frames.
    fn zoom_at(&mut self, p: Option<(f64, f64)>, k: f64) {
        let z = (self.goal_zoom * k).clamp(0.4, 8.0);
        let p = p.unwrap_or(self.goal_pan);
        self.goal_pan = (p.0 - (p.0 - self.goal_pan.0) * self.goal_zoom / z, p.1 - (p.1 - self.goal_pan.1) * self.goal_zoom / z);
        self.goal_zoom = z;
    }

    /// Centers the camera on a node and zooms in on it.
    fn fly_to(&mut self, h: &Hit) {
        self.goal_pan = (h.x, h.y);
        self.goal_zoom = self.goal_zoom.max(2.2);
    }

    fn to_canvas(&self, col: u16, row: u16) -> Option<(f64, f64)> {
        if !self.area.contains(Position { x: col, y: row }) {
            return None;
        }
        let [x0, x1, y0, y1] = self.bounds;
        let fx = (col - self.area.x) as f64 + 0.5;
        let fy = (row - self.area.y) as f64 + 0.5;
        Some((x0 + fx / self.area.width as f64 * (x1 - x0), y1 - fy / self.area.height as f64 * (y1 - y0)))
    }

    fn hit(&self, p: (f64, f64)) -> Option<Hit> {
        let [x0, x1, ..] = self.bounds;
        let cw = (x1 - x0) / self.area.width.max(1) as f64;
        self.hits
            .iter()
            .map(|h| (h, ((h.x - p.0).powi(2) + (h.y - p.1).powi(2)).sqrt()))
            .filter(|(h, d)| *d <= (h.r * 1.6).max(cw * 2.5))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(h, _)| h.clone())
    }
}

// ---------------------------------------------------------------- the graph

/// A point of the floor at radius `r` and angle `a` (angles turn counterclockwise seen from above).
fn on_floor(r: f64, a: f64, y: f64) -> P3 {
    (r * a.cos(), y, -r * a.sin())
}

/// Where a moon sits: `rc` from its session, at `phi` around it measured from the outward
/// direction, its orbit tilted out of the floor by `tilt`.
fn moon(center: P3, ang: f64, rc: f64, phi: f64, tilt: f64) -> P3 {
    let (u, w) = ((ang.cos(), 0.0, -ang.sin()), (-ang.sin(), 0.0, -ang.cos()));
    let (c, s) = (phi.cos(), phi.sin());
    let (ct, st) = (tilt.cos(), tilt.sin());
    (center.0 + rc * (c * u.0 + s * ct * w.0), center.1 + rc * s * st, center.2 + rc * (c * u.2 + s * ct * w.2))
}

#[allow(clippy::too_many_arguments)]
fn build(
    world: &World,
    order: &[usize],
    names: &HashMap<usize, String>,
    maxr: f64,
    orbit: f64,
    m: f64,
    t: &Theme,
    alerts: &[Alert],
    collisions: &[Collision],
) -> (Vec<Node>, Vec<Edge>) {
    let now = world.now;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let up: u64 = world.sessions.iter().map(|s| s.tok_up.per_min(now)).sum();
    let down: u64 = world.sessions.iter().map(|s| s.tok_down.per_min(now)).sum();
    nodes.push(Node {
        id: "core".into(),
        kind: NodeKind::Core,
        label: "◆ CORE".into(),
        sub: format!("⇡{} ⇣{}", fmt_n(up), fmt_n(down)),
        parent: None,
        session: None,
        target: (0.0, 0.0, 0.0),
        active: world.sessions.iter().any(Session::busy),
        alive: true,
        needs: false,
        color: t.hi,
        r: maxr * 0.075,
        warn: None,
    });
    let (r1, r2, rc) = (maxr * 0.46, maxr * 0.92, maxr * 0.21);
    let n = order.len().max(1);
    let warn_of = |si: usize, a: usize| alerts.iter().find(|x| x.si == si && x.agent == a).map(Alert::short);
    // Shared resources: MCP servers and skills, with the sessions that use them.
    let mut res: Vec<(String, NodeKind, String, u32, Vec<(usize, bool)>)> = Vec::new();
    let mut res_ix: HashMap<String, usize> = HashMap::new();
    let mut note = |key: String, kind: NodeKind, label: String, weight: u32, user: (usize, bool)| {
        let i = *res_ix.entry(key.clone()).or_insert_with(|| {
            res.push((key, kind, label, 0, Vec::new()));
            res.len() - 1
        });
        res[i].3 += weight;
        res[i].4.push(user);
    };
    let mut session_ix: HashMap<usize, usize> = HashMap::new();
    for (k, &si) in order.iter().enumerate() {
        let s = &world.sessions[si];
        let ang = -PI / 2.0 + TAU * k as f64 / n as f64;
        let p = on_floor(r1, ang, 0.0);
        let busy = s.busy();
        let rate = s.tok_up.per_min(now) + s.tok_down.per_min(now);
        let sid = format!("s:{}", s.id);
        let color = rgb(t.towers[k % t.towers.len()]);
        let s_ix = nodes.len();
        session_ix.insert(si, s_ix);
        nodes.push(Node {
            id: sid.clone(),
            kind: NodeKind::Session,
            label: clip(names.get(&si).map_or(s.project(), String::as_str), 18),
            sub: if rate > 0 {
                format!("{}/m", fmt_n(rate))
            } else if s.alive {
                s.status.clone()
            } else {
                "ended".into()
            },
            parent: Some("core".into()),
            session: Some(si),
            target: p,
            active: busy,
            alive: s.alive,
            needs: s.needs_you(),
            color,
            r: maxr * 0.05,
            warn: warn_of(si, 0),
        });
        let needs = s.needs_you();
        edges.push(Edge { a: 0, b: s_ix, active: busy || needs, color: if needs { t.warn } else { color }, shared: false, alarm: None });

        let mut kids: Vec<(bool, i64, Node)> = Vec::new();
        for a in 1..s.agents.len() {
            let ag = &s.agents[a];
            let running = s.agent_running(a);
            // Generic agent types say little: their task's first words do.
            let label = match ag.kind.as_str() {
                "" | "general-purpose" | "fork" => ag.description.split_whitespace().take(2).collect::<Vec<_>>().join(" "),
                "workflow-subagent" => ag.description.clone(),
                k => k.to_string(),
            };
            kids.push((running, ag.last_at, Node {
                id: format!("a:{}:{a}", s.id),
                kind: NodeKind::Agent,
                label: clip(&label, 14),
                sub: clip(&ag.description, 24),
                parent: Some(sid.clone()),
                session: Some(si),
                target: p,
                active: running,
                alive: s.alive,
                needs: false,
                color: t.agent,
                r: maxr * 0.028,
                warn: warn_of(si, a),
            }));
        }
        for (w, wf) in s.workflows.iter().enumerate() {
            let step = wf.step.as_deref().and_then(|x| s.step_by_tool(x));
            let running = s.alive && step.is_some_and(|x| x.running());
            let race = insight::race(world, s, w).filter(|_| running).map(|r| vitals::race_chip(&r));
            kids.push((running, step.map_or(0, |x| x.started), Node {
                id: format!("w:{}:{w}", s.id),
                kind: NodeKind::Workflow,
                label: clip(&wf.name, 16),
                sub: race.unwrap_or_else(|| format!("{} phases · {} agents", wf.phases.len(), wf.agents.len().max(wf.live.len()))),
                parent: Some(sid.clone()),
                session: Some(si),
                target: p,
                active: running,
                alive: s.alive,
                needs: false,
                color: t.workflow,
                r: maxr * 0.032,
                warn: None,
            }));
        }
        kids.sort_by_key(|(running, at, node)| (Reverse(node.warn.is_some()), Reverse(*running), Reverse(*at)));
        kids.truncate(5);
        let mc = kids.len() as f64;
        for (j, (running, _, mut node)) in kids.into_iter().enumerate() {
            // Moons fan out away from the core, then circle their session slowly in 3D.
            let phi = (0.45 + 0.2 * m) * (j as f64 - (mc - 1.0) / 2.0) + orbit * 0.07;
            node.target = moon(p, ang, rc, phi, 0.5 * m);
            edges.push(Edge { a: s_ix, b: nodes.len(), active: running, color: node.color, shared: false, alarm: None });
            nodes.push(node);
        }

        let running_mcp = s.mcp_running();
        for (server, st) in &s.mcp {
            note(format!("m:{server}"), NodeKind::Mcp, server_label(server).to_string(), st.calls, (s_ix, running_mcp.contains(server)));
        }
        for (skill, count) in &s.skills {
            let short = skill.rsplit(':').next().unwrap_or(skill);
            note(format!("k:{skill}"), NodeKind::Skill, short.to_string(), *count, (s_ix, false));
        }
    }
    res.sort_by_key(|r| Reverse(r.3));
    res.truncate(16);
    let nr = res.len().max(1) as f64;
    for (i, (key, kind, label, weight, users)) in res.into_iter().enumerate() {
        let ang = -PI / 2.0 + TAU * (i as f64 + 0.5) / nr;
        let active = users.iter().any(|u| u.1);
        let color = if kind == NodeKind::Mcp { t.mcp } else { t.skill };
        let ix = nodes.len();
        nodes.push(Node {
            id: key,
            kind,
            label: clip(&label, 16),
            sub: format!("{weight} {} · {} session{}", if kind == NodeKind::Mcp { "calls" } else { "uses" }, users.len(), if users.len() == 1 { "" } else { "s" }),
            parent: Some("core".into()),
            session: None,
            // A crown above the floor in 3D.
            target: on_floor(r2, ang, r2 * 0.16 * m),
            active,
            alive: true,
            needs: false,
            color,
            r: maxr * (0.022 + 0.004 * (weight as f64).log2().max(0.0)),
            warn: None,
        });
        for (s_ix, running) in users {
            edges.push(Edge { a: s_ix, b: ix, active: running, color, shared: true, alarm: None });
        }
    }
    // Two sessions changing one file: a flashing link between them.
    for c in collisions {
        let ss: Vec<usize> = c.sessions().iter().filter_map(|si| session_ix.get(si).copied()).collect();
        let file = parse::file_name(&c.path).to_string();
        for pair in ss.windows(2) {
            edges.push(Edge { a: pair[0], b: pair[1], active: true, color: t.err, shared: false, alarm: Some(file.clone()) });
        }
    }
    (nodes, edges)
}

fn target_lines(h: &Hit, world: &World, t: &Theme) -> Vec<Line<'static>> {
    let kv = |k: &str, v: String| {
        Line::from(vec![Span::styled(format!("{k:<8} "), Style::new().fg(t.faint)), Span::styled(v, Style::new().fg(t.fg))])
    };
    let head = |s: String, c: Color| Line::from(Span::styled(s, Style::new().fg(c).add_modifier(Modifier::BOLD)));
    let now = world.now;
    let session = h.session.map(|si| &world.sessions[si]);
    let tail = |id: &str| id.rsplit(':').next().and_then(|x| x.parse::<usize>().ok());
    match (h.kind, session) {
        (NodeKind::Session, Some(s)) => {
            let fc = insight::forecast(world, s);
            let mut v = vec![
                head(s.project().to_uppercase(), t.hi),
                kv("title", s.title.clone()),
                kv("status", if s.alive { s.status.clone() } else { "ended".into() }),
                kv("model", format!("{} · ctx {} of {}", short_model(&s.agents[0].model), fmt_n(fc.ctx), fmt_n(fc.limit))),
                kv("flow", format!("⇡ {}/m · ⇣ {}/m", fmt_n(s.tok_up.per_min(now)), fmt_n(s.tok_down.per_min(now)))),
                kv("cache", format!("{} · {} so far", s.cache_ratio(now, 60).map_or("–".into(), |r| format!("{:.0}% hits", r * 100.0)), vitals::money(s.cost_micros))),
                kv("agents", format!("{} running of {}", s.running_agents(), s.agents.len() - 1)),
            ];
            if let Some((_, st)) = s.current() {
                v.push(kv("now", format!("{} {}", st.label().0, st.label().1)));
            }
            v
        }
        (NodeKind::Agent, Some(s)) => match tail(&h.id).and_then(|a| s.agents.get(a).map(|ag| (a, ag))) {
            Some((a, ag)) => vec![
                head(format!("AGENT · {}", ag.kind.to_uppercase()), t.agent),
                kv("task", ag.description.clone()),
                kv("state", if s.agent_running(a) { "running".into() } else { "done".into() }),
                kv("model", short_model(&ag.model).to_string()),
                kv("work", format!("{} steps · out {}", ag.step_count(), fmt_n(ag.tokens().output))),
                kv("said", ag.last_text.clone()),
            ],
            None => vec![],
        },
        (NodeKind::Workflow, Some(s)) => match tail(&h.id).and_then(|w| s.workflows.get(w).map(|wf| (w, wf))) {
            Some((w, wf)) => {
                let mut v = vec![
                    head(format!("WORKFLOW · {}", wf.name), t.workflow),
                    kv("status", if wf.status.is_empty() { "running".into() } else { wf.status.clone() }),
                    kv("phases", wf.phases.join(" → ")),
                    kv("agents", format!("{} · {} tokens", wf.agents.len().max(wf.live.len()), fmt_n(wf.tokens))),
                ];
                if let Some(r) = insight::race(world, s, w) {
                    v.push(kv("race", format!("{} · vs {}", vitals::race_chip(&r).trim_start_matches("△ "), r.ghost.name)));
                }
                v
            }
            None => vec![],
        },
        (NodeKind::Mcp, _) => {
            let server = h.id.trim_start_matches("m:");
            let (mut calls, mut errors, mut last, mut users) = (0, 0, String::new(), 0);
            for s in &world.sessions {
                if let Some(st) = s.mcp.get(server) {
                    calls += st.calls;
                    errors += st.errors;
                    last = st.last_tool.clone();
                    users += 1;
                }
            }
            vec![
                head(format!("MCP · {}", server_label(server)), t.mcp),
                kv("calls", format!("{calls} · {errors} failed")),
                kv("last", last),
                kv("used by", format!("{users} session{}", if users == 1 { "" } else { "s" })),
            ]
        }
        (NodeKind::Skill, _) => {
            let skill = h.id.trim_start_matches("k:");
            let uses: u32 = world.sessions.iter().flat_map(|s| s.skills.iter()).filter(|(n, _)| n == skill).map(|(_, c)| c).sum();
            vec![head(format!("SKILL · {skill}"), t.skill), kv("uses", uses.to_string())]
        }
        _ => {
            let up: u64 = world.sessions.iter().map(|s| s.tok_up.per_min(now)).sum();
            let down: u64 = world.sessions.iter().map(|s| s.tok_down.per_min(now)).sum();
            let out: u64 = world.sessions.iter().map(|s| s.tokens().output).sum();
            let cost: u64 = world.sessions.iter().map(|s| s.cost_micros).sum();
            vec![
                head("CORE · this machine".into(), t.hi),
                kv("sessions", world.sessions.iter().filter(|s| s.alive).count().to_string()),
                kv("flow", format!("⇡ {}/m · ⇣ {}/m", fmt_n(up), fmt_n(down))),
                kv("output", format!("{} tokens in total", fmt_n(out))),
                kv("price", format!("{} API-equivalent", vitals::money(cost))),
            ]
        }
    }
}

fn callout_rect(area: Rect) -> Rect {
    let w = 44.min(area.width / 2);
    let h = 11.min(area.height.saturating_sub(4));
    Rect::new(area.right().saturating_sub(w + 2), area.y + 2, w, h)
}

// ---------------------------------------------------------------- drawing

fn polar(c: (f64, f64), r: f64, a: f64) -> (f64, f64) {
    (c.0 + r * a.cos(), c.1 + r * a.sin())
}

#[allow(clippy::too_many_arguments)]
fn node_shape(ctx: &mut ratatui::widgets::canvas::Context, n: &Node, (x, y): (f64, f64), size: f64, color: Color, time: f64, born: f64, fx: bool, dim: bool, t: &Theme) {
    let appear = born.min(1.0);
    let r = size * (0.3 + 0.7 * appear * appear * (3.0 - 2.0 * appear));
    let color = if n.alive && !dim { color } else { t.faint };
    if n.needs && !dim {
        let on = !fx || (time * 2.0) as i64 % 2 == 0;
        ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * (2.0 + 0.3 * (time * 5.0).sin()), gap: 1, color: if on { t.warn } else { t.accent } });
    }
    let spin = if fx { time } else { 0.0 };
    match n.kind {
        NodeKind::Core => {
            let pulse = if n.active && fx { 1.0 + 0.08 * (time * 4.0).sin() } else { 1.0 };
            for k in 1..=4 {
                ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * 0.13 * k as f64, gap: 1, color: t.grad(0.0) });
            }
            ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * 0.75 * pulse, gap: 1, color: t.grad(0.35) });
            for k in 0..3 {
                ctx.draw(&Arc { c: (x, y), r: r * 1.25, a0: spin * 1.3 + TAU * k as f64 / 3.0, sweep: 1.3, color: t.grad(0.65) });
            }
            for k in 0..2 {
                ctx.draw(&Arc { c: (x, y), r: r * 1.65, a0: -spin * 0.8 + PI * k as f64, sweep: 2.1, color: t.grad(1.0) });
            }
            ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * 2.05, gap: 3, color: t.dim });
        }
        NodeKind::Session => {
            let pulse = if n.active && fx { 1.0 + 0.12 * (time * 5.0 + x).sin() } else { 1.0 };
            ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * pulse, gap: 1, color });
            if n.alive {
                for k in 1..=3 {
                    ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * 0.16 * k as f64, gap: 1, color });
                }
            }
            if n.active {
                ctx.draw(&Arc { c: (x, y), r: r * 1.6, a0: spin * 2.2, sweep: 2.0, color: t.hi });
                ctx.draw(&Arc { c: (x, y), r: r * 1.6, a0: spin * 2.2 + PI, sweep: 2.0, color });
            }
        }
        NodeKind::Agent => {
            ctx.draw(&Poly { c: (x, y), r, sides: 4, rot: if n.active { spin * 1.5 } else { 0.0 }, color });
            if n.active {
                ctx.draw(&Arc { c: (x, y), r: r * 1.7, a0: -spin * 2.0, sweep: 2.4, color: t.hi });
            }
        }
        NodeKind::Workflow => {
            ctx.draw(&Poly { c: (x, y), r, sides: 3, rot: -PI / 2.0 + if n.active { spin } else { 0.0 }, color });
            if n.active {
                ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * (1.5 + 0.2 * (time * 4.0).sin()), gap: 2, color: t.hi });
            }
        }
        NodeKind::Mcp => {
            ctx.draw(&Poly { c: (x, y), r, sides: 6, rot: 0.0, color });
            ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * 0.35, gap: 1, color });
            if n.active {
                ctx.draw(&Ring { c: (x, y), sx: 1.0, r: r * (1.4 + 0.25 * (time * 6.0).sin()), gap: 1, color: t.hi });
            }
        }
        NodeKind::Skill => {
            for k in 0..3 {
                let a = PI / 2.0 + k as f64 * PI / 3.0;
                ctx.draw(&Seg { a: polar((x, y), r, a), b: polar((x, y), r, a + PI), color });
            }
        }
    }
    if appear < 1.0 {
        ctx.draw(&Ring { c: (x, y), sx: 1.0, r: size * (1.0 + 3.0 * appear), gap: 1, color: t.hi });
    }
}

fn reticle(ctx: &mut ratatui::widgets::canvas::Context, c: (f64, f64), r: f64, time: f64, t: &Theme) {
    let rr = r * 2.3 + 2.0;
    for k in 0..4 {
        ctx.draw(&Arc { c, r: rr, a0: time * 0.9 + k as f64 * PI / 2.0, sweep: 0.75, color: t.accent });
    }
    for k in 0..4 {
        let a = k as f64 * PI / 2.0 + PI / 4.0;
        ctx.draw(&Seg { a: polar(c, rr + 1.0, a), b: polar(c, rr + 3.5, a), color: t.hi });
    }
}

/// Number of samples that covers a segment at the canvas resolution.
fn samples(p: &Painter, a: (f64, f64), b: (f64, f64)) -> usize {
    match (p.get_point(a.0, a.1), p.get_point(b.0, b.1)) {
        (Some(u), Some(v)) => u.0.abs_diff(v.0).max(u.1.abs_diff(v.1)) + 1,
        _ => 600,
    }
}

fn plot(p: &mut Painter, x: f64, y: f64, color: Color) {
    if let Some((gx, gy)) = p.get_point(x, y) {
        p.paint(gx, gy, color);
    }
}

fn segment(p: &mut Painter, a: (f64, f64), b: (f64, f64), color: Color) {
    let n = samples(p, a, b);
    for i in 0..=n {
        let f = i as f64 / n as f64;
        plot(p, a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f, color);
    }
}

/// Dots per canvas unit around `c`, so curves get enough samples at any zoom.
fn density(p: &Painter, c: (f64, f64), r: f64) -> f64 {
    match (p.get_point(c.0, c.1), p.get_point(c.0 + r, c.1)) {
        (Some(a), Some(b)) => (a.0.abs_diff(b.0) as f64 / r).max(0.5),
        _ => 2.0,
    }
}

struct Seg {
    a: (f64, f64),
    b: (f64, f64),
    color: Color,
}

impl Shape for Seg {
    fn draw(&self, p: &mut Painter) {
        segment(p, self.a, self.b, self.color);
    }
}

/// A dashed line whose dashes move along it when `phase` changes.
struct Dashed {
    a: (f64, f64),
    b: (f64, f64),
    on: f64,
    off: f64,
    phase: f64,
    color: Color,
}

impl Shape for Dashed {
    fn draw(&self, p: &mut Painter) {
        let len = (self.b.0 - self.a.0).hypot(self.b.1 - self.a.1);
        if len <= 0.0 {
            return;
        }
        let n = samples(p, self.a, self.b);
        for i in 0..=n {
            let f = i as f64 / n as f64;
            if (len * f + self.phase).rem_euclid(self.on + self.off) < self.on {
                plot(p, self.a.0 + (self.b.0 - self.a.0) * f, self.a.1 + (self.b.1 - self.a.1) * f, self.color);
            }
        }
    }
}

/// A circle (or, with `sx` above 1, an ellipse stretched across) drawn with every `gap`-th dot.
struct Ring {
    c: (f64, f64),
    r: f64,
    sx: f64,
    gap: usize,
    color: Color,
}

impl Shape for Ring {
    fn draw(&self, p: &mut Painter) {
        if self.r <= 0.0 {
            return;
        }
        let n = (TAU * self.r * self.sx * density(p, self.c, self.r) * 1.2).clamp(12.0, 6000.0) as usize;
        for i in (0..n).step_by(self.gap.max(1)) {
            let a = TAU * i as f64 / n as f64;
            plot(p, self.c.0 + self.r * self.sx * a.cos(), self.c.1 + self.r * a.sin(), self.color);
        }
    }
}

/// A circle on the floor (or at height `y`) around the core, seen through the camera: its far
/// side fades toward the background.
struct Ring3 {
    cam: Cam,
    r: f64,
    y: f64,
    gap: usize,
    base: Color,
    bg: Color,
    fade: f64,
    r2: f64,
}

impl Shape for Ring3 {
    fn draw(&self, p: &mut Painter) {
        if self.r <= 0.0 {
            return;
        }
        let scale = density(p, (0.0, 0.0), self.r);
        let n = (TAU * self.r * scale * 1.3).clamp(24.0, 6000.0) as usize;
        let mut last: Option<(f64, Color)> = None;
        for i in (0..n).step_by(self.gap.max(1)) {
            let a = TAU * i as f64 / n as f64;
            let (x, y, d, _) = self.cam.project(on_floor(self.r, a, self.y));
            let f = (-d / self.r2).clamp(0.0, 1.0) * self.fade;
            // Colors change in steps, so the fade does not cost a conversion per dot.
            let color = match last {
                Some((lf, c)) if (lf - f).abs() < 0.04 => c,
                _ => {
                    let c = fog(self.base, self.bg, f);
                    last = Some((f, c));
                    c
                }
            };
            plot(p, x, y, color);
        }
    }
}

struct Arc {
    c: (f64, f64),
    r: f64,
    a0: f64,
    sweep: f64,
    color: Color,
}

impl Shape for Arc {
    fn draw(&self, p: &mut Painter) {
        let n = (self.r * self.sweep.abs() * density(p, self.c, self.r) * 1.2).clamp(4.0, 2000.0) as usize;
        for i in 0..=n {
            let a = self.a0 + self.sweep * i as f64 / n as f64;
            plot(p, self.c.0 + self.r * a.cos(), self.c.1 + self.r * a.sin(), self.color);
        }
    }
}

/// A regular polygon.
struct Poly {
    c: (f64, f64),
    r: f64,
    sides: usize,
    rot: f64,
    color: Color,
}

impl Shape for Poly {
    fn draw(&self, p: &mut Painter) {
        let v: Vec<(f64, f64)> = (0..self.sides).map(|k| polar(self.c, self.r, self.rot + TAU * k as f64 / self.sides as f64)).collect();
        for k in 0..self.sides {
            segment(p, v[k], v[(k + 1) % self.sides], self.color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;

    fn demo_world(dir: &std::path::Path) -> World {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("s1.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","timestamp":"2026-10-16T10:00:00.000Z","cwd":"/tmp/demo","message":{"content":"hello"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-10-16T10:00:01.000Z","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"tool_use","id":"t1","name":"mcp__claude_ai_Notion__notion-search","input":{"query":"x"}}]}}"#,
                "\n"
            ),
        )
        .unwrap();
        let mut world = World::replay(&path, 20.0).unwrap();
        world.run_to_end();
        world
    }

    fn cell_of(hud: &HudState, h: &Hit, w: f64, ht: f64) -> (u16, u16) {
        let [x0, x1, y0, y1] = hud.bounds;
        (((h.x - x0) / (x1 - x0) * w) as u16, ((y1 - h.y) / (y1 - y0) * ht) as u16)
    }

    #[test]
    fn click_locks_double_click_opens_and_wheel_zooms_around_the_pointer() {
        let dir = std::env::temp_dir().join(format!("ccorbit-hud-{}", std::process::id()));
        let world = demo_world(&dir);
        let t = &crate::theme::THEMES[0];
        let area = Rect::new(0, 0, 120, 40);
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let names = HashMap::new();
        let mut hud = HudState::new();
        hud.set_flat(true);
        hud.settle = true;
        term.draw(|f| hud.draw(f, area, t, false, &world, &[0], &names, &[], &[])).unwrap();
        assert!(hud.hits.iter().any(|h| h.id == "m:claude_ai_Notion"), "the MCP server is on the outer ring");

        let s = hud.hits.iter().find(|h| h.kind == NodeKind::Session).unwrap().clone();
        let (col, row) = cell_of(&hud, &s, 120.0, 40.0);
        let ev = |kind| MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE };

        assert!(matches!(hud.on_mouse(ev(MouseEventKind::Down(MouseButton::Left))), HudAction::None));
        assert_eq!(hud.selected.as_deref(), Some(s.id.as_str()));
        hud.on_mouse(ev(MouseEventKind::Up(MouseButton::Left)));
        assert!(matches!(hud.on_mouse(ev(MouseEventKind::Down(MouseButton::Left))), HudAction::Open(0)));
        hud.on_mouse(ev(MouseEventKind::Up(MouseButton::Left)));

        let before = hud.to_canvas(col, row).unwrap();
        hud.on_mouse(ev(MouseEventKind::ScrollUp));
        hud.settle = true; // land the camera glide in one frame
        term.draw(|f| hud.draw(f, area, t, false, &world, &[0], &names, &[], &[])).unwrap();
        let after = hud.to_canvas(col, row).unwrap();
        assert!(hud.zoom > 1.0);
        assert!((before.0 - after.0).abs() < 1e-6 && (before.1 - after.1).abs() < 1e-6);

        // In the flat view dragging pans; a click on empty space drops the target.
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 2, row: 20, modifiers: KeyModifiers::NONE });
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Drag(MouseButton::Left), column: 12, row: 20, modifiers: KeyModifiers::NONE });
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: 12, row: 20, modifiers: KeyModifiers::NONE });
        assert!(hud.pan.0 < 0.0, "dragging right moves the view left");
        assert!(hud.selected.is_some(), "a drag keeps the target");
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 1, row: 38, modifiers: KeyModifiers::NONE });
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: 1, row: 38, modifiers: KeyModifiers::NONE });
        assert!(hud.selected.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_3d_view_turns_with_a_drag_and_still_hits_nodes() {
        let dir = std::env::temp_dir().join(format!("ccorbit-hud3-{}", std::process::id()));
        let world = demo_world(&dir);
        let t = &crate::theme::THEMES[0];
        let area = Rect::new(0, 0, 120, 40);
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let names = HashMap::new();
        let mut hud = HudState::new();
        hud.settle = true;
        term.draw(|f| hud.draw(f, area, t, false, &world, &[0], &names, &[], &[])).unwrap();
        let s = hud.hits.iter().find(|h| h.kind == NodeKind::Session).unwrap().clone();
        let (col, row) = cell_of(&hud, &s, 120.0, 40.0);
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: col, row, modifiers: KeyModifiers::NONE });
        assert_eq!(hud.selected.as_deref(), Some(s.id.as_str()), "a session on the tilted floor can be clicked");
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Drag(MouseButton::Left), column: col + 10, row, modifiers: KeyModifiers::NONE });
        hud.on_mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: col + 10, row, modifiers: KeyModifiers::NONE });
        assert!(hud.yaw < 0.0, "dragging across turns the floor");
        assert_eq!(hud.pan, (0.0, 0.0), "and does not pan");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
