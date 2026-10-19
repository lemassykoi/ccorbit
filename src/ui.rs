//! Terminal rendering: the boot screen, the wall of session cards with the signal feed, and the
//! live tree of one session. Redraws stay at 11 frames per second only while something moves.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

mod city;
pub(crate) mod vitals;

use crate::Opts;
use crate::engines::Engines;
use crate::hud::{HudAction, HudState};
use crate::insight::{self, Alert, Collision, Warn};
use crate::model::{Feed, Item, Kind, Phase, Session, Step};
use crate::parse::{clip, server_label, short_model};
use crate::source::World;
use crate::theme::{Rgb, THEMES, Theme, rgb, shade};
use crate::time::{fmt_dur, fmt_n};

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BARS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const CARD_W: u16 = 48;
const CARD_H: u16 = 8;
/// A card and its drop shadow.
const SLOT_H: u16 = CARD_H + 1;
const SKY_H: u16 = 13;
const TOWER_W: u16 = 7;
const TOWER_SLOT: u16 = 13;
const BOOT_MS: u128 = 1900;
const FRESH_MS: i64 = 1500;

const LOGO: [&str; 6] = [
    " ██████╗ ██████╗ ██████╗ ██████╗ ██████╗ ██╗████████╗",
    "██╔════╝██╔════╝██╔═══██╗██╔══██╗██╔══██╗██║╚══██╔══╝",
    "██║     ██║     ██║   ██║██████╔╝██████╔╝██║   ██║   ",
    "██║     ██║     ██║   ██║██╔══██╗██╔══██╗██║   ██║   ",
    "╚██████╗╚██████╗╚██████╔╝██║  ██║██████╔╝██║   ██║   ",
    " ╚═════╝ ╚═════╝ ╚═════╝ ╚═╝  ╚═╝╚═════╝ ╚═╝   ╚═╝   ",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Wall,
    Session,
    Hud,
    City,
}

struct App {
    started: Instant,
    /// A fixed animation clock in milliseconds, for tests.
    clock: Option<u128>,
    boot: bool,
    view: View,
    theme: usize,
    fx: bool,
    pick: usize,
    cols: usize,
    wall_scroll: usize,
    session: Option<String>,
    sel: usize,
    scroll: usize,
    page: usize,
    follow: bool,
    expanded: bool,
    quit: bool,
    /// Skyline tower heights, eased toward their target every frame.
    heights: HashMap<String, f64>,
    hud: HudState,
    city: city::CityState,
    /// Where the last frame drew each card (with its position in the order) and the tree rows.
    cards: Vec<(Rect, usize)>,
    tree_rows: Rect,
    last_click: Option<(Instant, u16, u16)>,
    mouse: bool,
    hover_card: Option<usize>,
    help: bool,
    toasts: Vec<Toast>,
    toast_seen: HashMap<String, i64>,
    waiting: HashSet<String>,
    toasts_ready: bool,
    /// Frames per second of the hologram.
    fps: u32,
    /// Warnings and collisions already announced, and compactions counted, per session.
    warned: HashSet<String>,
    compacted: HashMap<String, usize>,
    /// The local LLM engines listed with `--engines`, polled for a pulse in the footer.
    engines: Engines,
}

/// A short notice in the bottom right corner: an agent or workflow finished, a step failed, a
/// session needs you, a session started or ended.
struct Toast {
    born: Instant,
    icon: &'static str,
    text: String,
    color: Color,
    /// The same notice again shows once, with a count.
    count: u32,
}

impl Toast {
    fn new(icon: &'static str, text: String, color: Color) -> Toast {
        Toast { born: Instant::now(), icon, text, color, count: 1 }
    }
}

/// Adds a notice, or counts it on the same one already on screen, which stays without sliding in
/// again.
fn push_toast(toasts: &mut Vec<Toast>, toast: Toast) {
    match toasts.iter_mut().find(|t| t.text == toast.text && t.icon == toast.icon) {
        Some(same) => {
            same.count += 1;
            same.born = Instant::now().checked_sub(Duration::from_millis(300)).unwrap_or(same.born);
        }
        None => toasts.push(toast),
    }
}

impl App {
    fn new(opts: &Opts, replay: bool) -> App {
        App {
            started: Instant::now(),
            clock: None,
            boot: opts.boot,
            view: if opts.hud || opts.director {
                View::Hud
            } else if replay {
                View::Session
            } else {
                View::Wall
            },
            theme: opts.theme,
            fx: true,
            pick: 0,
            cols: 1,
            wall_scroll: 0,
            session: None,
            sel: 0,
            scroll: 0,
            page: 10,
            follow: true,
            expanded: false,
            quit: false,
            heights: HashMap::new(),
            hud: {
                let mut h = HudState::new();
                h.director_on(opts.director);
                h
            },
            city: city::CityState::new(),
            cards: Vec::new(),
            tree_rows: Rect::default(),
            last_click: None,
            mouse: true,
            hover_card: None,
            help: false,
            toasts: Vec::new(),
            toast_seen: HashMap::new(),
            waiting: HashSet::new(),
            toasts_ready: false,
            fps: opts.fps,
            warned: HashSet::new(),
            compacted: HashMap::new(),
            engines: opts.engines.as_deref().map(Engines::load).unwrap_or_else(Engines::none),
        }
    }

    fn t(&self) -> &'static Theme {
        &THEMES[self.theme]
    }

    fn ms(&self) -> u128 {
        self.clock.unwrap_or_else(|| self.started.elapsed().as_millis())
    }

    fn frame(&self) -> usize {
        (self.ms() / 90) as usize
    }
}

/// What a row of the session tree points at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowRef {
    Main,
    Item(usize, usize),
    Phase(usize, usize),
    WfAgent(usize, usize),
    /// A workflow agent followed live, with its own steps below it.
    Agent(usize),
}

#[derive(Clone, Copy)]
struct Row {
    depth: usize,
    r: RowRef,
}

struct Ctx<'a> {
    t: &'a Theme,
    now: i64,
    frame: usize,
    fx: bool,
    /// Warnings on running calls, by tool id, and the world for workflow races.
    marks: Option<&'a HashMap<String, String>>,
    world: Option<&'a World>,
}

/// Readings across all sessions, computed once per frame.
struct Insights {
    alerts: Vec<Alert>,
    collisions: Vec<Collision>,
    /// Short warning text by the tool id it is about.
    marks: HashMap<String, String>,
}

impl Insights {
    fn new(world: &World) -> Insights {
        let alerts = insight::alerts(world);
        let marks = alerts
            .iter()
            .map(|a| {
                let text = match a.warn {
                    Warn::Stall { factor, .. } => format!("⚠ {factor:.0}× usual"),
                    Warn::Loop { count } => format!("⚠ ×{count} in a row"),
                    Warn::Failing { count } => format!("⚠ failing ×{count}"),
                };
                (a.tool.clone(), text)
            })
            .collect();
        Insights { alerts, collisions: insight::collisions(world), marks }
    }

    fn of(&self, si: usize) -> impl Iterator<Item = &Alert> {
        self.alerts.iter().filter(move |a| a.si == si)
    }

    fn collides(&self, si: usize) -> Option<&Collision> {
        self.collisions.iter().find(|c| c.who.iter().any(|w| w.0 == si))
    }
}

pub fn run(term: &mut DefaultTerminal, mut world: World, opts: &Opts) -> io::Result<()> {
    let replay = world.replay_state().is_some();
    let mut app = App::new(opts, replay);
    let tick_every = Duration::from_millis(if replay { 80 } else { 250 });
    world.tick();
    let mut last_tick = Instant::now();
    let mut last_draw: Option<Instant> = None;
    let mut dirty = true;
    loop {
        if last_tick.elapsed() >= tick_every {
            dirty |= world.tick();
            dirty |= app.engines.poll();
            dirty |= collect_toasts(&mut app, &world);
            last_tick = Instant::now();
        }
        if !app.toasts.is_empty() {
            app.toasts.retain(|t| t.born.elapsed() < Duration::from_millis(5200));
        }
        if app.boot && app.ms() > BOOT_MS {
            app.boot = false;
            dirty = true;
        }
        let every = Duration::from_millis(if app.boot {
            30
        } else if app.view == View::Hud {
            // The sweep and the rings always turn: the hologram keeps its frame rate (30 by
            // default, `--fps`), and drops to 4 a second with effects off.
            if app.fx { 1000 / app.fps.max(1) as u64 } else { 250 }
        } else if world.animating() || !app.toasts.is_empty() {
            90
        } else {
            500
        });
        if dirty || last_draw.is_none_or(|t| t.elapsed() >= every) {
            term.draw(|f| draw(f, &mut app, &world))?;
            last_draw = Some(Instant::now());
            dirty = false;
        }
        let until_tick = tick_every.saturating_sub(last_tick.elapsed());
        let until_draw = every.saturating_sub(last_draw.map_or(Duration::ZERO, |t| t.elapsed()));
        let wait = until_tick.min(until_draw).max(Duration::from_millis(5));
        if event::poll(wait)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    on_key(&mut app, &mut world, k);
                    dirty = true;
                }
                Event::Mouse(m) => {
                    on_mouse(&mut app, &mut world, m);
                    // Pointer moves are drawn with the next frame; clicks and wheel right away.
                    dirty |= !matches!(m.kind, MouseEventKind::Moved);
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn draw(f: &mut Frame, app: &mut App, world: &World) {
    let t = app.t();
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(t.bg).fg(t.fg)), area);
    if app.boot {
        boot(f, area, app, world);
        return;
    }
    let [head, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(4), Constraint::Length(1)]).areas(area);
    let ins = Insights::new(world);
    header(f, head, app, world, &ins);
    let order = order(world);
    match app.view {
        View::Wall => wall(f, body, app, world, &order, &ins),
        View::Session => session_view(f, body, app, world, &order, &ins),
        View::City => city::draw(f, body, app, world, &order, &ins),
        View::Hud => {
            let names = names(world, &order);
            let engine = app.engines.blend_hit();
            app.hud.draw(f, body, t, app.fx, world, &order, &names, &ins.alerts, &ins.collisions, engine);
        }
    }
    footer(f, foot, app, world);
    toasts(f, body, app);
    if app.help {
        help(f, body, app);
    }
}

/// Display names: the project folder, plus the registry tag when two sessions share a folder.
fn names(world: &World, order: &[usize]) -> HashMap<usize, String> {
    let mut count: HashMap<&str, usize> = HashMap::new();
    for &si in order {
        *count.entry(world.sessions[si].project()).or_default() += 1;
    }
    order
        .iter()
        .map(|&si| {
            let s = &world.sessions[si];
            let p = s.project();
            (si, if count[p] > 1 { format!("{p}·{}", s.short_tag()) } else { p.to_string() })
        })
        .collect()
}

/// Position on the wall, which picks the session's identity color.
fn identity(order: &[usize], si: usize) -> usize {
    order.iter().position(|&i| i == si).unwrap_or(0)
}

/// Turns new notable events into toasts. The first pass only records where history ends.
fn collect_toasts(app: &mut App, world: &World) -> bool {
    let t = app.t();
    let order = order(world);
    let names = names(world, &order);
    let ready = app.toasts_ready;
    let mut fresh = Vec::new();
    for &si in &order {
        let s = &world.sessions[si];
        let name = names.get(&si).cloned().unwrap_or_default();
        let latest = s.feed.back().map_or(0, |f| f.at);
        let seen = match app.toast_seen.get(&s.id) {
            Some(&at) => at,
            None => {
                if ready && s.alive {
                    fresh.push(Toast::new("+", format!("{name} started"), t.session(identity(&order, si))));
                }
                latest
            }
        };
        for it in s.feed.iter().filter(|it| it.at > seen) {
            match (it.feed, it.kind) {
                // A shell command that fails is everyday business (a grep with no match, a test
                // run): the feed shows it; repeated failures raise their own warning.
                (Feed::Failed, Some(Kind::Shell)) => {}
                (Feed::Failed, _) => fresh.push(Toast::new("✗", format!("{} failed · {name}", it.label), t.err)),
                (Feed::Done, Some(Kind::Agent)) => fresh.push(Toast::new("◆", format!("agent done · {} · {name}", clip(&it.text, 28)), t.agent)),
                (Feed::Done, Some(Kind::Workflow)) => {
                    let race = it
                        .tool
                        .as_deref()
                        .and_then(|tool| s.step_by_tool(tool))
                        .and_then(|st| st.workflow)
                        .and_then(|w| insight::race(world, s, w))
                        .filter(|r| r.finished)
                        .map(|r| format!(" · {}", vitals::race_chip(&r).trim_start_matches("△ ")))
                        .unwrap_or_default();
                    fresh.push(Toast::new("△", format!("workflow done · {}{race} · {name}", clip(&it.text, 24)), t.workflow))
                }
                _ => {}
            }
        }
        app.toast_seen.insert(s.id.clone(), latest.max(seen));
        let n = s.compactions.len();
        let before = app.compacted.insert(s.id.clone(), n);
        if ready && before.is_some_and(|b| n > b) {
            let pre = s.compactions.last().map_or(0, |c| c.pre);
            fresh.push(Toast::new("⟳", format!("{name} compacted at {} tokens", fmt_n(pre)), t.accent));
        }
        let needs = s.needs_you();
        if needs && app.waiting.insert(s.id.clone()) && ready {
            fresh.push(Toast::new("⚑", format!("{name} needs you"), t.warn));
        } else if !needs {
            app.waiting.remove(&s.id);
        }
    }
    // Warnings and collisions, announced once each while they last.
    let mut live = HashSet::new();
    for a in insight::alerts(world) {
        let key = format!("{}:{}:{:?}", world.sessions[a.si].id, a.tool, std::mem::discriminant(&a.warn));
        if app.warned.insert(key.clone()) && ready {
            let name = names.get(&a.si).cloned().unwrap_or_default();
            fresh.push(Toast::new("⚠", format!("{} · {name}", a.short()), t.warn));
        }
        live.insert(key);
    }
    for c in insight::collisions(world) {
        let key = format!("collide:{}", c.path);
        if app.warned.insert(key.clone()) && ready {
            let who: Vec<String> = c.sessions().iter().map(|si| names.get(si).cloned().unwrap_or_default()).collect();
            let file = crate::parse::file_name(&c.path).to_string();
            fresh.push(Toast::new("✖", format!("collision · {file} · {}", who.join(" + ")), t.err));
        }
        live.insert(key);
    }
    app.warned.retain(|k| live.contains(k));
    app.toasts_ready = true;
    let changed = !fresh.is_empty();
    for toast in fresh {
        push_toast(&mut app.toasts, toast);
    }
    let extra = app.toasts.len().saturating_sub(4);
    app.toasts.drain(..extra);
    changed
}

fn toasts(f: &mut Frame, area: Rect, app: &App) {
    let t = app.t();
    let w = 50.min(area.width.saturating_sub(4));
    for (k, toast) in app.toasts.iter().rev().enumerate() {
        let age = toast.born.elapsed().as_secs_f64();
        // Slides in from the right, fades in its last second.
        let slide = ((0.25 - age).max(0.0) / 0.25 * w as f64) as u16;
        // Stacked upward from the bottom right, over the oldest information on screen.
        let Some(y) = area.bottom().checked_sub(2 + k as u16).filter(|&y| y > area.y) else { break };
        // The whole notice slides; the screen's edge cuts what is not in yet.
        let x = area.right().saturating_sub(w + 2) + slide;
        let rect = Rect::new(x, y, w, 1).intersection(area);
        if rect.is_empty() {
            continue;
        }
        let old = age > 4.2;
        let count = if toast.count > 1 {
            vec![Span::styled(format!(" ×{} ", toast.count), Style::new().fg(toast.color).add_modifier(Modifier::BOLD))]
        } else {
            vec![]
        };
        let line = fit(
            vec![
                Span::styled("▌", Style::new().fg(toast.color)),
                Span::styled(format!(" {} ", toast.icon), Style::new().fg(toast.color).add_modifier(Modifier::BOLD)),
            ],
            toast.text.clone(),
            Style::new().fg(if old { t.dim } else { t.hi }),
            count,
            w as usize,
        );
        f.render_widget(Paragraph::new(line).style(Style::new().bg(t.surface)), rect);
    }
}

fn help(f: &mut Frame, area: Rect, app: &App) {
    let t = app.t();
    let w = 102.min(area.width.saturating_sub(4));
    let h = 25.min(area.height.saturating_sub(2));
    let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(ratatui::widgets::Clear, rect);
    let inner = panel(f, rect, t, Line::from(Span::styled(" ? HELP · any key closes ", Style::new().fg(t.hi).add_modifier(Modifier::BOLD))), None, true, true);
    let head = |s: &str| Span::styled(format!("{s:<8}"), Style::new().fg(t.accent).add_modifier(Modifier::BOLD));
    let k = |s: &str| Span::styled(s.to_string(), Style::new().fg(t.hi).add_modifier(Modifier::BOLD));
    let d = |s: &str| Span::styled(s.to_string(), Style::new().fg(t.dim));
    let c = |s: &str, color: Color| Span::styled(s.to_string(), Style::new().fg(color).add_modifier(Modifier::BOLD));
    let lines = vec![
        Line::from(vec![
            head("VIEWS"),
            k("wall"),
            d(" start   "),
            k("⏎"),
            d(" session tree   "),
            k("g"),
            d(" HUD hologram   "),
            k("c"),
            d(" code city   "),
            k("esc"),
            d(" back"),
        ]),
        Line::from(vec![head("MOVE"), k("←↑↓→"), d(" or "), k("hjkl"), d(" select   "), k("tab"), d(" next session or target   "), k("⏎"), d(" open or fold")]),
        Line::from(vec![head(""), k("e"), d(" expand all   "), k("f"), d(" follow newest, or fly to target   "), k("0"), d(" reset the HUD view")]),
        Line::from(vec![head("LOOK"), k("t"), d(" theme   "), k("x"), d(" effects   "), k("m"), d(" mouse off, to select text   "), k("q"), d(" quit")]),
        Line::from(vec![head("MOUSE"), d("click selects · click again opens · wheel scrolls or zooms · drag pans")]),
        Line::from(vec![
            head("HUD"),
            k("drag"),
            d(" turn   "),
            k("v"),
            d(" flat or 3D   "),
            k("d"),
            d(" director (the camera runs itself)   "),
            k("space"),
            d(" ping"),
        ]),
        Line::from(vec![head("CITY"), k("[ ]"), d(" time window   "), k("⏎"), d(" open the session that touched the file last")]),
        Line::from(vec![head("REPLAY"), k("p"), d(" pause   "), k("<"), d(" "), k(">"), d(" slower, faster")]),
        Line::raw(""),
        Line::from(vec![
            head("STEPS"),
            c("⠋", t.warn),
            d(" running  "),
            c("⠋", t.workflow),
            d(" background  "),
            c("✓", t.ok),
            d(" done  "),
            c("✗", t.err),
            d(" failed  "),
            c("■", t.dim),
            d(" stopped  "),
            c("◌", t.faint),
            d(" never finished"),
        ]),
        Line::from(vec![
            head("CARDS"),
            c("◆", t.agent),
            d(" agents  "),
            c("↻", t.workflow),
            d(" background tasks  "),
            c("✦", t.skill),
            d(" skills  "),
            c("⇄", t.mcp),
            d(" MCP servers  "),
            c("⚑", t.warn),
            d(" waiting for you"),
        ]),
        Line::from(vec![
            head("FLOW"),
            c("⇡", t.accent),
            d(" sent per minute, cache included  "),
            c("⇣", t.hi),
            d(" generated  "),
            c("██░", t.fg),
            d(" context vs ceiling  "),
            c("⟳~12", t.warn),
            d(" calls left"),
        ]),
        Line::from(vec![
            head("VITALS"),
            c("⠔⠉⠢", t.comms),
            d(" thinking  "),
            c("⠔⠉⠢", t.hi),
            d(" writing  "),
            c("⠔⠉⠢", t.shell),
            d(" tools  "),
            c("◉ 97%", t.ok),
            d(" cache hits  "),
            c("✺", t.accent),
            d(" cache rebuilt  "),
            c("≈$", t.dim),
            d(" API-equivalent price"),
        ]),
        Line::from(vec![
            head("ALERTS"),
            c("⚠", t.warn),
            d(" call far slower than usual, or repeated  "),
            c("✖", t.err),
            d(" one file, two sessions  "),
            c("△", t.workflow),
            d(" race vs an earlier run"),
        ]),
        Line::from(vec![
            head("GRAPH"),
            c("◎", t.hi),
            d(" core  "),
            c("○", t.fg),
            d(" session  "),
            c("◇", t.agent),
            d(" agent  "),
            c("△", t.workflow),
            d(" workflow  "),
            c("⬡", t.mcp),
            d(" MCP server  "),
            c("✳", t.skill),
            d(" skill"),
        ]),
        Line::from(vec![
            head(""),
            c("•", t.prompt),
            d(" prompt in  "),
            c("•", t.agent),
            d(" call out  "),
            c("•", t.ok),
            d(" result back  "),
            c("•", t.err),
            d(" failed  "),
            c("•", t.accent),
            d(" token traffic"),
        ]),
        Line::raw(""),
        Line::from(vec![head("COLORS"), d("a session keeps one color on its card, tower, feed lines and HUD node")]),
        Line::from(vec![head("DATA"), d("read-only: opencode sessions, messages, parts · never writes")]),
    ];
    let lines: Vec<Line> = lines.into_iter().map(|l| l.patch_style(Style::new())).collect();
    f.render_widget(Paragraph::new(lines), Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner });
}

/// Sessions in a stable order, running ones first, so cards do not jump around.
fn order(world: &World) -> Vec<usize> {
    let mut v: Vec<usize> = (0..world.sessions.len()).collect();
    v.sort_by_key(|&i| (!world.sessions[i].alive, i));
    v
}

fn current_session(app: &App, world: &World, order: &[usize]) -> Option<usize> {
    app.session.as_ref().and_then(|id| world.sessions.iter().position(|s| &s.id == id)).or_else(|| order.first().copied())
}

// ---------------------------------------------------------------- boot

fn boot(f: &mut Frame, area: Rect, app: &App, world: &World) {
    let t = app.t();
    let ms = app.ms();
    let logo_w = LOGO[0].chars().count() as u16;
    let x = area.x + area.width.saturating_sub(logo_w) / 2;
    let y = area.y + area.height.saturating_sub(13) / 2;
    let reveal = ms as f64 / 650.0;
    for (i, l) in LOGO.iter().enumerate() {
        let chars: Vec<char> = l.chars().collect();
        let n = ((reveal - i as f64 * 0.08).clamp(0.0, 1.0) * chars.len() as f64) as usize;
        let mut spans = vec![Span::styled(chars[..n].iter().collect::<String>(), Style::new().fg(t.grad(i as f64 / 5.0)).add_modifier(Modifier::BOLD))];
        if n < chars.len() && n > 0 {
            spans.push(Span::styled("▓▒░", Style::new().fg(t.dim)));
        }
        let r = Rect::new(x, y + i as u16, logo_w, 1).intersection(area);
        if !r.is_empty() {
            f.render_widget(Paragraph::new(Line::from(spans)), r);
        }
    }
    let live = world.sessions.iter().filter(|s| s.alive).count();
    let agents: usize = world.sessions.iter().map(|s| s.agents.len()).sum();
    let mcp: usize = world.sessions.iter().map(|s| s.mcp.len()).sum();
    let db = world.db_path().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "storage".into());
    let lines = [
        format!(
            "{} TERMINAL v{}  ·  READ-ONLY  ·  {}",
            t.name.to_uppercase(),
            env!("CARGO_PKG_VERSION"),
            if app.engines.list.is_empty() { "NO NETWORK" } else { "ENGINE PULSE ON" }
        ),
        format!("> reading {db} .............. {live} live"),
        format!("> following parts .............. {agents} agents"),
        format!("> mcp connections seen ............... {mcp}"),
        "> ready".to_string(),
    ];
    let mut budget = (ms.saturating_sub(650) / 7) as usize;
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let lx = area.x + area.width.saturating_sub(width) / 2;
    for (k, l) in lines.iter().enumerate() {
        let len = l.chars().count();
        let n = budget.min(len);
        budget -= n;
        let mut spans = vec![Span::styled(l.chars().take(n).collect::<String>(), Style::new().fg(if k == 0 { t.accent } else { t.fg }))];
        let typing = n < len && (n > 0 || k == 0);
        let last_done = k == lines.len() - 1 && n == len;
        if (typing || last_done) && (ms / 250).is_multiple_of(2) {
            spans.push(Span::styled("█", Style::new().fg(t.hi)));
        }
        let r = Rect::new(lx, y + 7 + k as u16 + (k > 0) as u16, width + 1, 1).intersection(area);
        if !r.is_empty() {
            f.render_widget(Paragraph::new(Line::from(spans)), r);
        }
        if n < len {
            break;
        }
    }
}

// ---------------------------------------------------------------- header and footer

fn header(f: &mut Frame, area: Rect, app: &App, world: &World, ins: &Insights) {
    let t = app.t();
    let live = world.sessions.iter().filter(|s| s.alive).count();
    let busy = world.sessions.iter().filter(|s| s.busy()).count();
    let agents: usize = world.sessions.iter().map(Session::running_agents).sum();
    let mut left = pill(" ◆ CCORBIT ", t);
    left.push(Span::raw(" "));
    match world.replay_state() {
        Some(r) => left.push(Span::styled(
            format!(
                "{} REPLAY ×{} {:>3}% ",
                if r.paused {
                    "‖"
                } else if r.done() {
                    "■"
                } else {
                    "▶"
                },
                r.speed,
                (r.progress() * 100.0) as u32
            ),
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        )),
        None if world.is_demo() => {
            left.push(Span::styled(" DEMO ", Style::new().fg(t.bg).bg(t.accent).add_modifier(Modifier::BOLD)));
            left.push(Span::styled(" made-up sessions ", Style::new().fg(t.accent)));
        }
        None => {
            let on = busy == 0 || (app.ms() / 500).is_multiple_of(2);
            left.push(Span::styled(if on { "● LIVE " } else { "○ LIVE " }, Style::new().fg(t.ok).add_modifier(Modifier::BOLD)));
        }
    }
    left.push(Span::styled(format!(" {live} sessions · {busy} busy · {agents} agents running  "), Style::new().fg(t.fg)));
    let waiting = world.sessions.iter().filter(|s| s.needs_you()).count();
    if waiting > 0 {
        let on = (app.ms() / 500).is_multiple_of(2);
        left.push(Span::styled(
            format!(" ⚑ {waiting} need{} you ", if waiting == 1 { "s" } else { "" }),
            Style::new().fg(t.bg).bg(if on { t.warn } else { t.accent }).add_modifier(Modifier::BOLD),
        ));
        left.push(Span::raw("  "));
    }
    if !ins.collisions.is_empty() {
        let on = (app.ms() / 400).is_multiple_of(2);
        let n = ins.collisions.len();
        left.push(Span::styled(
            format!(" ✖ {n} collision{} ", if n == 1 { "" } else { "s" }),
            Style::new().fg(t.bg).bg(if on { t.err } else { t.warn }).add_modifier(Modifier::BOLD),
        ));
        left.push(Span::raw("  "));
    }
    let up: u64 = world.sessions.iter().map(|s| s.tok_up.per_sec(world.now)).sum();
    let down: u64 = world.sessions.iter().map(|s| s.tok_down.per_sec(world.now)).sum();
    left.push(Span::styled(format!("⇡ {}/s ", fmt_n(up)), Style::new().fg(t.accent)));
    left.push(Span::styled(format!("⇣ {}/s ", fmt_n(down)), Style::new().fg(t.hi)));
    let rate: u64 = world.sessions.iter().filter(|s| s.alive).map(|s| vitals::money_rate(s, world.now)).sum();
    if rate > 0 {
        left.push(Span::styled(format!("{}/h", vitals::money(rate)), Style::new().fg(t.dim)));
    }
    // The activity sparkline takes what the left side leaves, up to 24 buckets (2 minutes).
    let lw: usize = left.iter().map(Span::width).sum();
    let n = (area.width as usize).saturating_sub(lw + 11).min(24);
    let mut right = Vec::new();
    if n >= 6 {
        let mut total = vec![0u32; n];
        for s in &world.sessions {
            for (k, v) in s.activity.series(world.now, n).into_iter().enumerate() {
                total[k] += v;
            }
        }
        right.push(Span::styled("activity ", Style::new().fg(t.faint)));
        right.extend(sparkline(&total, t));
        right.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(fit(left, String::new(), Style::new(), right, area.width as usize)), area);
}

fn footer(f: &mut Frame, area: Rect, app: &App, world: &World) {
    let t = app.t();
    let keys: &[(&str, &str)] = match app.view {
        View::Wall => &[("←↑↓→", "select"), ("⏎", "open"), ("g", "hud"), ("c", "city"), ("t", "theme"), ("x", "effects"), ("q", "quit")],
        View::Hud => &[("tab", "target"), ("⏎", "open"), ("+-", "zoom"), ("v", "flat/3D"), ("d", "director"), ("0", "reset"), ("esc", "wall"), ("q", "quit")],
        View::City => &[("←↑↓→", "move"), ("⏎", "open session"), ("[ ]", "window"), ("g", "hud"), ("esc", "wall"), ("q", "quit")],
        View::Session => &[
            ("↑↓", "move"),
            ("⏎", "fold"),
            ("e", "expand all"),
            ("f", "follow"),
            ("tab", "next"),
            ("g", "hud"),
            ("esc", "wall"),
            ("t", "theme"),
            ("q", "quit"),
        ],
    };
    let mut spans = Vec::new();
    for (k, v) in keys {
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(t.bg).bg(t.dim)));
        spans.push(Span::styled(format!(" {v}  "), Style::new().fg(t.dim)));
    }
    if world.replay_state().is_some() {
        spans.push(Span::styled(" p ", Style::new().fg(t.bg).bg(t.dim)));
        spans.push(Span::styled(" pause  ", Style::new().fg(t.dim)));
        spans.push(Span::styled(" <> ", Style::new().fg(t.bg).bg(t.dim)));
        spans.push(Span::styled(" speed  ", Style::new().fg(t.dim)));
    }
    let mut right = Vec::new();
    // The engines the person listed with --engines: one short pulse line each, on the right.
    for strip in app.engines.strips() {
        right.push(Span::styled(format!("{strip}  "), Style::new().fg(t.accent)));
    }
    right.push(Span::styled(" ? ", Style::new().fg(t.bg).bg(t.accent)));
    right.push(Span::styled(" help  ", Style::new().fg(t.dim)));
    right.push(Span::styled(format!("{} ", t.name), Style::new().fg(t.faint)));
    f.render_widget(Paragraph::new(fit(spans, String::new(), Style::new(), right, area.width as usize)), area);
}

// ---------------------------------------------------------------- wall

fn wall(f: &mut Frame, area: Rect, app: &mut App, world: &World, order: &[usize], ins: &Insights) {
    if order.is_empty() {
        no_signal(f, area, app, "No running opencode session.", "Start `opencode` in any project: it shows up here within a couple of seconds.");
        return;
    }
    let cols = (area.width / CARD_W).max(1) as usize;
    let card_w = area.width / cols as u16;
    let want = order.len().div_ceil(cols) as u16 * SLOT_H;
    // The skyline and the feed take what the cards leave, when the screen is tall enough.
    let sky_h = if area.height >= want.min(2 * SLOT_H) + SKY_H + 5 { SKY_H } else { 0 };
    let room = area.height.saturating_sub(sky_h + 5 * u16::from(sky_h > 0));
    let grid_h = (want.min(room) / SLOT_H * SLOT_H).max(SLOT_H.min(area.height));
    let [grid, sky, feed] = Layout::vertical([Constraint::Length(grid_h), Constraint::Length(sky_h), Constraint::Min(0)]).areas(area);
    let rows_vis = (grid.height / SLOT_H).max(1) as usize;
    app.cols = cols;
    app.pick = app.pick.min(order.len() - 1);
    let pick_row = app.pick / cols;
    if pick_row < app.wall_scroll {
        app.wall_scroll = pick_row;
    } else if pick_row >= app.wall_scroll + rows_vis {
        app.wall_scroll = pick_row + 1 - rows_vis;
    }
    app.cards.clear();
    let names = names(world, order);
    for (k, &si) in order.iter().enumerate() {
        let (r, c) = (k / cols, k % cols);
        if r < app.wall_scroll || r >= app.wall_scroll + rows_vis {
            continue;
        }
        // A partial last row is centered.
        let in_row = (order.len() - r * cols).min(cols) as u16;
        let shift = (cols as u16 - in_row) * card_w / 2;
        let slot = Rect::new(grid.x + shift + c as u16 * card_w, grid.y + (r - app.wall_scroll) as u16 * SLOT_H, card_w, SLOT_H).intersection(grid);
        app.cards.push((slot, k));
        let name = names.get(&si).map_or("", String::as_str);
        card(f, slot, app, world, si, k, name, k == app.pick, app.hover_card == Some(k), ins);
    }
    if sky_h > 0 {
        skyline(f, sky, app, world, order);
    }
    if feed.height >= 4 {
        signal_feed(f, feed, app, world, order);
    }
}

#[allow(clippy::too_many_arguments)]
fn card(f: &mut Frame, rect: Rect, app: &App, world: &World, si: usize, k: usize, name: &str, selected: bool, hovered: bool, ins: &Insights) {
    let s = &world.sessions[si];
    let t = app.t();
    let cx = Ctx { t, now: world.now, frame: app.frame(), fx: app.fx, marks: Some(&ins.marks), world: Some(world) };
    let (busy, needs) = (s.busy(), s.needs_you());
    let me = t.session(k);
    let blink = (app.ms() / 500).is_multiple_of(2);
    let dot = if !s.alive {
        Span::styled("○", Style::new().fg(t.faint))
    } else if needs {
        Span::styled("⚑", Style::new().fg(t.warn).add_modifier(Modifier::BOLD))
    } else if busy {
        Span::styled(SPIN[cx.frame % SPIN.len()], Style::new().fg(me))
    } else {
        Span::styled("●", Style::new().fg(me))
    };
    let pill = if needs {
        Span::styled(" NEEDS YOU ", Style::new().fg(t.bg).bg(if blink { t.warn } else { t.accent }).add_modifier(Modifier::BOLD))
    } else if !s.alive {
        Span::styled(" ENDED ", Style::new().fg(t.dim))
    } else if busy {
        Span::styled(" BUSY ", Style::new().fg(t.bg).bg(t.ok).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(format!(" {} ", s.status.to_uppercase()), Style::new().fg(t.bg).bg(t.faint))
    };
    let title = Line::from(vec![
        Span::raw(" "),
        dot,
        Span::raw(" "),
        Span::styled(clip(name, rect.width.saturating_sub(18) as usize), Style::new().fg(if s.alive { me } else { t.dim }).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ]);
    let right = Line::from(vec![pill, Span::raw(" ")]);
    let inner = panel(f, rect, t, title, Some(right), selected || hovered || busy || needs, selected);
    let w = inner.width as usize;
    let mut lines = Vec::new();

    let title = if s.title.is_empty() { s.name.clone() } else { s.title.clone() };
    lines.push(fit(vec![], title, Style::new().fg(t.dim), vec![], w));

    let answering = if s.current().is_none() { s.phase_now().filter(|(p, _)| p.is_none()).map(|(_, since)| since) } else { None };
    lines.push(match (answering, s.current().or_else(|| s.last_step())) {
        (Some(since), _) => {
            // No call running yet the session works: the model is producing its next block.
            let left = vec![
                Span::styled(SPIN[cx.frame % SPIN.len()], Style::new().fg(t.comms)),
                Span::raw(" "),
                Span::styled(format!("{:<10} ", "model"), Style::new().fg(t.comms).add_modifier(Modifier::BOLD)),
            ];
            let right = vec![Span::styled(fmt_dur(cx.now - since), Style::new().fg(t.faint))];
            let text = "thinking or writing the next step";
            if app.fx { fit_shimmer(left, text, cx.frame, t.dim, t.comms, right, w) } else { fit(left, text.into(), Style::new().fg(t.dim), right, w) }
        }
        (None, Some((a, st))) => {
            let (label, text) = st.label();
            let mut left = Vec::new();
            if a != 0 {
                left.push(Span::styled("↳ ", Style::new().fg(t.agent)));
            }
            left.push(glyph(s, st, &cx));
            left.push(Span::raw(" "));
            left.push(Span::styled(format!("{} ", clip(&label, 10)), Style::new().fg(t.kind(st.kind)).add_modifier(Modifier::BOLD)));
            let running = st.running() && s.alive;
            let end = st.finished().unwrap_or(if s.alive { cx.now } else { s.last_at });
            let right = if running { fmt_dur(end - st.started) } else { format!("{} ago", fmt_dur(cx.now - end)) };
            let right = vec![Span::styled(right, Style::new().fg(t.faint))];
            if running && app.fx {
                fit_shimmer(left, &text, cx.frame, t.fg, t.hi, right, w)
            } else {
                fit(left, text, Style::new().fg(if running { t.fg } else { t.dim }), right, w)
            }
        }
        (None, None) => fit(vec![], "waiting for the first step".into(), Style::new().fg(t.faint), vec![], w),
    });

    // Only what exists, in words: "◆ 2 running of 11  ↻ 1 bg  ✦ 3 skills  ⇄ Notion +1".
    let total = s.agents.len() - 1;
    let running = s.running_agents();
    let mut chips: Vec<Span<'static>> = Vec::new();
    let mut chip = |text: String, color: Color| {
        if !chips.is_empty() {
            chips.push(Span::raw("  "));
        }
        chips.push(Span::styled(text, Style::new().fg(color)));
    };
    if let Some(c) = ins.collides(si) {
        chip(format!("✖ {}", crate::parse::file_name(&c.path)), t.err);
    }
    let mut warns = ins.of(si);
    if let Some(a) = warns.next() {
        let more = warns.count();
        chip(format!("⚠ {}{}", a.short(), if more > 0 { format!(" +{more}") } else { String::new() }), t.warn);
    }
    let race = (0..s.workflows.len())
        .rev()
        .filter(|&w| s.workflows[w].step.as_deref().and_then(|x| s.step_by_tool(x)).is_some_and(|x| x.running()))
        .find_map(|w| insight::race(world, s, w));
    if let Some(r) = race.filter(|_| s.alive) {
        chip(vitals::race_chip(&r), t.workflow);
    }
    if running > 0 {
        chip(format!("◆ {running} running of {total}"), t.agent);
    } else if total > 0 {
        chip(format!("◆ {total} agent{}", if total == 1 { "" } else { "s" }), t.agent);
    }
    let bg = s.background_running();
    if bg > 0 {
        chip(format!("↻ {bg} bg"), t.workflow);
    }
    if !s.skills.is_empty() {
        chip(format!("✦ {} skill{}", s.skills.len(), if s.skills.len() == 1 { "" } else { "s" }), t.skill);
    }
    let servers: Vec<&str> = s.mcp.keys().map(|k| server_label(k)).collect();
    let more = servers.len().saturating_sub(2);
    let servers_text = match servers.len() {
        0 => String::new(),
        _ => format!("⇄ {}{}", servers[..servers.len().min(2)].join(", "), if more > 0 { format!(" +{more}") } else { String::new() }),
    };
    if chips.is_empty() && servers_text.is_empty() {
        lines.push(fit(vec![], "no agent, skill or server used yet".into(), Style::new().fg(t.faint), vec![], w));
    } else {
        if !chips.is_empty() && !servers_text.is_empty() {
            chips.push(Span::raw("  "));
        }
        lines.push(fit(chips, servers_text, Style::new().fg(t.mcp), vec![], w));
    }
    let fc = insight::forecast(world, s);
    lines.push(flow_line(s, &cx, &fc, w));

    // The brainwave: what the session spends its time on, then the cache reactor and the price.
    let mut right = vec![Span::raw(" ")];
    right.extend(vitals::reactor(s, cx.now, app.ms(), t, app.engines.blend_hit()));
    right.push(Span::styled(format!(" {} {}", vitals::money(s.cost_micros), short_model(&s.agents[0].model)), Style::new().fg(t.faint)));
    let rw: usize = right.iter().map(Span::width).sum();
    let wave_w = w.saturating_sub(rw).min(64);
    let mut wave = vec![Span::raw(" ".repeat(w.saturating_sub(rw + wave_w)))];
    wave.extend(vitals::brainwave(s, cx.now, cx.frame, wave_w, None, t));
    wave.extend(right);
    lines.push(Line::from(wave));

    let prompt = s.last_prompt().unwrap_or("").to_string();
    lines.push(fit(vec![Span::styled("› ", Style::new().fg(t.prompt))], prompt, Style::new().fg(t.prompt), vec![], w));

    f.render_widget(Paragraph::new(lines), inner);
}

/// The latest events of every session, newest at the bottom, like a log.
fn signal_feed(f: &mut Frame, area: Rect, app: &App, world: &World, order: &[usize]) {
    let t = app.t();
    let cx = Ctx { t, now: world.now, frame: app.frame(), fx: app.fx, marks: None, world: None };
    let inner = panel(
        f,
        area,
        t,
        Line::from(Span::styled(" ▸ SIGNAL FEED ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD))),
        Some(Line::from(Span::styled(" all sessions ", Style::new().fg(t.faint)))),
        false,
        false,
    );
    let h = inner.height as usize;
    let mut items: Vec<(usize, &crate::model::FeedItem)> = order.iter().flat_map(|&si| world.sessions[si].feed.iter().map(move |it| (si, it))).collect();
    items.sort_by_key(|(_, it)| it.at);
    let start = items.len().saturating_sub(h);
    let w = inner.width as usize;
    let names = names(world, order);
    let lines: Vec<Line> = items[start..]
        .iter()
        .map(|&(si, it)| {
            let s = &world.sessions[si];
            let me = t.session(identity(order, si));
            let name = names.get(&si).map_or(s.project(), String::as_str);
            let fresh = cx.fx && cx.now - it.at < FRESH_MS && cx.now >= it.at;
            let mut left = vec![
                Span::styled(format!("{:>7} ", fmt_dur(cx.now - it.at)), Style::new().fg(t.faint)),
                Span::styled("▌", Style::new().fg(me)),
                Span::styled(format!("{:<16} ", clip(name, 16)), Style::new().fg(me).add_modifier(if fresh { Modifier::BOLD } else { Modifier::empty() })),
            ];
            let step = it.tool.as_deref().and_then(|id| s.step_by_tool(id));
            match (it.feed, step) {
                (Feed::Prompt, _) => left.push(Span::styled("›", Style::new().fg(t.prompt))),
                (Feed::Answer, _) => left.push(Span::styled("◂", Style::new().fg(t.fg))),
                (Feed::Start, Some(st)) => left.push(glyph(s, st, &cx)),
                (Feed::Failed, _) => left.push(Span::styled("✗", Style::new().fg(t.err))),
                _ => left.push(Span::styled("✓", Style::new().fg(t.ok))),
            }
            left.push(Span::raw(" "));
            if let Some(k) = it.kind {
                let verb = match it.feed {
                    Feed::Done => " done",
                    Feed::Failed => " failed",
                    _ => "",
                };
                left.push(Span::styled(
                    format!("{:<10} ", clip(&format!("{}{verb}", it.label), 10)),
                    Style::new().fg(if fresh { t.hi } else { t.kind(k) }).add_modifier(Modifier::BOLD),
                ));
            }
            let style = match it.feed {
                Feed::Prompt => Style::new().fg(t.prompt),
                Feed::Answer => Style::new().fg(t.fg),
                _ if fresh => Style::new().fg(t.hi),
                _ => Style::new().fg(t.dim),
            };
            fit(left, it.text.clone(), style, vec![], w)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn no_signal(f: &mut Frame, area: Rect, app: &App, msg: &str, hint: &str) {
    let t = app.t();
    let on = (app.ms() / 600).is_multiple_of(2);
    let lines = vec![
        Line::from(Span::styled("▓▒░   N O   S I G N A L   ░▒▓", Style::new().fg(if on { t.hi } else { t.dim }).add_modifier(Modifier::BOLD))),
        Line::raw(""),
        Line::from(Span::styled(msg.to_string(), Style::new().fg(t.fg))),
        Line::from(Span::styled(hint.to_string(), Style::new().fg(t.dim))),
    ];
    let y = area.y + area.height.saturating_sub(4) / 2;
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), Rect::new(area.x, y, area.width, 4.min(area.height)));
}

// ---------------------------------------------------------------- session view

fn session_view(f: &mut Frame, area: Rect, app: &mut App, world: &World, order: &[usize], ins: &Insights) {
    let Some(si) = current_session(app, world, order) else {
        no_signal(f, area, app, "No session to show.", "Press esc to go back to the wall.");
        return;
    };
    let s = &world.sessions[si];
    let (tree_area, detail_area, links_area) = if area.width >= 110 {
        let [l, r] = Layout::horizontal([Constraint::Min(60), Constraint::Length(46)]).areas(area);
        let [d, k] = Layout::vertical([Constraint::Min(8), Constraint::Length(12)]).areas(r);
        (l, d, k)
    } else {
        let [top, bottom] = Layout::vertical([Constraint::Min(6), Constraint::Length(9)]).areas(area);
        let [k, d] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(bottom);
        (top, d, k)
    };
    let vitals_h = if tree_area.height >= 30 { 9 } else { 7 };
    let [flow_area, tree_area] = Layout::vertical([Constraint::Length(vitals_h), Constraint::Min(3)]).areas(tree_area);
    vitals_panel(f, flow_area, app, world, s);
    let rows = rows(s);
    let me = t_session(app, order, si);
    tree(f, tree_area, app, world, s, &rows, me, ins);
    let sel = rows.get(app.sel).copied();
    detail(f, detail_area, app, world, s, sel);
    links(f, links_area, app, s);
}

fn rows(s: &Session) -> Vec<Row> {
    let mut out = vec![Row { depth: 0, r: RowRef::Main }];
    if !s.collapsed(0) {
        item_rows(s, 0, 1, &mut out, 0);
    }
    out
}

fn item_rows(s: &Session, a: usize, depth: usize, out: &mut Vec<Row>, guard: usize) {
    if guard > 8 {
        return;
    }
    for (i, item) in s.agents[a].items.iter().enumerate() {
        out.push(Row { depth, r: RowRef::Item(a, i) });
        let Item::Step(st) = item else { continue };
        if let Some(c) = st.child
            && !s.collapsed(c)
        {
            item_rows(s, c, depth + 1, out, guard + 1);
        }
        if let Some(w) = st.workflow
            && s.wf_expanded(w)
        {
            workflow_rows(s, w, depth + 1, out);
        }
    }
}

fn workflow_rows(s: &Session, w: usize, depth: usize, out: &mut Vec<Row>) {
    let wf = &s.workflows[w];
    if wf.phases.is_empty() {
        out.extend((0..wf.agents.len()).map(|j| Row { depth, r: RowRef::WfAgent(w, j) }));
        return;
    }
    let live_ids: HashSet<&str> = wf.live.iter().map(|&a| s.agents[a].id.as_str()).collect();
    for p in 0..wf.phases.len() {
        out.push(Row { depth, r: RowRef::Phase(w, p) });
        if !s.phase_open(w, p) {
            continue;
        }
        for &a in wf.live.iter().filter(|&&a| wf.phase_of(&s.agents[a].wf_phase) == Some(p)) {
            out.push(Row { depth: depth + 1, r: RowRef::Agent(a) });
            if !s.collapsed(a) {
                item_rows(s, a, depth + 2, out, 1);
            }
        }
        for (j, ag) in wf.agents.iter().enumerate() {
            if ag.phase == p && !live_ids.contains(ag.agent_id.as_str()) {
                out.push(Row { depth: depth + 1, r: RowRef::WfAgent(w, j) });
            }
        }
    }
}

fn t_session(app: &App, order: &[usize], si: usize) -> Color {
    app.t().session(identity(order, si))
}

#[allow(clippy::too_many_arguments)]
fn tree(f: &mut Frame, area: Rect, app: &mut App, world: &World, s: &Session, rows: &[Row], me: Color, ins: &Insights) {
    let t = app.t();
    let cx = Ctx { t, now: world.now, frame: app.frame(), fx: app.fx, marks: Some(&ins.marks), world: Some(world) };
    let busy = s.busy();
    let title = Line::from(vec![
        Span::styled(" ▸ ", Style::new().fg(t.fg)),
        Span::styled(s.project().to_string(), Style::new().fg(me).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {} ", clip(&s.title, 60)), Style::new().fg(t.dim)),
    ]);
    let right = Line::from(Span::styled(
        format!(" {} · {} ", if s.alive { s.status.to_uppercase() } else { "ENDED".into() }, if app.follow { "FOLLOW" } else { "SCROLL" }),
        Style::new().fg(t.faint),
    ));
    let inner = panel(f, area, t, title, Some(right), busy, false);
    app.tree_rows = inner;
    let h = inner.height as usize;
    if h == 0 || rows.is_empty() {
        return;
    }
    // Leave a line for the blinking cursor under the newest step while the session works.
    let cursor = app.follow && busy && h > 2;
    let cap = if cursor { h - 1 } else { h };
    app.page = cap.max(1);
    if app.follow {
        app.sel = rows.len() - 1;
    }
    app.sel = app.sel.min(rows.len() - 1);
    if app.sel < app.scroll {
        app.scroll = app.sel;
    }
    if app.sel >= app.scroll + cap {
        app.scroll = app.sel + 1 - cap;
    }
    app.scroll = app.scroll.min(rows.len().saturating_sub(cap));
    let end = (app.scroll + cap).min(rows.len());
    let w = inner.width as usize;
    let sweep = if app.fx && busy { Some((app.ms() / 45) as usize % (h + 40)) } else { None };
    let mut lines: Vec<Line> = (app.scroll..end)
        .map(|k| {
            let mut line = row_line(s, &rows[k], &cx, w);
            if k == app.sel {
                line = line.style(Style::new().bg(t.sel));
            }
            if sweep == Some(k - app.scroll) {
                line = line.patch_style(Style::new().add_modifier(Modifier::BOLD));
            }
            line
        })
        .collect();
    if cursor {
        let on = (app.ms() / 500).is_multiple_of(2);
        lines.push(Line::from(Span::styled(if on { "  █" } else { "   " }, Style::new().fg(t.fg))));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn row_line(s: &Session, row: &Row, cx: &Ctx, width: usize) -> Line<'static> {
    let t = cx.t;
    let mut left: Vec<Span<'static>> = vec![Span::raw("  ".repeat(row.depth))];
    match row.r {
        RowRef::Main => {
            let ag = &s.agents[0];
            left.push(fold_mark(s.collapsed(0), t));
            left.push(Span::styled("◆ main ", Style::new().fg(t.agent).add_modifier(Modifier::BOLD)));
            let right = format!("{} · ctx {} · out {}", short_model(&ag.model), fmt_n(ag.ctx), fmt_n(ag.tokens().output));
            fit(left, s.title.clone(), Style::new().fg(t.dim), vec![Span::styled(right, Style::new().fg(t.faint))], width)
        }
        RowRef::Item(a, i) => match &s.agents[a].items[i] {
            Item::Prompt { at, text } => {
                left.push(Span::styled("  › ", Style::new().fg(t.prompt).add_modifier(Modifier::BOLD)));
                let right = vec![Span::styled(fmt_dur(cx.now - at), Style::new().fg(t.faint))];
                fit(left, text.clone(), Style::new().fg(t.prompt).add_modifier(Modifier::BOLD), right, width)
            }
            Item::Step(st) => {
                left.push(match (st.child, st.workflow) {
                    (Some(c), _) => fold_mark(s.collapsed(c), t),
                    (None, Some(w)) => fold_mark(!s.wf_expanded(w), t),
                    _ => Span::raw("  "),
                });
                left.push(glyph(s, st, cx));
                left.push(Span::raw(" "));
                let (label, text) = st.label();
                let fresh = cx.fx && cx.now >= st.started && cx.now - st.started < FRESH_MS;
                let strong = matches!(st.kind, Kind::Agent | Kind::Workflow | Kind::Skill) || fresh;
                let mut lstyle = Style::new().fg(if fresh { t.hi } else { t.kind(st.kind) });
                if strong {
                    lstyle = lstyle.add_modifier(Modifier::BOLD);
                }
                left.push(Span::styled(format!("{:<10} ", clip(&label, 10)), lstyle));
                let running = st.running() && s.alive;
                let tstyle = Style::new().fg(if fresh {
                    t.hi
                } else if running {
                    t.fg
                } else {
                    t.dim
                });
                let mut right = Vec::new();
                if let Some(m) = cx.marks.and_then(|m| m.get(&st.id)) {
                    right.push(Span::styled(format!("{m} · "), Style::new().fg(t.warn).add_modifier(Modifier::BOLD)));
                }
                if let (Some(w), Some(world)) = (st.workflow, cx.world)
                    && let Some(r) = insight::race(world, s, w).filter(|_| running || st.finished().is_some())
                {
                    right.push(Span::styled(format!("{} · ", vitals::race_chip(&r)), Style::new().fg(t.workflow)));
                }
                if let Some(c) = st.child {
                    let ag = &s.agents[c];
                    right.push(Span::styled(format!("{} steps · out {} · ", ag.step_count(), fmt_n(ag.tokens().output)), Style::new().fg(t.faint)));
                }
                if let Some(w) = st.workflow {
                    let wf = &s.workflows[w];
                    if !wf.agents.is_empty() {
                        right.push(Span::styled(format!("{} agents · ", wf.agents.len()), Style::new().fg(t.faint)));
                    }
                }
                if st.bg.is_some() && running {
                    right.push(Span::styled("bg ", Style::new().fg(t.workflow)));
                }
                let end = st.finished().unwrap_or(if s.alive { cx.now } else { s.last_at });
                right.push(Span::styled(fmt_dur(end - st.started), Style::new().fg(if running { t.fg } else { t.faint })));
                fit(left, text, tstyle, right, width)
            }
        },
        RowRef::Phase(w, p) => {
            let wf = &s.workflows[w];
            left.push(fold_mark(!s.phase_open(w, p), t));
            let live: Vec<&str> = wf
                .live
                .iter()
                .filter(|&&a| wf.phase_of(&s.agents[a].wf_phase) == Some(p))
                .map(|&a| if s.agent_running(a) { "running" } else { "done" })
                .collect();
            let of_phase: Vec<&str> = if live.is_empty() { wf.agents.iter().filter(|a| a.phase == p).map(|a| a.state.as_str()).collect() } else { live };
            let done = of_phase.iter().filter(|s| **s == "done").count();
            let running = of_phase.iter().filter(|s| **s == "running").count();
            let rest = of_phase.len() - done - running;
            left.push(if !of_phase.is_empty() && done == of_phase.len() {
                Span::styled("✓", Style::new().fg(t.ok))
            } else if running > 0 && s.alive {
                Span::styled(SPIN[cx.frame % SPIN.len()], Style::new().fg(t.workflow))
            } else {
                Span::styled("○", Style::new().fg(t.faint))
            });
            left.push(Span::styled(format!(" {} ", p + 1), Style::new().fg(t.faint)));
            let right = if of_phase.is_empty() { String::new() } else { format!("✓{done} ▶{running} ·{rest}") };
            fit(left, wf.phases[p].clone(), Style::new().fg(t.workflow), vec![Span::styled(right, Style::new().fg(t.faint))], width)
        }
        RowRef::Agent(a) => {
            let ag = &s.agents[a];
            let running = s.agent_running(a);
            left.push(fold_mark(s.collapsed(a), t));
            left.push(if running {
                Span::styled(SPIN[cx.frame % SPIN.len()], Style::new().fg(t.workflow))
            } else {
                Span::styled("✓", Style::new().fg(t.ok))
            });
            left.push(Span::raw(" "));
            left.push(Span::styled(format!("{:<14} ", clip(&ag.description, 14)), Style::new().fg(t.workflow).add_modifier(Modifier::BOLD)));
            let end = if running { cx.now } else { ag.last_at };
            let right = vec![Span::styled(
                format!("{} steps · out {} · {} · {}", ag.step_count(), fmt_n(ag.tokens().output), short_model(&ag.model), fmt_dur(end - ag.started)),
                Style::new().fg(if running { t.fg } else { t.faint }),
            )];
            fit(left, ag.last_text.clone(), Style::new().fg(t.dim), right, width)
        }
        RowRef::WfAgent(w, j) => {
            let ag = &s.workflows[w].agents[j];
            left.push(Span::raw("  "));
            left.push(match ag.state.as_str() {
                "done" => Span::styled("✓", Style::new().fg(t.ok)),
                "running" if s.alive => Span::styled(SPIN[cx.frame % SPIN.len()], Style::new().fg(t.workflow)),
                "failed" | "error" | "killed" => Span::styled("✗", Style::new().fg(t.err)),
                _ => Span::styled("·", Style::new().fg(t.faint)),
            });
            left.push(Span::raw(" "));
            left.push(Span::styled(format!("{:<14} ", clip(&ag.label, 14)), Style::new().fg(t.workflow)));
            let text = if ag.last_tool.is_empty() { ag.last_summary.clone() } else { format!("{}: {}", ag.last_tool, ag.last_summary) };
            let right = vec![Span::styled(short_model(&ag.model).to_string(), Style::new().fg(t.faint))];
            fit(left, text, Style::new().fg(t.dim), right, width)
        }
    }
}

fn detail(f: &mut Frame, area: Rect, app: &App, world: &World, s: &Session, sel: Option<Row>) {
    let t = app.t();
    let now = world.now;
    let inner = panel(f, area, t, Line::from(Span::styled(" ▸ DETAIL ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD))), None, false, false);
    let kv = |k: &str, v: String| -> Line<'static> {
        Line::from(vec![Span::styled(format!("{k:<8} "), Style::new().fg(t.faint)), Span::styled(v, Style::new().fg(t.fg))])
    };
    let mut lines = Vec::new();
    match sel.map(|r| r.r).unwrap_or(RowRef::Main) {
        RowRef::Main => {
            let ag = &s.agents[0];
            let tk = s.tokens();
            lines.push(kv("project", s.cwd.clone()));
            lines.push(kv("session", format!("{} · {}", s.name, if s.alive { s.status.as_str() } else { "ended" })));
            lines.push(kv("model", short_model(&ag.model).to_string()));
            lines.push(kv("context", fmt_n(ag.ctx)));
            lines.push(kv("tokens", format!("in {} · cache {} · out {}", fmt_n(tk.input), fmt_n(tk.cache_read), fmt_n(tk.output))));
            lines.push(kv("agents", format!("{} spawned · {} running", s.agents.len() - 1, s.running_agents())));
            lines.push(kv("flow", format!("sent {}/s · generated {}/s", fmt_n(s.tok_up.per_sec(now)), fmt_n(s.tok_down.per_sec(now)))));
            if !ag.last_text.is_empty() {
                lines.push(kv("said", ag.last_text.clone()));
            }
        }
        RowRef::Item(a, i) => match &s.agents[a].items[i] {
            Item::Prompt { at, text } => {
                lines.push(kv("prompt", text.clone()));
                lines.push(kv("when", format!("{} ago", fmt_dur(now - at))));
            }
            Item::Step(st) => {
                step_detail(&mut lines, &kv, s, a, st, now);
                if let Some(r) = st.workflow.and_then(|w| insight::race(world, s, w)) {
                    race_detail(&mut lines, &kv, &r, s.workflows[st.workflow.unwrap_or(0)].phases.len(), inner.width as usize, t);
                }
            }
        },
        RowRef::Phase(w, p) => {
            let wf = &s.workflows[w];
            lines.push(kv("workflow", wf.name.clone()));
            lines.push(kv("phase", format!("{} of {} · {}", p + 1, wf.phases.len(), wf.phases[p])));
            let live: Vec<usize> = wf.live.iter().copied().filter(|&a| wf.phase_of(&s.agents[a].wf_phase) == Some(p)).collect();
            if live.is_empty() {
                let n = wf.agents.iter().filter(|a| a.phase == p).count();
                lines.push(kv("agents", if n == 0 { "none started yet".into() } else { n.to_string() }));
            } else {
                let running = live.iter().filter(|&&a| s.agent_running(a)).count();
                lines.push(kv("agents", format!("{} · {running} running", live.len())));
            }
            if let Some(r) = insight::race(world, s, w) {
                let fmt = |sp: Option<(i64, i64)>| sp.map_or("–".into(), |(a, b)| fmt_dur(b - a));
                let mine = r.live.get(p).copied().flatten().map(|(a, b)| (a, b.unwrap_or(r.elapsed)));
                lines.push(kv("this run", fmt(mine)));
                lines.push(kv("ghost", format!("{} in {}", fmt(r.ghost.spans.get(p).copied().flatten()), r.ghost.name)));
            }
        }
        RowRef::Agent(a) => {
            let ag = &s.agents[a];
            lines.push(kv("agent", ag.description.clone()));
            lines.push(kv("phase", ag.wf_phase.clone()));
            lines.push(kv("state", if s.agent_running(a) { "running".into() } else { "done".into() }));
            lines.push(kv("model", short_model(&ag.model).to_string()));
            lines.push(kv("work", format!("{} steps · ctx {} · out {}", ag.step_count(), fmt_n(ag.ctx), fmt_n(ag.tokens().output))));
            if !ag.last_text.is_empty() {
                lines.push(kv("said", ag.last_text.clone()));
            }
        }
        RowRef::WfAgent(w, j) => {
            let ag = &s.workflows[w].agents[j];
            lines.push(kv("agent", ag.label.clone()));
            lines.push(kv("state", ag.state.clone()));
            lines.push(kv("model", short_model(&ag.model).to_string()));
            lines.push(kv("last", format!("{} {}", ag.last_tool, ag.last_summary)));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn step_detail(lines: &mut Vec<Line<'static>>, kv: &dyn Fn(&str, String) -> Line<'static>, s: &Session, a: usize, st: &Step, now: i64) {
    let (_, text) = st.label();
    lines.push(kv("tool", st.name.clone()));
    lines.push(kv("about", text));
    let end = st.finished().unwrap_or(if s.alive { now } else { s.last_at });
    let dur = fmt_dur(end - st.started);
    lines.push(kv(
        "status",
        if st.running() && !s.alive {
            format!("never finished ({dur})")
        } else if st.running() {
            format!("{} for {dur}", if st.ended.is_some() { "background · running" } else { "running" })
        } else if st.failed() {
            format!("failed after {dur}")
        } else if st.stopped() {
            format!("{} after {dur}", st.bg.as_ref().map_or("stopped", |b| b.status.as_str()))
        } else {
            format!("done in {dur}")
        },
    ));
    lines.push(kv("started", format!("{} ago", fmt_dur(now - st.started))));
    if let Some(bg) = &st.bg {
        lines.push(kv("task", format!("{} · {}", bg.task_id, if bg.status.is_empty() { "running" } else { &bg.status })));
    }
    if a != 0 {
        let by = &s.agents[a];
        lines.push(kv("by", format!("{} · {}", by.kind, by.description)));
    }
    if let Some(c) = st.child {
        let ag = &s.agents[c];
        lines.push(kv("agent", format!("{} · {} · {}", ag.kind, short_model(&ag.model), ag.id)));
        lines.push(kv("work", format!("{} steps · ctx {} · out {}", ag.step_count(), fmt_n(ag.ctx), fmt_n(ag.tokens().output))));
        if !ag.last_text.is_empty() {
            lines.push(kv("said", ag.last_text.clone()));
        }
    }
    if let Some(w) = st.workflow {
        let wf = &s.workflows[w];
        lines.push(kv("workflow", format!("{} · {}", wf.name, if wf.status.is_empty() { "running" } else { &wf.status })));
        if !wf.phases.is_empty() {
            lines.push(kv("phases", wf.phases.join(" → ")));
        }
        if !wf.agents.is_empty() {
            lines.push(kv("agents", format!("{} · {} tokens", wf.agents.len(), fmt_n(wf.tokens))));
        }
    }
    if !st.result.is_empty() {
        lines.push(kv("result", st.result.clone()));
    }
}

/// The race of a workflow run against an earlier one: two lanes and where it stands.
fn race_detail(lines: &mut Vec<Line<'static>>, kv: &dyn Fn(&str, String) -> Line<'static>, r: &insight::Race, phases: usize, width: usize, t: &Theme) {
    lines.push(Line::raw(""));
    let against = if r.same_name { "its last run".to_string() } else { format!("{} (same phases)", r.ghost.name) };
    lines.push(kv("race", format!("vs {against} · {} took {}", if r.runs > 1 { format!("1 of {} runs,", r.runs) } else { "it".into() }, fmt_dur(r.ghost.dur))));
    lines.push(kv("now", vitals::race_chip(r).trim_start_matches("△ ").to_string()));
    let lane_w = width.saturating_sub(9);
    let lanes = vitals::race_lanes(r, phases, lane_w, t);
    for (label, lane) in ["this run", "ghost"].iter().zip(lanes) {
        let mut spans = vec![Span::styled(format!("{label:<8} "), Style::new().fg(t.faint))];
        spans.extend(lane.spans);
        lines.push(Line::from(spans));
    }
}

fn links(f: &mut Frame, area: Rect, app: &App, s: &Session) {
    let t = app.t();
    let frame = app.frame();
    let title = Line::from(Span::styled(" ⇄ CONNECTIONS · ✦ SKILLS ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)));
    let inner = panel(f, area, t, title, None, false, false);
    let w = inner.width as usize;
    let running = s.mcp_running();
    let mut lines = Vec::new();
    for (server, st) in &s.mcp {
        let live = running.contains(server);
        let left = vec![
            Span::styled(if live { SPIN[frame % SPIN.len()] } else { "⇄" }, Style::new().fg(if live { t.hi } else { t.mcp })),
            Span::styled(format!(" {:<14} ", clip(server_label(server), 14)), Style::new().fg(t.mcp).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:>3}× ", st.calls), Style::new().fg(t.fg)),
        ];
        let right = if st.errors > 0 { vec![Span::styled(format!(" {}✗", st.errors), Style::new().fg(t.err))] } else { vec![] };
        lines.push(fit(left, st.last_tool.clone(), Style::new().fg(t.dim), right, w));
    }
    if s.mcp.is_empty() {
        lines.push(Line::from(Span::styled("no MCP call yet", Style::new().fg(t.faint))));
    }
    lines.push(Line::raw(""));
    let mut skills = Vec::new();
    for (name, n) in &s.skills {
        skills.push(Span::styled(format!("✦ {name}"), Style::new().fg(t.skill)));
        if *n > 1 {
            skills.push(Span::styled(format!("×{n}"), Style::new().fg(t.faint)));
        }
        skills.push(Span::raw("  "));
    }
    if skills.is_empty() {
        skills.push(Span::styled("no skill yet", Style::new().fg(t.faint)));
    }
    lines.push(Line::from(skills));
    let bg = s.background_running();
    if bg > 0 {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("{} {bg} background task{} running", SPIN[frame % SPIN.len()], if bg == 1 { "" } else { "s" }),
            Style::new().fg(t.workflow),
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ---------------------------------------------------------------- token flow

/// `⇡ 3.4k/s ⇣ 812/s ▸····▸····▸ ███░░░ 45%`: tokens sent and generated per second over the
/// last bucket, a packet stream that moves while tokens flow, and how full the context window is.
fn flow_line(s: &Session, cx: &Ctx, fc: &crate::model::Forecast, w: usize) -> Line<'static> {
    let t = cx.t;
    let (up, down) = (s.tok_up.per_sec(cx.now), s.tok_down.per_sec(cx.now));
    let left = vec![
        Span::styled(format!("⇡{:>5}", fmt_n(up)), Style::new().fg(t.accent)),
        Span::styled("/s ", Style::new().fg(t.faint)),
        Span::styled(format!("⇣{:>5}", fmt_n(down)), Style::new().fg(t.hi)),
        Span::styled("/s ", Style::new().fg(t.faint)),
    ];
    let right = vitals::ctx_gauge(fc, 6, t);
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    let n = w.saturating_sub(lw + rw + 1);
    let mut spans = left;
    spans.push(stream(n, cx.frame, s.alive && up + down > 0, t.accent, t));
    spans.push(Span::raw(" "));
    spans.extend(right);
    Line::from(spans)
}

/// The session's vital signs: the brainwave of thinking, writing and tool time (one trace each
/// when there is room), token flow, the prompt cache and price, and the context against the
/// compaction ceiling.
fn vitals_panel(f: &mut Frame, area: Rect, app: &App, world: &World, s: &Session) {
    let t = app.t();
    let now = world.now;
    let frame = app.frame();
    let title = Line::from(vec![
        Span::styled(" ∿ VITALS ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)),
        Span::styled("· last 5 minutes ", Style::new().fg(t.faint)),
    ]);
    let inner = panel(f, area, t, title, None, s.busy(), false);
    let w = inner.width as usize;
    let mut lines = Vec::new();
    let (waves, _) = s.waves_live(now, 60);
    let sum = |v: &Vec<u64>| v.iter().sum::<u64>() as i64;
    if inner.height >= 6 {
        for p in [Phase::Think, Phase::Write, Phase::Tool] {
            let color = vitals::phase_color(t, Some(p));
            let mut right = format!(" {:>6}", fmt_dur(sum(&waves[p as usize])));
            if p == Phase::Think {
                right.push_str(&format!(" · {} tok", fmt_n(s.thinking.sum(now, 60))));
            }
            let name = match p {
                Phase::Think => "THINK",
                Phase::Write => "WRITE",
                Phase::Tool => "TOOLS",
            };
            let label = Span::styled(format!(" {name:<6}"), Style::new().fg(color).add_modifier(Modifier::BOLD));
            let n = w.saturating_sub(7 + 18).min(60);
            let mut spans = vec![label];
            spans.extend(vitals::brainwave(s, now, frame, n, Some(p), t));
            spans.push(Span::styled(right, Style::new().fg(t.dim)));
            lines.push(Line::from(spans));
        }
    } else {
        let mut spans = vec![Span::styled(" ∿ ", Style::new().fg(t.comms))];
        spans.extend(vitals::brainwave(s, now, frame, w.saturating_sub(40).min(60), None, t));
        spans.push(Span::styled(
            format!(" think {} · write {} · tools {}", fmt_dur(sum(&waves[0])), fmt_dur(sum(&waves[1])), fmt_dur(sum(&waves[2]))),
            Style::new().fg(t.dim),
        ));
        lines.push(Line::from(spans));
    }
    let (up, down) = (s.tok_up.per_sec(now), s.tok_down.per_sec(now));
    let mut spans = vec![Span::styled(" ⇡ sent ", Style::new().fg(t.accent))];
    spans.extend(sparkline(&s.tok_up.series(now, 12), t));
    spans.push(Span::styled(format!(" {}/s   ", fmt_n(up)), Style::new().fg(t.accent).add_modifier(Modifier::BOLD)));
    spans.push(Span::styled("⇣ generated ", Style::new().fg(t.hi)));
    spans.extend(sparkline(&s.tok_down.series(now, 12), t));
    spans.push(Span::styled(format!(" {}/s   ", fmt_n(down)), Style::new().fg(t.hi).add_modifier(Modifier::BOLD)));
    spans.push(Span::styled(format!("log {}/s", fmt_bytes(s.bytes.per_min(now) / 60)), Style::new().fg(t.dim)));
    lines.push(Line::from(spans));
    let mut spans = vec![Span::raw(" ")];
    spans.extend(vitals::reactor(s, now, app.ms(), t, app.engines.blend_hit()));
    spans.push(Span::styled(
        format!(
            " cache · read {} · written {} · fresh {} (5 min)   ",
            fmt_n(s.cache_r.sum(now, 60)),
            fmt_n(s.cache_w.sum(now, 60)),
            fmt_n(s.fresh_in.sum(now, 60))
        ),
        Style::new().fg(t.dim),
    ));
    if let Some(&(at, n)) = s.flares.back().filter(|f| now - f.0 < 3_600_000) {
        spans.push(Span::styled(format!("✺ rebuilt {} {} ago   ", fmt_n(n), fmt_dur(now - at)), Style::new().fg(t.faint)));
    }
    spans.push(Span::styled(
        format!("{} · {}/h API-equivalent", vitals::money(s.cost_micros), vitals::money(vitals::money_rate(s, now))),
        Style::new().fg(t.fg),
    ));
    lines.push(Line::from(spans));
    let fc = insight::forecast(world, s);
    let mut spans = vec![Span::styled(" ctx ", Style::new().fg(t.dim))];
    spans.extend(vitals::ctx_gauge(&fc, 12, t));
    spans.push(Span::styled(format!("  {}", vitals::compaction_text(s, &fc, now)), Style::new().fg(t.dim)));
    lines.push(Line::from(spans));
    f.render_widget(Paragraph::new(lines), inner);
}

/// Packets moving right while tokens flow, a quiet dotted line otherwise.
fn stream(n: usize, frame: usize, active: bool, color: Color, t: &Theme) -> Span<'static> {
    if !active {
        return Span::styled("·".repeat(n), Style::new().fg(t.faint));
    }
    let text: String = (0..n).map(|i| if (i + 5 - frame % 5).is_multiple_of(5) { '▸' } else { '·' }).collect();
    Span::styled(text, Style::new().fg(color))
}

fn fmt_bytes(b: u64) -> String {
    match b {
        0..=1023 => format!("{b} B"),
        1024..=1_048_575 => format!("{:.1} KB", b as f64 / 1024.0),
        _ => format!("{:.1} MB", b as f64 / 1_048_576.0),
    }
}

// ---------------------------------------------------------------- surfaces

/// A raised panel: filled surface, rounded border lit by the theme gradient along its top
/// edge when `lit`, and a soft drop shadow one cell down and right. Returns the inner area.
fn panel(f: &mut Frame, slot: Rect, t: &Theme, title: Line<'_>, right: Option<Line<'_>>, lit: bool, double: bool) -> Rect {
    let r = Rect { width: slot.width.saturating_sub(1), height: slot.height.saturating_sub(1), ..slot };
    if r.width < 3 || r.height < 3 {
        return Rect::default();
    }
    let mut block = Block::bordered()
        .border_type(if double { BorderType::Double } else { BorderType::Rounded })
        .border_style(Style::new().fg(if lit { t.dim } else { t.faint }))
        .style(Style::new().bg(t.surface))
        .title_top(title);
    if let Some(right) = right {
        block = block.title_top(right.right_aligned());
    }
    let inner = block.inner(r);
    f.render_widget(block, r);
    let buf = f.buffer_mut();
    drop_shadow(buf, r, t);
    if lit {
        light_edge(buf, r, t);
    }
    inner
}

fn drop_shadow(buf: &mut Buffer, r: Rect, t: &Theme) {
    let (right, bottom) = (r.right(), r.bottom());
    for y in r.y + 1..bottom {
        if let Some(c) = buf.cell_mut((right, y)) {
            c.set_symbol("▌").set_fg(t.shadow);
        }
    }
    for x in r.x + 1..right {
        if let Some(c) = buf.cell_mut((x, bottom)) {
            c.set_symbol("▀").set_fg(t.shadow);
        }
    }
    if let Some(c) = buf.cell_mut((right, bottom)) {
        c.set_symbol("▘").set_fg(t.shadow);
    }
}

/// Recolors the border cells of the top edge with the theme gradient; titles keep their colors.
fn light_edge(buf: &mut Buffer, r: Rect, t: &Theme) {
    let span = r.width.saturating_sub(1).max(1) as f64;
    for x in r.x..r.right() {
        if let Some(c) = buf.cell_mut((x, r.y))
            && matches!(c.symbol(), "─" | "═" | "╭" | "╮" | "╔" | "╗")
        {
            c.set_fg(t.grad((x - r.x) as f64 / span));
        }
    }
    for y in r.y + 1..r.bottom().saturating_sub(1) {
        if let Some(c) = buf.cell_mut((r.x, y)) {
            c.set_fg(t.grad(0.0));
        }
        if let Some(c) = buf.cell_mut((r.right() - 1, y)) {
            c.set_fg(t.grad(1.0));
        }
    }
}

/// Text on a gradient, with rounded half-block ends.
fn pill(text: &str, t: &Theme) -> Vec<Span<'static>> {
    let n = text.chars().count().max(2);
    let mut spans = vec![Span::styled("▐", Style::new().fg(t.grad(0.0)))];
    for (i, ch) in text.chars().enumerate() {
        spans.push(Span::styled(ch.to_string(), Style::new().fg(rgb((12, 12, 12))).bg(t.grad(i as f64 / (n - 1) as f64)).add_modifier(Modifier::BOLD)));
    }
    spans.push(Span::styled("▌", Style::new().fg(t.grad(1.0))));
    spans
}

// ---------------------------------------------------------------- skyline

/// One isometric tower per session. Height follows the tokens flowing per minute; windows
/// flicker and a beacon blinks while the session works.
fn skyline(f: &mut Frame, area: Rect, app: &mut App, world: &World, order: &[usize]) {
    let t = app.t();
    let title = Line::from(vec![
        Span::styled(" ◢ SKYLINE ", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)),
        Span::styled("· height = tokens per second ", Style::new().fg(t.faint)),
    ]);
    let any_busy = order.iter().any(|&si| world.sessions[si].busy());
    let names = names(world, order);
    let inner = panel(f, area, t, title, None, any_busy, false);
    if inner.height < 8 || inner.width < TOWER_SLOT {
        return;
    }
    let ground = inner.bottom() - 4;
    let max_h = ground.saturating_sub(inner.y + 3).max(1) as f64;
    let n = order.len().min((inner.width / TOWER_SLOT) as usize);
    let x_start = inner.x + (inner.width - n as u16 * TOWER_SLOT) / 2;
    let (now, frame) = (world.now, app.frame());
    {
        let buf = f.buffer_mut();
        for x in inner.x..inner.right() {
            if let Some(c) = buf.cell_mut((x, ground + 1)) {
                c.set_symbol("▔").set_fg(t.faint);
            }
        }
    }
    for (k, &si) in order.iter().take(n).enumerate() {
        let s = &world.sessions[si];
        let rate = s.tok_up.per_sec(now) + s.tok_down.per_sec(now);
        let target = if s.alive { 1.0 + (1.0 + rate as f64 / 2000.0).log2() } else { 1.0 };
        let h = app.heights.entry(s.id.clone()).or_insert(1.0);
        *h += (target.clamp(1.0, max_h) - *h) * 0.35;
        let height = (h.round() as u16).clamp(1, max_h as u16);
        let x0 = x_start + k as u16 * TOWER_SLOT + (TOWER_SLOT - TOWER_W - 3) / 2;
        let busy = s.busy();
        tower(f.buffer_mut(), x0, ground, height, t.towers[k % t.towers.len()], busy, s.alive, frame, k, t);
        let slot_x = x_start + k as u16 * TOWER_SLOT;
        let name = clip(names.get(&si).map_or(s.project(), String::as_str), (TOWER_SLOT - 1) as usize);
        let style = if busy {
            Style::new().fg(t.hi).add_modifier(Modifier::BOLD)
        } else if s.alive {
            Style::new().fg(t.fg)
        } else {
            Style::new().fg(t.faint)
        };
        let style = if s.alive { style.fg(t.session(k)) } else { style };
        let needs = s.needs_you();
        let label = if needs {
            "needs you".into()
        } else if rate > 0 {
            format!("{}/s", fmt_n(rate))
        } else if s.alive {
            "idle".into()
        } else {
            "ended".into()
        };
        let label_style = Style::new().fg(if needs {
            t.warn
        } else if rate > 0 {
            t.accent
        } else {
            t.faint
        });
        centered(f.buffer_mut(), slot_x, ground + 2, TOWER_SLOT, &name, style);
        centered(f.buffer_mut(), slot_x, ground + 3, TOWER_SLOT, &label, label_style);
    }
}

/// An oblique box: front face with windows, a top face one row up and one column right,
/// a darker side face, a long shadow on the ground, and a blinking beacon while busy.
#[allow(clippy::too_many_arguments)]
fn tower(buf: &mut Buffer, x0: u16, ground: u16, h: u16, base: Rgb, busy: bool, alive: bool, frame: usize, seed: usize, t: &Theme) {
    let tone = |f: f64| rgb(shade(base, if alive { f } else { f * 0.45 }));
    let (front, side, top, unlit) = (tone(1.0), tone(0.5), tone(1.35), tone(0.72));
    let lit = rgb((255, 226, 140));
    let w = TOWER_W;
    let top_y = ground + 1 - h;
    let mut put = |x: u16, y: u16, sym: &str, fg: Option<ratatui::style::Color>, bg: Option<ratatui::style::Color>| {
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
    for x in x0 + 1..=x0 + w {
        put(x, top_y - 1, "▄", Some(top), None);
    }
    for y in top_y..=ground {
        let row = ground - y;
        for x in x0..x0 + w {
            let col = x - x0;
            if row >= 1 && col % 2 == 1 && col < w - 1 {
                let k = hash(seed, x, y, if busy { frame / 7 } else { 0 });
                let on = if busy { !k.is_multiple_of(3) } else { alive && k.is_multiple_of(6) };
                put(x, y, "▪", Some(if on { lit } else { unlit }), Some(front));
            } else {
                put(x, y, " ", None, Some(front));
            }
        }
        if y < ground {
            put(x0 + w, y, " ", None, Some(side));
        } else {
            put(x0 + w, y, "▀", Some(side), None);
        }
    }
    for x in x0 + w + 1..=x0 + w + 2 {
        put(x, ground, "░", Some(t.shadow), None);
    }
    if busy && top_y >= 3 {
        put(x0 + 2, top_y - 2, "╻", Some(t.dim), None);
        let on = (frame / 6).is_multiple_of(2);
        put(x0 + 2, top_y - 3, if on { "●" } else { "·" }, Some(if on { t.err } else { t.faint }), None);
    }
}

fn centered(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    let len = text.chars().count() as u16;
    let start = x + width.saturating_sub(len) / 2;
    buf.set_stringn(start, y, text, width as usize, style);
}

fn hash(a: usize, b: u16, c: u16, d: usize) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in [a as u64, b as u64, c as u64, d as u64] {
        h ^= v;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^ (h >> 29)
}

// ---------------------------------------------------------------- pieces

fn glyph(s: &Session, st: &Step, cx: &Ctx) -> Span<'static> {
    let t = cx.t;
    if st.running() {
        if !s.alive {
            return Span::styled("◌", Style::new().fg(t.faint));
        }
        let color = if st.ended.is_some() { t.workflow } else { t.warn };
        return Span::styled(SPIN[cx.frame % SPIN.len()], Style::new().fg(color));
    }
    if st.failed() {
        Span::styled("✗", Style::new().fg(t.err))
    } else if st.stopped() {
        Span::styled("■", Style::new().fg(t.dim))
    } else {
        Span::styled("✓", Style::new().fg(t.ok))
    }
}

fn fold_mark(collapsed: bool, t: &Theme) -> Span<'static> {
    Span::styled(if collapsed { "▸ " } else { "▾ " }, Style::new().fg(t.dim))
}

fn sparkline(values: &[u32], t: &Theme) -> Vec<Span<'static>> {
    let max = values.iter().copied().max().unwrap_or(0).max(1);
    values
        .iter()
        .map(|&v| {
            if v == 0 {
                Span::styled("·", Style::new().fg(t.faint))
            } else {
                let level = (v * 8).div_ceil(max).clamp(1, 8) as usize;
                Span::styled(BARS[level], Style::new().fg(if level >= 6 { t.hi } else { t.fg }))
            }
        })
        .collect()
}

/// Like `fit`, with a bright band sweeping across the text: the step is still running.
fn fit_shimmer(left: Vec<Span<'static>>, text: &str, frame: usize, base: Color, hi: Color, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    let room = width.saturating_sub(lw + rw + usize::from(rw > 0));
    let text = clip(text, room);
    let n = text.chars().count();
    let pos = (frame % (n + 24)) as isize - 6;
    let mut spans = left;
    for (i, ch) in text.chars().enumerate() {
        let d = (i as isize - pos).abs();
        let style = match d {
            0..=1 => Style::new().fg(hi).add_modifier(Modifier::BOLD),
            2..=3 => Style::new().fg(hi),
            _ => Style::new().fg(base),
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    spans.push(Span::raw(" ".repeat(width.saturating_sub(lw + n + rw))));
    spans.extend(right);
    Line::from(spans)
}

/// Left spans, then a flexible text clipped to the room left, then right spans at the edge.
fn fit(left: Vec<Span<'static>>, flex: String, flex_style: Style, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    let room = width.saturating_sub(lw + rw + usize::from(rw > 0));
    let flex = clip(&flex, room);
    let pad = width.saturating_sub(lw + flex.chars().count() + rw);
    let mut spans = left;
    spans.push(Span::styled(flex, flex_style));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    Line::from(spans)
}

// ---------------------------------------------------------------- keys

fn on_key(app: &mut App, world: &mut World, k: KeyEvent) {
    if app.boot {
        app.boot = false;
        return;
    }
    if app.help {
        app.help = false;
        return;
    }
    if k.code == KeyCode::Char('?') {
        app.help = true;
        return;
    }
    match k.code {
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('t') => app.theme = (app.theme + 1) % THEMES.len(),
        KeyCode::Char('x') => app.fx = !app.fx,
        KeyCode::Char('p') => {
            if let Some(r) = world.replay_mut() {
                r.paused = !r.paused;
            }
        }
        KeyCode::Char('>') | KeyCode::Char('+') | KeyCode::Char('=') if app.view != View::Hud || k.code == KeyCode::Char('>') => {
            if let Some(r) = world.replay_mut() {
                r.speed = (r.speed * 2.0).min(1000.0);
            }
        }
        KeyCode::Char('<') | KeyCode::Char('-') if app.view != View::Hud || k.code == KeyCode::Char('<') => {
            if let Some(r) = world.replay_mut() {
                r.speed = (r.speed / 2.0).max(0.25);
            }
        }
        KeyCode::Char('m') => {
            app.mouse = !app.mouse;
            let mut out = io::stdout();
            let _ = if app.mouse { execute!(out, EnableMouseCapture) } else { execute!(out, DisableMouseCapture) };
        }
        KeyCode::Char('g') if app.view != View::Hud => app.view = View::Hud,
        KeyCode::Char('c') if !matches!(app.view, View::City) => app.view = View::City,
        _ => match app.view {
            View::Wall => wall_key(app, world, k),
            View::Session => session_key(app, world, k),
            View::Hud => hud_key(app, world, k),
            View::City => city_key(app, world, k),
        },
    }
}

fn hud_key(app: &mut App, world: &World, k: KeyEvent) {
    match k.code {
        KeyCode::Esc if app.hud.has_target() => {
            app.hud.on_key(k.code);
        }
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('w') | KeyCode::Char('g') => app.view = View::Wall,
        code => {
            if let HudAction::Open(si) = app.hud.on_key(code) {
                open_session(app, world, si);
            }
        }
    }
}

fn city_key(app: &mut App, world: &World, k: KeyEvent) {
    match k.code {
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('w') | KeyCode::Char('c') => app.view = View::Wall,
        KeyCode::Left | KeyCode::Char('h') => app.city.step(-1, 0),
        KeyCode::Right | KeyCode::Char('l') => app.city.step(1, 0),
        KeyCode::Up | KeyCode::Char('k') => app.city.step(0, 1),
        KeyCode::Down | KeyCode::Char('j') => app.city.step(0, -1),
        KeyCode::Char(']') => app.city.window = (app.city.window + 1).min(city::WINDOWS.len() - 1),
        KeyCode::Char('[') => app.city.window = app.city.window.saturating_sub(1),
        KeyCode::Enter => {
            if let Some(&si) = app.city.sel.as_ref().and_then(|p| app.city.owner.get(p)) {
                open_session(app, world, si);
            }
        }
        _ => {}
    }
}

fn open_session(app: &mut App, world: &World, si: usize) {
    app.session = Some(world.sessions[si].id.clone());
    app.pick = order(world).iter().position(|&i| i == si).unwrap_or(0);
    app.view = View::Session;
    app.follow = true;
    app.scroll = 0;
}

fn on_mouse(app: &mut App, world: &mut World, m: MouseEvent) {
    let double = |app: &mut App| {
        let d = app.last_click.is_some_and(|(at, x, y)| at.elapsed() < Duration::from_millis(450) && x == m.column && y == m.row);
        app.last_click = Some((Instant::now(), m.column, m.row));
        d
    };
    match app.view {
        View::Hud => {
            if let HudAction::Open(si) = app.hud.on_mouse(m) {
                open_session(app, world, si);
            }
        }
        View::City => match m.kind {
            MouseEventKind::Moved => app.city.hover = app.city.hit(m.column, m.row),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(p) = app.city.hit(m.column, m.row) {
                    let again = double(app) || app.city.sel.as_deref() == Some(p.as_str());
                    app.city.sel = Some(p.clone());
                    if again && let Some(&si) = app.city.owner.get(&p) {
                        open_session(app, world, si);
                    }
                }
            }
            MouseEventKind::ScrollUp => app.city.step(0, 1),
            MouseEventKind::ScrollDown => app.city.step(0, -1),
            _ => {}
        },
        View::Wall => match m.kind {
            MouseEventKind::Moved => {
                let at = ratatui::layout::Position { x: m.column, y: m.row };
                app.hover_card = app.cards.iter().find(|(r, _)| r.contains(at)).map(|&(_, k)| k);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let at = ratatui::layout::Position { x: m.column, y: m.row };
                if let Some(&(_, k)) = app.cards.iter().find(|(r, _)| r.contains(at)) {
                    let again = double(app) || app.pick == k;
                    app.pick = k;
                    if again && let Some(&si) = order(world).get(k) {
                        open_session(app, world, si);
                    }
                }
            }
            MouseEventKind::ScrollDown => app.pick = (app.pick + app.cols).min(world.sessions.len().saturating_sub(1)),
            MouseEventKind::ScrollUp => app.pick = app.pick.saturating_sub(app.cols),
            _ => {}
        },
        View::Session => match m.kind {
            MouseEventKind::ScrollDown => {
                app.follow = false;
                app.sel += 3;
            }
            MouseEventKind::ScrollUp => {
                app.follow = false;
                app.sel = app.sel.saturating_sub(3);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let r = app.tree_rows;
                if r.contains(ratatui::layout::Position { x: m.column, y: m.row }) {
                    app.follow = false;
                    app.sel = app.scroll + (m.row - r.y) as usize;
                    if double(app) {
                        let order = order(world);
                        if let Some(si) = current_session(app, world, &order) {
                            let s = &mut world.sessions[si];
                            if let Some(row) = rows(s).get(app.sel).copied() {
                                toggle(s, row);
                            }
                        }
                    }
                }
            }
            _ => {}
        },
    }
}

fn wall_key(app: &mut App, world: &World, k: KeyEvent) {
    let order = order(world);
    if order.is_empty() {
        return;
    }
    let last = order.len() - 1;
    match k.code {
        KeyCode::Left | KeyCode::Char('h') => app.pick = app.pick.saturating_sub(1),
        KeyCode::Right | KeyCode::Char('l') => app.pick = (app.pick + 1).min(last),
        KeyCode::Up | KeyCode::Char('k') => app.pick = app.pick.saturating_sub(app.cols),
        KeyCode::Down | KeyCode::Char('j') => app.pick = (app.pick + app.cols).min(last),
        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(&si) = order.get(app.pick) {
                app.session = Some(world.sessions[si].id.clone());
                app.view = View::Session;
                app.follow = true;
                app.scroll = 0;
            }
        }
        _ => {}
    }
}

fn session_key(app: &mut App, world: &mut World, k: KeyEvent) {
    let order = order(world);
    let Some(si) = current_session(app, world, &order) else {
        if matches!(k.code, KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('w')) {
            app.view = View::Wall;
        }
        return;
    };
    let pos = order.iter().position(|&i| i == si).unwrap_or(0);
    let switch = |app: &mut App, idx: usize| {
        app.session = Some(world.sessions[order[idx]].id.clone());
        app.pick = idx;
        app.follow = true;
        app.scroll = 0;
    };
    match k.code {
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('w') => {
            app.view = View::Wall;
            app.pick = pos;
        }
        KeyCode::Tab | KeyCode::Char(']') => switch(app, (pos + 1) % order.len()),
        KeyCode::BackTab | KeyCode::Char('[') => switch(app, (pos + order.len() - 1) % order.len()),
        KeyCode::Up | KeyCode::Char('k') => {
            app.follow = false;
            app.sel = app.sel.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.follow = false;
            app.sel += 1;
        }
        KeyCode::PageUp => {
            app.follow = false;
            app.sel = app.sel.saturating_sub(app.page);
        }
        KeyCode::PageDown => {
            app.follow = false;
            app.sel += app.page;
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.follow = false;
            app.sel = app.sel.saturating_sub(app.page / 2);
        }
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.follow = false;
            app.sel += app.page / 2;
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.follow = false;
            app.sel = 0;
        }
        KeyCode::End | KeyCode::Char('G') => app.follow = true,
        KeyCode::Char('f') => app.follow = !app.follow,
        KeyCode::Char('e') => {
            app.expanded = !app.expanded;
            world.sessions[si].set_all_expanded(app.expanded);
        }
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
            let s = &mut world.sessions[si];
            if let Some(row) = rows(s).get(app.sel).copied() {
                app.follow = false;
                toggle(s, row);
            }
        }
        _ => {}
    }
}

fn toggle(s: &mut Session, row: Row) {
    match row.r {
        RowRef::Main => {
            let c = s.collapsed(0);
            s.agents[0].collapsed = Some(!c);
        }
        RowRef::Item(a, i) => {
            let (child, wf) = match &s.agents[a].items[i] {
                Item::Step(st) => (st.child, st.workflow),
                Item::Prompt { .. } => (None, None),
            };
            if let Some(c) = child {
                let v = s.collapsed(c);
                s.agents[c].collapsed = Some(!v);
            } else if let Some(w) = wf {
                let v = s.wf_expanded(w);
                s.workflows[w].expanded = Some(!v);
            }
        }
        RowRef::Phase(w, p) => {
            let v = s.phase_open(w, p);
            if let Some(o) = s.workflows[w].open.get_mut(p) {
                *o = Some(!v);
            }
        }
        RowRef::Agent(a) => {
            let c = s.collapsed(a);
            s.agents[a].collapsed = Some(!c);
        }
        RowRef::WfAgent(..) => {}
    }
}

// ---------------------------------------------------------------- plain text

/// The boot screen, the wall, the first session and the HUD drawn off screen as colored HTML
/// pages (`boot.html`, `wall.html`, `session.html`, `hud.html` in `dir`), for screenshots.
pub fn snapshot_html(world: &World, opts: &Opts, w: u16, h: u16, dir: &std::path::Path) -> io::Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(opts, false);
    let mut term = Terminal::new(TestBackend::new(w, h))?;
    app.engines.poll();
    app.started = Instant::now().checked_sub(Duration::from_millis(1100)).unwrap_or_else(Instant::now);
    app.hud.settle = true;
    // harness: allow write (developer snapshot, hidden --snapshot-html)
    std::fs::create_dir_all(dir)?;
    let frames = [
        ("boot", true, View::Wall),
        ("wall", false, View::Wall),
        ("session", false, View::Session),
        ("hud", false, View::Hud),
        ("hud-lock", false, View::Hud),
        ("hud-flat", false, View::Hud),
        ("director", false, View::Hud),
        ("city", false, View::City),
        ("help", false, View::Wall),
    ];
    for (name, boot, view) in frames {
        app.boot = boot;
        app.view = view;
        app.help = name == "help";
        if name == "wall" {
            // A sample notice, to check its look; it only exists in these screenshots.
            let t = app.t();
            let born = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
            app.toasts = vec![Toast { born, icon: "◆", text: "agent done · Audit the test suite · api".into(), color: t.agent, count: 1 }];
        } else {
            app.toasts.clear();
        }
        if name == "hud-lock" {
            // Lock the first session: core, then the first session in Tab order.
            app.hud.on_key(KeyCode::Tab);
            app.hud.on_key(KeyCode::Tab);
        }
        if name == "hud-flat" {
            app.hud.on_key(KeyCode::Esc);
            app.hud.set_flat(true);
            app.hud.settle = true;
        }
        if name == "director" {
            app.hud.set_flat(false);
            app.hud.director_on(true);
            app.hud.settle = true;
        }
        term.draw(|f| draw(f, &mut app, world))?;
        // harness: allow write (developer snapshot, hidden --snapshot-html)
        std::fs::write(dir.join(format!("{name}.html")), buffer_html(term.backend().buffer()))?;
    }
    Ok(())
}

fn buffer_html(buf: &Buffer) -> String {
    let mut out = String::from(
        "<!doctype html><meta charset=utf-8><style>body{margin:0;background:#121212}\
         pre{margin:0;font:15px/18px Menlo,'Apple Braille',monospace;letter-spacing:0}\
         span{display:inline-block;width:9px;height:18px;text-align:center;overflow:hidden;vertical-align:top}</style><pre>",
    );
    let area = buf.area;
    for y in 0..area.height {
        for x in 0..area.width {
            let Some(c) = buf.cell((x, y)) else { continue };
            let (fg, bg) = (css(c.fg, (208, 208, 208)), css(c.bg, (18, 18, 18)));
            let bold = if c.modifier.contains(Modifier::BOLD) { ";font-weight:bold" } else { "" };
            // Braille is drawn as its raised dots only, the way terminals show it (the browser's
            // Braille font also outlines the unraised ones).
            if let Some(bits) = c.symbol().chars().next().map(|ch| ch as u32).filter(|&u| (0x2801..=0x28FF).contains(&u)).map(|u| u - 0x2800) {
                const AT: [(u32, f32, f32); 8] = [
                    (0x01, 2.5, 2.5),
                    (0x02, 2.5, 6.8),
                    (0x04, 2.5, 11.1),
                    (0x40, 2.5, 15.4),
                    (0x08, 6.5, 2.5),
                    (0x10, 6.5, 6.8),
                    (0x20, 6.5, 11.1),
                    (0x80, 6.5, 15.4),
                ];
                let _ = write!(out, "<span style=\"background:{bg}\"><svg width=9 height=18>");
                for (bit, x, y) in AT {
                    if bits & bit != 0 {
                        let _ = write!(out, "<circle cx={x} cy={y} r=1.25 fill=\"{fg}\"/>");
                    }
                }
                out.push_str("</svg></span>");
                continue;
            }
            let sym = match c.symbol() {
                "<" => "&lt;",
                ">" => "&gt;",
                "&" => "&amp;",
                " " | "" => "&nbsp;",
                s => s,
            };
            let _ = write!(out, "<span style=\"color:{fg};background:{bg}{bold}\">{sym}</span>");
        }
        out.push('\n');
    }
    out.push_str("</pre>");
    out
}

/// A ratatui color as CSS, resolving the xterm 256-color palette.
fn css(c: Color, default: (u8, u8, u8)) -> String {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    let (r, g, b) = match c {
        Color::Reset => default,
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) if i < 16 => BASE[i as usize],
        Color::Indexed(i) if i < 232 => {
            let i = i - 16;
            let l = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (l(i / 36), l((i / 6) % 6), l(i % 6))
        }
        Color::Indexed(i) => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
        Color::Black => BASE[0],
        Color::Red => BASE[1],
        Color::Green => BASE[2],
        Color::Yellow => BASE[3],
        Color::Blue => BASE[4],
        Color::Magenta => BASE[5],
        Color::Cyan => BASE[6],
        Color::Gray => BASE[7],
        Color::DarkGray => BASE[8],
        Color::LightRed => BASE[9],
        Color::LightGreen => BASE[10],
        Color::LightYellow => BASE[11],
        Color::LightBlue => BASE[12],
        Color::LightMagenta => BASE[13],
        Color::LightCyan => BASE[14],
        Color::White => BASE[15],
    };
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Draws `n` frames of the hologram off screen and reports the time each took, to check the
/// frame budget (33 ms at 30 frames per second).
pub fn bench_hud(world: &mut World, opts: &Opts, w: u16, h: u16, n: usize) -> io::Result<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(opts, false);
    let mut term = Terminal::new(TestBackend::new(w, h))?;
    app.boot = false;
    app.view = View::Hud;
    let mut times = Vec::with_capacity(n);
    for k in 0..n {
        if k % 8 == 0 {
            world.tick();
        }
        let start = Instant::now();
        term.draw(|f| draw(f, &mut app, world))?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    let avg = times.iter().sum::<f64>() / n as f64;
    Ok(format!("{n} frames at {w}x{h}: average {avg:.2} ms, median {:.2} ms, slowest {:.2} ms", times[n / 2], times[n - 1]))
}

/// One frame of the wall and of the first session, drawn off screen and printed as text.
pub fn snapshot(world: &World, opts: &Opts, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = App::new(opts, false);
    // One poll each: a headless snapshot shows the pulse the run loop would have gathered.
    app.engines.poll();
    let mut out = String::new();
    let Ok(mut term) = Terminal::new(TestBackend::new(w, h)) else { return out };
    // The boot screen half-way through its animation, then every view.
    app.started = Instant::now().checked_sub(Duration::from_millis(1100)).unwrap_or_else(Instant::now);
    app.hud.settle = true;
    for (boot, view) in [(true, View::Wall), (false, View::Wall), (false, View::Session), (false, View::Hud), (false, View::City)] {
        app.boot = boot;
        app.view = view;
        if term.draw(|f| draw(f, &mut app, world)).is_err() {
            break;
        }
        let buf = term.backend().buffer();
        for y in 0..h {
            let line: String = (0..w).map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol())).collect();
            let _ = writeln!(out, "{}", line.trim_end());
        }
        out.push('\n');
    }
    out
}

/// Every session, fully expanded, as plain text.
pub fn dump(world: &mut World, opts: &Opts) -> String {
    // The engine pulse is asked for by --engines; one poll here, so a headless dump shows it.
    let mut engines = opts.engines.as_deref().map(Engines::load).unwrap_or_else(Engines::none);
    engines.poll();
    let order = order(world);
    for &si in &order {
        world.sessions[si].set_all_expanded(true);
    }
    let t = &THEMES[0];
    let cx = Ctx { t, now: world.now, frame: 0, fx: false, marks: None, world: Some(world) };
    let mut out = String::new();
    if order.is_empty() {
        out.push_str("no running opencode session\n");
    }
    for &si in &order {
        let s = &world.sessions[si];
        let tk = s.tokens();
        let _ = writeln!(
            out,
            "== {} [{}] {} · {} · in {} · cache {} · out {}",
            s.project(),
            if s.alive { s.status.as_str() } else { "ended" },
            s.name,
            s.title,
            fmt_n(tk.input),
            fmt_n(tk.cache_read),
            fmt_n(tk.output)
        );
        for r in rows(s) {
            let line = row_line(s, &r, &cx, 150);
            let text: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
            let _ = writeln!(out, "{}", text.trim_end());
        }
        for (server, st) in &s.mcp {
            let _ = writeln!(out, "   ⇄ {} · {} calls · {} errors · last {}", server_label(server), st.calls, st.errors, st.last_tool);
        }
        if !s.skills.is_empty() {
            let names: Vec<String> = s.skills.iter().map(|(n, c)| format!("{n}×{c}")).collect();
            let _ = writeln!(out, "   ✦ {}", names.join("  "));
        }
        let now = world.now;
        let fc = crate::insight::forecast(world, s);
        let _ = writeln!(
            out,
            "   ◉ cache {} · writes {} reads {} fresh {} (5 min) · ≈ ${:.2}{} · {} compactions · ctx {}/{}{}{}",
            if s.cache_r.sum(now, 64) + s.cache_w.sum(now, 64) == 0 {
                "–".into()
            } else {
                s.cache_ratio(now, 60).map_or("–".into(), |r| format!("{:.0}%", r * 100.0))
            },
            fmt_n(s.cache_w.sum(now, 60)),
            fmt_n(s.cache_r.sum(now, 60)),
            fmt_n(s.fresh_in.sum(now, 60)),
            s.cost_micros as f64 / 1e6,
            if s.unpriced > 0 { format!(" (+{} tokens unpriced)", fmt_n(s.unpriced)) } else { String::new() },
            s.compactions.len(),
            fmt_n(fc.ctx),
            fmt_n(fc.limit),
            if fc.known { "" } else { " (guess)" },
            fc.calls.map_or(String::new(), |c| format!(" · compaction in ~{c:.0} calls ({} per call)", fmt_n(fc.per_call as u64))),
        );
        let w: Vec<String> = s.waves.iter().map(|a| fmt_dur(a.sum(now, 64) as i64)).collect();
        let _ = writeln!(out, "   ∿ think {} · write {} · tool {} (5 min) · {} files touched", w[0], w[1], w[2], s.files.len());
        for (w, wf) in s.workflows.iter().enumerate() {
            if let Some(r) = crate::insight::race(world, s, w) {
                let _ = writeln!(
                    out,
                    "   △ {} vs {}{} · elapsed {} · phase {:?} · delta {} · eta {}",
                    wf.name,
                    r.ghost.name,
                    if r.same_name { "" } else { " (same phases)" },
                    fmt_dur(r.elapsed),
                    r.phase,
                    fmt_dur(r.delta),
                    fmt_dur(r.eta)
                );
            }
        }
        out.push('\n');
    }
    for a in crate::insight::alerts(world) {
        let _ = writeln!(out, "⚠ {} · {}", world.sessions[a.si].project(), a.short());
    }
    for c in crate::insight::collisions(world) {
        let who: Vec<&str> = c.sessions().iter().map(|&si| world.sessions[si].project()).collect();
        let _ = writeln!(out, "✖ collision {} · {}", c.path, who.join(" + "));
    }
    let _ = writeln!(out, "{} past workflow runs on record", world.history.len());
    for strip in engines.strips() {
        let _ = writeln!(out, "⚡ {strip}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_notice_twice_shows_once_with_a_count() {
        let mut toasts = Vec::new();
        for _ in 0..4 {
            push_toast(&mut toasts, Toast::new("⚠", "Bash failing ×3 · api".into(), Color::Yellow));
        }
        push_toast(&mut toasts, Toast::new("◆", "agent done · Audit · api".into(), Color::Magenta));
        assert_eq!(toasts.len(), 2);
        assert_eq!(toasts[0].count, 4);
        assert!(toasts[0].born.elapsed() >= Duration::from_millis(250), "a counted notice does not slide in again");
    }

    /// The moment every golden screen shows.
    const FIXED: i64 = 1_790_000_000_000;

    fn demo_world() -> World {
        let mut world = World::demo_at(FIXED);
        world.run_to_end();
        world
    }

    /// One screen of the demo at the fixed moment, as text.
    fn render(world: &World, name: &str, w: u16, h: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let opts = crate::Opts::new(std::path::PathBuf::from("/nonexistent"));
        let mut app = App::new(&opts, false);
        app.boot = false;
        app.clock = Some(4_200);
        app.hud.settle = true;
        app.hud.frozen = Some(12.5);
        match name {
            "wall" => {}
            "session" => {
                app.view = View::Session;
                app.session = Some(world.sessions[0].id.clone());
            }
            "hud" => app.view = View::Hud,
            "hud-flat" => {
                app.view = View::Hud;
                app.hud.set_flat(true);
            }
            "city" => app.view = View::City,
            "help" => app.help = true,
            other => panic!("no screen named {other}"),
        }
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        term.draw(|f| draw(f, &mut app, world)).expect("draw");
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string() + "\n").collect()
    }

    fn is_dot(c: char) -> bool {
        ('\u{2800}'..='\u{28ff}').contains(&c)
    }

    /// Compares `got` with its golden copy in tests/golden/. Text must match exactly. Braille dots
    /// may move in a few cells: floating point is not bit-identical across systems.
    fn check_golden(name: &str, got: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.txt"));
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().expect("golden folder")).expect("create tests/golden");
            std::fs::write(&path, got).expect("write golden copy");
            return;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{} is missing: run `UPDATE_GOLDEN=1 cargo test`, then read the new file before committing it", path.display()))
            .replace('\r', "");
        let (wl, gl): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
        let blank = |c: char| c == ' ' || c == '\u{2800}';
        let (mut changed, mut dots, mut moved) = (Vec::new(), 0usize, 0usize);
        for i in 0..wl.len().max(gl.len()) {
            let (a, b): (Vec<char>, Vec<char>) = (wl.get(i).unwrap_or(&"").chars().collect(), gl.get(i).unwrap_or(&"").chars().collect());
            let mut text_differs = false;
            for k in 0..a.len().max(b.len()) {
                let (x, y) = (a.get(k).copied().unwrap_or(' '), b.get(k).copied().unwrap_or(' '));
                if is_dot(x) || is_dot(y) {
                    dots += 1;
                    if x != y {
                        // A dot that moved, appeared or vanished is jitter; a dot that became text is not.
                        if (is_dot(x) || blank(x)) && (is_dot(y) || blank(y)) {
                            moved += 1;
                        } else {
                            text_differs = true;
                        }
                    }
                } else if x != y {
                    text_differs = true;
                }
            }
            if text_differs {
                changed.push(format!("  line {:>2}\n    golden: {}\n    now:    {}", i + 1, wl.get(i).unwrap_or(&""), gl.get(i).unwrap_or(&"")));
            }
        }
        let allowed = (dots / 50).max(6);
        if !changed.is_empty() || moved > allowed {
            panic!(
                "{name}: the screen changed ({} lines of text differ; {moved} of {dots} dot cells moved, {allowed} allowed).\n{}\n\n\
                 If the change is intended: run `UPDATE_GOLDEN=1 cargo test`, read `git diff tests/golden`, and say in the commit why the screen changed.\n\
                 If it is not: the last change broke this screen.",
                changed.len(),
                changed.into_iter().take(12).collect::<Vec<_>>().join("\n")
            );
        }
    }

    #[test]
    fn screens_match_their_golden_copies() {
        let world = demo_world();
        for (w, h) in [(120, 36), (170, 50)] {
            for name in ["wall", "session", "hud", "hud-flat", "city", "help"] {
                check_golden(&format!("{name}-{w}x{h}"), &render(&world, name, w, h));
            }
        }
    }

    #[test]
    fn readings_match_their_golden_copy() {
        let mut world = demo_world();
        check_golden("demo-dump", &dump(&mut world, &crate::Opts::new(std::path::PathBuf::new())));
    }

    #[test]
    fn every_screen_survives_small_and_odd_terminals() {
        let world = demo_world();
        for (w, h) in [(30, 10), (40, 12), (61, 17), (80, 24), (99, 30), (300, 90)] {
            for name in ["wall", "session", "hud", "hud-flat", "city", "help"] {
                let text = render(&world, name, w, h);
                assert_eq!(text.lines().count(), h as usize, "{name} at {w}x{h} draws every row");
            }
        }
    }
}
