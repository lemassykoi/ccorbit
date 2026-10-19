//! The live source: every opencode session on this machine, read from the SQLite storage
//! opencode writes (`~/.local/share/opencode/opencode.db`, opened read-only). Rows are turned
//! into transcript-style records and fed through the same reader as recorded transcripts:
//! `Session::ingest` stays the only place that knows what a record means.
//!
//! Privacy, as everywhere in ccorbit: bodies and file contents are never kept. `input.oldString`,
//! `newString`, `content`, `prompt` and `reasoning.text` are read only to derive counts and a
//! first-line summary, never stored.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::model::Session;
use crate::parse;
use crate::time::fmt_iso;

/// Sessions quiet for longer than this drop off the wall; their data stays in the database and
/// the card returns as soon as anything is written to them again. A session cannot be told
/// cancelled from resting except by silence (opencode writes nothing on close), so this trades
/// how long a stale card hangs around against the longest call that runs without touching a
/// row: a genuine tool in flight writes nothing until it ends, and the monitors people run
/// here top out around twenty minutes.
const ACTIVE_WINDOW: i64 = 20 * 60_000;
/// The most sessions shown at once, newest activity first.
const MAX_SESSIONS: usize = 48;
/// How often the database is polled.
const POLL_EVERY: Duration = Duration::from_secs(1);
/// A streamed part (reasoning, text) counts as finished when its row has not changed for this
/// long: the row holds the true completion time, so the brainwave stays exact despite the delay.
const STREAM_STABLE: i64 = 3_000;
/// Rows moved within this and the session is working, whatever the last message says (the
/// short gap where a prompt has landed and its answer's row does not exist yet).
const BLIP: i64 = 15_000;
/// An open assistant turn with no row moving for this long is a crashed turn, not work in
/// progress: a tool running past it is shown idle and its step closed as lost (it is rare that
/// a legitimate call runs silent this long, and the card heals itself on the next row).
const DEAD: i64 = 10 * 60_000;

pub struct Open {
    pub db_path: PathBuf,
    conn: Option<Connection>,
    last_poll: Option<Instant>,
    /// Per-session read state, keyed by opencode session id (roots and children alike).
    lanes: HashMap<String, Lane>,
    /// MCP server names: a tool called `{server}_{tool}` is an MCP call, not a built-in.
    servers: Vec<String>,
}

/// What has been read and emitted for one opencode session.
#[derive(Default)]
struct Lane {
    /// Highest `part.rowid` already seen.
    part_row: i64,
    /// Highest `part.time_updated` already seen.
    part_upd: i64,
    /// Highest `message.rowid` already seen.
    msg_row: i64,
    /// part rowid -> (state emitted for it, `time_updated` emitted for it).
    emitted: HashMap<i64, (String, i64)>,
    /// message id -> role: a text part of a user message is a prompt, of an assistant a reply.
    roles: HashMap<String, String>,
    /// opencode child session id -> agent index in the parent's `Session`.
    children: HashMap<String, usize>,
    /// message id -> its completion time, for calls left open when the turn closed.
    msg_done: HashMap<String, i64>,
    /// When any row of this session last moved: the heartbeat of the work.
    last_act: i64,
    /// Tokens of the last step-finish: the context size a compaction fires at.
    ctx_last: u64,
    model: String,
}

impl Open {
    /// Starts following a database; nothing is read until the first `tick`.
    pub fn watch(db_path: PathBuf) -> Open {
        Open { db_path, conn: None, last_poll: None, lanes: HashMap::new(), servers: mcp_servers() }
    }

    /// Reads whatever is new. Returns whether anything changed.
    pub fn tick(&mut self, sessions: &mut Vec<Session>, now: i64) -> bool {
        if self.last_poll.is_some_and(|t| t.elapsed() < POLL_EVERY) {
            return false;
        }
        self.last_poll = Some(Instant::now());
        if self.conn.is_none() && !self.connect() {
            return false;
        }
        match self.poll(sessions, now) {
            Ok(c) => c,
            Err(_) => {
                // A migration or a replaced file: start again from a fresh connection next tick.
                self.conn = None;
                false
            }
        }
    }

    fn connect(&mut self) -> bool {
        if !self.db_path.exists() {
            return false;
        }
        // Read-only: opencode keeps writing; WAL readers never block and are never blocked.
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        match Connection::open_with_flags(&self.db_path, flags) {
            Ok(c) => {
                self.conn = Some(c);
                true
            }
            Err(_) => false,
        }
    }

    fn poll(&mut self, sessions: &mut Vec<Session>, now: i64) -> rusqlite::Result<bool> {
        let conn = self.conn.as_ref().expect("connected by tick");
        let mut changed = false;

        // Sessions active in the window, plus the roots of any active child.
        let mut rows: Vec<(String, Option<String>, String, String, String)> = Vec::new();
        {
            let mut st = conn.prepare(
                "select s.id, s.parent_id, s.directory, s.title,
                        coalesce(json_extract(s.model,'$.id'),'')
                 from session s
                 where s.time_archived is null
                   and (s.time_updated > ?1
                        or exists (select 1 from session c where c.parent_id = s.id and c.time_updated > ?1))
                 order by s.time_updated desc limit ?2",
            )?;
            let it = st.query_map(params![now - ACTIVE_WINDOW, MAX_SESSIONS as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            for r in it {
                rows.push(r?);
            }
        }
        let active: HashMap<&str, bool> = rows.iter().map(|(id, parent, ..)| (id.as_str(), parent.is_none())).collect();
        let roots: Vec<&(String, Option<String>, String, String, String)> = rows.iter().filter(|(_, parent, ..)| parent.is_none()).collect();
        let root_ids: Vec<&str> = roots.iter().map(|(id, ..)| id.as_str()).collect();

        // Sessions that fell out of the window go, and with them their lanes.
        sessions.retain(|s| {
            let keep = active.contains_key(s.id.as_str());
            if !keep {
                self.lanes.remove(&s.id);
            }
            keep
        });
        let live_lanes: Vec<String> = self.lanes.keys().cloned().collect();
        for key in live_lanes {
            let root_of = rows.iter().find_map(|(id, parent, ..)| (id == &key).then_some(parent.clone())).flatten();
            let keep = root_ids.contains(&key.as_str()) || root_of.as_deref().is_some_and(|p| root_ids.contains(&p));
            if !keep {
                self.lanes.remove(&key);
            }
        }

        for (id, _parent, dir, title, model) in &roots {
            let si = match sessions.iter().position(|s| &s.id == id) {
                Some(i) => i,
                None => {
                    let mut s = Session::new(id);
                    s.cwd = dir.clone();
                    s.name = format!("projects-{}", tag_of(id));
                    s.title = title.clone();
                    s.alive = true;
                    sessions.push(s);
                    changed = true;
                    sessions.len() - 1
                }
            };
            sessions[si].alive = true;
            if sessions[si].title != *title {
                sessions[si].title = title.clone();
                changed = true;
            }
            let mut lane = self.lanes.remove(id).unwrap_or_default();
            if lane.model.is_empty() {
                lane.model = model.clone();
            }
            changed |= read_session(conn, &mut sessions[si], id, 0, &mut lane, now, &self.servers)?;

            // Child sessions run as agents of their root, hung under the task call that launched them.
            let mut kids: Vec<String> = Vec::new();
            {
                let mut st = conn.prepare("select id from session where parent_id = ?1 and time_archived is null order by time_created")?;
                let it = st.query_map(params![id], |r| r.get(0))?;
                for r in it {
                    kids.push(r?);
                }
            }
            for kid in kids {
                let (agent, mut klane) = match lane.children.get(&kid) {
                    Some(&a) => (a, self.lanes.remove(&kid).unwrap_or_default()),
                    None => {
                        let (kind, description, parent_tool) = task_origin(conn, id, &kid);
                        let a = sessions[si].add_agent(&kid, &kind, &description, parent_tool);
                        (a, self.lanes.remove(&kid).unwrap_or_default())
                    }
                };
                if klane.model.is_empty() {
                    klane.model = session_model(conn, &kid).unwrap_or_default();
                }
                changed |= read_session(conn, &mut sessions[si], &kid, agent, &mut klane, now, &self.servers)?;
                lane.children.insert(kid.clone(), agent);
                self.lanes.insert(kid, klane);
            }

            // Liveness. A session works when its rows are moving: an open assistant turn, or a
            // prompt whose answer row does not exist yet, or a question waiting on the person.
            // A turn that has been silent past DEAD crashed rather than worked (opencode leaves
            // its rows dangling); its open calls close as lost so the card stops shimmering.
            let mut quiet = now - lane.last_act;
            for kid in lane.children.keys() {
                if let Some(k) = self.lanes.get(kid) {
                    quiet = quiet.min(now - k.last_act);
                }
            }
            let (role, completed) = last_message(conn, id)?;
            let turn_open = role.as_deref() == Some("assistant") && completed.is_none();
            let waiting = sessions[si].steps().any(|(_, st)| st.ended.is_none() && matches!(st.name.as_str(), "AskUserQuestion" | "ExitPlanMode"));
            let working = waiting || (turn_open && quiet <= DEAD) || (role.as_deref() == Some("user") && quiet <= BLIP);
            if !working {
                sessions[si].abandon_running(now);
            }
            let status = if working { "busy" } else { "idle" };
            if sessions[si].status != status {
                sessions[si].status = status.to_string();
                changed = true;
            }
            self.lanes.insert(id.clone(), lane);
        }
        Ok(changed)
    }
}

/// Messages and parts of one opencode session (`sid`), emitted as records into the agent `a` of
/// `s` (the root session the child, if any, belongs to).
fn read_session(conn: &Connection, s: &mut Session, sid: &str, a: usize, lane: &mut Lane, now: i64, servers: &[String]) -> rusqlite::Result<bool> {
    let mut changed = false;

    // New messages: role decides how text parts read; completion times let us close calls the
    // turn abandoned; their updates are part of the session's heartbeat.
    {
        let mut st = conn.prepare("select rowid, id, json_extract(data,'$.role'), json_extract(data,'$.time.completed'), time_updated from message where session_id = ?1 and rowid > ?2 order by rowid")?;
        let it = st.query_map(params![sid, lane.msg_row], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, i64>(4)?))
        })?;
        for r in it {
            let (rowid, id, role, completed, upd) = r?;
            if let Some(role) = role {
                lane.roles.insert(id.clone(), role);
            }
            if let Some(c) = completed {
                lane.msg_done.insert(id, c);
                if lane.msg_done.len() > 8192 {
                    lane.msg_done.clear();
                }
            }
            lane.msg_row = lane.msg_row.max(rowid);
            lane.last_act = lane.last_act.max(upd);
            changed = true;
        }
    }

    // Parts: new rows, and rows whose state advanced (a tool finishing updates its row).
    let mut parts: Vec<(i64, Value, i64, i64, String)> = Vec::new();
    {
        let mut st = conn.prepare(
            "select rowid, data, time_created, time_updated, message_id
             from part
             where session_id = ?1 and (rowid > ?2 or time_updated > ?3)
             order by time_created, rowid",
        )?;
        let it = st.query_map(params![sid, lane.part_row, lane.part_upd], |r| {
            let data: String = r.get(1)?;
            Ok((r.get(0)?, serde_json::from_str::<Value>(&data).unwrap_or(Value::Null), r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        for r in it {
            let (rowid, v, tc, tu, msg_id) = r?;
            if rowid > lane.part_row {
                lane.part_row = rowid;
            }
            // part_upd advances only where a row is marked processed below: a row held back by
            // the streaming-stability wait must stay inside the re-scan window until it emits.
            parts.push((rowid, v, tc, tu, msg_id));
        }
    }
    let model = lane.model.clone();
    for (rowid, v, tc, tu, msg_id) in parts {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let state_key = part_state_key(&v, kind);
        let (seen_state, seen_tu) = lane.emitted.get(&rowid).cloned().unwrap_or_default();
        if seen_state == state_key && seen_tu == tu {
            continue; // unchanged since it was emitted
        }
        match kind {
            "tool" => {
                let status = v["state"]["status"].as_str().unwrap_or("pending").to_string();
                let first = seen_state.is_empty();
                let done = matches!(status.as_str(), "completed" | "error");
                if first {
                    let line = tool_use_record(&v, &fmt_iso(tc), &msg_id, servers);
                    s.ingest(a, &line);
                    changed = true;
                }
                // opencode fills the input (the command, the path) a moment after creating the
                // row, and a fast call jumps straight to completed between two polls: refresh
                // the step on any advance of the row, not only while it is still running.
                if !first && tu != seen_tu {
                    let (_, clean) = map_tool(tool_of(&v), &v["state"]["input"], servers);
                    s.set_step_input(&call_step_id(&v, &msg_id), &clean);
                    changed = true;
                }
                if done && (first || seen_state != state_key) {
                    let line = tool_result_record(&v, &fmt_iso(tu), &status, &msg_id);
                    s.ingest(a, &line);
                    // The task tool closing also closes the child it launched.
                    if tool_of(&v) == "task"
                        && let Some(kid) = v["state"]["metadata"]["sessionId"].as_str()
                        && let Some(&ag) = lane.children.get(kid)
                    {
                        s.agents[ag].done = true;
                    }
                    changed = true;
                } else if !done && lane.msg_done.contains_key(&msg_id) {
                    // The turn closed without ever finishing this call: the process died mid
                    // -flight. Close it at the turn's end rather than shimmer forever.
                    let lost_at = lane.msg_done[&msg_id];
                    let gone =
                        json!({"callID": v["callID"], "tool": tool_of(&v), "state": {"status": "completed", "output": "no result: the turn ended without it"}});
                    s.ingest(a, &tool_result_record(&gone, &fmt_iso(lost_at), "completed", &msg_id));
                    changed = true;
                }
                {
                    lane.part_upd = lane.part_upd.max(tu);
                    lane.emitted.insert(rowid, (state_key, tu.max(seen_tu)));
                }
            }
            "step-finish" => {
                if !seen_state.is_empty() {
                    continue; // counted once: opencode may touch the row later, usage must not double
                }
                lane.ctx_last = v["tokens"]["total"].as_u64().unwrap_or(lane.ctx_last);
                let usage = step_usage(&v);
                let line =
                    json!({"type":"assistant","timestamp":fmt_iso(tu),"message":{"id":format!("{msg_id}-{rowid}"),"model":model,"usage":usage,"content":[]}})
                        .to_string();
                s.ingest(a, &line);
                {
                    lane.part_upd = lane.part_upd.max(tu);
                    lane.emitted.insert(rowid, (state_key, tu));
                }
                changed = true;
            }
            "reasoning" | "text" => {
                // Streamed rows keep their true completion time; emit once the row has been quiet
                // for a moment — or at once when its turn has closed or it is a prompt.
                let user_prompt = lane.roles.get(&msg_id).map(String::as_str) == Some("user");
                let turn_closed = lane.msg_done.contains_key(&msg_id);
                if !seen_state.is_empty() || !(tu.saturating_add(STREAM_STABLE) <= now || user_prompt || turn_closed) {
                    continue;
                }
                let line = match kind {
                    "reasoning" => json!({"type":"assistant","timestamp":fmt_iso(tu),"message":{"id":format!("{msg_id}-{rowid}"),"model":model,"content":[{"type":"thinking"}]}}).to_string(),
                    _ if user_prompt => json!({"type":"user","timestamp":fmt_iso(tc),"message":{"content":v.get("text").and_then(Value::as_str).unwrap_or("")}}).to_string(),
                    _ => json!({"type":"assistant","timestamp":fmt_iso(tu),"message":{"id":format!("{msg_id}-{rowid}"),"model":model,"content":[{"type":"text","text":v.get("text").and_then(Value::as_str).unwrap_or("")}]}}).to_string(),
                };
                s.ingest(a, &line);
                {
                    lane.part_upd = lane.part_upd.max(tu);
                    lane.emitted.insert(rowid, (state_key, tu));
                }
                changed = true;
            }
            "compaction" => {
                if a != 0 || !seen_state.is_empty() {
                    continue;
                }
                let line = json!({
                    "type":"system","subtype":"compact_boundary","timestamp":fmt_iso(tu),
                    "compactMetadata":{"trigger":if v.get("auto").and_then(Value::as_bool)==Some(false){"manual"}else{"auto"},"preTokens":lane.ctx_last}
                })
                .to_string();
                s.ingest(a, &line);
                {
                    lane.part_upd = lane.part_upd.max(tu);
                    lane.emitted.insert(rowid, (state_key, tu));
                }
                changed = true;
            }
            _ => {
                lane.part_upd = lane.part_upd.max(tu);
                lane.emitted.insert(rowid, (state_key, tu));
            }
        }
    }

    Ok(changed)
}

/// The last message of a session: its role and, for an assistant, its completion time. The
/// freshest row answers "is this session working": an open assistant turn or a prompt without
/// an answer yet means the agent is in flight.
fn last_message(conn: &Connection, sid: &str) -> rusqlite::Result<(Option<String>, Option<i64>)> {
    conn.query_row(
        "select json_extract(data,'$.role'), json_extract(data,'$.time.completed') from message where session_id = ?1 order by rowid desc limit 1",
        params![sid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .map(|o| o.unwrap_or((None, None)))
}

fn tool_of(v: &Value) -> &str {
    v.get("tool").and_then(Value::as_str).unwrap_or("")
}

/// What counts as "the same version" of a part row for the emit-once map: a tool's status, the
/// row's type otherwise.
fn part_state_key(v: &Value, kind: &str) -> String {
    match kind {
        "tool" => format!("tool:{}", v["state"]["status"].as_str().unwrap_or("pending")),
        other => other.to_string(),
    }
}

fn tag_of(sid: &str) -> &str {
    sid.strip_prefix("ses_").unwrap_or(sid).get(..4).unwrap_or("0000")
}

/// The `mcp` server names of the user's opencode config, longest first so `online_search` wins
/// over a shorter server name that could prefix it.
fn mcp_servers() -> Vec<String> {
    let data = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".config")).join("opencode").join("opencode.json");
    let text = std::fs::read_to_string(&data).unwrap_or_default();
    let mut names: Vec<String> = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|c| c.get("mcp").and_then(Value::as_object).map(|m| m.keys().cloned().collect()))
        .unwrap_or_default();
    names.sort_by_key(|a| std::cmp::Reverse(a.len()));
    names
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// The default database: `$XDG_DATA_HOME/opencode/opencode.db`.
pub fn default_db() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".local").join("share"));
    base.join("opencode").join("opencode.db")
}

fn session_model(conn: &Connection, sid: &str) -> Option<String> {
    let raw: Option<String> = conn.query_row("select model from session where id = ?1", params![sid], |r| r.get(0)).ok().flatten();
    let v = serde_json::from_str::<Value>(&raw?).ok()?;
    v.get("id").and_then(Value::as_str).map(str::to_string)
}

/// The `task` call in `parent` that launched child session `kid`: its type, description and the
/// step id it is tracked by (the synthetic one `step_id` builds), so the child hangs under it.
/// callID, message_id, subagent_type, description, command.
type TaskRow = (Option<String>, String, Option<String>, Option<String>, Option<String>);

fn task_origin(conn: &Connection, parent: &str, kid: &str) -> (String, String, Option<String>) {
    let row: Option<TaskRow> = conn
        .query_row(
            "select json_extract(data,'$.callID'), message_id, json_extract(data,'$.state.input.subagent_type'), json_extract(data,'$.state.input.description'), json_extract(data,'$.state.input.command')
             from part
             where session_id = ?1 and json_extract(data,'$.tool') = 'task' and json_extract(data,'$.state.metadata.sessionId') = ?2
             order by rowid limit 1",
            params![parent, kid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .unwrap_or(None);
    match row {
        Some((Some(call), msg, sub, desc, command)) => {
            let kind = sub.or(command).unwrap_or_else(|| "subagent".to_string());
            (kind, desc.unwrap_or_default(), Some(step_id(&call, &msg)))
        }
        _ => ("subagent".to_string(), String::new(), None),
    }
}

/// The id a step is tracked by: the model's call id, made unique per message.
fn step_id(call: &str, msg_id: &str) -> String {
    format!("{call}:{msg_id}")
}

fn call_step_id(v: &Value, msg_id: &str) -> String {
    step_id(v["callID"].as_str().unwrap_or("call"), msg_id)
}

/// Claude-shaped names and safe inputs for the built-in tools; anything matching a configured
/// MCP server becomes `mcp__{server}__{tool}`; the rest passes through with clipped scalars.
fn map_tool(name: &str, input: &Value, servers: &[String]) -> (String, Value) {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match name {
        "bash" => {
            let mut i = json!({"command": first_chunk(&s("command"))});
            // Only keys opencode actually filled: an empty `description` would shadow the command.
            if !s("description").is_empty() {
                i["description"] = json!(first_chunk(&s("description")));
            }
            if input["run_in_background"].as_bool().unwrap_or(false) {
                i["run_in_background"] = json!(true);
            }
            ("Bash".into(), i)
        }
        "read" => ("Read".into(), json!({"file_path": s("filePath")})),
        "write" => ("Write".into(), json!({"file_path": s("filePath")})),
        "edit" => ("Edit".into(), json!({"file_path": s("filePath")})),
        "glob" => ("Glob".into(), json!({"pattern": s("pattern")})),
        "grep" => ("Grep".into(), json!({"pattern": s("pattern"), "path": s("path")})),
        "webfetch" => ("WebFetch".into(), json!({"url": s("url")})),
        "task" => ("Task".into(), json!({"subagent_type": s("subagent_type"), "description": s("description")})),
        "skill" => ("Skill".into(), json!({"skill": if input["name"].is_string() { s("name") } else { s("skill") }})),
        "question" => (
            "AskUserQuestion".into(),
            json!({"questions": input["questions"].as_array().map(|qs| qs.iter().map(|q| json!({"question": first_chunk(q["question"].as_str().unwrap_or(""))})).collect::<Vec<_>>()).unwrap_or_default()}),
        ),
        "todowrite" => {
            let n = input["todos"].as_array().map(Vec::len).unwrap_or(0);
            ("todowrite".into(), json!({"description": format!("{n} tasks")}))
        }
        "invalid" => ("invalid".into(), json!({})),
        n => {
            for server in servers {
                if let Some(tool) = n.strip_prefix(&format!("{server}_")) {
                    return (format!("mcp__{server}__{tool}"), clean_input(n, input));
                }
            }
            (n.to_string(), clean_input(n, input))
        }
    }
}

/// Inputs of tools ccorbit does not know keep short scalars (a summary is read from the first
/// string) but not bodies or nesting.
fn clean_input(_name: &str, input: &Value) -> Value {
    let Some(map) = input.as_object() else { return json!({}) };
    let mut out = serde_json::Map::new();
    for (k, v) in map {
        match v {
            Value::String(s) if s.chars().count() <= 200 => {
                out.insert(k.clone(), json!(first_chunk(s)));
            }
            Value::String(s) => {
                let clipped: String = s.chars().take(120).collect();
                out.insert(k.clone(), Value::String(format!("{clipped}…")));
            }
            Value::Bool(_) | Value::Number(_) => {
                out.insert(k.clone(), v.clone());
            }
            _ => {}
        }
    }
    Value::Object(out)
}

/// The first non-empty line of a possibly long text, clipped the way every summary is.
fn first_chunk(s: &str) -> String {
    parse::clip(parse::first_line(s), 200)
}

fn tool_use_record(v: &Value, ts: &str, msg_id: &str, servers: &[String]) -> String {
    let (name, clean) = map_tool(tool_of(v), &v["state"]["input"], servers);
    json!({
        "type": "assistant", "timestamp": ts,
        "message": {"content": [{"type": "tool_use", "id": call_step_id(v, msg_id), "name": name, "input": clean}]}
    })
    .to_string()
}

fn tool_result_record(v: &Value, ts: &str, status: &str, msg_id: &str) -> String {
    let raw = tool_of(v);
    // opencode ends a failed shell call as `completed` with a non-zero `metadata.exit`: the
    // status alone would hide every `make` error behind a checkmark.
    let error = status == "error" || v["state"]["metadata"]["exit"].as_i64().is_some_and(|e| e != 0);
    let output = v["state"]["output"].as_str().or_else(|| v["state"]["metadata"]["output"].as_str()).unwrap_or("");
    let input = &v["state"]["input"];
    let (tur, content) = match raw {
        "edit" => {
            let (added, removed) = line_delta(input["oldString"].as_str().unwrap_or(""), input["newString"].as_str().unwrap_or(""));
            (json!({"structuredPatch": [{"lines": patch_lines(added, removed)}]}), "Edit applied".to_string())
        }
        "write" => {
            let lines = input["content"].as_str().map(str::lines).map_or(0, Iterator::count);
            (json!({"type": "create", "content": "\n".repeat(lines.max(1))}), "Write applied".to_string())
        }
        _ => (json!({}), first_chunk(output)),
    };
    json!({
        "type": "user", "timestamp": ts,
        "message": {"content": [{"type": "tool_result", "tool_use_id": call_step_id(v, msg_id), "is_error": error, "content": content}]},
        "toolUseResult": tur
    })
    .to_string()
}

/// Added and removed line counts of an edit, by line multiset: the only part of the content
/// ccorbit keeps, and only as counts.
fn line_delta(old: &str, new: &str) -> (usize, usize) {
    fn count(s: &str) -> HashMap<&str, usize> {
        let mut m: HashMap<&str, usize> = HashMap::new();
        for l in s.lines().filter(|l| !l.trim().is_empty()) {
            *m.entry(l).or_default() += 1;
        }
        m
    }
    let (o, n) = (count(old), count(new));
    let added = n.iter().map(|(l, c)| c.saturating_sub(o.get(*l).copied().unwrap_or(0))).sum();
    let removed = o.iter().map(|(l, c)| c.saturating_sub(n.get(*l).copied().unwrap_or(0))).sum();
    (added, removed)
}

fn patch_lines(added: usize, removed: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(added + removed);
    for _ in 0..added {
        out.push("+x".to_string());
    }
    for _ in 0..removed {
        out.push("-x".to_string());
    }
    out
}

/// The usage block of one `step-finish`: opencode counts input, output, reasoning, cache and a
/// price of its own, which local models have none of in `price.rs`.
fn step_usage(v: &Value) -> Value {
    let t = &v["tokens"];
    let mut u = json!({
        "input_tokens": t["input"].as_u64().unwrap_or(0),
        "cache_creation_input_tokens": t["cache"]["write"].as_u64().unwrap_or(0),
        "cache_read_input_tokens": t["cache"]["read"].as_u64().unwrap_or(0),
        "output_tokens": t["output"].as_u64().unwrap_or(0),
        "output_tokens_details": {"thinking_tokens": t["reasoning"].as_u64().unwrap_or(0)}
    });
    if let Some(cost) = v["cost"].as_f64().filter(|c| *c > 0.0) {
        u["ccorbit_cost_micros"] = json!((cost * 1_000_000.0).round() as u64);
    }
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_only_line_counts() {
        assert_eq!(line_delta("a\nb\nc", "a\nb\nc"), (0, 0));
        assert_eq!(line_delta("a\nb", "a\nb\nc\nc"), (2, 0));
        assert_eq!(line_delta("a\nb\nc", "a"), (0, 2));
        let l = patch_lines(2, 1);
        assert_eq!(l.iter().filter(|x| x.starts_with('+')).count(), 2);
        assert_eq!(l.iter().filter(|x| x.starts_with('-')).count(), 1);
    }

    #[test]
    fn tool_names_map_to_claude_shapes() {
        let (n, i) = map_tool("edit", &json!({"filePath": "/a.rs", "oldString": "x", "newString": "y"}), &[]);
        assert_eq!(n, "Edit");
        assert_eq!(i["file_path"], "/a.rs");
        assert!(i.get("oldString").is_none(), "file content must never travel into the model");
        let (n, _) = map_tool("bash", &json!({"command": "cargo build\nrest"}), &[]);
        assert_eq!(n, "Bash");
        let (n, i) = map_tool("task", &json!({"subagent_type": "explore", "description": "read docs", "prompt": "a very long prompt"}), &[]);
        assert_eq!(n, "Task");
        assert!(i.get("prompt").is_none(), "the subagent prompt body is not kept");
        let (n, _) = map_tool("librarian_search_sessions", &json!({"query": "x"}), &["librarian".to_string(), "online_search".to_string()]);
        assert_eq!(n, "mcp__librarian__search_sessions");
        let (n, _) = map_tool("online_search_tavily_search", &json!({"query": "x"}), &["librarian".to_string(), "online_search".to_string()]);
        assert_eq!(n, "mcp__online_search__tavily_search");
        let (n, i) = map_tool("question", &json!({"questions": [{"question": "which one?\nsecond"}]}), &[]);
        assert_eq!(n, "AskUserQuestion");
        assert_eq!(i["questions"][0]["question"], "which one?");
    }

    #[test]
    fn long_inputs_are_clipped_not_kept() {
        let i = clean_input("x", &json!({"q": "a".repeat(400), "n": 3, "b": true, "deep": {"a": 1}}));
        assert!(i["q"].as_str().unwrap().chars().count() <= 121);
        assert_eq!(i["n"], 3);
        assert_eq!(i["b"], true);
        assert!(i.get("deep").is_none());
    }

    #[test]
    fn step_usage_carries_tokens_and_price() {
        let u = step_usage(&json!({"tokens": {"input": 10, "output": 3, "reasoning": 1, "cache": {"read": 7, "write": 2}}, "cost": 0.000123}));
        assert_eq!(u["input_tokens"], 10);
        assert_eq!(u["output_tokens_details"]["thinking_tokens"], 1);
        assert_eq!(u["ccorbit_cost_micros"], 123);
        let u = step_usage(&json!({"tokens": {}}));
        assert!(u.get("ccorbit_cost_micros").is_none());
    }

    #[test]
    fn ids_are_unique_per_call_and_message() {
        assert_eq!(step_id("c1", "m1"), "c1:m1");
        assert_ne!(step_id("c1", "m1"), step_id("c1", "m2"));
        let v = json!({"callID": "c1"});
        assert_eq!(call_step_id(&v, "m1"), "c1:m1");
    }

    #[test]
    fn records_feed_the_reader_end_to_end() {
        // A prompt, a step that opens and closes, a usage line: what a real session of one
        // bash call looks like after translation.
        let mut s = Session::new("ses_test");
        s.alive = true;
        s.ingest(0, &json!({"type":"user","timestamp":fmt_iso(1_000),"message":{"content":"do the thing"}}).to_string());
        let call = json!({"callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls -la"},"output":"total 3"}});
        s.ingest(0, &tool_use_record(&call, &fmt_iso(2_000), "m1", &[]));
        assert!(s.step_by_tool("c1:m1").is_some());
        assert!(s.step_by_tool("c1:m1").unwrap().ended.is_none());
        s.ingest(0, &tool_result_record(&call, &fmt_iso(3_000), "completed", "m1"));
        let st = s.step_by_tool("c1:m1").unwrap();
        assert_eq!(st.ended, Some(3_000));
        assert!(!st.error);
        s.ingest(0, &json!({"type":"assistant","timestamp":fmt_iso(4_000),"message":{"id":"m1-1","model":"qwen3.8","usage":{"input_tokens":10,"output_tokens":3,"output_tokens_details":{"thinking_tokens":1}},"content":[]}}).to_string());
        assert_eq!(s.tokens().output, 3);
        assert_eq!(s.agents[0].ctx, 10);
        assert_eq!(s.last_prompt(), Some("do the thing"));
    }

    fn db_with_fast_bash() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "create table message (id text primary key, session_id text, data text, time_created int, time_updated int);
             create table part (id text primary key, session_id text, message_id text, data text, time_created int, time_updated int);",
        )
        .unwrap();
        c.execute("insert into message values ('m1','s1','{\"role\":\"assistant\"}',1000,1000)", []).unwrap();
        c
    }

    #[test]
    fn a_call_observed_first_pending_and_then_completed_gets_its_command() {
        // The race from the live wall: opencode creates the tool row with an empty input, and a
        // fast call reaches `completed` between two polls, so the input only ever arrives in the
        // same update that closes the step.
        let c = db_with_fast_bash();
        c.execute(
            "insert into part values ('p1','s1','m1','{\"type\":\"tool\",\"tool\":\"bash\",\"callID\":\"c1\",\"state\":{\"status\":\"pending\",\"input\":{}}}',1000,1000)",
            [],
        )
        .unwrap();
        let mut s = Session::new("s1");
        s.alive = true;
        let mut lane = Lane::default();
        read_session(&c, &mut s, "s1", 0, &mut lane, 2000, &[]).unwrap();
        let st = s.step_by_tool("c1:m1").expect("the step opened");
        assert_eq!(st.summary, "", "the command had not been written yet");
        assert!(st.ended.is_none());

        c.execute(
            "update part set data='{\"type\":\"tool\",\"tool\":\"bash\",\"callID\":\"c1\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"ls -l\\nmore\"},\"output\":\"total 1\"}}', time_updated=1500",
            [],
        )
        .unwrap();
        read_session(&c, &mut s, "s1", 0, &mut lane, 3000, &[]).unwrap();
        let st = s.step_by_tool("c1:m1").unwrap();
        assert_eq!(st.summary, "ls -l", "the input that arrived with the result refreshes the step");
        assert_eq!(st.ended, Some(1500), "closed at the row's completion time");
        assert_eq!(st.result, "total 1");
        assert!(s.feed.iter().any(|f| f.tool.as_deref() == Some("c1:m1") && f.text == "ls -l"), "the signal feed line carries the command too");
    }

    #[test]
    fn a_row_seen_first_in_its_final_state_needs_no_refresh() {
        let c = db_with_fast_bash();
        c.execute(
            "insert into part values ('p1','s1','m1','{\"type\":\"tool\",\"tool\":\"edit\",\"callID\":\"c9\",\"state\":{\"status\":\"completed\",\"input\":{\"filePath\":\"/x/y.rs\",\"oldString\":\"a\",\"newString\":\"a\\nb\"},\"output\":\"Edit applied\"}}',1000,1200)",
            [],
        )
        .unwrap();
        let mut s = Session::new("s1");
        s.alive = true;
        let mut lane = Lane::default();
        read_session(&c, &mut s, "s1", 0, &mut lane, 3000, &[]).unwrap();
        let st = s.step_by_tool("c9:m1").unwrap();
        assert_eq!(st.summary, "/x/y.rs");
        assert_eq!(st.ended, Some(1200));
        assert_eq!(s.files["/x/y.rs"].added, 1, "the edit's line count survives the fast path");
    }

    #[test]
    fn a_nonzero_exit_is_a_failure_even_when_the_row_completed() {
        let mut s = Session::new("s1");
        s.alive = true;
        let call = json!({"callID": "c4", "tool": "bash", "state": {"status": "completed", "input": {"command": "exit 3"}, "output": "BATTERY-4-about-to-fail\n", "metadata": {"exit": 3}}});
        s.ingest(0, &tool_use_record(&call, &fmt_iso(1_000), "m1", &[]));
        s.ingest(0, &tool_result_record(&call, &fmt_iso(2_000), "completed", "m1"));
        assert!(s.step_by_tool("c4:m1").unwrap().error, "a red cross for a non-zero exit");
        let ok =
            json!({"callID": "c5", "tool": "bash", "state": {"status": "completed", "input": {"command": "true"}, "output": "fine", "metadata": {"exit": 0}}});
        s.ingest(0, &tool_use_record(&ok, &fmt_iso(3_000), "m2", &[]));
        s.ingest(0, &tool_result_record(&ok, &fmt_iso(4_000), "completed", "m2"));
        assert!(!s.step_by_tool("c5:m2").unwrap().error, "exit 0 stays green");
    }

    #[test]
    fn assistant_text_reaches_the_feed_as_an_answer() {
        let c = db_with_fast_bash();
        c.execute("update message set data='{\"role\":\"assistant\"}', time_updated=1500 where id='m1'", []).unwrap();
        c.execute("insert into part values ('pt','s1','m1','{\"type\":\"text\",\"text\":\"the model says hello\\nsecond line\"}',1200,1200)", []).unwrap();
        let mut s = Session::new("s1");
        s.alive = true;
        let mut lane = Lane::default();
        // not stable yet (now - tu < 3 s): nothing
        read_session(&c, &mut s, "s1", 0, &mut lane, 2000, &[]).unwrap();
        assert!(!s.feed.iter().any(|f| f.feed == crate::model::Feed::Answer), "still streaming");
        // settled: the answer lands, first line only
        read_session(&c, &mut s, "s1", 0, &mut lane, 9_000, &[]).unwrap();
        let a = s.feed.iter().rev().find(|f| f.feed == crate::model::Feed::Answer).expect("the answer is in the feed");
        assert_eq!(a.text, "the model says hello", "the feed keeps the one-line summary");
        // and it is emitted once
        read_session(&c, &mut s, "s1", 0, &mut lane, 12_000, &[]).unwrap();
        assert_eq!(s.feed.iter().filter(|f| f.feed == crate::model::Feed::Answer).count(), 1);
    }
}
