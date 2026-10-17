//! Session state built from Claude Code transcript lines: agents, their steps, background
//! tasks, workflow runs, MCP connections, skills, a short event feed and an activity histogram.
//! Message bodies are never kept, only one-line summaries.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use serde_json::Value;

use crate::parse;
use crate::price;
use crate::time::parse_iso_ms;

/// Most items kept per agent; the oldest are dropped so memory stays flat on long sessions.
const MAX_ITEMS: usize = 4000;
const TRIM_ITEMS: usize = 1000;
const FEED_LEN: usize = 64;
pub const BUCKET_MS: i64 = 5000;
const BUCKETS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Agent,
    Skill,
    Workflow,
    Mcp,
    Shell,
    File,
    Web,
    Comms,
    Tool,
}

/// A background task launched by a step: an async agent, a background command or a workflow run.
pub struct Bg {
    pub task_id: String,
    pub done: Option<i64>,
    pub status: String,
}

pub struct Step {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub summary: String,
    pub started: i64,
    pub ended: Option<i64>,
    pub error: bool,
    pub result: String,
    pub bg: Option<Bg>,
    pub child: Option<usize>,
    pub workflow: Option<usize>,
    /// The task a `TaskStop` call targets.
    pub target: Option<String>,
    /// The file a file tool works on.
    pub path: Option<String>,
    /// Hash of the tool name and its whole input: the same call made again has the same one.
    pub sig: u64,
    /// What this call is compared with to tell a slow one (`Bash:cargo`, `Agent:Explore`), or
    /// none for calls that legitimately wait (questions, background launches, monitors).
    pub key: Option<String>,
}

impl Step {
    /// Waiting for its result, or a background task that has not reported back yet.
    pub fn running(&self) -> bool {
        match (self.ended, &self.bg) {
            (None, _) => true,
            (Some(_), Some(bg)) => bg.done.is_none(),
            (Some(_), None) => false,
        }
    }

    pub fn finished(&self) -> Option<i64> {
        match &self.bg {
            Some(bg) => bg.done,
            None => self.ended,
        }
    }

    pub fn failed(&self) -> bool {
        self.error || self.bg.as_ref().is_some_and(|b| matches!(b.status.as_str(), "failed" | "error"))
    }

    /// Stopped on purpose, or lost when the Claude Code process restarted.
    pub fn stopped(&self) -> bool {
        self.bg.as_ref().is_some_and(|b| matches!(b.status.as_str(), "killed" | "stopped" | "lost"))
    }

    /// Column label and text: MCP calls show their server, everything else its tool name.
    pub fn label(&self) -> (String, String) {
        if self.kind == Kind::Mcp {
            let (server, tool) = parse::mcp_parts(&self.name);
            let text = if self.summary.is_empty() { tool.to_string() } else { format!("{tool} · {}", self.summary) };
            return (parse::server_label(server).to_string(), text);
        }
        (self.name.clone(), self.summary.clone())
    }
}

// Nearly every item is a step, so boxing the large variant would add an allocation per step and
// save nothing.
#[allow(clippy::large_enum_variant)]
pub enum Item {
    Prompt { at: i64, text: String },
    Step(Step),
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Tokens {
    /// Fresh input plus cache writes: everything the model read without the cache's help.
    pub input: u64,
    pub fresh: u64,
    pub cache_write: u64,
    /// The part of the cache writes kept for an hour, priced above the 5-minute ones.
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub output: u64,
    /// The part of the output spent thinking (its text is not in the transcript, only its size).
    pub thinking: u64,
}

impl Tokens {
    fn from_usage(u: &Value) -> Tokens {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let (fresh, cache_write) = (n(&u["input_tokens"]), n(&u["cache_creation_input_tokens"]));
        Tokens {
            input: fresh + cache_write,
            fresh,
            cache_write,
            cache_write_1h: n(&u["cache_creation"]["ephemeral_1h_input_tokens"]).min(cache_write),
            cache_read: n(&u["cache_read_input_tokens"]),
            output: n(&u["output_tokens"]),
            thinking: n(&u["output_tokens_details"]["thinking_tokens"]),
        }
    }

    pub fn plus(self, o: Tokens) -> Tokens {
        Tokens {
            input: self.input + o.input,
            fresh: self.fresh + o.fresh,
            cache_write: self.cache_write + o.cache_write,
            cache_write_1h: self.cache_write_1h + o.cache_write_1h,
            cache_read: self.cache_read + o.cache_read,
            output: self.output + o.output,
            thinking: self.thinking + o.thinking,
        }
    }

    fn minus(self, o: Tokens) -> Tokens {
        Tokens {
            input: self.input.saturating_sub(o.input),
            fresh: self.fresh.saturating_sub(o.fresh),
            cache_write: self.cache_write.saturating_sub(o.cache_write),
            cache_write_1h: self.cache_write_1h.saturating_sub(o.cache_write_1h),
            cache_read: self.cache_read.saturating_sub(o.cache_read),
            output: self.output.saturating_sub(o.output),
            thinking: self.thinking.saturating_sub(o.thinking),
        }
    }
}

/// What a session spends its time on, read from the gaps between transcript records: each
/// thinking block is written when it ends, each text or tool call when it is complete, each tool
/// result when the tool returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Think,
    Write,
    Tool,
}

/// A context compaction, from the `compact_boundary` record Claude Code writes.
pub struct Compaction {
    pub at: i64,
    /// Context size when it fired.
    pub pre: u64,
    pub auto: bool,
    /// The main agent's model at the time: the ceiling depends on its context window.
    pub model: String,
}

/// When the context will next be compacted, from how fast it grew over the last calls.
pub struct Forecast {
    pub ctx: u64,
    /// The ceiling where compaction fires at the latest: the largest automatic compaction seen
    /// with this model, or the window size.
    pub limit: u64,
    /// The ceiling comes from compactions seen, not from a guess of the window size.
    pub known: bool,
    pub per_call: f64,
    pub calls: Option<f64>,
    pub secs: Option<f64>,
}

/// One file a session read or changed. Only counts and times are kept, never the content.
#[derive(Default)]
pub struct FileStat {
    pub reads: u32,
    pub edits: u32,
    pub added: u32,
    pub removed: u32,
    pub created: bool,
    pub last_read: i64,
    pub last_edit: i64,
    pub last_agent: usize,
    /// The latest edits: which agent, when.
    pub editors: Vec<(usize, i64)>,
}

impl FileStat {
    pub fn last(&self) -> i64 {
        self.last_read.max(self.last_edit)
    }
}

/// Durations of finished calls of one kind, in doubling bins from half a second up.
#[derive(Clone, Copy, Default)]
pub struct Hist {
    pub n: u32,
    bins: [u32; 20],
}

impl Hist {
    fn add(&mut self, ms: i64) {
        let bin = ((ms.max(1) as f64 / 500.0).log2().floor().max(0.0) as usize).min(19);
        self.bins[bin] += 1;
        self.n += 1;
    }

    pub fn merge(&mut self, o: &Hist) {
        self.n += o.n;
        for (a, b) in self.bins.iter_mut().zip(o.bins) {
            *a += b;
        }
    }

    /// The upper edge of the bin holding the `q` quantile: rounds up, so "usual" is never
    /// underestimated and a slow call is flagged late rather than early.
    pub fn quantile(&self, q: f64) -> i64 {
        let want = (self.n as f64 * q).ceil().max(1.0) as u32;
        let mut seen = 0;
        for (i, &c) in self.bins.iter().enumerate() {
            seen += c;
            if seen >= want {
                return 500 << (i + 1);
            }
        }
        500 << 20
    }
}

const FILES_KEPT: usize = 400;
const CTX_SAMPLES: usize = 24;

pub struct Agent {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub model: String,
    pub parent_tool: Option<String>,
    pub prompt: String,
    pub items: Vec<Item>,
    pub ctx: u64,
    pub started: i64,
    pub last_at: i64,
    pub last_text: String,
    pub collapsed: Option<bool>,
    /// Gave its final answer: a subagent's last turn, or a workflow agent's structured output.
    pub done: bool,
    /// The workflow run this agent works for, and its phase title.
    pub wf_run: String,
    pub wf_phase: String,
    /// Time of the last record that closed a phase interval.
    mark: i64,
    committed: Tokens,
    current: Tokens,
    current_msg: String,
}

impl Agent {
    fn new(id: &str, kind: &str, description: &str, parent_tool: Option<String>) -> Agent {
        Agent {
            id: id.to_string(),
            kind: kind.to_string(),
            description: description.to_string(),
            model: String::new(),
            parent_tool,
            prompt: String::new(),
            items: Vec::new(),
            ctx: 0,
            started: 0,
            last_at: 0,
            last_text: String::new(),
            collapsed: None,
            done: false,
            wf_run: String::new(),
            wf_phase: String::new(),
            mark: 0,
            committed: Tokens::default(),
            current: Tokens::default(),
            current_msg: String::new(),
        }
    }

    pub fn tokens(&self) -> Tokens {
        self.committed.plus(self.current)
    }

    pub fn step_count(&self) -> usize {
        self.items.iter().filter(|i| matches!(i, Item::Step(_))).count()
    }

    /// One API message is split over several records that repeat its usage: count it once.
    /// Returns what this record adds, and whether it starts a new message.
    fn add_usage(&mut self, msg_id: &str, u: &Value) -> (Tokens, bool) {
        let t = Tokens::from_usage(u);
        let fresh_msg = msg_id != self.current_msg;
        let before = if fresh_msg { Tokens::default() } else { self.current };
        if fresh_msg {
            self.committed = self.committed.plus(self.current);
            self.current_msg = msg_id.to_string();
        }
        self.current = t;
        self.ctx = t.input + t.cache_read;
        (t.minus(before), fresh_msg)
    }
}

pub struct WfAgent {
    pub agent_id: String,
    pub label: String,
    pub phase: usize,
    pub state: String,
    pub model: String,
    pub last_tool: String,
    pub last_summary: String,
}

pub struct Workflow {
    pub name: String,
    pub run_id: String,
    pub task_id: String,
    pub status: String,
    pub phases: Vec<String>,
    /// Phases folded open or shut by hand; untouched ones are open while an agent works there.
    pub open: Vec<Option<bool>>,
    pub agents: Vec<WfAgent>,
    /// Agents followed live from their own transcripts while the run works.
    pub live: Vec<usize>,
    pub tokens: u64,
    pub expanded: Option<bool>,
    pub step: Option<String>,
}

impl Workflow {
    fn new(name: String, phases: Vec<String>, step: Option<String>) -> Workflow {
        Workflow {
            name,
            run_id: String::new(),
            task_id: String::new(),
            status: String::new(),
            open: vec![None; phases.len()],
            phases,
            agents: Vec::new(),
            live: Vec::new(),
            tokens: 0,
            expanded: None,
            step,
        }
    }

    /// Phase index of a live agent, matched by its phase title.
    pub fn phase_of(&self, title: &str) -> Option<usize> {
        self.phases.iter().position(|p| p == title)
    }
}

#[derive(Default)]
pub struct McpStat {
    pub calls: u32,
    pub errors: u32,
    pub last_tool: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Prompt,
    Start,
    Done,
    Failed,
}

pub struct FeedItem {
    pub at: i64,
    pub feed: Feed,
    pub kind: Option<Kind>,
    pub label: String,
    pub text: String,
    pub tool: Option<String>,
}

/// Amounts (events, tokens, bytes) in 5-second buckets, for sparklines and rates.
pub struct Activity {
    buckets: [u32; BUCKETS],
    head: i64,
}

impl Activity {
    fn new() -> Activity {
        Activity { buckets: [0; BUCKETS], head: 0 }
    }

    fn add(&mut self, at: i64, n: u64) {
        let b = at.div_euclid(BUCKET_MS);
        if b > self.head {
            let shift = (b - self.head).min(BUCKETS as i64);
            for k in 1..=shift {
                self.buckets[(self.head + k).rem_euclid(BUCKETS as i64) as usize] = 0;
            }
            self.head = b;
        }
        if self.head - b < BUCKETS as i64 {
            let i = b.rem_euclid(BUCKETS as i64) as usize;
            self.buckets[i] = self.buckets[i].saturating_add(n.min(u32::MAX as u64) as u32);
        }
    }

    /// The last `n` buckets ending at `now`, oldest first.
    pub fn series(&self, now: i64, n: usize) -> Vec<u32> {
        let nb = now.div_euclid(BUCKET_MS);
        (0..n as i64)
            .rev()
            .map(|k| {
                let b = nb - k;
                if b > self.head || self.head - b >= BUCKETS as i64 { 0 } else { self.buckets[b.rem_euclid(BUCKETS as i64) as usize] }
            })
            .collect()
    }

    /// Total over the last minute.
    pub fn per_min(&self, now: i64) -> u64 {
        self.sum(now, (60_000 / BUCKET_MS) as usize)
    }

    /// Total over the last `n` buckets.
    pub fn sum(&self, now: i64, n: usize) -> u64 {
        self.series(now, n).iter().map(|&v| v as u64).sum()
    }

    /// Spreads a duration in milliseconds over the buckets it covers.
    fn add_span(&mut self, start: i64, end: i64) {
        let mut t = start.max(end - BUCKETS as i64 * BUCKET_MS);
        while t < end {
            let edge = ((t.div_euclid(BUCKET_MS) + 1) * BUCKET_MS).min(end);
            self.add(t, (edge - t) as u64);
            t = edge;
        }
    }
}

pub struct Session {
    pub id: String,
    /// The Claude Code process, from the registry (0 when unknown).
    pub pid: i64,
    pub name: String,
    pub cwd: String,
    pub status: String,
    pub alive: bool,
    pub title: String,
    pub dir: PathBuf,
    pub agents: Vec<Agent>,
    pub workflows: Vec<Workflow>,
    pub mcp: BTreeMap<String, McpStat>,
    pub skills: Vec<(String, u32)>,
    pub feed: VecDeque<FeedItem>,
    pub activity: Activity,
    /// Tokens sent to the model (fresh input, cache writes and cache reads).
    pub tok_up: Activity,
    /// Tokens the model generated.
    pub tok_down: Activity,
    /// Bytes appended to the session's transcripts.
    pub bytes: Activity,
    /// Input tokens by where they came from: read fresh, written to the cache, read from it.
    pub fresh_in: Activity,
    pub cache_w: Activity,
    pub cache_r: Activity,
    pub thinking: Activity,
    /// Milliseconds spent thinking, writing and waiting for tools, summed over all agents.
    pub waves: [Activity; 3],
    /// Large cache writes (the prompt cache rebuilt or warmed up): when, how many tokens.
    pub flares: VecDeque<(i64, u64)>,
    /// Estimated API price of the tokens so far, in millionths of a dollar, and its rate.
    pub cost_micros: u64,
    pub cost: Activity,
    /// Tokens of models with no known price, left out of the estimate.
    pub unpriced: u64,
    pub compactions: Vec<Compaction>,
    /// Context size after each call of the main agent since the last compaction.
    pub ctx_samples: VecDeque<(i64, u64)>,
    pub files: HashMap<String, FileStat>,
    /// How long finished calls took, by kind, to tell when a running one is unusually slow.
    pub durations: HashMap<String, Hist>,
    pub last_at: i64,
    /// Start of the current Claude Code process: background tasks launched before it are gone.
    pub proc_started: i64,
    index: HashMap<String, (usize, usize)>,
    agent_ids: HashMap<String, usize>,
    tasks: HashMap<String, String>,
    pending_child: HashMap<String, usize>,
    /// Completion notices that arrived before the step they close (keyed by tool id and task id).
    pending_done: HashMap<String, (i64, String)>,
    /// Workflow agents seen before the call that launched their run (keyed by run id).
    pending_wf: HashMap<String, Vec<usize>>,
}

impl Session {
    pub fn new(id: &str, dir: PathBuf) -> Session {
        Session {
            id: id.to_string(),
            pid: 0,
            name: String::new(),
            cwd: String::new(),
            status: String::new(),
            alive: false,
            title: String::new(),
            dir,
            agents: vec![Agent::new("main", "main", "", None)],
            workflows: Vec::new(),
            mcp: BTreeMap::new(),
            skills: Vec::new(),
            feed: VecDeque::new(),
            activity: Activity::new(),
            tok_up: Activity::new(),
            tok_down: Activity::new(),
            bytes: Activity::new(),
            fresh_in: Activity::new(),
            cache_w: Activity::new(),
            cache_r: Activity::new(),
            thinking: Activity::new(),
            waves: [Activity::new(), Activity::new(), Activity::new()],
            flares: VecDeque::new(),
            cost_micros: 0,
            cost: Activity::new(),
            unpriced: 0,
            compactions: Vec::new(),
            ctx_samples: VecDeque::new(),
            files: HashMap::new(),
            durations: HashMap::new(),
            last_at: 0,
            proc_started: 0,
            index: HashMap::new(),
            agent_ids: HashMap::new(),
            tasks: HashMap::new(),
            pending_child: HashMap::new(),
            pending_done: HashMap::new(),
            pending_wf: HashMap::new(),
        }
    }

    /// Project folder name, the most recognizable label for a session.
    pub fn project(&self) -> &str {
        self.cwd.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(&self.name)
    }

    pub fn has_agent(&self, id: &str) -> bool {
        self.agent_ids.contains_key(id)
    }

    pub fn agent_index(&self, id: &str) -> Option<usize> {
        self.agent_ids.get(id).copied()
    }

    /// Registers a subagent and hangs it under the step that spawned it, now or once seen.
    pub fn add_agent(&mut self, id: &str, kind: &str, description: &str, parent_tool: Option<String>) -> usize {
        if let Some(&a) = self.agent_ids.get(id) {
            return a;
        }
        let a = self.agents.len();
        self.agents.push(Agent::new(id, kind, description, parent_tool.clone()));
        self.agent_ids.insert(id.to_string(), a);
        if let Some(tool) = parent_tool {
            match self.index.get(&tool).copied() {
                Some((pa, pi)) => {
                    if let Some(Item::Step(st)) = self.agents[pa].items.get_mut(pi) {
                        st.child = Some(a);
                    }
                }
                None => {
                    self.pending_child.insert(tool, a);
                }
            }
        }
        a
    }

    /// Registers an agent of a workflow run, followed from `subagents/workflows/<run>/`, and
    /// hangs it under its run now or once the call that launched the run is read.
    pub fn add_wf_agent(&mut self, run: &str, id: &str, label: &str, phase: &str) -> usize {
        if let Some(&a) = self.agent_ids.get(id) {
            return a;
        }
        let a = self.add_agent(id, "workflow-subagent", label, None);
        self.agents[a].wf_run = run.to_string();
        self.agents[a].wf_phase = phase.to_string();
        match self.workflows.iter_mut().find(|w| w.run_id == run) {
            Some(w) => w.live.push(a),
            None => self.pending_wf.entry(run.to_string()).or_default().push(a),
        }
        a
    }

    fn link_run(&mut self, w: usize, run: &str) {
        if run.is_empty() {
            return;
        }
        self.workflows[w].run_id = run.to_string();
        if let Some(agents) = self.pending_wf.remove(run) {
            self.workflows[w].live.extend(agents);
        }
    }

    pub fn ingest(&mut self, a: usize, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if a == 0 && v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let ts = v.get("timestamp").and_then(Value::as_str).and_then(parse_iso_ms);
        if let Some(t) = ts {
            let ag = &mut self.agents[a];
            if ag.started == 0 {
                ag.started = t;
            }
            ag.last_at = ag.last_at.max(t);
            self.last_at = self.last_at.max(t);
        }
        let at = ts.unwrap_or(self.agents[a].last_at);
        self.bytes.add(at, line.len() as u64 + 1);
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "assistant" => self.on_assistant(a, &v["message"], at),
            "user" => self.on_user(a, &v, at),
            "attachment" => {
                if let Some(p) = v["attachment"].get("prompt").and_then(Value::as_str)
                    && p.contains("<task-notification>")
                {
                    self.on_notifications(p, at);
                }
            }
            "queue-operation" => {
                if let Some(c) = v.get("content").and_then(Value::as_str)
                    && c.contains("<task-notification>")
                {
                    self.on_notifications(c, at);
                }
            }
            "system" if a == 0 && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") => {
                let m = &v["compactMetadata"];
                self.compactions.push(Compaction {
                    at,
                    pre: m.get("preTokens").and_then(Value::as_u64).unwrap_or(0),
                    auto: m.get("trigger").and_then(Value::as_str) != Some("manual"),
                    model: self.agents[0].model.clone(),
                });
                self.ctx_samples.clear();
                self.agents[0].mark = at;
            }
            "ai-title" => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    self.title = t.to_string();
                }
            }
            "agent-name" if self.title.is_empty() => {
                if let Some(t) = v.get("agentName").and_then(Value::as_str) {
                    self.title = t.to_string();
                }
            }
            _ => {}
        }
    }

    /// Counts the time since the agent's last record as spent in `phase`, at most `cap`.
    fn span(&mut self, a: usize, phase: Phase, at: i64, cap: i64) {
        let ag = &mut self.agents[a];
        if ag.mark > 0 && at > ag.mark {
            self.waves[phase as usize].add_span(ag.mark.max(at - cap), at);
        }
        ag.mark = ag.mark.max(at);
    }

    fn on_assistant(&mut self, a: usize, msg: &Value, at: i64) {
        let ag = &mut self.agents[a];
        if let Some(m) = msg.get("model").and_then(Value::as_str)
            && !m.starts_with('<')
        {
            ag.model = m.to_string();
        }
        if a != 0 && msg.get("stop_reason").and_then(Value::as_str) == Some("end_turn") {
            ag.done = true;
        }
        // The gap since the agent's last record went into producing this block.
        let first = msg["content"][0]["type"].as_str().unwrap_or("");
        let phase = match first {
            "thinking" | "redacted_thinking" => Some(Phase::Think),
            "text" | "tool_use" | "server_tool_use" => Some(Phase::Write),
            t if t.ends_with("_tool_result") => Some(Phase::Tool),
            _ => None,
        };
        if let Some(p) = phase {
            self.span(a, p, at, 10 * 60_000);
        }
        if let (Some(id), Some(u)) = (msg.get("id").and_then(Value::as_str), msg.get("usage")) {
            let (d, new_call) = self.agents[a].add_usage(id, u);
            self.tok_up.add(at, d.input + d.cache_read);
            self.tok_down.add(at, d.output);
            self.fresh_in.add(at, d.fresh);
            self.cache_w.add(at, d.cache_write);
            self.cache_r.add(at, d.cache_read);
            self.thinking.add(at, d.thinking);
            if d.cache_write >= 20_000 && d.cache_write > d.cache_read {
                if self.flares.len() == 8 {
                    self.flares.pop_front();
                }
                self.flares.push_back((at, d.cache_write));
            }
            let fast = u.get("speed").and_then(Value::as_str) == Some("fast");
            match price::cost_micros(&self.agents[a].model, &d, fast) {
                Some(c) => {
                    self.cost_micros += c;
                    self.cost.add(at, c);
                }
                None => self.unpriced += d.input + d.cache_read + d.output,
            }
            if a == 0 && new_call {
                if self.ctx_samples.len() == CTX_SAMPLES {
                    self.ctx_samples.pop_front();
                }
                self.ctx_samples.push_back((at, self.agents[0].ctx));
            }
        }
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else { return };
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let line = parse::first_line(b.get("text").and_then(Value::as_str).unwrap_or(""));
                    if !line.is_empty() {
                        self.agents[a].last_text = parse::clip(line, 300);
                    }
                }
                Some("tool_use") => self.start_step(a, b, at),
                _ => {}
            }
        }
    }

    fn start_step(&mut self, a: usize, b: &Value, at: i64) {
        let Some(id) = b.get("id").and_then(Value::as_str) else { return };
        if self.index.contains_key(id) {
            return;
        }
        let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
        let input = b.get("input").unwrap_or(&Value::Null);
        let kind = parse::kind(name);
        let mut step = Step {
            id: id.to_string(),
            name: name.to_string(),
            kind,
            summary: parse::summarize(name, input),
            started: at,
            ended: None,
            error: false,
            result: String::new(),
            bg: None,
            child: self.pending_child.remove(id),
            workflow: None,
            target: matches!(name, "TaskStop" | "KillShell")
                .then(|| input.get("task_id").or(input.get("shell_id")).and_then(Value::as_str).map(str::to_string))
                .flatten(),
            path: matches!(name, "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
                .then(|| input.get("file_path").or(input.get("notebook_path")).and_then(Value::as_str).map(str::to_string))
                .flatten(),
            sig: parse::signature(name, input),
            key: parse::duration_key(name, input),
        };
        match kind {
            Kind::Mcp => {
                let (server, tool) = parse::mcp_parts(name);
                let st = self.mcp.entry(server.to_string()).or_default();
                st.calls += 1;
                st.last_tool = tool.to_string();
            }
            Kind::Skill => {
                if let Some(k) = input.get("skill").and_then(Value::as_str) {
                    self.note_skill(k);
                }
            }
            Kind::Workflow => {
                let (wname, phases) = parse::workflow_meta(input);
                step.workflow = Some(self.workflows.len());
                self.workflows.push(Workflow::new(wname, phases, Some(id.to_string())));
            }
            _ => {}
        }
        let (label, text) = step.label();
        self.push_feed(FeedItem { at, feed: Feed::Start, kind: Some(kind), label, text, tool: Some(id.to_string()) });
        let items = &mut self.agents[a].items;
        self.index.insert(id.to_string(), (a, items.len()));
        items.push(Item::Step(step));
        self.trim(a);
    }

    fn on_user(&mut self, a: usize, v: &Value, at: i64) {
        let meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);
        let content = &v["message"]["content"];
        if let Some(text) = content.as_str() {
            // A prompt: the time before it was spent waiting for the person, not working.
            let ag = &mut self.agents[a];
            ag.mark = ag.mark.max(at);
            self.on_user_text(a, text, at, meta);
            return;
        }
        let Some(blocks) = content.as_array() else { return };
        if blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")) {
            self.span(a, Phase::Tool, at, 2 * 3_600_000);
        } else {
            let ag = &mut self.agents[a];
            ag.mark = ag.mark.max(at);
        }
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_result") => {
                    let Some(id) = b.get("tool_use_id").and_then(Value::as_str) else { continue };
                    let error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    let text = parse::content_text(b.get("content").unwrap_or(&Value::Null));
                    self.end_step(id, error, &text, v.get("toolUseResult").unwrap_or(&Value::Null), at);
                }
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        self.on_user_text(a, t, at, meta);
                    }
                }
                _ => {}
            }
        }
    }

    fn on_user_text(&mut self, a: usize, text: &str, at: i64, meta: bool) {
        let t = text.trim();
        if t.contains("<task-notification>") {
            self.on_notifications(t, at);
            return;
        }
        if let Some(cmd) = parse::tag(t, "command-name") {
            let cmd = cmd.trim_start_matches('/').to_string();
            if t.contains("<skill-format>true") {
                self.note_skill(&cmd);
            }
            if a == 0 {
                self.push_prompt(format!("/{cmd}"), at);
            }
            return;
        }
        if meta || t.is_empty() || t.starts_with('<') {
            return;
        }
        if a == 0 {
            self.push_prompt(parse::clip(parse::first_line(t), 300), at);
        } else if self.agents[a].prompt.is_empty() {
            self.agents[a].prompt = parse::clip(parse::first_line(t), 300);
        }
    }

    fn push_prompt(&mut self, text: String, at: i64) {
        self.push_feed(FeedItem { at, feed: Feed::Prompt, kind: None, label: String::new(), text: text.clone(), tool: None });
        self.agents[0].items.push(Item::Prompt { at, text });
        self.trim(0);
    }

    fn end_step(&mut self, id: &str, error: bool, text: &str, tur: &Value, at: i64) {
        let Some(&(a, i)) = self.index.get(id) else { return };
        let Some(Item::Step(st)) = self.agents[a].items.get_mut(i) else { return };
        if st.ended.is_some() {
            return;
        }
        st.ended = Some(at.max(st.started));
        st.error = error;
        st.result = parse::clip(parse::first_line(text), 300);
        let task = match st.kind {
            Kind::Agent if tur.get("isAsync").and_then(Value::as_bool) == Some(true) => tur.get("agentId").and_then(Value::as_str),
            Kind::Workflow => parse::word_after(text, "Task ID: "),
            Kind::Shell => tur.get("backgroundTaskId").and_then(Value::as_str).or_else(|| parse::word_after(text, "running in background with ID: ")),
            _ => None,
        }
        .map(str::to_string);
        if let Some(task) = &task {
            let mut bg = Bg { task_id: task.clone(), done: None, status: "running".into() };
            let early = self.pending_done.remove(id).or_else(|| self.pending_done.remove(task));
            if let Some((done, status)) = early {
                bg.done = Some(done.max(at));
                bg.status = status;
            } else if self.proc_started > 0 && at < self.proc_started {
                bg.done = Some(self.proc_started);
                bg.status = "lost".into();
            }
            st.bg = Some(bg);
        }
        let stop = if error { None } else { st.target.clone() };
        let (kind, wf, name) = (st.kind, st.workflow, st.name.clone());
        let (label, summary) = st.label();
        let (path, key, took, bg) = (st.path.clone(), st.key.clone(), at - st.started, st.bg.is_some());
        if !error {
            if let Some(k) = key.filter(|_| !bg) {
                self.durations.entry(k).or_default().add(took);
            }
            if let Some(p) = path {
                self.touch(&p, &name, a, tur, at);
            }
            if name == "StructuredOutput" && a != 0 {
                self.agents[a].done = true;
            }
            if let (Some(w), Some(run)) = (wf, tur.get("runId").and_then(Value::as_str)) {
                self.link_run(w, run);
            }
        }
        if kind == Kind::Mcp && error {
            self.mcp.entry(parse::mcp_parts(&name).0.to_string()).or_default().errors += 1;
        }
        if error {
            self.push_feed(FeedItem { at, feed: Feed::Failed, kind: Some(kind), label, text: summary, tool: Some(id.to_string()) });
        } else if kind == Kind::Agent && task.is_none() {
            self.push_feed(FeedItem { at, feed: Feed::Done, kind: Some(kind), label, text: summary, tool: Some(id.to_string()) });
        }
        if let Some(task) = task {
            self.tasks.insert(task.clone(), id.to_string());
            if let Some(w) = wf {
                self.workflows[w].task_id = task;
            }
        }
        if let Some(target) = stop {
            self.close_task(&target, None, at, "stopped");
        }
    }

    /// Marks a background task finished, by tool id or task id; remembers it if not seen yet.
    fn close_task(&mut self, task: &str, tool: Option<&str>, at: i64, status: &str) -> bool {
        let tool = tool.map(str::to_string).or_else(|| self.tasks.get(task).cloned());
        let found = tool.as_deref().and_then(|t| self.index.get(t).copied());
        let Some((a, i)) = found else {
            for key in tool.iter().map(String::as_str).chain([task]).filter(|k| !k.is_empty()) {
                self.pending_done.insert(key.to_string(), (at, status.to_string()));
            }
            return false;
        };
        let Some(Item::Step(st)) = self.agents[a].items.get_mut(i) else { return false };
        let bg = st.bg.get_or_insert_with(|| Bg { task_id: task.to_string(), done: None, status: String::new() });
        if bg.done.is_some() {
            return false;
        }
        bg.done = Some(at);
        bg.status = status.to_string();
        if st.ended.is_none() {
            st.ended = Some(at);
        }
        true
    }

    fn on_notifications(&mut self, text: &str, at: i64) {
        for chunk in text.split("<task-notification>").skip(1) {
            let task = parse::tag(chunk, "task-id").unwrap_or("").to_string();
            let status = parse::tag(chunk, "status").unwrap_or("completed").to_string();
            let tool = parse::tag(chunk, "tool-use-id").map(str::to_string);
            if !self.close_task(&task, tool.as_deref(), at, &status) {
                continue;
            }
            let Some(st) = tool.as_deref().or_else(|| self.tasks.get(&task).map(String::as_str)).and_then(|t| self.step_by_tool(t)) else { continue };
            let (label, summary) = st.label();
            let (kind, failed, id) = (st.kind, st.failed(), st.id.clone());
            self.push_feed(FeedItem { at, feed: if failed { Feed::Failed } else { Feed::Done }, kind: Some(kind), label, text: summary, tool: Some(id) });
        }
    }

    /// A `workflows/wf_*.json` run record. Claude Code writes it when the run ends.
    pub fn on_workflow_json(&mut self, v: &Value) {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let (run_id, task_id) = (s("runId"), s("taskId"));
        let found = self.workflows.iter().position(|w| (!run_id.is_empty() && w.run_id == run_id) || (!task_id.is_empty() && w.task_id == task_id));
        let w = match found {
            Some(w) => w,
            None => {
                let step = self.tasks.get(&task_id).cloned();
                let w = self.workflows.len();
                self.workflows.push(Workflow::new(s("workflowName"), Vec::new(), step.clone()));
                if let Some((a, i)) = step.and_then(|t| self.index.get(&t).copied())
                    && let Some(Item::Step(st)) = self.agents[a].items.get_mut(i)
                {
                    st.workflow.get_or_insert(w);
                }
                w
            }
        };
        self.link_run(w, &run_id);
        let wf = &mut self.workflows[w];
        if !task_id.is_empty() {
            wf.task_id = task_id;
        }
        let name = s("workflowName");
        if !name.is_empty() {
            wf.name = name;
        }
        wf.status = s("status");
        wf.tokens = v.get("totalTokens").and_then(Value::as_u64).unwrap_or(0);
        let mut phases = Vec::new();
        let mut agents = Vec::new();
        for p in v.get("workflowProgress").and_then(Value::as_array).into_iter().flatten() {
            let f = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            match p.get("type").and_then(Value::as_str) {
                Some("workflow_phase") => phases.push(f("title")),
                Some("workflow_agent") => agents.push(WfAgent {
                    agent_id: f("agentId"),
                    label: f("label"),
                    phase: p.get("phaseIndex").and_then(Value::as_u64).unwrap_or(1).saturating_sub(1) as usize,
                    state: f("state"),
                    model: f("model"),
                    last_tool: f("lastToolName"),
                    last_summary: parse::clip(parse::first_line(&f("lastToolSummary")), 200),
                }),
                _ => {}
            }
        }
        if !phases.is_empty() {
            wf.phases = phases;
        }
        let ended: Vec<String> = agents.iter().filter(|g| g.state != "running").map(|g| g.agent_id.clone()).collect();
        wf.agents = agents;
        wf.open.resize(wf.phases.len(), None);
        for id in ended {
            if let Some(&a) = self.agent_ids.get(&id) {
                self.agents[a].done = true;
            }
        }
        let wf = &mut self.workflows[w];
        let finished = !matches!(wf.status.as_str(), "" | "running" | "pending");
        let step = wf.step.clone();
        let status = wf.status.clone();
        if finished {
            let end = v.get("startTime").and_then(Value::as_i64).unwrap_or(0) + v.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
            if let Some((a, i)) = step.and_then(|t| self.index.get(&t).copied())
                && let Some(Item::Step(st)) = self.agents[a].items.get_mut(i)
                && st.ended.is_some()
            {
                let bg = st.bg.get_or_insert_with(|| Bg { task_id: String::new(), done: None, status: String::new() });
                if bg.done.is_none() {
                    bg.done = Some(end.max(st.started));
                    bg.status = status;
                }
            }
        }
    }

    /// Records a read or a change of a file. From an edit's result only the counts of added and
    /// removed lines are kept; file contents and edited text are never stored.
    fn touch(&mut self, path: &str, tool: &str, a: usize, tur: &Value, at: i64) {
        if !self.files.contains_key(path)
            && self.files.len() >= FILES_KEPT
            && let Some(old) = self.files.iter().min_by_key(|(_, f)| f.last()).map(|(k, _)| k.clone())
        {
            self.files.remove(&old);
        }
        let f = self.files.entry(path.to_string()).or_default();
        if tool == "Read" {
            f.reads += 1;
            f.last_read = f.last_read.max(at);
            return;
        }
        let (mut added, mut removed) = (0u32, 0u32);
        for hunk in tur.get("structuredPatch").and_then(Value::as_array).into_iter().flatten() {
            for l in hunk.get("lines").and_then(Value::as_array).into_iter().flatten() {
                match l.as_str().and_then(|s| s.chars().next()) {
                    Some('+') => added += 1,
                    Some('-') => removed += 1,
                    _ => {}
                }
            }
        }
        if tur.get("type").and_then(Value::as_str) == Some("create") {
            f.created = true;
            added += tur.get("content").and_then(Value::as_str).map_or(0, |c| c.lines().count() as u32);
        }
        f.edits += 1;
        f.added += added;
        f.removed += removed;
        f.last_edit = f.last_edit.max(at);
        f.last_agent = a;
        f.editors.push((a, at));
        if f.editors.len() > 6 {
            f.editors.remove(0);
        }
    }

    fn note_skill(&mut self, name: &str) {
        match self.skills.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 += 1,
            None => self.skills.push((name.to_string(), 1)),
        }
    }

    fn push_feed(&mut self, item: FeedItem) {
        self.activity.add(item.at, 1);
        if self.feed.len() == FEED_LEN {
            self.feed.pop_front();
        }
        self.feed.push_back(item);
    }

    fn trim(&mut self, a: usize) {
        if self.agents[a].items.len() <= MAX_ITEMS {
            return;
        }
        self.agents[a].items.drain(..TRIM_ITEMS);
        self.index.retain(|_, (ia, _)| *ia != a);
        for (i, item) in self.agents[a].items.iter().enumerate() {
            if let Item::Step(s) = item {
                self.index.insert(s.id.clone(), (a, i));
            }
        }
    }

    pub fn step_by_tool(&self, tool: &str) -> Option<&Step> {
        let &(a, i) = self.index.get(tool)?;
        match self.agents[a].items.get(i)? {
            Item::Step(s) => Some(s),
            Item::Prompt { .. } => None,
        }
    }

    pub fn steps(&self) -> impl Iterator<Item = (usize, &Step)> {
        self.agents.iter().enumerate().flat_map(|(a, ag)| {
            ag.items.iter().filter_map(move |i| match i {
                Item::Step(s) => Some((a, s)),
                Item::Prompt { .. } => None,
            })
        })
    }

    pub fn agent_running(&self, a: usize) -> bool {
        if !self.alive {
            return false;
        }
        if a == 0 {
            return self.status == "busy" || self.agents[0].items.iter().rev().take(50).any(|i| matches!(i, Item::Step(s) if s.ended.is_none()));
        }
        let ag = &self.agents[a];
        if !ag.wf_run.is_empty() {
            // A workflow agent works until it answers or its run ends.
            return !ag.done
                && self
                    .workflows
                    .iter()
                    .find(|w| w.run_id == ag.wf_run)
                    .and_then(|w| w.step.as_deref())
                    .and_then(|t| self.step_by_tool(t))
                    .is_none_or(Step::running);
        }
        match &self.agents[a].parent_tool {
            Some(t) => self.step_by_tool(t).is_some_and(Step::running),
            None => false,
        }
    }

    pub fn running_agents(&self) -> usize {
        (1..self.agents.len()).filter(|&a| self.agent_running(a)).count()
    }

    pub fn background_running(&self) -> usize {
        if !self.alive {
            return 0;
        }
        self.steps().filter(|(_, s)| s.ended.is_some() && s.running()).count()
    }

    /// Working: a busy main loop, a step waiting for its result, or a background agent or
    /// workflow. Background shell commands (dev servers, watchers) alone do not count.
    pub fn busy(&self) -> bool {
        self.alive && (self.status == "busy" || self.steps().any(|(_, s)| s.ended.is_none() || (s.running() && s.kind != Kind::Shell)))
    }

    /// Waiting on the person: a question or a plan to approve is open.
    pub fn needs_you(&self) -> bool {
        self.alive
            && (self.status.contains("wait") || self.steps().any(|(_, s)| s.ended.is_none() && matches!(s.name.as_str(), "AskUserQuestion" | "ExitPlanMode")))
    }

    /// Registry name suffix (`projects-63` -> `63`), to tell apart sessions in the same folder.
    pub fn short_tag(&self) -> &str {
        match self.name.rsplit_once('-') {
            Some((_, tag)) if !tag.is_empty() => tag,
            _ => &self.id[..self.id.len().min(4)],
        }
    }

    /// The most recent step still waiting for its result, preferring the deepest agent.
    pub fn current(&self) -> Option<(usize, &Step)> {
        if !self.alive {
            return None;
        }
        self.steps().filter(|(_, s)| s.ended.is_none()).max_by_key(|(_, s)| s.started)
    }

    pub fn last_step(&self) -> Option<(usize, &Step)> {
        self.agents
            .iter()
            .enumerate()
            .filter_map(|(a, ag)| {
                ag.items.iter().rev().find_map(|i| match i {
                    Item::Step(s) => Some((a, s)),
                    Item::Prompt { .. } => None,
                })
            })
            .max_by_key(|(_, s)| s.started)
    }

    pub fn last_prompt(&self) -> Option<&str> {
        self.agents[0].items.iter().rev().find_map(|i| match i {
            Item::Prompt { text, .. } => Some(text.as_str()),
            Item::Step(_) => None,
        })
    }

    pub fn tokens(&self) -> Tokens {
        self.agents.iter().fold(Tokens::default(), |t, a| t.plus(a.tokens()))
    }

    /// Where the context stands against the compaction ceiling (see `insight::ceiling`), and
    /// how many calls are left at the pace of the last ones. Claude Code may compact earlier.
    pub fn forecast(&self, ceiling: Option<u64>) -> Forecast {
        let ctx = self.agents[0].ctx;
        let (limit, known) = match ceiling.filter(|&c| c > ctx) {
            Some(c) => (c, true),
            // The window size is not in the transcript: 200k, or 1M once usage is past it.
            None => (if ctx > 200_000 { 1_000_000 } else { 200_000 }, false),
        };
        let recent: Vec<(i64, u64)> = self.ctx_samples.iter().rev().take(8).rev().copied().collect();
        let (mut per_call, mut calls, mut secs) = (0.0, None, None);
        if let (Some(&(t0, c0)), Some(&(t1, c1))) = (recent.first(), recent.last()) {
            let n = (recent.len() - 1) as f64;
            if n >= 2.0 && c1 > c0 {
                per_call = (c1 - c0) as f64 / n;
                let left = limit.saturating_sub(ctx) as f64 / per_call;
                calls = Some(left);
                if t1 > t0 {
                    secs = Some(left * (t1 - t0) as f64 / n / 1000.0);
                }
            }
        }
        Forecast { ctx, limit, known, per_call, calls, secs }
    }

    /// What the session does right now: waiting for a tool (since when), or producing an answer
    /// whose kind (thinking or writing) is only known once its record is written.
    pub fn phase_now(&self) -> Option<(Option<Phase>, i64)> {
        if !self.alive {
            return None;
        }
        if let Some((_, st)) = self.current().filter(|(_, st)| st.bg.is_none()) {
            return Some((Some(Phase::Tool), st.started));
        }
        (self.status == "busy").then(|| (None, self.agents[0].mark))
    }

    /// The three phase traces over the last `n` buckets, with the intervals still open added
    /// live: tool time for agents waiting on a tool, and a fourth trace for answers in progress.
    pub fn waves_live(&self, now: i64, n: usize) -> ([Vec<u64>; 3], Vec<u64>) {
        let mut out: [Vec<u64>; 3] = std::array::from_fn(|p| self.waves[p].series(now, n).into_iter().map(u64::from).collect());
        let mut pending = vec![0u64; n];
        if !self.alive {
            return (out, pending);
        }
        let first = now.div_euclid(BUCKET_MS) - n as i64 + 1;
        let add = |series: &mut Vec<u64>, from: i64| {
            let mut t = from.max(first * BUCKET_MS);
            while t < now {
                let edge = ((t.div_euclid(BUCKET_MS) + 1) * BUCKET_MS).min(now);
                let k = (t.div_euclid(BUCKET_MS) - first) as usize;
                if k < n {
                    series[k] += (edge - t) as u64;
                }
                t = edge;
            }
        };
        for (a, ag) in self.agents.iter().enumerate() {
            if ag.mark == 0 || !self.agent_running(a) {
                continue;
            }
            let waiting = ag.items.iter().rev().take(50).any(|i| matches!(i, Item::Step(s) if s.ended.is_none()));
            if waiting {
                add(&mut out[Phase::Tool as usize], ag.mark);
            } else {
                add(&mut pending, ag.mark);
            }
        }
        (out, pending)
    }

    /// Hit ratio of the prompt cache over the last `n` buckets.
    pub fn cache_ratio(&self, now: i64, n: usize) -> Option<f64> {
        let (f, w, r) = (self.fresh_in.sum(now, n), self.cache_w.sum(now, n), self.cache_r.sum(now, n));
        (f + w + r > 0).then(|| r as f64 / (f + w + r) as f64)
    }

    pub fn mcp_running(&self) -> HashSet<String> {
        if !self.alive {
            return HashSet::new();
        }
        self.steps().filter(|(_, s)| s.kind == Kind::Mcp && s.ended.is_none()).map(|(_, s)| parse::mcp_parts(&s.name).0.to_string()).collect()
    }

    pub fn collapsed(&self, a: usize) -> bool {
        self.agents[a].collapsed.unwrap_or(a != 0 && !self.agent_running(a))
    }

    pub fn phase_open(&self, w: usize, p: usize) -> bool {
        let wf = &self.workflows[w];
        wf.open.get(p).copied().flatten().unwrap_or_else(|| wf.live.iter().any(|&a| wf.phase_of(&self.agents[a].wf_phase) == Some(p) && self.agent_running(a)))
    }

    pub fn wf_expanded(&self, w: usize) -> bool {
        let wf = &self.workflows[w];
        wf.expanded.unwrap_or_else(|| wf.step.as_deref().and_then(|t| self.step_by_tool(t)).is_some_and(|s| s.running() && self.alive))
    }

    pub fn set_all_expanded(&mut self, open: bool) {
        for (a, ag) in self.agents.iter_mut().enumerate() {
            ag.collapsed = Some(if a == 0 { false } else { !open });
        }
        for wf in &mut self.workflows {
            wf.expanded = Some(open);
            wf.open.iter_mut().for_each(|o| *o = Some(open));
        }
    }
}

/// A finished workflow run, from its `wf_*.json` record: what a running one is raced against.
pub struct PastRun {
    pub run_id: String,
    pub name: String,
    pub phases: Vec<String>,
    pub start: i64,
    pub dur: i64,
    pub status: String,
    /// Each phase's start and end, as offsets from the run's start.
    pub spans: Vec<Option<(i64, i64)>>,
}

impl PastRun {
    pub fn from_json(v: &Value) -> Option<PastRun> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let start = v.get("startTime").and_then(Value::as_i64)?;
        let dur = v.get("durationMs").and_then(Value::as_i64)?;
        let mut phases = Vec::new();
        let mut agents = Vec::new();
        for p in v.get("workflowProgress").and_then(Value::as_array).into_iter().flatten() {
            match p.get("type").and_then(Value::as_str) {
                Some("workflow_phase") => phases.push(p.get("title").and_then(Value::as_str).unwrap_or("").to_string()),
                Some("workflow_agent") => {
                    let (Some(i), Some(at)) = (p.get("phaseIndex").and_then(Value::as_u64), p.get("startedAt").and_then(Value::as_i64)) else { continue };
                    let took = p.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
                    agents.push((i.saturating_sub(1) as usize, at - start, at - start + took));
                }
                _ => {}
            }
        }
        let mut spans = vec![None; phases.len()];
        for &(i, a, b) in &agents {
            if let Some(slot) = spans.get_mut(i) {
                let (lo, hi): (i64, i64) = slot.unwrap_or((a, b));
                *slot = Some((lo.min(a), hi.max(b)));
            }
        }
        Some(PastRun { run_id: s("runId"), name: s("workflowName"), phases, start, dur, status: s("status"), spans })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: serde_json::Value) -> String {
        v.to_string()
    }

    #[test]
    fn tracks_steps_agents_and_background_tasks() {
        let mut s = Session::new("s", PathBuf::from("/nowhere"));
        s.alive = true;
        let t = "2026-10-16T10:00:00.000Z";
        s.ingest(0, &line(serde_json::json!({"type":"user","timestamp":t,"message":{"content":"hello there"}})));
        s.ingest(0, &line(serde_json::json!({"type":"assistant","timestamp":t,"message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":10,"output_tokens":5},"content":[
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls","description":"List files"}},
            {"type":"tool_use","id":"t2","name":"Agent","input":{"description":"Audit","subagent_type":"Explore"}},
            {"type":"tool_use","id":"t3","name":"mcp__claude_ai_Notion__notion-search","input":{"query":"x"}}
        ]}})));
        // the same message split over another record repeats its usage
        s.ingest(
            0,
            &line(serde_json::json!({"type":"assistant","timestamp":t,"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":7},"content":[]}})),
        );
        assert_eq!(s.agents[0].tokens().output, 7);

        let a = s.add_agent("ag1", "Explore", "Audit", Some("t2".into()));
        assert_eq!(s.step_by_tool("t2").unwrap().child, Some(a));
        assert!(s.agent_running(a));

        s.ingest(0, &line(serde_json::json!({"type":"user","timestamp":"2026-10-16T10:00:02.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a\nb"}]}})));
        s.ingest(0, &line(serde_json::json!({"type":"user","timestamp":"2026-10-16T10:00:02.000Z","toolUseResult":{"isAsync":true,"agentId":"ag1"},"message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"launched"}]}})));
        let t1 = s.step_by_tool("t1").unwrap();
        assert!(!t1.running());
        assert_eq!(t1.finished().unwrap() - t1.started, 2000);
        assert!(s.step_by_tool("t2").unwrap().running(), "async agent still running in background");

        s.ingest(0, &line(serde_json::json!({"type":"queue-operation","timestamp":"2026-10-16T10:01:00.000Z","content":"<task-notification>\n<task-id>ag1</task-id>\n<tool-use-id>t2</tool-use-id>\n<status>completed</status>\n</task-notification>"})));
        assert!(!s.step_by_tool("t2").unwrap().running());
        assert!(!s.agent_running(a));
        assert_eq!(s.mcp["claude_ai_Notion"].calls, 1);
        assert_eq!(s.last_prompt(), Some("hello there"));
    }

    fn ts(sec: i64) -> String {
        format!("2026-10-16T10:{:02}:{:02}.000Z", sec / 60, sec % 60)
    }

    fn call(s: &mut Session, a: usize, sec: i64, id: &str, first: &str, usage: serde_json::Value, tools: &[(&str, &str, serde_json::Value)]) {
        let mut content = vec![serde_json::json!({"type": first, "text": "x", "thinking": ""})];
        if first == "tool_use" {
            content.clear();
        }
        for (tid, name, input) in tools {
            content.push(serde_json::json!({"type":"tool_use","id":tid,"name":name,"input":input}));
        }
        s.ingest(
            a,
            &line(serde_json::json!({"type":"assistant","timestamp":ts(sec),"message":{"id":id,"model":"claude-sonnet-4-5","usage":usage,"content":content}})),
        );
    }

    fn result(s: &mut Session, a: usize, sec: i64, tid: &str, tur: serde_json::Value) {
        result_text(s, a, sec, tid, tur, "ok");
    }

    fn result_text(s: &mut Session, a: usize, sec: i64, tid: &str, tur: serde_json::Value, text: &str) {
        s.ingest(a, &line(serde_json::json!({"type":"user","timestamp":ts(sec),"toolUseResult":tur,"message":{"content":[{"type":"tool_result","tool_use_id":tid,"content":text}]}})));
    }

    #[test]
    fn reads_phases_cache_files_and_compactions() {
        let mut s = Session::new("s", PathBuf::from("/nowhere"));
        s.alive = true;
        s.ingest(0, &line(serde_json::json!({"type":"user","timestamp":ts(0),"message":{"content":"go"}})));
        // 20 s thinking, then 5 s writing the tool call, then a 10 s tool.
        let u = serde_json::json!({"input_tokens":5,"cache_creation_input_tokens":30000,"cache_read_input_tokens":1000,"output_tokens":50,
            "output_tokens_details":{"thinking_tokens":40},"cache_creation":{"ephemeral_1h_input_tokens":30000,"ephemeral_5m_input_tokens":0}});
        call(&mut s, 0, 20, "m1", "thinking", u.clone(), &[]);
        call(&mut s, 0, 25, "m1", "tool_use", u, &[("e1", "Edit", serde_json::json!({"file_path":"/p/a.rs","old_string":"x","new_string":"y"}))]);
        result(&mut s, 0, 35, "e1", serde_json::json!({"filePath":"/p/a.rs","structuredPatch":[{"lines":[" a","-b","+c","+d"]}],"originalFile":"secret"}));
        let now = parse_iso_ms(&ts(40)).unwrap();
        let w: Vec<u64> = (0..3).map(|p| s.waves[p].sum(now, 64)).collect();
        assert_eq!(w, vec![20_000, 5_000, 10_000]);
        assert_eq!(s.thinking.sum(now, 64), 40, "thinking counted once over the split records");
        assert_eq!(s.cache_w.sum(now, 64), 30_000);
        assert_eq!(s.flares.len(), 1, "a large cache write is a flare");
        let f = &s.files["/p/a.rs"];
        assert_eq!((f.edits, f.added, f.removed), (1, 2, 1));
        // 5 fresh at 3 + 30k 1h writes at 6 + 1k reads at 0.3 + 50 out at 15, per million.
        assert_eq!(s.cost_micros, (5.0f64 * 3.0 + 30_000.0 * 6.0 + 1_000.0 * 0.3 + 50.0 * 15.0).round() as u64);

        s.ingest(
            0,
            &line(serde_json::json!({"type":"system","subtype":"compact_boundary","timestamp":ts(50),"compactMetadata":{"trigger":"auto","preTokens":180000}})),
        );
        assert_eq!(s.compactions[0].pre, 180_000);
        assert!(!s.forecast(None).known);
        assert_eq!(s.forecast(Some(180_000)).limit, 180_000);
        for (k, ctx) in [(60, 100_000u64), (70, 110_000), (80, 120_000), (90, 130_000)] {
            call(&mut s, 0, k, &format!("c{k}"), "text", serde_json::json!({"input_tokens":1,"cache_read_input_tokens":ctx - 1,"output_tokens":1}), &[]);
        }
        let fc = s.forecast(Some(180_000));
        assert_eq!(fc.per_call, 10_000.0);
        assert_eq!(fc.calls, Some(5.0));
        assert_eq!(fc.secs, Some(50.0));
    }

    #[test]
    fn follows_workflow_agents_live_and_links_them_to_their_run() {
        let mut s = Session::new("s", PathBuf::from("/nowhere"));
        s.alive = true;
        // The run's agent shows up before the launching call is read.
        let a = s.add_wf_agent("wf_1", "ag1", "build:x", "Build");
        let script = "export const meta = {\n  name: 'demo',\n  phases: [{ title: 'Build' }, { title: 'Review' }],\n}";
        call(&mut s, 0, 1, "m1", "tool_use", serde_json::json!({"output_tokens":1}), &[("w1", "Workflow", serde_json::json!({"script": script}))]);
        result_text(
            &mut s,
            0,
            2,
            "w1",
            serde_json::json!({"status":"async_launched","taskId":"t1","runId":"wf_1"}),
            "Workflow launched in background. Task ID: t1",
        );
        assert_eq!(s.workflows[0].live, vec![a]);
        assert!(s.agent_running(a));
        call(&mut s, a, 3, "m2", "tool_use", serde_json::json!({"output_tokens":1}), &[("so", "StructuredOutput", serde_json::json!({}))]);
        result(&mut s, a, 4, "so", serde_json::json!({}));
        assert!(!s.agent_running(a), "a structured answer ends a workflow agent");
    }

    #[test]
    fn reads_past_runs_with_phase_spans() {
        let v = serde_json::json!({"runId":"wf_9","workflowName":"demo","status":"completed","startTime":1000,"durationMs":9000,"totalTokens":5,
            "workflowProgress":[{"type":"workflow_phase","index":1,"title":"Build"},{"type":"workflow_phase","index":2,"title":"Review"},
            {"type":"workflow_agent","phaseIndex":1,"startedAt":1000,"durationMs":3000},{"type":"workflow_agent","phaseIndex":2,"startedAt":4000,"durationMs":5000},
            {"type":"workflow_agent","phaseIndex":2,"startedAt":4500,"durationMs":1000}]});
        let r = PastRun::from_json(&v).unwrap();
        assert_eq!(r.spans, vec![Some((0, 3000)), Some((3000, 8000))]);
    }
}
