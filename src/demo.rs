//! `--demo`: four made-up sessions at work, for trying ccorbit without a live opencode or showing it
//! without showing real work. Their activity is written as transcript records and fed through
//! the same reader as real ones. Nothing is read from disk and nothing is written.
//!
//! The script covers what ccorbit can show: thinking and writing, file reads and edits, tests,
//! subagents, a workflow raced against its earlier runs, MCP servers, skills, a question waiting
//! for an answer, a call repeated in a loop, two sessions editing one file, a rebuilt prompt
//! cache and a context close to compaction.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use serde_json::{Value, json};

use crate::model::{PastRun, Session};
use crate::time::fmt_iso;

/// How far back the demo starts, so the screens are full from the first frame.
const PREFILL_MS: i64 = 6 * 60_000;

struct Profile {
    name: &'static str,
    cwd: &'static str,
    title: &'static str,
    model: &'static str,
    files: &'static [&'static str],
    test: &'static str,
    prompts: &'static [&'static str],
    searches: &'static [&'static str],
    ctx: u64,
}

/// Two sessions share the web app (and one file, `src/routes/checkout.tsx`), one works on an
/// API close to its compaction ceiling, one on a docs site.
static PROFILES: [Profile; 4] = [
    Profile {
        name: "webapp-12",
        cwd: "/home/demo/code/webapp",
        title: "Checkout redesign",
        model: "qwen3.8-flash-next",
        files: &["src/components/CartSummary.tsx", "src/components/PaymentForm.tsx", "src/styles/checkout.css", "src/lib/pricing.ts", "src/lib/coupons.ts"],
        test: "npm test -- checkout",
        prompts: &["make the checkout a single page", "the coupon field should validate inline", "run the tests and fix what breaks"],
        searches: &["inline form validation accessible errors", "Stripe payment element appearance api"],
        ctx: 240_000,
    },
    Profile {
        name: "webapp-47",
        cwd: "/home/demo/code/webapp",
        title: "Server-side rendering",
        model: "qwen3.8-flash-next",
        files: &["server/render.ts", "server/cache.ts", "src/entry-server.tsx", "vite.config.ts"],
        test: "npm run test:ssr",
        prompts: &["render the product pages on the server", "cache rendered pages for a minute"],
        searches: &["vite ssr streaming react 19", "stale-while-revalidate page cache node"],
        ctx: 410_000,
    },
    Profile {
        name: "api-3",
        cwd: "/home/demo/code/api",
        title: "Rate limiter per tenant",
        model: "qwen3.8-flash-next",
        files: &["limiter/token_bucket.go", "limiter/tenant.go", "limiter/limiter_test.go", "cmd/api/main.go", "internal/store/redis.go"],
        test: "go test ./limiter/...",
        prompts: &["make the limiter per tenant", "add a burst allowance", "load test it at 5k requests per second"],
        searches: &["token bucket redis lua atomic", "go rate limiter per key memory"],
        ctx: 895_000,
    },
    Profile {
        name: "docs-site-9",
        cwd: "/home/demo/code/docs-site",
        title: "Docs search",
        model: "qwen3.8",
        files: &["content/guide/search.md", "src/search/index.ts", "src/search/rank.ts", "astro.config.mjs"],
        test: "pnpm build",
        prompts: &["add full-text search to the docs", "rank exact title matches first"],
        searches: &["pagefind astro integration", "bm25 title boost"],
        ctx: 120_000,
    },
];

const SHARED_FILE: &str = "src/routes/checkout.tsx";
const PHASES: [&str; 4] = ["Build", "Review", "Verify", "Fix"];

enum Act {
    /// A transcript record for the main agent (`""`) or the agent with this id.
    Line(String, Value),
    Agent {
        id: String,
        kind: &'static str,
        desc: String,
        tool: String,
    },
    WfAgent {
        run: String,
        id: String,
        label: String,
        phase: &'static str,
    },
    Status(&'static str),
    WfDone(Value),
}

struct Ev {
    at: i64,
    seq: u64,
    si: usize,
    act: Act,
}

impl PartialEq for Ev {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for Ev {}
impl PartialOrd for Ev {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Ev {
    /// Earliest first out of the heap.
    fn cmp(&self, o: &Self) -> Ordering {
        (o.at, o.seq).cmp(&(self.at, self.seq))
    }
}

pub struct Demo {
    queue: BinaryHeap<Ev>,
    seq: u64,
    rng: u64,
    /// Per session: when its next task may start, its context size, its running workflow.
    cursor: [i64; 4],
    ctx: [u64; 4],
    wf_until: [i64; 4],
    runs: u64,
}

impl Demo {
    /// The four sessions, and the earlier runs of the workflow the demo races against.
    pub fn start(now: i64) -> (Demo, Vec<Session>, Vec<PastRun>) {
        let mut sessions = Vec::new();
        for p in &PROFILES {
            let mut s = Session::new(&format!("demo-{}", p.name));
            s.name = p.name.into();
            s.cwd = p.cwd.into();
            s.alive = true;
            s.status = "idle".into();
            sessions.push(s);
        }
        let t0 = now - PREFILL_MS;
        let mut d = Demo {
            queue: BinaryHeap::new(),
            seq: 0,
            rng: 0x2545_f491_4f6c_dd1d,
            cursor: [t0, t0 + 9_000, t0 + 4_000, t0 + 15_000],
            ctx: std::array::from_fn(|i| PROFILES[i].ctx),
            wf_until: [0; 4],
            runs: 0,
        };
        for (si, p) in PROFILES.iter().enumerate() {
            d.push(t0 - 1000, si, Act::Line(String::new(), json!({"type": "ai-title", "aiTitle": p.title})));
        }
        // The API session was compacted once at its ceiling: that is what its gauge counts down to.
        d.call(2, "", t0 - 900, "text", vec![], 300, 0, 2000, false);
        d.push(
            t0 - 800,
            2,
            Act::Line(String::new(), json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": "auto", "preTokens": 968_000}})),
        );
        // The webapp session starts a workflow early, so a race is under way from the first frame.
        d.workflow(0, t0 + 200_000);
        let history = (0..3).map(|k| past_run(now - (k + 1) * 86_400_000, 345_000 + k * 20_000)).collect();
        (d, sessions, history)
    }

    /// Plays every event due by `now`, writing the next tasks of each session as needed.
    pub fn tick(&mut self, now: i64, sessions: &mut [Session], history: &mut Vec<PastRun>) -> bool {
        for si in 0..PROFILES.len() {
            while self.cursor[si] < now + 30_000 {
                self.episode(si);
            }
        }
        let mut changed = false;
        while self.queue.peek().is_some_and(|e| e.at <= now) {
            let Some(ev) = self.queue.pop() else { break };
            let s = &mut sessions[ev.si];
            match ev.act {
                Act::Line(agent, mut v) => {
                    let a = if agent.is_empty() { Some(0) } else { s.agent_index(&agent) };
                    if let (Some(a), Some(o)) = (a, v.as_object_mut()) {
                        o.insert("timestamp".into(), Value::String(fmt_iso(ev.at)));
                        s.ingest(a, &v.to_string());
                    }
                }
                Act::Agent { id, kind, desc, tool } => {
                    s.add_agent(&id, kind, &desc, Some(tool));
                }
                Act::WfAgent { run, id, label, phase } => {
                    s.add_wf_agent(&run, &id, &label, phase);
                }
                Act::Status(st) => s.status = st.into(),
                Act::WfDone(v) => {
                    s.on_workflow_json(&v);
                    if let Some(r) = PastRun::from_json(&v) {
                        history.push(r);
                    }
                }
            }
            changed = true;
        }
        changed
    }

    fn push(&mut self, at: i64, si: usize, act: Act) {
        self.seq += 1;
        self.queue.push(Ev { at, seq: self.seq, si, act });
    }

    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn between(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.rand() % (hi - lo).max(1) as u64) as i64
    }

    fn chance(&mut self, p: f64) -> bool {
        (self.rand() % 1000) as f64 / 1000.0 < p
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.rand() % items.len() as u64) as usize]
    }

    fn id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}_demo{:x}", self.seq)
    }

    /// One API call: an optional thinking record, then the text or tool calls, with usage that
    /// grows the context. Returns when the last record is written.
    #[allow(clippy::too_many_arguments)]
    fn call(&mut self, si: usize, agent: &str, at: i64, kind: &str, tools: Vec<(String, &str, Value)>, out: u64, think: u64, ctx: u64, rebuild: bool) -> i64 {
        let msg = self.id("msg");
        let model = if agent.is_empty() {
            PROFILES[si].model
        } else if agent.starts_with("ex") {
            "qwen3.8-flash"
        } else {
            "qwen3.8-coder"
        };
        let write = if rebuild { ctx.saturating_sub(4000) } else { self.between(800, 4000) as u64 };
        // opencode prices its own calls; the table has no local model, so the demo carries the
        // number the same way a live session does (this is what the reader bakes into usage).
        let cost = (3.0 * 4.0 + write as f64 * 5.0 + (ctx.saturating_sub(write)) as f64 * 0.20 + out as f64 * 20.0).round() as u64;
        let usage = json!({
            "input_tokens": 3,
            "cache_creation_input_tokens": write,
            "cache_read_input_tokens": ctx.saturating_sub(write),
            "output_tokens": out,
            "output_tokens_details": {"thinking_tokens": think},
            "cache_creation": {"ephemeral_1h_input_tokens": write, "ephemeral_5m_input_tokens": 0},
            "ccorbit_cost_micros": cost,
            "speed": "standard"
        });
        let mut t = at;
        if think > 0 {
            let content = json!([{"type": "thinking", "thinking": "", "signature": "demo"}]);
            self.push(t, si, Act::Line(agent.into(), json!({"type": "assistant", "message": {"id": msg, "model": model, "usage": usage, "content": content}})));
            t += self.between(800, 3000);
        }
        let content: Vec<Value> = if tools.is_empty() {
            vec![json!({"type": kind, "text": "Done. The change is in and the tests pass."})]
        } else {
            tools.iter().map(|(id, name, input)| json!({"type": "tool_use", "id": id, "name": name, "input": input})).collect()
        };
        self.push(t, si, Act::Line(agent.into(), json!({"type": "assistant", "message": {"id": msg, "model": model, "usage": usage, "content": content}})));
        t
    }

    #[allow(clippy::too_many_arguments)]
    fn result(&mut self, si: usize, agent: &str, at: i64, tool: &str, text: &str, error: bool, tur: Value) {
        let line = json!({"type": "user", "toolUseResult": tur, "message": {"content": [{"type": "tool_result", "tool_use_id": tool, "content": text, "is_error": error}]}});
        self.push(at, si, Act::Line(agent.into(), line));
    }

    fn prompt(&mut self, si: usize, at: i64, text: &str) {
        self.push(at, si, Act::Line(String::new(), json!({"type": "user", "message": {"content": text}})));
    }

    /// The next piece of work for a session: mostly coding, sometimes an agent, a workflow, a
    /// question, a loop, MCP calls or a skill. Ends with a short pause.
    fn episode(&mut self, si: usize) {
        let t = self.cursor[si];
        self.push(t, si, Act::Status("busy"));
        let roll = self.rand() % 100;
        let mut end = match roll {
            _ if si == 0 && self.wf_until[0] < t && self.runs < 50 && roll < 12 => self.workflow(0, t),
            0..=44 => self.coding(si, t, "", 4),
            45..=59 => self.subagent(si, t),
            60..=67 => self.question(si, t),
            68..=74 if si == 2 || si == 3 => self.looping(si, t),
            75..=86 => self.mcp(si, t),
            87..=92 => self.skill(si, t),
            _ => self.coding(si, t, "", 3),
        };
        // The API session drifts toward its ceiling, then gets compacted.
        if self.ctx[si] > 962_000 {
            end += 2000;
            let pre = self.ctx[si];
            self.push(
                end,
                si,
                Act::Line(String::new(), json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": "auto", "preTokens": pre}})),
            );
            self.ctx[si] = 70_000;
        }
        let pause = if self.chance(0.3) { self.between(8_000, 20_000) } else { self.between(1_500, 5_000) };
        self.push(end + 300, si, Act::Status("idle"));
        self.cursor[si] = end + pause;
    }

    fn grow(&mut self, si: usize, agent: &str) -> u64 {
        if !agent.is_empty() {
            return self.between(18_000, 60_000) as u64;
        }
        let step = if si == 2 { self.between(600, 1_500) } else { self.between(1_000, 4_000) };
        self.ctx[si] += step as u64;
        self.ctx[si]
    }

    /// Thinking, a few tool calls (reads, searches, edits, tests), then a short answer.
    fn coding(&mut self, si: usize, mut t: i64, agent: &str, steps: usize) -> i64 {
        let p = &PROFILES[si];
        if agent.is_empty() && self.chance(0.4) {
            let text = self.pick(p.prompts);
            self.prompt(si, t, text);
            t += 500;
        }
        let rebuild = agent.is_empty() && self.chance(0.12);
        for k in 0..steps {
            let ctx = self.grow(si, agent);
            let think = if self.chance(0.7) { self.between(80, 2500) as u64 } else { 0 };
            t += self.between(1_500, 7_000);
            let tool = self.id("toolu");
            let file = if si <= 1 && self.chance(0.08) { SHARED_FILE } else { self.pick(p.files) };
            let path = format!("{}/{file}", p.cwd);
            let (name, input, dur) = match self.rand() % 10 {
                0..=2 => ("Read", json!({"file_path": path}), self.between(150, 700)),
                3 => ("Grep", json!({"pattern": "TODO|FIXME", "path": p.cwd}), self.between(300, 1200)),
                4..=6 => ("Edit", json!({"file_path": path, "old_string": "a", "new_string": "b"}), self.between(150, 500)),
                7 => ("WebSearch", json!({"query": self.pick(p.searches)}), self.between(2_000, 5_000)),
                _ => ("Bash", json!({"command": p.test, "description": "Run the tests"}), self.between(4_000, 14_000)),
            };
            let out = self.between(120, 1800) as u64;
            t = self.call(si, agent, t, "text", vec![(tool.clone(), name, input)], out, think, ctx, rebuild && k == 0);
            t += dur;
            let tur = if name == "Edit" {
                let (add, del) = (self.between(1, 40), self.between(0, 15));
                let lines: Vec<String> = (0..add).map(|_| "+x".to_string()).chain((0..del).map(|_| "-y".to_string())).collect();
                json!({"filePath": path, "structuredPatch": [{"lines": lines}]})
            } else {
                json!({})
            };
            self.result(si, agent, t, &tool, "ok", false, tur);
        }
        t += self.between(2_000, 6_000);
        let ctx = self.grow(si, agent);
        let (out, think) = (self.between(300, 1500) as u64, self.between(100, 900) as u64);
        self.call(si, agent, t, "text", vec![], out, think, ctx, false)
    }

    /// A subagent mapping the code, its steps nested under the call that started it.
    fn subagent(&mut self, si: usize, mut t: i64) -> i64 {
        let tool = self.id("toolu");
        let id = self.id("ex");
        let desc = self.pick(&["Map the payment flow", "Find every caller of the limiter", "Audit the test suite", "Check the search index size"]);
        let ctx = self.grow(si, "");
        t = self.call(si, "", t + 1500, "text", vec![(tool.clone(), "Agent", json!({"subagent_type": "Explore", "description": desc}))], 400, 300, ctx, false);
        self.push(t + 200, si, Act::Agent { id: id.clone(), kind: "Explore", desc: desc.into(), tool: tool.clone() });
        self.push(t + 300, si, Act::Line(id.clone(), json!({"type": "user", "message": {"content": desc}})));
        let end = self.coding(si, t + 400, &id, 4);
        self.result(si, "", end + 500, &tool, "Mapped: 6 files, 2 entry points.", false, json!({}));
        end + 500
    }

    /// A workflow in four phases whose agents work in the background while the session goes on.
    fn workflow(&mut self, si: usize, t: i64) -> i64 {
        self.runs += 1;
        let (tool, task, run) = (self.id("toolu"), format!("wdemo{}", self.runs), format!("wf_demo{}", self.runs));
        let script = format!(
            "export const meta = {{\n  name: 'review-and-fix',\n  description: 'Build, review, verify and fix',\n  phases: [{}],\n}}",
            PHASES.iter().map(|p| format!("{{ title: '{p}' }}")).collect::<Vec<_>>().join(", ")
        );
        let ctx = self.grow(si, "");
        let t = self.call(si, "", t + 1000, "text", vec![(tool.clone(), "Workflow", json!({"script": script}))], 900, 600, ctx, false);
        let text = format!("Workflow launched in background. Task ID: {task}\nSummary: review and fix the checkout");
        self.result(si, "", t + 800, &tool, &text, false, json!({"status": "async_launched", "taskId": task, "runId": run, "workflowName": "review-and-fix"}));
        // Build (two in turn), Review (three at once), Verify (three), Fix (one).
        let plan: [(usize, &[(&str, i64)]); 4] = [
            (0, &[("build:checkout", 75_000), ("build:tests", 65_000)]),
            (1, &[("review:design", 55_000), ("review:a11y", 48_000), ("review:security", 62_000)]),
            (2, &[("verify:design", 35_000), ("verify:a11y", 30_000), ("verify:security", 40_000)]),
            (3, &[("fix", 70_000)]),
        ];
        let mut at = t + 2_000;
        let mut agents = Vec::new();
        for (phase, members) in plan {
            let sequential = phase == 0;
            let mut phase_end = at;
            for &(label, dur) in members {
                let id = self.id("wa");
                let start = if sequential { phase_end } else { at };
                self.push(start, si, Act::WfAgent { run: run.clone(), id: id.clone(), label: label.into(), phase: PHASES[phase] });
                self.push(start + 100, si, Act::Line(id.clone(), json!({"type": "user", "message": {"content": label}})));
                let mut s = start + 200;
                while s < start + dur - 6_000 {
                    s = self.coding(si, s, &id, 1);
                }
                let so = self.id("toolu");
                s = self.call(si, &id, s + 1_000, "text", vec![(so.clone(), "StructuredOutput", json!({"ok": true}))], 300, 100, 30_000, false);
                self.result(si, &id, s + 200, &so, "ok", false, json!({}));
                agents.push(json!({"type": "workflow_agent", "agentId": id, "label": label, "phaseIndex": phase + 1, "state": "done", "startedAt": start, "durationMs": s - start, "model": "qwen3.8-coder"}));
                phase_end = phase_end.max(s + 300);
            }
            at = phase_end + 1_000;
        }
        let note =
            format!("<task-notification>\n<task-id>{task}</task-id>\n<tool-use-id>{tool}</tool-use-id>\n<status>completed</status>\n</task-notification>");
        self.push(at, si, Act::Line(String::new(), json!({"type": "queue-operation", "content": note})));
        let mut progress: Vec<Value> = PHASES.iter().enumerate().map(|(i, p)| json!({"type": "workflow_phase", "index": i + 1, "title": p})).collect();
        progress.extend(agents);
        let start = t;
        self.push(at + 100, si, Act::WfDone(json!({"runId": run, "taskId": task, "workflowName": "review-and-fix", "status": "completed", "startTime": start, "durationMs": at - start, "totalTokens": 1_800_000, "workflowProgress": progress})));
        self.wf_until[si] = at;
        t + 1_000
    }

    /// A question left open until someone answers: the session needs you.
    fn question(&mut self, si: usize, t: i64) -> i64 {
        let tool = self.id("toolu");
        let q = self.pick(&["Ship the new checkout behind a flag?", "Keep the old endpoint for a release?", "Index the changelog too?"]);
        let ctx = self.grow(si, "");
        let t = self.call(si, "", t + 2_000, "text", vec![(tool.clone(), "AskUserQuestion", json!({"questions": [{"question": q}]}))], 200, 400, ctx, false);
        let wait = self.between(12_000, 22_000);
        self.result(si, "", t + wait, &tool, "Yes", false, json!({}));
        t + wait + 1_000
    }

    /// The same failing test command four times in a row, then a fix.
    fn looping(&mut self, si: usize, mut t: i64) -> i64 {
        let cmd = json!({"command": PROFILES[si].test, "description": "Run the tests"});
        for _ in 0..4 {
            let tool = self.id("toolu");
            let ctx = self.grow(si, "");
            let at = t + self.between(1_500, 3_000);
            t = self.call(si, "", at, "text", vec![(tool.clone(), "Bash", cmd.clone())], 150, 0, ctx, false);
            t += self.between(3_000, 6_000);
            self.result(si, "", t, &tool, "FAIL limiter_test.go:42: expected 429, got 200", true, json!({}));
        }
        self.coding(si, t, "", 2)
    }

    /// Calls to MCP servers: issues on GitHub, a ticket on Linear.
    fn mcp(&mut self, si: usize, mut t: i64) -> i64 {
        for (name, input) in [("mcp__github__search_issues", json!({"query": "checkout is:open"})), ("mcp__linear__get_issue", json!({"id": "ENG-412"}))] {
            let tool = self.id("toolu");
            let ctx = self.grow(si, "");
            let at = t + self.between(1_500, 4_000);
            t = self.call(si, "", at, "text", vec![(tool.clone(), name, input)], 200, 300, ctx, false);
            t += self.between(900, 3_000);
            self.result(si, "", t, &tool, "3 results", false, json!({}));
        }
        self.coding(si, t, "", 2)
    }

    fn skill(&mut self, si: usize, t: i64) -> i64 {
        let tool = self.id("toolu");
        let skill = self.pick(&["frontend-design", "code-review", "webapp-testing"]);
        let ctx = self.grow(si, "");
        let t = self.call(si, "", t + 1_500, "text", vec![(tool.clone(), "Skill", json!({"skill": skill}))], 200, 200, ctx, false);
        self.result(si, "", t + 400, &tool, "loaded", false, json!({}));
        self.coding(si, t + 500, "", 3)
    }
}

/// An earlier run of the demo workflow, for the race.
fn past_run(start: i64, dur: i64) -> PastRun {
    let f = dur as f64 / 345_000.0;
    let at = |ms: i64| (ms as f64 * f) as i64;
    PastRun {
        run_id: format!("wf_demo_past{start}"),
        name: "review-and-fix".into(),
        phases: PHASES.iter().map(|p| p.to_string()).collect(),
        start,
        dur,
        status: "completed".into(),
        spans: vec![Some((0, at(150_000))), Some((at(150_000), at(215_000))), Some((at(215_000), at(260_000))), Some((at(260_000), dur))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_fills_every_reading() {
        let now = 1_790_000_000_000;
        let (mut demo, mut sessions, mut history) = Demo::start(now);
        demo.tick(now, &mut sessions, &mut history);
        assert_eq!(sessions.len(), 4);
        assert!(sessions.iter().all(|s| !s.feed.is_empty()), "every session has events");
        assert!(sessions[0].workflows.first().is_some_and(|w| !w.live.is_empty()), "the workflow's agents are followed live");
        assert!(!sessions[0].files.is_empty() && sessions[0].cost_micros > 0);
        let api = &sessions[2];
        assert!(api.compactions.len() == 1 && api.agents[0].ctx > 900_000, "the API session sits near its ceiling: {}", api.agents[0].ctx);
        assert!(sessions.iter().any(|s| s.agents.len() > 1), "subagents ran");
        // An hour later it is still going, and the race found its earlier runs.
        let later = now + 3_600_000;
        demo.tick(later, &mut sessions, &mut history);
        assert!(history.len() > 3, "finished workflow runs join the history");
    }
}
