//! Small, tolerant readers for the pieces of transcript records ccorbit shows.

use serde_json::Value;

use crate::model::Kind;

pub fn kind(name: &str) -> Kind {
    match name {
        "Agent" | "Task" => Kind::Agent,
        "Skill" => Kind::Skill,
        "Workflow" => Kind::Workflow,
        "Bash" | "Monitor" | "KillShell" | "TaskStop" => Kind::Shell,
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "Grep" | "Glob" | "LS" => Kind::File,
        "WebFetch" | "WebSearch" => Kind::Web,
        "SendMessage" | "ListAgents" | "AskUserQuestion" => Kind::Comms,
        n if n.starts_with("mcp__") => Kind::Mcp,
        _ => Kind::Tool,
    }
}

/// `mcp__claude_ai_Notion__notion-search` -> (`claude_ai_Notion`, `notion-search`).
pub fn mcp_parts(name: &str) -> (&str, &str) {
    let rest = name.strip_prefix("mcp__").unwrap_or(name);
    rest.split_once("__").unwrap_or((rest, ""))
}

pub fn server_label(server: &str) -> &str {
    server.strip_prefix("claude_ai_").unwrap_or(server)
}

/// One line describing what a tool call is about, from its input.
pub fn summarize(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    let text = match name {
        "Bash" => s("description").or(s("command")).map(str::to_string),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            s("file_path").or(s("notebook_path")).map(short_path)
        }
        "Grep" => s("pattern").map(|p| match s("path") {
            Some(d) => format!("{p}  in {}", short_path(d)),
            None => p.to_string(),
        }),
        "Glob" => s("pattern").map(str::to_string),
        "WebFetch" => s("url").map(str::to_string),
        "WebSearch" | "ToolSearch" => s("query").map(str::to_string),
        "Agent" | "Task" => Some(format!(
            "{} · {}",
            s("subagent_type").unwrap_or("general-purpose"),
            s("description").unwrap_or("")
        )),
        "Skill" => s("skill").map(|k| match s("args") {
            Some(a) if !a.is_empty() => format!("{k} {a}"),
            _ => k.to_string(),
        }),
        "Workflow" => Some(workflow_meta(input).0),
        "SendMessage" => s("to").map(|t| format!("to {t}")),
        "AskUserQuestion" => input["questions"][0]["question"].as_str().map(str::to_string),
        "Artifact" => Some(format!(
            "{} {}",
            s("action").unwrap_or("publish"),
            s("file_path").or(s("url")).unwrap_or("")
        )),
        _ => None,
    };
    let text = text.or_else(|| first_string(input)).unwrap_or_default();
    clip(first_line(&text), 200)
}

/// Same tool, same input, same hash: how a call made again is recognized.
pub fn signature(name: &str, input: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    input.to_string().hash(&mut h);
    h.finish()
}

/// What a call's duration is compared with: the tool, the program a shell command runs, the
/// agent type. Calls that wait on purpose (a question, a background launch, a monitor) have none.
pub fn duration_key(name: &str, input: &Value) -> Option<String> {
    if input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match name {
        "AskUserQuestion" | "ExitPlanMode" | "Workflow" | "Monitor" | "TaskOutput" | "SendMessage" | "ScheduleWakeup" => None,
        "Bash" => Some(format!("Bash:{}", command_word(input.get("command").and_then(Value::as_str)?))),
        "Agent" | "Task" => Some(format!("Agent:{}", input.get("subagent_type").and_then(Value::as_str).unwrap_or("general-purpose"))),
        n => Some(n.to_string()),
    }
}

/// The program a shell command runs: `cd x && FOO=1 timeout 60 ./target/release/ccorbit` -> `ccorbit`.
pub fn command_word(cmd: &str) -> String {
    for seg in cmd.split(['&', ';', '|', '\n']) {
        let mut words = seg.split_whitespace().map(|w| w.trim_matches(|c| matches!(c, '(' | ')' | '"' | '\'')));
        while let Some(w) = words.next() {
            if w.is_empty() || w.contains('=') || w.starts_with('-') || w.parse::<f64>().is_ok() {
                continue;
            }
            match w {
                "cd" | "export" | "set" => break,
                "time" | "timeout" | "env" | "nice" | "sudo" | "exec" | "command" => continue,
                _ => return file_name(w).trim_end_matches(".exe").to_string(),
            }
        }
    }
    "sh".into()
}

fn first_string(v: &Value) -> Option<String> {
    v.as_object()?
        .values()
        .filter_map(Value::as_str)
        .find(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// The last part of a path, whatever its separators (`/`, or `\` on Windows).
pub fn file_name(p: &str) -> &str {
    p.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(p)
}

/// The path relative to `root` when it lies inside it.
pub fn under<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(root)?;
    rest.strip_prefix(['/', '\\']).filter(|r| !r.is_empty())
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        return p.to_string();
    }
    format!("…/{}", parts[parts.len() - 2..].join("/"))
}

pub fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Text of a tool result or message content: a string, or the first text block.
pub fn content_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    v.as_array()
        .into_iter()
        .flatten()
        .find_map(|b| b.get("text").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

/// Content of the first `<name>…</name>` element.
pub fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim())
}

/// The word that follows `prefix`, as in `Task ID: wsvplqpqh`.
pub fn word_after<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let start = text.find(prefix)? + prefix.len();
    let word = text[start..].split(|c: char| c.is_whitespace() || c == ',' || c == ')').next()?;
    let word = word.trim_end_matches('.');
    (!word.is_empty()).then_some(word)
}

/// Name and phase titles from the `meta` literal at the top of a workflow script.
pub fn workflow_meta(input: &Value) -> (String, Vec<String>) {
    if let Some(n) = input.get("name").and_then(Value::as_str) {
        return (n.to_string(), Vec::new());
    }
    let script = input.get("script").and_then(Value::as_str).unwrap_or("");
    let meta = match script.find("export const meta") {
        Some(i) => &script[i..],
        None => script,
    };
    let meta = &meta[..meta.find("\n}").unwrap_or(meta.len())];
    let name = js_string_after(meta, "name:").unwrap_or_else(|| "workflow".to_string());
    let mut phases = Vec::new();
    if let Some(p) = meta.find("phases") {
        let mut rest = &meta[p..];
        while let Some(i) = rest.find("title:") {
            rest = &rest[i + 6..];
            if let Some(t) = js_string_at(rest) {
                phases.push(t);
            }
        }
    }
    (name, phases)
}

fn js_string_after(s: &str, key: &str) -> Option<String> {
    js_string_at(&s[s.find(key)? + key.len()..])
}

fn js_string_at(s: &str) -> Option<String> {
    let s = s.trim_start();
    let quote = s.chars().next().filter(|c| matches!(c, '\'' | '"' | '`'))?;
    let mut out = String::new();
    let mut chars = s[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c if c == quote => return Some(out),
            c => out.push(c),
        }
    }
    None
}

pub fn short_model(m: &str) -> &str {
    let m = match m.find("claude-") {
        Some(i) => &m[i + 7..],
        None => m,
    };
    match m.rsplit_once('-') {
        Some((head, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => m,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_workflow_meta() {
        let input = json!({"script": "export const meta = {\n  name: 'verify-platform',\n  description: 'x',\n  phases: [\n    { title: 'Research', detail: 'a' },\n    { title: \"Build\" },\n  ],\n}\nconst x = { title: 'not a phase' }"});
        let (name, phases) = workflow_meta(&input);
        assert_eq!(name, "verify-platform");
        assert_eq!(phases, vec!["Research", "Build"]);
    }

    #[test]
    fn reads_notifications_and_ids() {
        let n = "<task-notification>\n<task-id>bitwf2sge</task-id>\n<status>completed</status>\n</task-notification>";
        assert_eq!(tag(n, "task-id"), Some("bitwf2sge"));
        assert_eq!(tag(n, "status"), Some("completed"));
        assert_eq!(word_after("Workflow launched in background. Task ID: wsvplqpqh\nSummary", "Task ID: "), Some("wsvplqpqh"));
    }

    #[test]
    fn names() {
        assert_eq!(mcp_parts("mcp__claude_ai_Notion__notion-search"), ("claude_ai_Notion", "notion-search"));
        assert_eq!(short_model("claude-opus-5-5"), "opus-5-5");
        assert_eq!(short_model("eu.anthropic.claude-haiku-4-5-20251001"), "haiku-4-5");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(file_name("/home/me/src/ui.rs"), "ui.rs");
        assert_eq!(file_name("C:\\Users\\me\\src\\ui.rs"), "ui.rs");
        assert_eq!(under("C:\\code\\app\\src\\a.rs", "C:\\code\\app"), Some("src\\a.rs"));
        assert_eq!(under("/code/app/src/a.rs", "/code/app"), Some("src/a.rs"));
        assert_eq!(under("/code/apple/a.rs", "/code/app"), None);
        assert_eq!(command_word("C:\\tools\\cargo.exe build"), "cargo");
    }

    #[test]
    fn keys_calls_by_what_they_run() {
        assert_eq!(command_word("cd /x && SDKROOT=/y cargo build --release 2>&1 | tail -5"), "cargo");
        assert_eq!(command_word("timeout 60 ./target/release/ccorbit --dump"), "ccorbit");
        assert_eq!(command_word("(cd web && pnpm test)"), "pnpm");
        assert_eq!(duration_key("Bash", &json!({"command": "git status"})).as_deref(), Some("Bash:git"));
        assert_eq!(duration_key("Bash", &json!({"command": "npm run dev", "run_in_background": true})), None);
        assert_eq!(duration_key("Agent", &json!({"subagent_type": "Explore"})).as_deref(), Some("Agent:Explore"));
        assert_eq!(duration_key("AskUserQuestion", &json!({})), None);
        assert_eq!(signature("Read", &json!({"file_path": "/a"})), signature("Read", &json!({"file_path": "/a"})));
        assert_ne!(signature("Read", &json!({"file_path": "/a"})), signature("Read", &json!({"file_path": "/b"})));
    }
}
