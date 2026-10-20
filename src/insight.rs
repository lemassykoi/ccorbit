//! Readings drawn from all sessions at once: calls that run far longer than usual or repeat
//! themselves, files that two sessions change at the same time, and how a running workflow
//! compares with an earlier run of the same kind.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::model::{Forecast, Hist, Item, PastRun, Session, Step};
use crate::source::World;

/// The same call this many times in a row is a loop.
const LOOP_RUN: usize = 4;
/// The same tool failing this many times in a row.
const FAIL_RUN: usize = 3;
/// Nothing shorter than this is ever called slow: builds and test runs legitimately take minutes.
const STALL_MIN_MS: i64 = 5 * 60_000;
/// Calls of a kind needed before its durations count as a baseline.
const STALL_SAMPLES: u32 = 5;
const RECENT_MS: i64 = 10 * 60_000;
const COLLIDE_MS: i64 = 15 * 60_000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Warn {
    /// Running `factor` times its usual (median) duration.
    Stall {
        factor: f64,
        usual: i64,
    },
    Loop {
        count: usize,
    },
    Failing {
        count: usize,
    },
}

pub struct Alert {
    pub si: usize,
    pub agent: usize,
    pub tool: String,
    pub label: String,
    pub warn: Warn,
}

impl Alert {
    /// `Bash 7× usual`, `Read ×5 in a row`, `Edit failing ×3`.
    pub fn short(&self) -> String {
        match self.warn {
            Warn::Stall { factor, .. } => format!("{} {:.0}× usual", self.label, factor),
            Warn::Loop { count } => format!("{} ×{count} in a row", self.label),
            Warn::Failing { count } => format!("{} failing ×{count}", self.label),
        }
    }
}

/// The largest automatic compaction seen with `model` in any session: where the model
/// compacts at the latest. It may compact earlier, so this is a ceiling, not a promise.
pub fn ceiling(world: &World, model: &str) -> Option<u64> {
    world.sessions.iter().flat_map(|s| &s.compactions).filter(|c| c.auto && c.model == model).map(|c| c.pre).max()
}

pub fn forecast(world: &World, s: &Session) -> Forecast {
    s.forecast(ceiling(world, &s.agents[0].model))
}

/// Every session's finished durations of `key`, merged.
fn baseline(world: &World, key: &str) -> Hist {
    let mut h = Hist::default();
    for s in &world.sessions {
        if let Some(d) = s.durations.get(key) {
            h.merge(d);
        }
    }
    h
}

/// Calls running unusually long, agents repeating one call, tools failing again and again.
/// Only for agents still at work, so a warning goes away once the agent moves on.
pub fn alerts(world: &World) -> Vec<Alert> {
    let now = world.now;
    let mut out = Vec::new();
    let mut cache: HashMap<String, Hist> = HashMap::new();
    for (si, s) in world.sessions.iter().enumerate() {
        if !s.alive {
            continue;
        }
        for a in 0..s.agents.len() {
            if !s.agent_running(a) {
                continue;
            }
            let steps: Vec<&Step> = s.agents[a]
                .items
                .iter()
                .rev()
                .filter_map(|i| match i {
                    Item::Step(st) => Some(st),
                    Item::Prompt { .. } => None,
                })
                .take(24)
                .collect();
            for st in steps.iter().filter(|st| st.ended.is_none() && st.bg.is_none()) {
                let Some(key) = &st.key else { continue };
                let took = now - st.started;
                if took < STALL_MIN_MS {
                    continue;
                }
                let h = *cache.entry(key.clone()).or_insert_with(|| baseline(world, key));
                if h.n < STALL_SAMPLES {
                    continue;
                }
                let (usual, p90) = (h.quantile(0.5), h.quantile(0.9));
                if took >= 8 * usual && took >= 3 * p90 {
                    out.push(Alert {
                        si,
                        agent: a,
                        tool: st.id.clone(),
                        label: st.name.clone(),
                        warn: Warn::Stall { factor: took as f64 / usual as f64, usual },
                    });
                }
            }
            let Some(last) = steps.first() else { continue };
            let recent = last.ended.is_none() || now - last.started < RECENT_MS;
            if !recent || last.key.is_none() {
                continue;
            }
            let same = steps.iter().take_while(|st| st.sig == last.sig).count();
            if same >= LOOP_RUN {
                out.push(Alert { si, agent: a, tool: last.id.clone(), label: last.name.clone(), warn: Warn::Loop { count: same } });
                continue;
            }
            let failing = steps.iter().filter(|st| st.ended.is_some()).take_while(|st| st.failed() && st.name == last.name).count();
            if failing >= FAIL_RUN {
                out.push(Alert { si, agent: a, tool: last.id.clone(), label: last.name.clone(), warn: Warn::Failing { count: failing } });
            }
        }
    }
    out
}

/// One recent edit of a file: session, agent, time.
type Edit = (usize, usize, i64);

/// A file changed by two sessions, or by two agents of one session both still at work, within
/// a quarter of an hour. Only sessions on the *same machine* can collide on a path: two hosts each
/// editing `/home/x/app.ts` are editing different files, so the path is qualified by host.
pub struct Collision {
    pub host: String,
    pub path: String,
    /// Session, agent, time of each recent edit.
    pub who: Vec<Edit>,
    pub at: i64,
}

impl Collision {
    pub fn sessions(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.who.iter().map(|w| w.0).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

pub fn collisions(world: &World) -> Vec<Collision> {
    let now = world.now;
    let mut by_path: BTreeMap<(&str, &str), Vec<Edit>> = BTreeMap::new();
    for (si, s) in world.sessions.iter().enumerate() {
        if !s.alive {
            continue;
        }
        for (path, f) in &s.files {
            if now - f.last_edit > COLLIDE_MS {
                continue;
            }
            for &(a, at) in f.editors.iter().filter(|(_, at)| now - at <= COLLIDE_MS) {
                by_path.entry((s.host.as_str(), path.as_str())).or_default().push((si, a, at));
            }
        }
    }
    let mut out: Vec<Collision> = by_path
        .into_iter()
        .filter(|(_, who)| {
            let sessions: HashSet<usize> = who.iter().map(|w| w.0).collect();
            if sessions.len() > 1 {
                return true;
            }
            // Agents of one session editing one after the other is normal; at once is not.
            let s = &world.sessions[who[0].0];
            let working: HashSet<usize> = who.iter().map(|w| w.1).filter(|&a| s.agent_running(a)).collect();
            working.len() > 1
        })
        .map(|((host, path), who)| Collision { host: host.to_string(), path: path.to_string(), at: who.iter().map(|w| w.2).max().unwrap_or(0), who })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.at));
    out
}

/// A running workflow raced against an earlier run: the same workflow if it ran before, or one
/// with the same phases.
pub struct Race<'a> {
    pub ghost: &'a PastRun,
    pub same_name: bool,
    /// Comparable runs found.
    pub runs: usize,
    pub elapsed: i64,
    /// The phase the run is in, and when each phase started and ended (offsets from the start).
    pub phase: Option<usize>,
    pub live: Vec<Option<(i64, Option<i64>)>>,
    /// Behind (positive) or ahead of the ghost, measured where the current phase started.
    pub delta: i64,
    /// Time left at the ghost's pace; negative once past it.
    pub eta: i64,
    pub finished: bool,
}

pub fn race<'a>(world: &'a World, s: &Session, w: usize) -> Option<Race<'a>> {
    let wf = s.workflows.get(w)?;
    let step = wf.step.as_deref().and_then(|t| s.step_by_tool(t))?;
    let started = step.started;
    let done = step.finished().filter(|_| !step.running());
    let candidates = |same: &dyn Fn(&PastRun) -> bool| -> Vec<&'a PastRun> {
        world
            .history
            .iter()
            .filter(|r| r.run_id != wf.run_id && r.status == "completed" && r.start + r.dur <= started && r.dur > 0)
            .filter(|r| same(r))
            .collect()
    };
    let by_name = candidates(&|r: &PastRun| r.name == wf.name);
    let (pool, same_name) = if !by_name.is_empty() {
        (by_name, true)
    } else if !wf.phases.is_empty() {
        (candidates(&|r: &PastRun| r.phases == wf.phases), false)
    } else {
        (Vec::new(), false)
    };
    let ghost = *pool.last()?;
    let n = wf.phases.len();
    let (mut begin, mut end, mut open) = (vec![None::<i64>; n], vec![None::<i64>; n], vec![false; n]);
    for &a in &wf.live {
        let ag = &s.agents[a];
        let Some(p) = wf.phase_of(&ag.wf_phase) else { continue };
        if ag.started == 0 {
            continue;
        }
        let b = ag.started - started;
        begin[p] = Some(begin[p].map_or(b, |x: i64| x.min(b)));
        if s.agent_running(a) {
            open[p] = true;
        } else {
            let e = ag.last_at - started;
            end[p] = Some(end[p].map_or(e, |x: i64| x.max(e)));
        }
    }
    let live: Vec<Option<(i64, Option<i64>)>> = (0..n).map(|p| begin[p].map(|b| (b, if open[p] { None } else { end[p] }))).collect();
    let phase = live.iter().rposition(Option::is_some);
    let delta = phase.and_then(|p| Some(live[p]?.0 - ghost.spans.get(p).copied().flatten()?.0)).unwrap_or(0);
    let elapsed = done.unwrap_or(world.now) - started;
    Some(Race { ghost, same_name, runs: pool.len(), elapsed, phase, live, delta, eta: ghost.dur + delta - elapsed, finished: done.is_some() })
}
