//! The director: on `d`, or after two minutes without input, the HUD camera runs itself like a
//! live broadcast. It flies to whatever just happened (a session waiting for you, two sessions
//! changing the same file, a failure, a slow or looping call, an agent or a workflow finishing),
//! holds the shot a few seconds under a caption, and between events slowly circles the whole
//! graph while the caption tours the sessions. An inset follows the busiest session.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::insight::{Alert, Collision};
use crate::model::{Feed, Kind};
use crate::parse::clip;
use crate::source::World;
use crate::theme::Theme;
use crate::time::{fmt_dur, fmt_n};
use crate::ui::vitals;

const SHOT_SECS: f64 = 7.0;
const TOUR_SECS: f64 = 6.0;

struct Shot {
    /// The node the camera follows; none for the wide view.
    node: Option<String>,
    title: String,
    sub: String,
    color: Color,
    born: Instant,
    secs: f64,
    prio: u8,
}

pub struct Director {
    pub on: bool,
    shot: Option<Shot>,
    queue: Vec<Shot>,
    /// Feed position per session, and warnings already shown, so each event is shot once.
    seen: HashMap<String, i64>,
    shown: HashSet<String>,
    tour: usize,
}

impl Director {
    pub fn new() -> Director {
        Director { on: false, shot: None, queue: Vec::new(), seen: HashMap::new(), shown: HashSet::new(), tour: 0 }
    }

    /// Starting over forgets what happened before: only what happens from now on is shot.
    pub fn set(&mut self, on: bool) {
        if on && !self.on {
            self.seen.clear();
            self.shown.clear();
            self.queue.clear();
            self.shot = None;
        }
        self.on = on;
    }

    /// Picks the shot and returns the node the camera should follow (none for the wide view).
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        world: &World,
        order: &[usize],
        names: &HashMap<usize, String>,
        alerts: &[Alert],
        collisions: &[Collision],
        ids: &HashSet<&str>,
        t: &Theme,
    ) -> Option<String> {
        let now = Instant::now();
        let first = self.seen.is_empty();
        let name = |si: usize| names.get(&si).cloned().unwrap_or_else(|| world.sessions[si].project().to_string());
        let mut found: Vec<Shot> = Vec::new();
        let mut shot = |node: String, prio: u8, title: String, sub: String, color: Color| {
            found.push(Shot { node: Some(node), title, sub, color, born: now, secs: SHOT_SECS, prio });
        };
        for &si in order {
            let s = &world.sessions[si];
            let sid = format!("s:{}", s.id);
            let latest = s.feed.back().map_or(0, |f| f.at);
            let seen = *self.seen.entry(s.id.clone()).or_insert(latest);
            for it in s.feed.iter().filter(|it| it.at > seen) {
                let node = it
                    .tool
                    .as_deref()
                    .and_then(|tool| s.step_by_tool(tool))
                    .and_then(|st| match st.kind {
                        Kind::Agent => st.child.map(|c| format!("a:{}:{c}", s.id)),
                        Kind::Workflow => st.workflow.map(|w| format!("w:{}:{w}", s.id)),
                        _ => None,
                    })
                    .filter(|id| ids.contains(id.as_str()))
                    .unwrap_or_else(|| sid.clone());
                let (prio, what, color) = match (it.feed, it.kind) {
                    (Feed::Failed, _) => (8, format!("{} failed", it.label), t.err),
                    (Feed::Done, Some(Kind::Workflow)) => (6, "workflow done".to_string(), t.workflow),
                    (Feed::Start, Some(Kind::Workflow)) => (5, "workflow launched".to_string(), t.workflow),
                    (Feed::Done, Some(Kind::Agent)) => (5, "agent done".to_string(), t.agent),
                    (Feed::Start, Some(Kind::Agent)) => (4, "agent launched".to_string(), t.agent),
                    (Feed::Prompt, _) => (3, "new prompt".to_string(), t.prompt),
                    (Feed::Answer, _) => (3, "the agent says".to_string(), t.fg),
                    _ => continue,
                };
                shot(node, prio, format!("{} · {what}", name(si)), it.text.clone(), color);
            }
            self.seen.insert(s.id.clone(), latest.max(seen));
            let key = format!("needs:{}", s.id);
            if s.needs_you() {
                if self.shown.insert(key) && !first {
                    let about = s.current().map(|(_, st)| st.label().1).unwrap_or_default();
                    shot(sid.clone(), 10, format!("{} · needs you", name(si)), about, t.warn);
                }
            } else {
                self.shown.remove(&key);
            }
        }
        for a in alerts {
            let s = &world.sessions[a.si];
            if self.shown.insert(format!("alert:{}:{}", s.id, a.tool)) && !first {
                let node = if a.agent == 0 { format!("s:{}", s.id) } else { format!("a:{}:{}", s.id, a.agent) };
                let node = if ids.contains(node.as_str()) { node } else { format!("s:{}", s.id) };
                shot(node, 7, format!("{} · ⚠ {}", name(a.si), a.short()), "an agent may be stuck".into(), t.warn);
            }
        }
        for c in collisions {
            if self.shown.insert(format!("collide:{}", c.path)) && !first {
                let who: Vec<String> = c.sessions().iter().map(|&si| name(si)).collect();
                if let Some(&si) = c.sessions().first() {
                    let node = format!("s:{}", world.sessions[si].id);
                    shot(node, 9, format!("✖ collision · {}", crate::parse::file_name(&c.path)), who.join(" + "), t.err);
                }
            }
        }
        self.queue.extend(found);
        self.queue.sort_by_key(|s| std::cmp::Reverse(s.prio));
        self.queue.truncate(8);
        let over = self.shot.as_ref().is_none_or(|s| now.duration_since(s.born).as_secs_f64() > s.secs);
        let urgent = match (&self.shot, self.queue.first()) {
            (Some(cur), Some(next)) => next.prio > cur.prio + 1,
            _ => false,
        };
        if over || urgent {
            self.shot = if self.queue.is_empty() { self.tour_shot(world, order, names, t, now) } else { Some(self.queue.remove(0)) };
        }
        self.shot.as_ref().and_then(|s| s.node.clone())
    }

    /// The wide view, its caption on the next session in turn.
    fn tour_shot(&mut self, world: &World, order: &[usize], names: &HashMap<usize, String>, t: &Theme, now: Instant) -> Option<Shot> {
        let live: Vec<usize> = order.iter().copied().filter(|&si| world.sessions[si].alive).collect();
        let si = *live.get(self.tour % live.len().max(1))?;
        self.tour += 1;
        let s = &world.sessions[si];
        let wnow = world.now;
        let doing = match s.phase_now() {
            Some((Some(p), since)) => format!("{} for {}", vitals::phase_name(Some(p)), fmt_dur(wnow - since)),
            Some((None, since)) => format!("answering for {}", fmt_dur(wnow - since)),
            None => s.status.clone(),
        };
        let title = format!("{} · {doing}", names.get(&si).cloned().unwrap_or_else(|| s.project().to_string()));
        let sub = format!(
            "⇡ {}/m  ⇣ {}/m  ·  cache {}  ·  {}/h  ·  {}",
            fmt_n(s.tok_up.per_min(wnow)),
            fmt_n(s.tok_down.per_min(wnow)),
            s.cache_ratio(wnow, 60).map_or("–".into(), |r| format!("{:.0}%", r * 100.0)),
            vitals::money(vitals::money_rate(s, wnow)),
            if s.title.is_empty() { s.name.clone() } else { s.title.clone() }
        );
        Some(Shot { node: None, title, sub, color: t.session(order.iter().position(|&i| i == si).unwrap_or(0)), born: now, secs: TOUR_SECS, prio: 0 })
    }

    /// The caption bar along the bottom and the inset on the busiest session.
    pub fn draw(&self, f: &mut Frame, area: Rect, t: &Theme, world: &World, order: &[usize], names: &HashMap<usize, String>) {
        let Some(shot) = &self.shot else { return };
        let w = area.width.saturating_sub(6);
        let bar = Rect::new(area.x + 3, area.bottom().saturating_sub(4), w, 2);
        f.render_widget(Clear, bar);
        let blink = (shot.born.elapsed().as_millis() / 500) % 2 == 0;
        let left = shot.born.elapsed().as_secs_f64() / shot.secs;
        let meter = 12usize;
        let done = ((1.0 - left.clamp(0.0, 1.0)) * meter as f64).round() as usize;
        let row1 = Line::from(vec![
            Span::styled(" ● ON AIR ", Style::new().fg(t.bg).bg(if blink { t.err } else { t.accent }).add_modifier(Modifier::BOLD)),
            Span::styled(" DIRECTOR ", Style::new().fg(t.bg).bg(t.dim).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(clip(&shot.title, w.saturating_sub(40) as usize), Style::new().fg(shot.color).add_modifier(Modifier::BOLD)),
        ]);
        let row2 = Line::from(vec![Span::styled(format!("  {}", clip(&shot.sub, w.saturating_sub(36) as usize)), Style::new().fg(t.fg))]);
        let style = Style::new().bg(t.surface);
        f.render_widget(Paragraph::new(vec![row1, row2]).style(style), bar);
        let right = Line::from(vec![
            Span::styled("━".repeat(done), Style::new().fg(shot.color)),
            Span::styled("─".repeat(meter - done), Style::new().fg(t.faint)),
            Span::styled("  d · exit ", Style::new().fg(t.faint)),
        ]);
        let rw = right.width() as u16;
        f.buffer_mut().set_line(bar.right().saturating_sub(rw + 1), bar.y, &right, rw);

        // The inset: the session moving the most tokens right now.
        let busiest = order
            .iter()
            .copied()
            .filter(|&si| world.sessions[si].alive)
            .max_by_key(|&si| world.sessions[si].tok_up.per_min(world.now) + world.sessions[si].tok_down.per_min(world.now));
        let Some(si) = busiest else { return };
        let s = &world.sessions[si];
        let (iw, ih) = (46.min(area.width / 2), 8u16);
        if area.height < ih + 8 {
            return;
        }
        let rect = Rect::new(area.right().saturating_sub(iw + 2), area.y + 2, iw, ih);
        f.render_widget(Clear, rect);
        let k = order.iter().position(|&i| i == si).unwrap_or(0);
        let name = names.get(&si).cloned().unwrap_or_else(|| s.project().to_string());
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(t.session(k))).style(Style::new().bg(t.surface)).title_top(
            Line::from(vec![
                Span::styled(" ◢ BUSIEST ", Style::new().fg(t.dim).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{name} "), Style::new().fg(t.session(k)).add_modifier(Modifier::BOLD)),
            ]),
        );
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let now = world.now;
        let iw = inner.width as usize;
        let step = match s.current() {
            Some((_, st)) => {
                let (label, text) = st.label();
                format!("{label} · {text}")
            }
            None => "thinking or writing the next step".into(),
        };
        let mut wave = vec![Span::raw(" ")];
        wave.extend(vitals::brainwave(s, now, (shot.born.elapsed().as_millis() / 90) as usize, iw.saturating_sub(2).min(60), None, t));
        let mut cache = vec![Span::raw(" ")];
        cache.extend(vitals::reactor(s, now, shot.born.elapsed().as_millis(), t, None));
        cache.push(Span::styled(format!("  {} · {}/h", vitals::money(s.cost_micros), vitals::money(vitals::money_rate(s, now))), Style::new().fg(t.dim)));
        let lines = vec![
            Line::from(Span::styled(format!(" {}", clip(&step, iw.saturating_sub(1))), Style::new().fg(t.fg))),
            Line::from(wave),
            Line::from(vec![
                Span::styled(format!(" ⇡ {}/m", fmt_n(s.tok_up.per_min(now))), Style::new().fg(t.accent)),
                Span::styled(format!("  ⇣ {}/m", fmt_n(s.tok_down.per_min(now))), Style::new().fg(t.hi)),
                Span::styled(format!("  ctx {}", fmt_n(s.agents[0].ctx)), Style::new().fg(t.dim)),
            ]),
            Line::from(cache),
            Line::from(Span::styled(format!(" › {}", clip(s.last_prompt().unwrap_or(""), iw.saturating_sub(3))), Style::new().fg(t.prompt))),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }
}
