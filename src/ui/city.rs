//! The code city: every file the sessions read or changed, as an isometric town. Each project
//! is a district and each file a building whose height follows how many lines changed, in the
//! color of the session that touched it last. Fresh edits glow, windows light up while a file is
//! being read, and a file that two sessions change at once flashes red.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{App, Insights, fit, identity, names, panel};
use crate::parse::clip;
use crate::source::World;
use crate::theme::{Rgb, mix, rgb, shade};
use crate::time::{fmt_dur, fmt_n};

/// A building: front face 3 cells wide, its side, and a gap.
const LOT_W: u16 = 5;
const FACE_W: u16 = 3;
pub const WINDOWS: [(&str, i64); 4] = [("last hour", 3_600_000), ("last 6 hours", 21_600_000), ("last 24 hours", 86_400_000), ("everything loaded", i64::MAX)];

pub struct Lot {
    pub path: String,
    pub rel: String,
    pub reads: u32,
    pub edits: u32,
    pub added: u32,
    pub removed: u32,
    pub created: bool,
    pub last_read: i64,
    pub last_edit: i64,
    /// The session and agent that touched it last.
    pub owner: usize,
    pub agent: usize,
    pub sessions: Vec<usize>,
    pub collide: bool,
}

impl Lot {
    fn last(&self) -> i64 {
        self.last_read.max(self.last_edit)
    }
}

pub struct District {
    pub name: String,
    pub root: String,
    /// Wall position of the session it belongs to, for its color.
    pub k: usize,
    pub lots: Vec<usize>,
}

/// Every file touched within the window, merged across sessions and grouped by project: a
/// file belongs to the session folder that contains it most closely; the rest is "elsewhere".
pub fn gather(world: &World, order: &[usize], window: i64, ins: &Insights) -> (Vec<District>, Vec<Lot>) {
    let now = world.now;
    let colliding: HashSet<&str> = ins.collisions.iter().map(|c| c.path.as_str()).collect();
    let mut roots: Vec<(String, usize, String)> = Vec::new();
    for &si in order {
        let s = &world.sessions[si];
        if !s.cwd.is_empty() && !roots.iter().any(|r| r.0 == s.cwd) {
            roots.push((s.cwd.clone(), identity(order, si), s.project().to_string()));
        }
    }
    let mut lots: Vec<Lot> = Vec::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    for &si in order {
        let s = &world.sessions[si];
        for (path, f) in &s.files {
            if now.saturating_sub(f.last()) > window {
                continue;
            }
            let i = *by_path.entry(path.clone()).or_insert_with(|| {
                lots.push(Lot {
                    path: path.clone(),
                    rel: String::new(),
                    reads: 0,
                    edits: 0,
                    added: 0,
                    removed: 0,
                    created: false,
                    last_read: 0,
                    last_edit: 0,
                    owner: si,
                    agent: 0,
                    sessions: Vec::new(),
                    collide: colliding.contains(path.as_str()),
                });
                lots.len() - 1
            });
            let l = &mut lots[i];
            if f.last() >= l.last() {
                l.owner = si;
                l.agent = f.last_agent;
            }
            l.reads += f.reads;
            l.edits += f.edits;
            l.added += f.added;
            l.removed += f.removed;
            l.created |= f.created;
            l.last_read = l.last_read.max(f.last_read);
            l.last_edit = l.last_edit.max(f.last_edit);
            if !l.sessions.contains(&si) {
                l.sessions.push(si);
            }
        }
    }
    let mut districts: Vec<District> = Vec::new();
    let mut by_root: HashMap<String, usize> = HashMap::new();
    for (i, l) in lots.iter_mut().enumerate() {
        let root = roots.iter().filter(|r| crate::parse::under(&l.path, &r.0).is_some()).max_by_key(|r| r.0.len());
        let (key, name, k) = match root {
            Some((root, k, name)) => (root.clone(), name.clone(), *k),
            None => (String::new(), "elsewhere".to_string(), usize::MAX),
        };
        l.rel = if key.is_empty() { tilde(&l.path) } else { crate::parse::under(&l.path, &key).unwrap_or(&l.path).to_string() };
        let d = *by_root.entry(key.clone()).or_insert_with(|| {
            districts.push(District { name, root: key, k, lots: Vec::new() });
            districts.len() - 1
        });
        districts[d].lots.push(i);
    }
    // The busiest districts first, "elsewhere" last.
    let heat = |d: &District| d.lots.iter().map(|&i| lots[i].last()).max().unwrap_or(0);
    districts.sort_by_key(|d| (d.root.is_empty(), std::cmp::Reverse(heat(d))));
    (districts, lots)
}

fn tilde(p: &str) -> String {
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

/// Where the last frame put each building, for the mouse and the arrow keys.
#[derive(Default)]
pub struct CityState {
    pub sel: Option<String>,
    pub hover: Option<String>,
    pub window: usize,
    /// Buildings in drawing order (back to front): screen box, path, global column, depth row.
    pub hits: Vec<(Rect, String, usize, usize)>,
    pub owner: HashMap<String, usize>,
}

impl CityState {
    pub fn new() -> CityState {
        CityState { window: 2, ..CityState::default() }
    }

    /// The front-most building under a cell.
    pub fn hit(&self, x: u16, y: u16) -> Option<String> {
        let p = ratatui::layout::Position { x, y };
        self.hits.iter().rev().find(|h| h.0.contains(p)).map(|h| h.1.clone())
    }

    /// Moves the selection one lot over (`dc` columns, `dd` rows deeper), to the nearest lot.
    pub fn step(&mut self, dc: i64, dd: i64) {
        let Some(cur) = self.sel.as_ref().and_then(|s| self.hits.iter().find(|h| &h.1 == s)).map(|h| (h.2 as i64, h.3 as i64)) else {
            self.sel = self.hits.last().map(|h| h.1.clone());
            return;
        };
        let want = (cur.0 + dc, cur.1 + dd);
        let better = self
            .hits
            .iter()
            .filter(|h| {
                let (c, d) = (h.2 as i64, h.3 as i64);
                (dc == 0 || (c - cur.0).signum() == dc.signum()) && (dd == 0 || (d - cur.1).signum() == dd.signum())
            })
            .min_by_key(|h| (h.2 as i64 - want.0).abs() * 3 + (h.3 as i64 - want.1).abs());
        if let Some(h) = better {
            self.sel = Some(h.1.clone());
        }
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &mut App, world: &World, order: &[usize], ins: &Insights) {
    let t = app.t();
    let now = world.now;
    let (wname, window) = WINDOWS[app.city.window.min(WINDOWS.len() - 1)];
    let (districts, lots) = gather(world, order, window, ins);
    let (city_area, side) = if area.width >= 120 {
        let [a, b] = Layout::horizontal([Constraint::Min(60), Constraint::Length(46)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };
    let edited = lots.iter().filter(|l| l.edits > 0).count();
    let title = Line::from(vec![
        Span::styled(" ◢ CODE CITY ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)),
        Span::styled(format!("· {} files, {edited} changed · {wname} ", lots.len()), Style::new().fg(t.faint)),
    ]);
    let right = Line::from(Span::styled(" height = lines changed · [ ] window ", Style::new().fg(t.faint)));
    let busy = lots.iter().any(|l| now - l.last_edit < 120_000);
    let inner = panel(f, city_area, t, title, Some(right), busy, false);
    app.city.hits.clear();
    app.city.owner = lots.iter().map(|l| (l.path.clone(), l.owner)).collect();
    if lots.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(Span::styled("No file read or changed in this window.", Style::new().fg(t.dim))),
            Line::from(Span::styled("Press ] for a longer window.", Style::new().fg(t.faint))),
        ])
        .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(msg, Rect { y: inner.y + inner.height / 2, height: 2.min(inner.height), ..inner });
        return;
    }
    if inner.height < 8 || inner.width < 20 {
        return;
    }
    // Taller screens get more rows of buildings, so more files fit and the sky is not empty.
    let depth: u16 = (inner.height.saturating_sub(12) / 5).clamp(2, 7);
    let ground = inner.bottom() - 4;
    let max_h = ground.saturating_sub(inner.y + 2 * depth + 2).max(2);

    // Every district gets a share of the width (at least two columns), in proportion to the
    // files it holds; the busiest come first when not all of them fit.
    let gap = 2 * depth + 4;
    let shown_districts = districts.len().min((inner.width / (2 * LOT_W + gap)).max(1) as usize);
    let room = inner.width.saturating_sub(gap * shown_districts as u16) / LOT_W;
    // Files outside every project matter least: they get a fifth of the width at most.
    let need: Vec<u16> = districts[..shown_districts]
        .iter()
        .map(|d| {
            let n = (d.lots.len() as u16).div_ceil(depth).max(1);
            if d.root.is_empty() && shown_districts > 1 { n.min(room / 5).max(2) } else { n }
        })
        .collect();
    let total: u16 = need.iter().sum();
    let mut cols: Vec<u16> = if total <= room {
        need.clone()
    } else {
        need.iter().map(|&n| ((n as u32 * room as u32 / total.max(1) as u32) as u16).clamp(1, n).max(2.min(n))).collect()
    };
    // Rounding leftovers go to the districts that are cut the most.
    while cols.iter().sum::<u16>() < room.min(total) {
        let Some(i) = (0..cols.len()).filter(|&i| cols[i] < need[i]).max_by_key(|&i| need[i] - cols[i]) else { break };
        cols[i] += 1;
    }
    while cols.iter().sum::<u16>() > room && cols.iter().any(|&c| c > 1) {
        let i = (0..cols.len()).max_by_key(|&i| cols[i]).unwrap_or(0);
        cols[i] -= 1;
    }
    let plan: Vec<(usize, u16)> = cols.into_iter().enumerate().collect();
    let used: u16 = plan.iter().map(|&(_, c)| c * LOT_W + gap).sum();
    let mut x = inner.x + inner.width.saturating_sub(used) / 2 + 1;
    let peak = lots.iter().map(|l| ((1 + l.added + l.removed) as f64).log2()).fold(1.0, f64::max);
    let scale = ((max_h - 1) as f64 / peak).min(2.2);
    let frame = app.frame();
    let mut gcol = 0usize;
    let sel = app.city.sel.clone().or_else(|| lots.iter().max_by_key(|l| l.last_edit).map(|l| l.path.clone()));
    app.city.sel = sel.clone();
    for &(d, cols) in &plan {
        let dist = &districts[d];
        // The hottest files fit; then they sit by folder, so a street is a directory.
        let mut mine: Vec<usize> = dist.lots.clone();
        mine.sort_by_key(|&i| std::cmp::Reverse(lots[i].last()));
        let hidden = mine.len().saturating_sub((cols * depth) as usize);
        mine.truncate((cols * depth) as usize);
        mine.sort_by(|&a, &b| lots[a].rel.cmp(&lots[b].rel));
        let base: Rgb = if dist.k == usize::MAX { (150, 150, 150) } else { t.towers[dist.k % t.towers.len()] };
        let block_w = cols * LOT_W + 2 * depth;
        // Street line and the district's name.
        let buf = f.buffer_mut();
        for cx in x..(x + block_w).min(inner.right()) {
            if let Some(c) = buf.cell_mut((cx, ground + 1)) {
                c.set_symbol("▔").set_fg(t.faint);
            }
        }
        let color = if dist.k == usize::MAX { t.dim } else { t.session(dist.k) };
        let name = clip(&dist.name, block_w as usize);
        let nx = x + block_w.saturating_sub(name.chars().count() as u16) / 2;
        buf.set_stringn(nx, ground + 2, &name, block_w as usize, Style::new().fg(color).add_modifier(Modifier::BOLD));
        let sub = format!("{} files{}", dist.lots.len(), if hidden > 0 { format!(" · {hidden} not shown") } else { String::new() });
        let sub = clip(&sub, block_w as usize);
        let sx = x + block_w.saturating_sub(sub.chars().count() as u16) / 2;
        buf.set_stringn(sx, ground + 3, &sub, block_w as usize, Style::new().fg(t.faint));
        // Back rows first, so the front ones hide what is behind them.
        let mut placed: Vec<(usize, u16, u16)> = mine.iter().enumerate().map(|(i, &li)| (li, i as u16 / depth, i as u16 % depth)).collect();
        placed.sort_by_key(|&(_, c, r)| (std::cmp::Reverse(r), c));
        for (li, c, r) in placed {
            let l = &lots[li];
            let x0 = x + c * LOT_W + 2 * r;
            let gy = ground - 2 * r;
            let h = if l.edits > 0 { (1.0 + ((1 + l.added + l.removed) as f64).log2() * scale).round() as u16 } else { 1 };
            let h = h.clamp(1, max_h);
            let picked = sel.as_deref() == Some(l.path.as_str());
            let hovered = app.city.hover.as_deref() == Some(l.path.as_str());
            let age = now - l.last_edit.max(l.last_read);
            let tint = if l.edits == 0 { mix(base, (110, 110, 110), 0.6) } else { base };
            let bright = match age {
                a if a < 120_000 => 1.25,
                a if a < 1_800_000 => 1.0,
                a if a < 6 * 3_600_000 => 0.7,
                _ => 0.45,
            };
            let reading = now - l.last_read < 600_000;
            let alarm = l.collide && (app.ms() / 300).is_multiple_of(2);
            let base_c = if alarm { (255, 70, 70) } else { shade(tint, bright) };
            building(f.buffer_mut(), x0, gy, h, base_c, reading, frame, li, t.shadow);
            let top = gy.saturating_sub(h + 1);
            let buf = f.buffer_mut();
            if l.collide {
                put(buf, x0 + 2, top.saturating_sub(1), "◆", if alarm { t.err } else { t.warn });
            } else if now - l.last_edit < 90_000 && (frame / 5).is_multiple_of(2) {
                put(buf, x0 + 2, top.saturating_sub(1), "●", t.accent);
            }
            if picked || hovered {
                put(buf, x0 + 2, top.saturating_sub(if l.collide || now - l.last_edit < 90_000 { 2 } else { 1 }), "▼", if picked { t.hi } else { t.dim });
            }
            let rect = Rect::new(x0, top, FACE_W + 2, h + 2).intersection(inner);
            app.city.hits.push((rect, l.path.clone(), gcol + c as usize, r as usize));
        }
        gcol += cols as usize;
        x += block_w + 4;
    }
    if shown_districts < districts.len() {
        let more = format!(" +{} districts off screen ", districts.len() - shown_districts);
        f.buffer_mut().set_string(city_area.x + 2, city_area.bottom().saturating_sub(2), more, Style::new().fg(t.faint));
    }
    // The building pointed at, named at the top.
    if let Some(l) = app.city.hover.as_ref().or(sel.as_ref()).and_then(|p| lots.iter().find(|l| &l.path == p)) {
        let names = names(world, order);
        let who = names.get(&l.owner).cloned().unwrap_or_default();
        let line = fit(
            vec![Span::styled(format!(" {} ", l.rel), Style::new().fg(t.hi).add_modifier(Modifier::BOLD))],
            format!("{} · {who}", touch_text(l, now)),
            Style::new().fg(t.dim),
            vec![],
            inner.width.saturating_sub(2) as usize,
        );
        f.render_widget(Paragraph::new(line), Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 1));
    }
    if let Some(side) = side {
        side_panel(f, side, app, world, order, &lots, ins, sel.as_deref());
    }
}

fn touch_text(l: &Lot, now: i64) -> String {
    if l.edits > 0 {
        format!("{} {} ago · +{} −{}", if l.created { "created" } else { "edited" }, fmt_dur(now - l.last_edit), fmt_n(l.added as u64), fmt_n(l.removed as u64))
    } else {
        format!("read {} ago", fmt_dur(now - l.last_read))
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, sym: &str, fg: Color) {
    if let Some(c) = buf.cell_mut((x, y)) {
        c.set_symbol(sym).set_fg(fg);
    }
}

/// An oblique box like the skyline towers, smaller: front face with a column of windows, a top
/// face one row up and one column right, a darker side, and a short shadow.
#[allow(clippy::too_many_arguments)]
fn building(buf: &mut Buffer, x0: u16, gy: u16, h: u16, base: Rgb, reading: bool, frame: usize, seed: usize, shadow: Color) {
    let (front, side, top, unlit) = (rgb(base), rgb(shade(base, 0.5)), rgb(shade(base, 1.35)), rgb(shade(base, 0.72)));
    let lit = rgb((255, 226, 140));
    let top_y = gy + 1 - h;
    let mut cell = |x: u16, y: u16, sym: &str, fg: Option<Color>, bg: Option<Color>| {
        if let Some(c) = buf.cell_mut((x, y)) {
            c.set_symbol(sym);
            if let Some(fg) = fg {
                c.set_fg(fg);
            }
            if let Some(bg) = bg {
                c.set_bg(bg);
            }
        }
    };
    for x in x0 + 1..=x0 + FACE_W {
        cell(x, top_y - 1, "▄", Some(top), None);
    }
    for y in top_y..=gy {
        for x in x0..x0 + FACE_W {
            if x == x0 + 1 && y < gy && h >= 2 {
                let on = reading && !(seed + y as usize + frame / 8).is_multiple_of(3);
                cell(x, y, "▪", Some(if on { lit } else { unlit }), Some(front));
            } else {
                cell(x, y, " ", None, Some(front));
            }
        }
        if y < gy {
            cell(x0 + FACE_W, y, " ", None, Some(side));
        } else {
            cell(x0 + FACE_W, y, "▀", Some(side), None);
        }
    }
    cell(x0 + FACE_W + 1, gy, "░", Some(shadow), None);
}

#[allow(clippy::too_many_arguments)]
fn side_panel(f: &mut Frame, area: Rect, app: &App, world: &World, order: &[usize], lots: &[Lot], ins: &Insights, sel: Option<&str>) {
    let t = app.t();
    let now = world.now;
    let names = names(world, order);
    let n_coll = ins.collisions.len() as u16;
    let [lot_area, coll_area, hot_area] =
        Layout::vertical([Constraint::Length(10), Constraint::Length(if n_coll > 0 { 3 + n_coll.min(4) } else { 0 }), Constraint::Min(4)]).areas(area);
    let inner = panel(f, lot_area, t, Line::from(Span::styled(" ◢ LOT ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD))), None, false, false);
    let kv = |k: &str, v: String, c: Color| Line::from(vec![Span::styled(format!("{k:<8} "), Style::new().fg(t.faint)), Span::styled(v, Style::new().fg(c))]);
    let mut lines = Vec::new();
    if let Some(l) = sel.and_then(|p| lots.iter().find(|l| l.path == p)) {
        let owner = names.get(&l.owner).cloned().unwrap_or_default();
        let s = &world.sessions[l.owner];
        let agent = if l.agent == 0 { "main".to_string() } else { s.agents.get(l.agent).map_or("agent".into(), |a| clip(&a.description, 24)) };
        lines.push(Line::from(Span::styled(clip(&l.rel, inner.width as usize), Style::new().fg(t.hi).add_modifier(Modifier::BOLD))));
        lines.push(kv(
            "last",
            format!("{} · {owner} · {agent}", if now - l.last() < 60_000 { "now".into() } else { format!("{} ago", fmt_dur(now - l.last())) }),
            t.session(identity(order, l.owner)),
        ));
        if l.edits > 0 {
            lines.push(kv(
                "changed",
                format!("{}× · +{} −{} lines{}", l.edits, fmt_n(l.added as u64), fmt_n(l.removed as u64), if l.created { " · created" } else { "" }),
                t.fg,
            ));
        }
        if l.reads > 0 {
            lines.push(kv("read", format!("{}× · {} ago", l.reads, fmt_dur(now - l.last_read)), t.fg));
        }
        let who: Vec<String> = l.sessions.iter().map(|si| names.get(si).cloned().unwrap_or_default()).collect();
        lines.push(kv("by", who.join(", "), t.dim));
        if l.collide {
            lines.push(kv("alarm", "changed by two at once".into(), t.err));
        }
        lines.push(Line::from(Span::styled("⏎ opens the session that touched it last", Style::new().fg(t.faint))));
    }
    f.render_widget(Paragraph::new(lines), inner);

    if n_coll > 0 {
        let title = Line::from(Span::styled(" ✖ COLLISIONS ", Style::new().fg(t.err).add_modifier(Modifier::BOLD)));
        let inner = panel(f, coll_area, t, title, None, true, false);
        let lines: Vec<Line> = ins
            .collisions
            .iter()
            .take(4)
            .map(|c| {
                let who: Vec<String> = c.sessions().iter().map(|si| names.get(si).cloned().unwrap_or_default()).collect();
                fit(
                    vec![Span::styled(format!("{} ", clip(crate::parse::file_name(&c.path), 18)), Style::new().fg(t.err))],
                    who.join(" + "),
                    Style::new().fg(t.fg),
                    vec![],
                    inner.width as usize,
                )
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    let title = Line::from(Span::styled(" ▲ MOST CHANGED ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)));
    let inner = panel(f, hot_area, t, title, None, false, false);
    let mut hot: Vec<&Lot> = lots.iter().filter(|l| l.edits > 0).collect();
    hot.sort_by_key(|l| std::cmp::Reverse(l.added + l.removed));
    let top = hot.first().map_or(1, |l| (l.added + l.removed).max(1));
    let w = inner.width as usize;
    let lines: Vec<Line> = hot
        .iter()
        .take(inner.height as usize)
        .map(|l| {
            let bar = ((l.added + l.removed) as f64 / top as f64 * 6.0).ceil() as usize;
            let color = t.session(identity(order, l.owner));
            let mut style = Style::new().fg(if sel == Some(l.path.as_str()) { t.hi } else { t.fg });
            if now - l.last_edit < 120_000 {
                style = style.add_modifier(Modifier::BOLD);
            }
            fit(
                vec![Span::styled(format!("{:<6} ", "▇".repeat(bar.min(6))), Style::new().fg(color))],
                crate::parse::file_name(&l.path).to_string(),
                style,
                vec![Span::styled(format!("+{} −{}", fmt_n(l.added as u64), fmt_n(l.removed as u64)), Style::new().fg(t.faint))],
                w,
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}
