//! Where events come from: the live files of every running Claude Code session, or a recorded
//! transcript played back at speed. Both only read files.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::model::{PastRun, Session};
use crate::tail::Tail;
use crate::time::{now_ms, parse_iso_ms};

const REGISTRY_EVERY: Duration = Duration::from_secs(2);
const DIRS_EVERY: Duration = Duration::from_secs(1);

pub struct World {
    pub sessions: Vec<Session>,
    pub now: i64,
    pub mode: Mode,
    /// Finished workflow runs of every project, oldest first.
    pub history: Vec<PastRun>,
}

pub enum Mode {
    Live(Live),
    Replay(Replay),
}

pub struct Live {
    pub claude_dir: PathBuf,
    tracks: Vec<Track>,
    last_registry: Option<Instant>,
    last_dirs: Option<Instant>,
}

/// Files followed for one session.
struct Track {
    tails: Vec<(usize, Tail)>,
    agents: HashSet<String>,
    workflows: HashMap<PathBuf, SystemTime>,
    /// Workflow runs already finished when first seen: their record has the agents, so their
    /// (large) agent transcripts are not read.
    done_runs: HashSet<String>,
}

impl Track {
    fn new(transcript: PathBuf) -> Track {
        Track { tails: vec![(0, Tail::new(transcript))], agents: HashSet::new(), workflows: HashMap::new(), done_runs: HashSet::new() }
    }
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
    pub fn live(claude_dir: PathBuf) -> World {
        World {
            sessions: Vec::new(),
            now: now_ms(),
            history: load_history(&claude_dir),
            mode: Mode::Live(Live { claude_dir, tracks: Vec::new(), last_registry: None, last_dirs: None }),
        }
    }

    /// Indexes a transcript and its subagent and workflow files for playback.
    pub fn replay(path: &Path, speed: f64) -> io::Result<World> {
        let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("replay").to_string();
        let dir = path.with_extension("");
        let mut s = Session::new(&id, dir.clone());
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
        // Runs that ended before the replayed moment can be raced against.
        let claude_dir = dir.parent().and_then(Path::parent).and_then(Path::parent).map(Path::to_path_buf);
        let history = claude_dir.map(|d| load_history(&d)).unwrap_or_default();
        Ok(World {
            sessions: vec![s],
            now: start,
            history,
            mode: Mode::Replay(Replay { events, files, next: 0, sim: start as f64, last: Instant::now(), speed, paused: false }),
        })
    }

    /// Reads whatever is new. Returns whether anything changed.
    pub fn tick(&mut self) -> bool {
        match &mut self.mode {
            Mode::Live(live) => {
                self.now = now_ms();
                live_tick(live, &mut self.sessions, &mut self.history)
            }
            Mode::Replay(r) => replay_tick(r, &mut self.sessions, &mut self.now, false),
        }
    }

    /// Loads everything available now: one live scan, or the whole replay.
    pub fn run_to_end(&mut self) {
        match &mut self.mode {
            Mode::Live(live) => {
                self.now = now_ms();
                live_tick(live, &mut self.sessions, &mut self.history);
            }
            Mode::Replay(r) => {
                replay_tick(r, &mut self.sessions, &mut self.now, true);
            }
        }
    }

    pub fn replay_state(&self) -> Option<&Replay> {
        match &self.mode {
            Mode::Replay(r) => Some(r),
            Mode::Live(_) => None,
        }
    }

    pub fn replay_mut(&mut self) -> Option<&mut Replay> {
        match &mut self.mode {
            Mode::Replay(r) => Some(r),
            Mode::Live(_) => None,
        }
    }

    pub fn animating(&self) -> bool {
        match &self.mode {
            Mode::Replay(r) => !r.done() && !r.paused,
            Mode::Live(_) => self.sessions.iter().any(Session::busy),
        }
    }
}

impl Replay {
    pub fn done(&self) -> bool {
        self.next >= self.events.len()
    }

    pub fn progress(&self) -> f64 {
        if self.events.is_empty() {
            1.0
        } else {
            self.next as f64 / self.events.len() as f64
        }
    }
}

fn live_tick(live: &mut Live, sessions: &mut Vec<Session>, history: &mut Vec<PastRun>) -> bool {
    let mut changed = false;
    if live.last_registry.map_or(true, |t| t.elapsed() >= REGISTRY_EVERY) {
        changed |= scan_registry(live, sessions);
        live.last_registry = Some(Instant::now());
    }
    let scan_dirs = live.last_dirs.map_or(true, |t| t.elapsed() >= DIRS_EVERY);
    if scan_dirs {
        for (s, track) in sessions.iter_mut().zip(live.tracks.iter_mut()) {
            if s.alive {
                changed |= scan_agents(track, s);
            }
        }
    }
    for (s, track) in sessions.iter_mut().zip(live.tracks.iter_mut()) {
        for (a, tail) in &mut track.tails {
            let a = *a;
            if let Ok(true) = tail.poll(|line| s.ingest(a, line)) {
                changed = true;
            }
        }
    }
    // After the transcripts, so a finished run finds the step that launched it.
    if scan_dirs {
        for (s, track) in sessions.iter_mut().zip(live.tracks.iter_mut()) {
            if s.alive {
                changed |= scan_workflows(track, s, history);
            }
        }
        live.last_dirs = Some(Instant::now());
    }
    changed
}

/// `~/.claude/sessions/<pid>.json` lists every running Claude Code session, whatever its project.
fn scan_registry(live: &mut Live, sessions: &mut Vec<Session>) -> bool {
    let mut changed = false;
    let mut seen = HashSet::new();
    for path in read_dir_paths(&live.claude_dir.join("sessions")) {
        // Only the registry records; the `.key` files beside them are never opened.
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(v) = read_json(&path) else { continue };
        let Some(pid) = v.get("pid").and_then(Value::as_i64) else { continue };
        let Some(sid) = v.get("sessionId").and_then(Value::as_str) else { continue };
        if !pid_alive(pid) {
            continue;
        }
        seen.insert(sid.to_string());
        let cwd = str_of(&v, "cwd");
        let i = match sessions.iter().position(|s| s.id == sid) {
            Some(i) => i,
            None => {
                let transcript = find_transcript(&live.claude_dir, cwd, sid);
                let mut s = Session::new(sid, transcript.with_extension(""));
                s.cwd = cwd.to_string();
                sessions.push(s);
                live.tracks.push(Track::new(transcript));
                changed = true;
                sessions.len() - 1
            }
        };
        // A new session has no transcript yet, and the folder name is a guess until it does.
        let main = &mut live.tracks[i].tails[0].1;
        if !main.path.exists() {
            let found = find_transcript(&live.claude_dir, cwd, sid);
            if found.exists() {
                sessions[i].dir = found.with_extension("");
                *main = Tail::new(found);
            }
        }
        if s_proc(&mut sessions[i], v.get("startedAt").and_then(Value::as_i64).unwrap_or(0)) {
            changed = true;
        }
        let s = &mut sessions[i];
        let (name, status) = (str_of(&v, "name"), str_of(&v, "status"));
        if !s.alive || s.name != name || s.status != status {
            s.alive = true;
            s.name = name.to_string();
            s.status = status.to_string();
            changed = true;
        }
    }
    for s in sessions.iter_mut() {
        if s.alive && !seen.contains(&s.id) {
            s.alive = false;
            s.status = "ended".into();
            changed = true;
        }
    }
    changed
}

fn s_proc(s: &mut Session, started: i64) -> bool {
    let changed = s.proc_started != started;
    s.proc_started = started;
    changed
}

fn scan_agents(track: &mut Track, s: &mut Session) -> bool {
    let mut changed = false;
    let sub = s.dir.join("subagents");
    for meta in read_dir_paths(&sub) {
        let Some(id) = name_of(&meta).strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")) else { continue };
        if track.agents.contains(id) || s.has_agent(id) {
            continue;
        }
        let Some(v) = read_json(&meta) else { continue };
        let id = id.to_string();
        let a = s.add_agent(&id, str_of(&v, "agentType"), str_of(&v, "description"), v.get("toolUseId").and_then(Value::as_str).map(str::to_string));
        track.tails.push((a, Tail::new(sub.join(format!("agent-{id}.jsonl")))));
        track.agents.insert(id);
        changed = true;
    }
    // Workflow agents write their transcripts under `subagents/workflows/<run>/` while the run
    // works; a run already finished when first seen is read from its record instead.
    for run_dir in read_dir_paths(&sub.join("workflows")) {
        let run = name_of(&run_dir).to_string();
        if !run.starts_with("wf_") || track.done_runs.contains(&run) {
            continue;
        }
        let first_sight = !track.agents.iter().any(|k| k.starts_with(&format!("{run}/")));
        let finished = s.dir.join("workflows").join(format!("{run}.json")).exists();
        if finished {
            // One last look for a run seen working (its agents keep being followed), none for
            // one that was already over.
            track.done_runs.insert(run.clone());
            if first_sight {
                continue;
            }
        }
        for meta in read_dir_paths(&run_dir) {
            let Some(id) = name_of(&meta).strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")) else { continue };
            let key = format!("{run}/{id}");
            if track.agents.contains(&key) {
                continue;
            }
            let Some(v) = read_json(&meta) else { continue };
            let a = s.add_wf_agent(&run, id, str_of(&v, "description"), str_of(&v, "workflowPhase"));
            track.tails.push((a, Tail::new(run_dir.join(format!("agent-{id}.jsonl")))));
            track.agents.insert(key);
            changed = true;
        }
    }
    changed
}

fn scan_workflows(track: &mut Track, s: &mut Session, history: &mut Vec<PastRun>) -> bool {
    let mut changed = false;
    for path in read_dir_paths(&s.dir.join("workflows")) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else { continue };
        if track.workflows.get(&path) == Some(&mtime) {
            continue;
        }
        if let Some(v) = read_json(&path) {
            s.on_workflow_json(&v);
            if let Some(run) = PastRun::from_json(&v).filter(|r| !history.iter().any(|h| h.run_id == r.run_id)) {
                history.push(run);
            }
            changed = true;
        }
        track.workflows.insert(path, mtime);
    }
    changed
}

/// Every finished workflow run on this machine, read once at start: `projects/*/*/workflows/`.
fn load_history(claude_dir: &Path) -> Vec<PastRun> {
    let mut runs: Vec<PastRun> = read_dir_paths(&claude_dir.join("projects"))
        .into_iter()
        .flat_map(|project| read_dir_paths(&project))
        .filter(|p| p.is_dir())
        .flat_map(|session| read_dir_paths(&session.join("workflows")))
        .filter(|p| name_of(p).starts_with("wf_") && p.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter_map(|p| read_json(&p))
        .filter_map(|v| PastRun::from_json(&v))
        .collect();
    runs.sort_by_key(|r| r.start);
    runs
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
        if let (Some(c), Some(v)) = (cwd.as_deref_mut(), &v) {
            if c.is_empty() {
                c.push_str(str_of(v, "cwd"));
            }
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

fn find_transcript(claude_dir: &Path, cwd: &str, sid: &str) -> PathBuf {
    let projects = claude_dir.join("projects");
    let slug: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let guess = projects.join(slug).join(format!("{sid}.jsonl"));
    if guess.exists() {
        return guess;
    }
    read_dir_paths(&projects)
        .into_iter()
        .map(|p| p.join(format!("{sid}.jsonl")))
        .find(|p| p.exists())
        .unwrap_or(guess)
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

#[cfg(unix)]
fn pid_alive(pid: i64) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let Ok(pid) = i32::try_from(pid) else { return false };
    // Signal 0 sends nothing: it only asks whether the process exists.
    pid > 0 && unsafe { kill(pid, 0) } == 0
}

#[cfg(not(unix))]
fn pid_alive(_pid: i64) -> bool {
    true
}
