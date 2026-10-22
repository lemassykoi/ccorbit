//! Remote opencode hosts: the other side of `--export`. Where `src/opencode.rs` reads *this* box's
//! `opencode.db`, a central ccorbit lists peer machines in a `--hosts` JSON file and polls each one's
//! exporter, replaying the body-free summary operations it publishes into the same `Session` state.
//! Everything here is read-only `http://` to the addresses you listed, one short GET per host per
//! second (backing off on failure), through `src/net.rs` — the only socket file. Nothing in this
//! module touches a database or a port directly.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::model::Session;
use crate::net;
use crate::opencode::{EXPORT_FORMAT, HostEnvelope, OPENCODE_SEEN, apply_ops};

/// Poll a host every second: the exporter itself ticks the database at 1 s, so this keeps the
/// central as close to local as the LAN allows. Failure backs off from this up to `MAX_BACKOFF`.
const POLL_EVERY: Duration = Duration::from_millis(1000);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// A host that has not answered in this long is shown `stale` (its cards dim) but kept; a rebooting
/// box (.49 boots Windows) should not make its sessions flash away.
const STALE_AFTER: Duration = Duration::from_secs(15);
/// Past this a silent host's sessions are dropped entirely, and its applied-operation cursors too.
const DROP_AFTER: Duration = Duration::from_secs(20 * 60);

pub struct Host {
    pub name: String,
    url: String,
    token: Option<String>,
    /// Ready to show: the last poll answered with a well-formed envelope.
    pub alive: bool,
    /// A live host that has not answered recently: cards stay but read stale.
    pub stale: bool,
    last_ok: Option<Instant>,
    next_poll: Option<Instant>,
    fail: u32,
    /// Sessions this host had at its last good answer (for the footer strip).
    seen: usize,
    error: Option<String>,
    /// `host clock − central clock` at the last good answer, ms. Used only for age labels; the
    /// busy/idle verdict is made on the host, so skew cannot flip it.
    pub skew_ms: i64,
    /// How long the last poll's GET took, ms (0 before the first). Read by `--trace` to show a host
    /// that is slow to answer -- the kind of stall that makes the wall (and the director) lag.
    pub last_ms: u128,
}

pub struct Hosts {
    pub list: Vec<Host>,
    pub error: Option<String>,
    /// qualified id ("host:ses_…") → operations already replayed, so each poll rebuilds only on change.
    applied: HashMap<String, usize>,
    /// qualified id → what the exporter *said* it had (`total ops`, `retained ops`, `children`). Held
    /// for `--trace`, which compares it with the central's replayed `Session` to spot a fidelity gap.
    pub reported: HashMap<String, (u64, u64, u64)>,
}

impl Hosts {
    /// No `--hosts`: the central watches only its own machine.
    pub fn none() -> Hosts {
        Hosts { list: Vec::new(), error: None, applied: HashMap::new(), reported: HashMap::new() }
    }

    /// Reads the file; a bad path or bad file is kept as `error` and shown, not fatal (like `--engines`).
    pub fn load(path: &std::path::Path) -> Hosts {
        match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            None => Hosts { list: Vec::new(), error: Some(format!("cannot read {}", path.display())), applied: HashMap::new(), reported: HashMap::new() },
            Some(v) => {
                let mut list = Vec::new();
                for e in v.as_array().into_iter().flatten() {
                    let Some(url) = e.get("url").and_then(Value::as_str) else { continue };
                    // Only the ccorbit export protocol; an unknown `kind` is skipped, not guessed.
                    if !matches!(e.get("kind").and_then(Value::as_str).unwrap_or("ccorbit"), "ccorbit" | "") {
                        continue;
                    }
                    let token = e.get("token_file").and_then(Value::as_str).map(net::expand_home).and_then(|p| net::read_key(&p));
                    list.push(Host {
                        name: e.get("name").and_then(Value::as_str).unwrap_or("host").to_string(),
                        url: url.to_string(),
                        token,
                        alive: false,
                        stale: false,
                        last_ok: None,
                        next_poll: None,
                        fail: 0,
                        seen: 0,
                        error: None,
                        skew_ms: 0,
                        last_ms: 0,
                    });
                }
                Hosts { list, error: None, applied: HashMap::new(), reported: HashMap::new() }
            }
        }
    }

    /// Polls the hosts whose turn it is and merges their sessions into `sessions` (the shared list
    /// every screen reads; the local reader leaves host-qualified ids alone). Returns whether
    /// anything changed.
    pub fn tick(&mut self, sessions: &mut Vec<Session>, now: i64) -> bool {
        let mut changed = false;
        for h in &mut self.list {
            changed |= h.poll(sessions, now, &mut self.applied, &mut self.reported);
        }
        changed
    }

    /// One short line per host, for the footer.
    pub fn strips(&self) -> Vec<String> {
        let mut out: Vec<String> = self.list.iter().map(Host::strip).collect();
        if let Some(err) = &self.error {
            out.push(format!("hosts: {err}"));
        }
        out
    }

    /// Per-host telemetry lines for `--trace`: the poll latency and the reported session count, so a
    /// slow or half-dead host is visible in the log next to the wall's own timings.
    pub fn trace(&self) -> Vec<String> {
        self.list
            .iter()
            .map(|h| {
                let err = h.error.as_ref().map(|e| format!(" err={e}")).unwrap_or_default();
                let stale = if h.stale { " stale" } else { "" };
                format!(
                    "  host {name} alive={alive}{stale} get={ms}ms sessions={seen} skew={skew}ms{err}",
                    name = h.name,
                    alive = h.alive,
                    stale = stale,
                    ms = h.last_ms,
                    seen = h.seen,
                    skew = h.skew_ms / 1000,
                    err = err
                )
            })
            .collect()
    }
}

impl Host {
    fn poll(&mut self, sessions: &mut Vec<Session>, now: i64, applied: &mut HashMap<String, usize>, reported: &mut HashMap<String, (u64, u64, u64)>) -> bool {
        if self.next_poll.is_some_and(|t| t.elapsed() < POLL_EVERY) {
            return false;
        }
        let endpoint = self.export_url();
        let t0 = Instant::now();
        match net::http_get(&endpoint, self.token.as_deref()) {
            Ok(body) => {
                self.last_ms = t0.elapsed().as_millis();
                self.ingest(&body, sessions, now, applied, reported)
            }
            Err(_) => {
                self.last_ms = t0.elapsed().as_millis();
                self.fail += 1;
                self.next_poll = Some(Instant::now() + POLL_EVERY.mul_f32((self.fail.min(4) as f32) * 2.0).min(MAX_BACKOFF));
                self.stale = self.last_ok.is_some_and(|t| t.elapsed() >= STALE_AFTER);
                // A host that is fully down never reaches the success path, so prune here too once
                // it has been unreachable past DROP_AFTER (a rebooting box keeps its cards until then).
                if self.last_ok.is_some_and(|t| t.elapsed() >= DROP_AFTER) {
                    let me = self.name.clone();
                    let prefix = format!("{me}:");
                    let before = sessions.len();
                    sessions.retain(|s| s.host != me);
                    if sessions.len() != before {
                        applied.retain(|k, _| !k.starts_with(&prefix));
                        reported.retain(|k, _| !k.starts_with(&prefix));
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Parse and apply one envelope. A wrong format or opencode version is surfaced, never guessed.
    fn ingest(
        &mut self,
        body: &str,
        sessions: &mut Vec<Session>,
        now: i64,
        applied: &mut HashMap<String, usize>,
        reported: &mut HashMap<String, (u64, u64, u64)>,
    ) -> bool {
        let parsed = serde_json::from_str::<Value>(body).ok().and_then(|v| HostEnvelope::from_json(&v));
        self.next_poll = Some(Instant::now() + POLL_EVERY);
        let Some(env) = parsed else {
            self.error = Some("bad export".into());
            self.fail += 1;
            return false;
        };
        if env.format != EXPORT_FORMAT {
            self.error = Some(format!("export v{} (this ccorbit reads v{EXPORT_FORMAT})", env.format));
            self.alive = false;
            self.stale = false;
            return false;
        }
        if !env.opencode.is_empty() && env.opencode != OPENCODE_SEEN {
            // Not fatal: same ccorbit build, but the peer's opencode differs. Say so in the strip.
            self.error = Some(format!("opencode {}", env.opencode));
        } else {
            self.error = None;
        }
        let mut changed = !self.alive || self.stale;
        self.alive = true;
        self.stale = false;
        self.fail = 0;
        self.last_ok = Some(Instant::now());
        self.skew_ms = env.now - now;

        let me = self.name.clone();
        let mut present: HashSet<String> = HashSet::new();
        for hs in &env.sessions {
            let qual = format!("{me}:{}", hs.id);
            present.insert(qual.clone());
            // What the exporter says this session holds -- kept for `--trace` to diff against the
            // central's replayed Session (a mismatch localises a fidelity gap to the replay).
            reported.insert(qual.clone(), (hs.count as u64, hs.ops.len() as u64, hs.children.len() as u64));
            // Rebuild from the whole operation log whenever the host signals new activity. The
            // trigger is `hs.count`, the exporter's monotonic total of operations ever emitted -- NOT
            // the retained log length: a long session that hits MAX_OPS keeps a constant retained
            // length while its contents shift, and rebuilding on that length froze the card (a
            // 476-message Jarvis). The exporter ships the full retained log in reader order, so a
            // child's AddAgent always precedes its Ingest lines; replaying it onto a fresh Session is
            // correct by construction (and removes the earlier suffix-append crash class).
            let built = applied.get(&qual).copied();
            if built != Some(hs.count) {
                let mut fresh = Session::new(&qual);
                fresh.host = me.clone();
                fresh.name = format!("projects-{}", tag_of(&hs.id));
                fresh.cwd = hs.directory.clone();
                fresh.title = hs.title.clone();
                fresh.alive = true;
                apply_ops(&mut fresh, &hs.ops);
                if fresh.agents.first().is_some_and(|a| a.model.is_empty()) && !hs.model.is_empty() {
                    fresh.agents[0].model = hs.model.clone();
                }
                match sessions.iter().position(|s| s.id == qual) {
                    Some(i) => sessions[i] = fresh,
                    None => sessions.push(fresh),
                }
                applied.insert(qual, hs.count);
                changed = true;
            } else if let Some(i) = sessions.iter().position(|s| s.id == qual) {
                // Log unchanged: still pick up a folder or title the host renamed.
                let s = &mut sessions[i];
                if s.cwd != hs.directory {
                    s.cwd = hs.directory.clone();
                    changed = true;
                }
                if s.title != hs.title {
                    s.title = hs.title.clone();
                    changed = true;
                }
            }
        }

        // Sessions this host no longer reports (archived, deleted, or out of the active window) go;
        // a host silent past DROP_AFTER loses all its cards (it is not coming back this session).
        let drop_all = self.last_ok.is_some_and(|t| t.elapsed() >= DROP_AFTER);
        let before = sessions.len();
        sessions.retain(|s| {
            if s.host != me {
                return true; // another host's, or a local session: not ours to remove
            }
            !drop_all && present.contains(&s.id)
        });
        if sessions.len() != before {
            let prefix = format!("{me}:");
            applied.retain(|k, _| !k.starts_with(&prefix) || present.contains(k));
            reported.retain(|k, _| !k.starts_with(&prefix) || present.contains(k));
            changed = true;
        }
        self.seen = present.len();
        changed
    }

    fn export_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        if base.ends_with("/export") { base.to_string() } else { format!("{base}/export") }
    }

    fn strip(&self) -> String {
        if !self.alive {
            return match &self.error {
                Some(e) => format!("{} · {e}", self.name),
                None => format!("{} · down", self.name),
            };
        }
        let mut parts = vec![format!("{} · {} sess", self.name, self.seen)];
        if self.stale {
            parts.push("stale".into());
        }
        // Surface a big clock gap: liveness is decided host-side, but a skewed box's "x ago" labels
        // drift, and a rebooting box (bad NTP) is worth seeing at a glance. Only at two seconds up.
        let skew = self.skew_ms / 1000;
        if skew.abs() >= 2 {
            parts.push(format!("clock {skew:+}s"));
        }
        if let Some(e) = &self.error {
            parts.push(e.clone());
        }
        parts.join(" · ")
    }
}

/// The four-character tag from an opencode id, for a remote card's `name` (mirrors the local reader).
fn tag_of(sid: &str) -> String {
    sid.strip_prefix("ses_").unwrap_or(sid).chars().take(4).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::{HostEnvelope, HostSession, Op};
    use crate::time::fmt_iso;
    use serde_json::json;

    /// Two operations that make a session edit `file` (a tool_use then a tool_result with patch
    /// line counts) — enough for the code city and collision readings to see a touched file.
    fn edit_ops(id: &str, file: &str) -> Vec<Op> {
        let use_line =
            json!({"type":"assistant","timestamp":fmt_iso(5000),"message":{"content":[{"type":"tool_use","id":id,"name":"Edit","input":{"file_path":file}}]}})
                .to_string();
        let res_line = json!({"type":"user","timestamp":fmt_iso(5000),"message":{"content":[{"type":"tool_result","tool_use_id":id,"is_error":false,"content":"Edit applied"}]},"toolUseResult":{"structuredPatch":[{"lines":["+x","-x"]}]}}).to_string();
        vec![Op::Ingest { agent: 0, line: use_line }, Op::Ingest { agent: 0, line: res_line }]
    }

    fn body(host: &str, sessions: Vec<HostSession>) -> String {
        HostEnvelope { format: EXPORT_FORMAT, opencode: OPENCODE_SEEN.to_string(), host: host.to_string(), now: 5000, sessions }.to_json().to_string()
    }

    fn hs(id: &str, dir: &str, file: &str) -> HostSession {
        let ops = edit_ops(&format!("c{id}"), file);
        HostSession {
            id: id.to_string(),
            directory: dir.to_string(),
            title: String::new(),
            model: "qwen3.8".to_string(),
            children: vec![],
            count: ops.len(),
            ops,
        }
    }

    fn a_host(name: &str) -> Host {
        Host {
            name: name.to_string(),
            url: "http://h/".into(),
            token: None,
            alive: false,
            stale: false,
            last_ok: None,
            next_poll: None,
            fail: 0,
            seen: 0,
            error: None,
            skew_ms: 0,
            last_ms: 0,
        }
    }

    #[test]
    fn two_hosts_with_the_same_id_folder_and_file_stay_apart() {
        // The core disambiguation: same raw id, same cwd, same edited path on two machines are
        // three different things — separate cards, and NOT a collision.
        let mut a = a_host("a");
        let mut b = a_host("b");
        let mut sessions: Vec<Session> = Vec::new();
        let mut applied: HashMap<String, usize> = HashMap::new();
        let mut reported: HashMap<String, (u64, u64, u64)> = HashMap::new();
        a.ingest(&body("a", vec![hs("ses_1", "/home/u/app", "/home/u/app/x.ts")]), &mut sessions, 5000, &mut applied, &mut reported);
        b.ingest(&body("b", vec![hs("ses_1", "/home/u/app", "/home/u/app/x.ts")]), &mut sessions, 5000, &mut applied, &mut reported);

        assert_eq!(sessions.len(), 2, "both hosts' sessions are present");
        let mut ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["a:ses_1", "b:ses_1"], "ids are host-qualified and unique");
        assert!(sessions.iter().all(|s| s.alive));
        // The two hosts sharing a file path is not a collision.
        let world = crate::source::World::probe(sessions, 5000);
        assert!(crate::insight::collisions(&world).is_empty(), "same path on two hosts is two files");
    }

    #[test]
    fn same_host_same_file_is_a_real_collision() {
        let mut a = a_host("a");
        let mut sessions: Vec<Session> = Vec::new();
        let mut applied: HashMap<String, usize> = HashMap::new();
        let mut reported: HashMap<String, (u64, u64, u64)> = HashMap::new();
        let both = vec![hs("ses_1", "/home/u/app", "/home/u/app/x.ts"), hs("ses_2", "/home/u/app", "/home/u/app/x.ts")];
        a.ingest(&body("a", both), &mut sessions, 5000, &mut applied, &mut reported);
        let world = crate::source::World::probe(sessions, 5000);
        let cols = crate::insight::collisions(&world);
        assert_eq!(cols.len(), 1, "two local sessions editing one file collide");
        assert_eq!(cols[0].host, "a");
    }

    #[test]
    fn a_host_that_stops_reporting_a_session_loses_its_card() {
        // Archiving/deleting or falling out of the window removes it from the envelope; the central
        // must drop the card (presence is liveness), while another host's card is untouched.
        let mut a = a_host("a");
        let mut sessions: Vec<Session> = Vec::new();
        let mut applied: HashMap<String, usize> = HashMap::new();
        let mut reported: HashMap<String, (u64, u64, u64)> = HashMap::new();
        a.ingest(
            &body("a", vec![hs("ses_1", "/home/u/app", "/home/u/app/x.ts"), hs("ses_2", "/home/u/app", "/home/u/app/y.ts")]),
            &mut sessions,
            5000,
            &mut applied,
            &mut reported,
        );
        assert_eq!(sessions.len(), 2);
        a.ingest(&body("a", vec![hs("ses_2", "/home/u/app", "/home/u/app/y.ts")]), &mut sessions, 6000, &mut applied, &mut reported);
        assert_eq!(sessions.len(), 1, "the unreported session dropped");
        assert_eq!(sessions[0].id, "a:ses_2");
    }

    #[test]
    fn a_shrinking_operation_log_rebuilds_the_session() {
        // A host restart (or a reader trim) sends a log shorter than what was already applied; the
        // central rebuilds the session from it rather than appending onto a mismatched prefix.
        let mut a = a_host("a");
        let mut sessions: Vec<Session> = Vec::new();
        let mut applied: HashMap<String, usize> = HashMap::new();
        let mut reported: HashMap<String, (u64, u64, u64)> = HashMap::new();
        // first: four operations (two edits of x.ts)
        let mut four = edit_ops("c1", "/home/u/app/x.ts");
        four.extend(edit_ops("c2", "/home/u/app/x.ts"));
        let first = HostSession {
            id: "ses_1".into(),
            directory: "/home/u/app".into(),
            title: String::new(),
            model: "x".into(),
            children: vec![],
            count: four.len(),
            ops: four,
        };
        a.ingest(&body("a", vec![first]), &mut sessions, 5000, &mut applied, &mut reported);
        assert_eq!(applied.get("a:ses_1").copied(), Some(4));
        // then: a shorter log (two operations) on a different file → a rebuild, not an append
        let ops = edit_ops("cz", "/home/u/app/z.ts");
        let second = HostSession {
            id: "ses_1".into(),
            directory: "/home/u/app".into(),
            title: String::new(),
            model: "x".into(),
            children: vec![],
            count: ops.len(),
            ops,
        };
        a.ingest(&body("a", vec![second]), &mut sessions, 5000, &mut applied, &mut reported);
        let s = sessions.iter().find(|s| s.id == "a:ses_1").unwrap();
        assert!(s.files.contains_key("/home/u/app/z.ts"), "rebuilt from the fresh log");
        assert!(!s.files.contains_key("/home/u/app/x.ts"), "the old state was replaced, not appended");
    }

    #[test]
    fn a_child_agent_in_the_full_log_is_built_even_after_a_desynced_cursor() {
        // The crash: a prior poll left the central with a 3-op log (main only), then the host
        // (after a restart/re-emit) ships a longer log whose AddAgent sits in the range a naive
        // suffix-append would have skipped -- so the child's Ingest would index a missing agent.
        // Rebuilding from the full log must instead register the agent. Regression for the
        // model.rs:578 index-out-of-bounds panic seen on the wall.
        let mut a = a_host("a");
        let mut sessions: Vec<Session> = Vec::new();
        let mut applied: HashMap<String, usize> = HashMap::new();
        let mut reported: HashMap<String, (u64, u64, u64)> = HashMap::new();
        applied.insert("a:ses_1".to_string(), 3); // the stale cursor from before the desync
        let ops = vec![
            Op::Ingest { agent: 0, line: json!({"type":"user","timestamp":fmt_iso(1000),"message":{"content":"a"}}).to_string() },
            Op::Ingest { agent: 0, line: json!({"type":"user","timestamp":fmt_iso(1500),"message":{"content":"b"}}).to_string() },
            Op::AddAgent { agent: 1, id: "ses_kid".to_string(), kind: "explore".to_string(), description: "sub".to_string(), parent_tool: None },
            Op::Ingest { agent: 1, line: json!({"type":"assistant","timestamp":fmt_iso(2000),"message":{"id":"k1","model":"qwen3.8","content":[{"type":"tool_use","id":"tc","name":"Read","input":{"file_path":"/x"}}]}}).to_string() },
        ];
        let hs = HostSession {
            id: "ses_1".to_string(),
            directory: "/d".to_string(),
            title: "t".to_string(),
            model: "qwen3.8".to_string(),
            children: vec!["ses_kid".to_string()],
            count: ops.len(),
            ops,
        };
        a.ingest(&body("a", vec![hs]), &mut sessions, 5000, &mut applied, &mut reported);
        let s = sessions.iter().find(|s| s.id == "a:ses_1").expect("the card");
        assert_eq!(s.agents.len(), 2, "the child agent was built, not skipped (and no panic)");
        assert!(s.step_by_tool("tc").is_some(), "the child's step replayed onto its agent");
    }

    #[test]
    fn a_capped_log_still_rebuilds_when_the_total_grows() {
        // The freeze: a long session hits MAX_OPS, so the retained log length stops changing while
        // new activity still happens (front trimmed as the tail grows). Rebuild must key on the
        // monotonic `count`, not the retained length -- the old length check froze the card.
        let mut a = a_host("a");
        let mut sessions: Vec<Session> = Vec::new();
        let mut applied: HashMap<String, usize> = HashMap::new();
        let mut reported: HashMap<String, (u64, u64, u64)> = HashMap::new();
        let s1 = HostSession {
            id: "s".into(),
            directory: "/d".into(),
            title: "t".into(),
            model: String::new(),
            children: vec![],
            count: 9_000,
            ops: edit_ops("c1", "/f.ts"),
        };
        a.ingest(&body("a", vec![s1]), &mut sessions, 5000, &mut applied, &mut reported);
        assert_eq!(applied.get("a:s").copied(), Some(9_000));
        // Same retained length (2 ops) but the total advanced -> must rebuild from the new tail.
        let s2 = HostSession {
            id: "s".into(),
            directory: "/d".into(),
            title: "t".into(),
            model: String::new(),
            children: vec![],
            count: 9_001,
            ops: edit_ops("c2", "/g.ts"),
        };
        a.ingest(&body("a", vec![s2]), &mut sessions, 5000, &mut applied, &mut reported);
        assert_eq!(applied.get("a:s").copied(), Some(9_001), "rebuild fired on the total, not the length");
        let s = sessions.iter().find(|s| s.id == "a:s").unwrap();
        assert!(s.files.contains_key("/g.ts"), "rebuilt from the fresh retained tail");
        assert!(!s.files.contains_key("/f.ts"), "the old retained state was replaced");
    }

    #[test]
    fn version_mismatch_is_shown_not_guessed() {
        let mut a = a_host("a");
        let mut sessions: Vec<Session> = Vec::new();
        let mut applied: HashMap<String, usize> = HashMap::new();
        let mut reported: HashMap<String, (u64, u64, u64)> = HashMap::new();
        let bad =
            HostEnvelope { format: EXPORT_FORMAT + 1, opencode: OPENCODE_SEEN.to_string(), host: "a".into(), now: 0, sessions: vec![] }.to_json().to_string();
        a.ingest(&bad, &mut sessions, 5000, &mut applied, &mut reported);
        assert!(!a.alive, "a wrong wire format is refused");
        assert!(a.error.as_deref().is_some_and(|e| e.contains("export v")), "and the strip says why");
    }
}
