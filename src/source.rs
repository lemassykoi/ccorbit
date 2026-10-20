//! Where events come from: the live opencode sessions on this machine (through `opencode`),
//! or a recorded transcript played back at speed. Both only read files.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use crate::demo::Demo;
use crate::hosts::Hosts;
use crate::model::{PastRun, Session};
use crate::opencode::Open;
use crate::time::{now_ms, parse_iso_ms};

pub struct World {
    pub sessions: Vec<Session>,
    pub now: i64,
    pub mode: Mode,
    /// Finished workflow runs of every project, oldest first. The opencode source has none.
    pub history: Vec<PastRun>,
    /// A fixed clock instead of the real one, so tests see the same moment every time.
    pub frozen: Option<i64>,
    /// Remote opencode hosts polled into the same wall (empty without `--hosts`).
    pub hosts: Hosts,
}

pub enum Mode {
    Opencode(Open),
    Replay(Replay),
    Demo(Demo),
}

pub struct Replay {
    events: Vec<Ev>,
    files: Vec<File>,
    next: usize,
    sim: f64,
    last: Instant,
    pub speed: f64,
    pub paused: bool,
}

struct Ev {
    at: i64,
    what: EvKind,
}

enum EvKind {
    Line { agent: usize, file: usize, off: u64, len: usize },
    Workflow(PathBuf),
}

impl World {
    /// The live source: this machine's opencode sessions from its storage database, plus any
    /// remote hosts listed in `--hosts` (folded into the same wall, keyed by host-qualified ids).
    pub fn opencode(db_path: PathBuf, hosts_file: Option<PathBuf>) -> World {
        let hosts = hosts_file.map(|p| Hosts::load(&p)).unwrap_or_else(Hosts::none);
        World { sessions: Vec::new(), now: now_ms(), history: Vec::new(), mode: Mode::Opencode(Open::watch(db_path)), frozen: None, hosts }
    }

    /// Made-up sessions, generated as they go (`--demo`).
    pub fn demo() -> World {
        let now = now_ms();
        let (demo, sessions, history) = Demo::start(now);
        World { sessions, now, history, mode: Mode::Demo(demo), frozen: None, hosts: Hosts::none() }
    }

    /// The demo at a fixed moment that never moves: the same screens every time, for tests.
    #[cfg(test)]
    pub fn demo_at(now: i64) -> World {
        let (demo, sessions, history) = Demo::start(now);
        World { sessions, now, history, mode: Mode::Demo(demo), frozen: Some(now), hosts: Hosts::none() }
    }

    /// A world of caller-supplied sessions at a frozen moment, for readings tests (collisions ...).
    #[cfg(test)]
    pub fn probe(sessions: Vec<Session>, now: i64) -> World {
        let mode = Mode::Replay(Replay { events: Vec::new(), files: Vec::new(), next: 0, sim: 0.0, last: Instant::now(), speed: 1.0, paused: false });
        World { sessions, now, history: Vec::new(), mode, frozen: Some(now), hosts: Hosts::none() }
    }

    pub fn is_demo(&self) -> bool {
        matches!(self.mode, Mode::Demo(_))
    }

    /// The opencode database being followed, for status lines (none in demo and replay).
    pub fn db_path(&self) -> Option<&Path> {
        match &self.mode {
            Mode::Opencode(oc) => Some(&oc.db_path),
            _ => None,
        }
    }

    /// Indexes a transcript and its subagent and workflow files for playback.
    pub fn replay(path: &Path, speed: f64) -> io::Result<World> {
        let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("replay").to_string();
        let dir = path.with_extension("");
        let mut s = Session::new(&id);
        s.name = "replay".into();
        s.status = "replay".into();
        s.alive = true;
        let mut files = Vec::new();
        let mut events = Vec::new();
        index_file(path, 0, &mut files, &mut events, Some(&mut s.cwd))?;
        let sub = dir.join("subagents");
        let mut metas: Vec<PathBuf> = read_dir_paths(&sub).into_iter().filter(|p| name_of(p).ends_with(".meta.json")).collect();
        metas.sort();
        for meta in metas {
            let Some(id) = name_of(&meta).strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")).map(str::to_string) else { continue };
            let Some(v) = read_json(&meta) else { continue };
            let a = s.add_agent(&id, str_of(&v, "agentType"), str_of(&v, "description"), v.get("toolUseId").and_then(Value::as_str).map(str::to_string));
            let jsonl = sub.join(format!("agent-{id}.jsonl"));
            if jsonl.exists() {
                index_file(&jsonl, a, &mut files, &mut events, None)?;
            }
        }
        for wf in read_dir_paths(&dir.join("workflows")) {
            if wf.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(v) = read_json(&wf) else { continue };
            let at = v.get("startTime").and_then(Value::as_i64).unwrap_or(0) + v.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
            events.push(Ev { at, what: EvKind::Workflow(wf) });
        }
        events.sort_by_key(|e| e.at);
        let start = events.first().map_or(0, |e| e.at);
        Ok(World {
            sessions: vec![s],
            now: start,
            history: Vec::new(),
            mode: Mode::Replay(Replay { events, files, next: 0, sim: start as f64, last: Instant::now(), speed, paused: false }),
            frozen: None,
            hosts: Hosts::none(),
        })
    }

    /// Reads whatever is new. Returns whether anything changed.
    pub fn tick(&mut self) -> bool {
        let clock = self.frozen.unwrap_or_else(now_ms);
        let mut changed = match &mut self.mode {
            Mode::Opencode(oc) => {
                self.now = clock;
                oc.tick(&mut self.sessions, self.now)
            }
            Mode::Replay(r) => replay_tick(r, &mut self.sessions, &mut self.now, false),
            Mode::Demo(d) => {
                self.now = clock;
                d.tick(self.now, &mut self.sessions, &mut self.history)
            }
        };
        // Remote hosts fold into the same wall; a no-op without --hosts (and not run in replay/demo).
        if matches!(self.mode, Mode::Opencode(_)) {
            let now = self.now;
            changed |= self.hosts.tick(&mut self.sessions, now);
        }
        changed
    }

    /// Loads everything available now: one live poll, or the whole replay.
    pub fn run_to_end(&mut self) {
        let clock = self.frozen.unwrap_or_else(now_ms);
        match &mut self.mode {
            Mode::Opencode(oc) => {
                self.now = clock;
                oc.tick(&mut self.sessions, self.now);
                // A second poll lets rows whose stream settled in between arrive.
                oc.tick(&mut self.sessions, self.now);
                let now = self.now;
                self.hosts.tick(&mut self.sessions, now);
            }
            Mode::Replay(r) => {
                replay_tick(r, &mut self.sessions, &mut self.now, true);
            }
            Mode::Demo(d) => {
                self.now = clock;
                d.tick(self.now, &mut self.sessions, &mut self.history);
            }
        }
    }

    pub fn replay_state(&self) -> Option<&Replay> {
        match &self.mode {
            Mode::Replay(r) => Some(r),
            _ => None,
        }
    }

    pub fn replay_mut(&mut self) -> Option<&mut Replay> {
        match &mut self.mode {
            Mode::Replay(r) => Some(r),
            _ => None,
        }
    }

    pub fn animating(&self) -> bool {
        match &self.mode {
            Mode::Replay(r) => !r.done() && !r.paused,
            Mode::Opencode(_) | Mode::Demo(_) => self.sessions.iter().any(Session::busy),
        }
    }
}

impl Replay {
    pub fn done(&self) -> bool {
        self.next >= self.events.len()
    }

    pub fn progress(&self) -> f64 {
        if self.events.is_empty() { 1.0 } else { self.next as f64 / self.events.len() as f64 }
    }
}

fn replay_tick(r: &mut Replay, sessions: &mut [Session], now: &mut i64, to_end: bool) -> bool {
    let dt = r.last.elapsed().as_secs_f64() * 1000.0;
    r.last = Instant::now();
    let Some(s) = sessions.first_mut() else { return false };
    if r.done() {
        s.alive = false;
        return false;
    }
    if to_end {
        r.sim = f64::MAX;
    } else if !r.paused {
        r.sim += dt * r.speed;
        // Skip idle stretches: never wait more than 1.5 s of real time for the next event.
        let max_gap = 1500.0 * r.speed;
        let next_at = r.events[r.next].at as f64;
        if next_at - r.sim > max_gap {
            r.sim = next_at - max_gap;
        }
    }
    let mut changed = false;
    while r.next < r.events.len() && r.events[r.next].at as f64 <= r.sim {
        match &r.events[r.next].what {
            EvKind::Line { agent, file, off, len } => {
                if let Some(line) = read_at(&mut r.files[*file], *off, *len) {
                    s.ingest(*agent, &line);
                }
            }
            EvKind::Workflow(path) => {
                if let Some(v) = read_json(path) {
                    s.on_workflow_json(&v);
                }
            }
        }
        *now = r.events[r.next].at;
        r.next += 1;
        changed = true;
    }
    if !to_end && r.sim > *now as f64 && r.sim < f64::MAX {
        *now = r.sim as i64;
    }
    if r.done() {
        s.alive = false;
        s.status = "replayed".into();
    }
    changed
}

/// Records the offset and timestamp of every line; the lines themselves are read again on playback.
fn index_file(path: &Path, agent: usize, files: &mut Vec<File>, events: &mut Vec<Ev>, mut cwd: Option<&mut String>) -> io::Result<()> {
    let file_idx = files.len();
    let mut reader = BufReader::new(File::open(path)?);
    let first = events.len();
    let (mut off, mut last_at) = (0u64, 0i64);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
        let v = serde_json::from_slice::<Value>(line).ok();
        if let (Some(c), Some(v)) = (cwd.as_deref_mut(), &v)
            && c.is_empty()
        {
            c.push_str(str_of(v, "cwd"));
        }
        let at = v.as_ref().and_then(|v| v.get("timestamp")).and_then(Value::as_str).and_then(parse_iso_ms).unwrap_or(last_at);
        last_at = at;
        if !line.is_empty() {
            events.push(Ev { at, what: EvKind::Line { agent, file: file_idx, off, len: line.len() } });
        }
        off += n as u64;
    }
    // Leading records without a timestamp take the first one that follows.
    if let Some(first_at) = events[first..].iter().map(|e| e.at).find(|&t| t != 0) {
        for e in &mut events[first..] {
            if e.at == 0 {
                e.at = first_at;
            }
        }
    }
    files.push(File::open(path)?);
    Ok(())
}

fn read_at(file: &mut File, off: u64, len: usize) -> Option<String> {
    file.seek(SeekFrom::Start(off)).ok()?;
    let mut buf = vec![0; len];
    file.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn name_of(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}
