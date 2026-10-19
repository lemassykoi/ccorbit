//! Small instruments read from a session's vital signs: the brainwave of what it spends its time
//! on, the prompt-cache reactor, the context gauge with its compaction forecast, and the race of
//! a running workflow against an earlier run.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::insight::Race;
use crate::model::{BUCKET_MS, Forecast, Phase, Session};
use crate::theme::Theme;
use crate::time::{fmt_dur, fmt_n};

/// Thinking, writing, waiting on tools, and an answer still in progress (kind not known yet).
pub fn phase_color(t: &Theme, p: Option<Phase>) -> Color {
    match p {
        Some(Phase::Think) => t.comms,
        Some(Phase::Write) => t.hi,
        Some(Phase::Tool) => t.shell,
        None => t.dim,
    }
}

pub fn phase_name(p: Option<Phase>) -> &'static str {
    match p {
        Some(Phase::Think) => "thinking",
        Some(Phase::Write) => "writing",
        Some(Phase::Tool) => "tools",
        None => "answering",
    }
}

/// Braille dot bit for a column (0 or 1) and a row (0 at the top to 3).
fn dot(col: usize, row: usize) -> u32 {
    const BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    BITS[col][row.min(3)]
}

/// One row of brainwave, a cell per 5-second bucket, oldest first. The trace swings as far as
/// the session worked in that bucket (agents add up) and takes the color of what dominated;
/// `only` keeps a single channel. The newest cell keeps moving while work is in progress.
pub fn brainwave(s: &Session, now: i64, frame: usize, n: usize, only: Option<Phase>, t: &Theme) -> Vec<Span<'static>> {
    if n == 0 {
        return Vec::new();
    }
    let (waves, pending) = s.waves_live(now, n);
    let mut out = Vec::with_capacity(n);
    let mut prev = 2usize;
    let newest = now.div_euclid(BUCKET_MS);
    // Each session gets its own trace: two busy sessions must not look identical.
    let seed = s.id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    for k in 0..n {
        // The trace is tied to absolute time, so it scrolls with the buckets instead of changing.
        let bucket = newest - (n - 1 - k) as i64;
        let parts = [waves[0][k], waves[1][k], waves[2][k], pending[k]];
        let (total, color) = match only {
            Some(p) => (parts[p as usize], phase_color(t, Some(p))),
            None => {
                let top = (0..4).max_by_key(|&i| parts[i]).unwrap_or(0);
                let p = [Some(Phase::Think), Some(Phase::Write), Some(Phase::Tool), None][top];
                (parts.iter().sum(), phase_color(t, p))
            }
        };
        if total == 0 {
            let cell = char::from_u32(0x2800 | dot(0, 2) | dot(1, 2) | span_bits(0, prev, 2)).unwrap_or('⠤');
            out.push(Span::styled(cell.to_string(), Style::new().fg(t.faint)));
            prev = 2;
            continue;
        }
        // Low activity stays visible; more than one agent at work saturates. A steady jitter per
        // bucket keeps a busy trace irregular, like a real one.
        let jitter = 0.55 + 0.45 * (((bucket as u64 ^ seed).wrapping_mul(2_654_435_761) >> 7) % 1000) as f64 / 1000.0;
        let amp = (total as f64 / BUCKET_MS as f64).min(1.0).sqrt() * jitter;
        let live = k == n - 1;
        let phase = bucket as f64 * 1.9 + (seed % 628) as f64 / 100.0 + if live { frame as f64 * 0.8 } else { 0.0 };
        let y = |a: f64| (1.5 - 1.6 * amp * a.sin()).round().clamp(0.0, 3.0) as usize;
        let (y0, y1) = (y(phase), y(phase + 0.95));
        let bits = span_bits(0, prev, y0) | span_bits(1, y0, y1);
        prev = y1;
        let cell = char::from_u32(0x2800 | bits).unwrap_or('⠒');
        let mut style = Style::new().fg(color);
        if amp > 0.7 {
            style = style.add_modifier(Modifier::BOLD);
        }
        out.push(Span::styled(cell.to_string(), style));
    }
    out
}

/// Dots of one column from row `a` to row `b`, so the trace reads as a continuous line.
fn span_bits(col: usize, a: usize, b: usize) -> u32 {
    (a.min(b)..=a.max(b)).fold(0, |bits, r| bits | dot(col, r))
}

/// `◉ 97%`: share of the input served from the prompt cache over the last five minutes. The
/// core flares (`✺`) for a few seconds when the cache is rebuilt or warmed up.
pub fn reactor(s: &Session, now: i64, ms: u128, t: &Theme, engine: Option<f64>) -> Vec<Span<'static>> {
    let flare = s.flares.back().is_some_and(|&(at, _)| now >= at && now - at < 8000);
    // opencode never reports what the engine's prefix cache actually hit: when the session has
    // no cache reading at all, the engine's own share (polled with --engines) is the truth, and
    // with no engines the honest answer is `–`, not a permanent red 0%.
    let seen = s.cache_r.sum(now, 64) + s.cache_w.sum(now, 64) > 0;
    let Some(r) = (if seen { s.cache_ratio(now, 60) } else { engine.map(|p| p / 100.0) }) else {
        return vec![Span::styled("◌ –", Style::new().fg(t.faint))];
    };
    let color = if r >= 0.9 {
        t.ok
    } else if r >= 0.7 {
        t.warn
    } else {
        t.err
    };
    let core = if flare {
        let on = (ms / 160).is_multiple_of(2);
        Span::styled("✺", Style::new().fg(if on { t.accent } else { t.hi }).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("◉", Style::new().fg(color))
    };
    vec![core, Span::styled(format!(" {:.0}%", r * 100.0), Style::new().fg(color))]
}

/// `≈$185`: the estimated price of the session, rounded to the dollar (cents would claim a
/// precision the estimate has not earned). Under a dollar the cents stay, or it reads as free.
pub fn money(micros: u64) -> String {
    let d = micros as f64 / 1e6;
    if d >= 1000.0 {
        format!("≈${:.1}k", d / 1000.0)
    } else if d >= 1.0 {
        format!("≈${d:.0}")
    } else {
        format!("≈${d:.2}")
    }
}

/// Price per hour at the pace of the last five minutes.
pub fn money_rate(s: &Session, now: i64) -> u64 {
    s.cost.sum(now, 60) * 12
}

/// Context fill against the compaction ceiling, then the percentage, or `⟳~12` (calls left at
/// the current pace) once compaction is near.
pub fn ctx_gauge(fc: &Forecast, width: usize, t: &Theme) -> Vec<Span<'static>> {
    let ratio = fc.ctx as f64 / fc.limit.max(1) as f64;
    let filled = ((ratio * width as f64).round() as usize).min(width);
    let color = if ratio > 0.85 {
        t.err
    } else if ratio > 0.65 {
        t.warn
    } else {
        t.fg
    };
    let mut v = vec![Span::styled("█".repeat(filled), Style::new().fg(color)), Span::styled("░".repeat(width - filled), Style::new().fg(t.faint))];
    match fc.calls.filter(|&c| c < 30.0) {
        Some(c) => v.push(Span::styled(format!(" ⟳~{:.0}", c.max(0.0)), Style::new().fg(if c < 5.0 { t.err } else { t.warn }).add_modifier(Modifier::BOLD))),
        None => v.push(Span::styled(format!(" {:>2}%", (ratio * 100.0) as u32), Style::new().fg(t.dim))),
    }
    v
}

/// One line about the next compaction: the ceiling, how near it is, how many happened.
pub fn compaction_text(s: &Session, fc: &Forecast, now: i64) -> String {
    let mut out = format!("{} / {} {}", fmt_n(fc.ctx), fmt_n(fc.limit), if fc.known { "ceiling (seen)" } else { "window (guessed)" });
    match (fc.calls, fc.secs) {
        (Some(c), Some(secs)) => out.push_str(&format!(" · in ~{c:.0} calls, ~{}", fmt_dur((secs * 1000.0) as i64))),
        (Some(c), None) => out.push_str(&format!(" · in ~{c:.0} calls")),
        _ => {}
    }
    if let Some(last) = s.compactions.last() {
        let n = s.compactions.len();
        out.push_str(&format!(" · {n} compaction{}, last at {} {} ago", if n == 1 { "" } else { "s" }, fmt_n(last.pre), fmt_dur(now - last.at)));
    }
    out
}

/// `△ 8m behind · ETA 12m` for a running race, `△ 3m faster` once it is over.
pub fn race_chip(r: &Race) -> String {
    if r.finished {
        let d = r.elapsed - r.ghost.dur;
        return format!("△ {} {}", fmt_dur(d.abs()), if d <= 0 { "faster" } else { "slower" });
    }
    let pace = match r.delta {
        d if d > 30_000 => format!("{} behind", fmt_dur(d)),
        d if d < -30_000 => format!("{} ahead", fmt_dur(-d)),
        _ => "on pace".into(),
    };
    if r.eta >= 0 { format!("△ {pace} · ETA {}", fmt_dur(r.eta)) } else { format!("△ {} past the ghost", fmt_dur(-r.eta)) }
}

/// Two lanes on one time scale: this run (solid) and the earlier one (shaded), each phase in
/// its own shade, with the ghost's position now marked `◆`.
pub fn race_lanes(r: &Race, phases: usize, width: usize, t: &Theme) -> Vec<Line<'static>> {
    let span = r.ghost.dur.max(r.elapsed).max(1) as f64 * 1.05;
    let col = |ms: i64| ((ms.max(0) as f64 / span) * width as f64).floor() as usize;
    let shade = |p: usize| t.grad(if phases > 1 { p as f64 / (phases - 1) as f64 } else { 0.0 });
    let lane = |spans: Vec<(usize, i64, i64)>, glyph: &str, mark: Option<usize>| -> Line<'static> {
        let mut cells: Vec<Span<'static>> = (0..width).map(|_| Span::styled("·", Style::new().fg(t.faint))).collect();
        for (p, a, b) in spans {
            let (from, to) = (col(a), col(b).max(col(a) + 1).min(width));
            for cell in cells.iter_mut().take(to).skip(from) {
                *cell = Span::styled(glyph.to_string(), Style::new().fg(shade(p)));
            }
        }
        if let Some(m) = mark.filter(|&m| m < width) {
            cells[m] = Span::styled("◆", Style::new().fg(t.hi).add_modifier(Modifier::BOLD));
        }
        Line::from(cells)
    };
    let live: Vec<(usize, i64, i64)> = r.live.iter().enumerate().filter_map(|(p, s)| s.map(|(a, b)| (p, a, b.unwrap_or(r.elapsed)))).collect();
    let ghost: Vec<(usize, i64, i64)> = r.ghost.spans.iter().enumerate().filter_map(|(p, s)| s.map(|(a, b)| (p, a, b))).collect();
    let now = (!r.finished).then(|| col(r.elapsed));
    vec![lane(live, "█", None), lane(ghost, "▒", now)]
}
