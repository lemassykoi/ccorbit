//! The local LLM engines behind the sessions: a small read-only pulse (cache hits, KV fill,
//! decode rate, queue) polled from engines the person lists in an `--engines` JSON file. Each
//! entry is `{name, url, kind, key_file}` with `kind` `vllm` (a Prometheus `/metrics`) or
//! `strata` (a status `/status`). `http://` only, one short connection per poll, replies read
//! but never written: this is the one place ccorbit talks to a port, and only to the addresses
//! listed in that file.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs}; // harness: allow network (engines listed in --engines)
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Poll every two seconds: the gauges are averages over a second anyway, and the frames are 30.
const POLL_EVERY: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(700);
const READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// Never swallow more than this from one reply.
const MAX_REPLY: u64 = 4 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Vllm,
    Strata,
}

pub struct Engine {
    pub name: String,
    url: String,
    kind: Kind,
    key: Option<String>,
    /// Ready to show: the last poll answered.
    pub alive: bool,
    /// Share of prompt tokens served from the prefix cache (vLLM counters, lifetime total).
    pub hit_pct: Option<f64>,
    /// Raw counter pair behind `hit_pct`, so several engines can be blended.
    hit_hw: Option<(f64, f64)>,
    /// KV pool fill, per cent.
    pub kv_pct: Option<f64>,
    pub running: Option<u64>,
    pub waiting: Option<u64>,
    /// Generated tokens per second, from the delta between two polls.
    pub tps: Option<f64>,
    /// Mean time to first token, seconds.
    pub ttft_s: Option<f64>,
    /// What the engine says it is doing (strata: thinking, writing, idle).
    pub phase: Option<String>,
    prev: Option<(u64, Instant)>,
    last_poll: Option<Instant>,
}

/// The engine list, loaded from `--engines`.
pub struct Engines {
    pub list: Vec<Engine>,
    pub error: Option<String>,
}

impl Engines {
    /// No engines asked for: the footer shows no pulse.
    pub fn none() -> Engines {
        Engines { list: Vec::new(), error: None }
    }

    /// Reads the file; a bad path or a bad file is kept as `error` and shown, not fatal.
    pub fn load(path: &Path) -> Engines {
        match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            None => Engines { list: Vec::new(), error: Some(format!("cannot read {}", path.display())) },
            Some(v) => {
                let mut list = Vec::new();
                for e in v.as_array().into_iter().flatten() {
                    let Some(url) = e.get("url").and_then(Value::as_str) else { continue };
                    let kind = match e.get("kind").and_then(Value::as_str).unwrap_or("") {
                        "strata" => Kind::Strata,
                        "vllm" => Kind::Vllm,
                        _ if url.ends_with("/status") || url.contains("status") => Kind::Strata,
                        _ => Kind::Vllm,
                    };
                    let key = e
                        .get("key_file")
                        .and_then(Value::as_str)
                        .map(expand_home)
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .map(|k| k.trim().to_string())
                        .filter(|k| !k.is_empty());
                    list.push(Engine {
                        name: e.get("name").and_then(Value::as_str).unwrap_or("engine").to_string(),
                        url: url.to_string(),
                        kind,
                        key,
                        alive: false,
                        hit_pct: None,
                        hit_hw: None,
                        kv_pct: None,
                        running: None,
                        waiting: None,
                        tps: None,
                        ttft_s: None,
                        phase: None,
                        prev: None,
                        last_poll: None,
                    });
                }
                Engines { list, error: None }
            }
        }
    }

    /// Polls the engines whose turn it is. Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for e in &mut self.list {
            changed |= e.poll();
        }
        changed
    }

    /// The share of prompts served from the prefix cache across every answering vLLM engine:
    /// the card's cache reactor reads this when opencode itself has no cache numbers.
    pub fn blend_hit(&self) -> Option<f64> {
        let (mut h, mut q) = (0.0, 0.0);
        for e in &self.list {
            if e.alive
                && let Some((eh, eq)) = e.hit_hw
            {
                h += eh;
                q += eq;
            }
        }
        (q > 0.0).then_some(h / q * 100.0)
    }

    /// One short line per engine, for the footer.
    pub fn strips(&self) -> Vec<String> {
        let mut out: Vec<String> = self.list.iter().map(Engine::strip).collect();
        if let Some(err) = &self.error {
            out.push(format!("engines: {err}"));
        }
        out
    }
}

impl Engine {
    fn poll(&mut self) -> bool {
        if self.last_poll.is_some_and(|t| t.elapsed() < POLL_EVERY) {
            return false;
        }
        self.last_poll = Some(Instant::now());
        let (ok, body) = match http_get(&self.url, self.key.as_deref()) {
            Ok(b) => (true, b),
            Err(_) => (false, String::new()),
        };
        let was = self.alive;
        self.alive = ok;
        let mut changed = was != ok;
        if !ok {
            self.prev = None;
            self.tps = None;
            return changed;
        }
        match self.kind {
            Kind::Vllm => changed |= self.read_prometheus(&body),
            Kind::Strata => changed |= self.read_status(&body),
        }
        changed
    }

    /// Sum of every `name` series in Prometheus text, with or without labels.
    fn read_prometheus(&mut self, body: &str) -> bool {
        let mut sum: HashMap<&str, f64> = HashMap::new();
        for line in body.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once(' ') else { continue };
            let name = key.split('{').next().unwrap_or(key);
            if let Ok(v) = val.trim().parse::<f64>() {
                *sum.entry(name).or_default() += v;
            }
        }
        let get = |names: &[&str]| -> Option<f64> { names.iter().find_map(|n| sum.get(*n).copied()) };
        let hits = get(&["vllm:prefix_cache_hits_total", "vllm:external_prefix_cache_hits_total"]);
        let queries = get(&["vllm:prefix_cache_queries_total", "vllm:external_prefix_cache_queries_total"]);
        let generated = sum.get("vllm:generation_tokens_total").copied();
        self.hit_hw = match (hits, queries) {
            (Some(h), Some(q)) if q > 0.0 => Some((h, q)),
            _ => None,
        };
        let old = std::mem::replace(&mut self.hit_pct, self.hit_hw.map(|(h, q)| h / q * 100.0));
        let old_kv = std::mem::replace(&mut self.kv_pct, get(&["vllm:kv_cache_usage_perc", "vllm:gpu_cache_usage_perc"]).map(|v| v * 100.0));
        let old_run = std::mem::replace(&mut self.running, get(&["vllm:num_requests_running"]).map(|v| v as u64));
        let old_wait = std::mem::replace(&mut self.waiting, get(&["vllm:num_requests_waiting"]).map(|v| v as u64));
        let old_ttft = std::mem::replace(
            &mut self.ttft_s,
            match (sum.get("vllm:time_to_first_token_seconds_sum").copied(), sum.get("vllm:time_to_first_token_seconds_count").copied()) {
                (Some(s), Some(c)) if c > 0.0 => Some(s / c),
                _ => None,
            },
        );
        // Decode rate: generated tokens between two polls. Only vLLM counters feed it.
        let old_tps = self.tps;
        if let Some(g) = generated.map(|v| v as u64) {
            let now = Instant::now();
            if let Some((pg, pt)) = self.prev {
                let dt = pt.elapsed().as_secs_f64();
                if dt > 0.3 {
                    self.tps = Some(g.saturating_sub(pg) as f64 / dt);
                }
            }
            self.prev = Some((g, now));
        }
        old != self.hit_pct || old_kv != self.kv_pct || old_run != self.running || old_wait != self.waiting || old_ttft != self.ttft_s || old_tps != self.tps
    }

    /// The status document a llama.cpp-style server writes while it works.
    fn read_status(&mut self, body: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(body) else { return false };
        let old = (self.running, self.phase.clone());
        self.running = v.get("busy").and_then(Value::as_bool).map(u64::from);
        self.waiting = v.get("queued").and_then(Value::as_u64);
        // The phase field goes stale once the engine is quiet: show it only while busy.
        self.phase = if self.running.unwrap_or(0) > 0 { v.get("phase").and_then(Value::as_str).map(str::to_string) } else { None };
        old != (self.running, self.phase.clone())
    }

    pub fn strip(&self) -> String {
        if !self.alive {
            return format!("{} · down", self.name);
        }
        let mut parts = vec![self.name.clone()];
        if let Some(p) = self.hit_pct {
            parts.push(format!("hit {p:.1}%"));
        }
        if let Some(p) = self.kv_pct {
            parts.push(format!("kv {p:.0}%"));
        }
        if let Some(r) = self.running {
            parts.push(if self.kind == Kind::Strata { if r > 0 { "busy".into() } else { "idle".into() } } else { format!("run {r}") });
        }
        if let Some(w) = self.waiting.filter(|w| *w > 0) {
            parts.push(format!("wait {w}"));
        }
        if let Some(t) = self.tps {
            parts.push(format!("{t:.0} t/s"));
        }
        if let Some(t) = self.ttft_s {
            parts.push(format!("ttft {t:.2}s"));
        }
        if let Some(p) = &self.phase {
            parts.push(p.clone());
        }
        parts.join(" · ")
    }
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

/// One short HTTP/1.1 GET, no keep-alive, capped reply. Only for the plain-HTTP loopback and
/// LAN endpoints listed in the engines file.
fn http_get(url: &str, key: Option<&str>) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// engine urls are read")?;
    let (host_port, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "bad port")?),
        None => (host_port.to_string(), 80u16),
    };
    let addr = (host.as_str(), port)
        .to_socket_addrs() // harness: allow network (engines listed in --engines)
        .map_err(|e| format!("resolve {host}: {e}"))?
        .next()
        .ok_or("no address")?;
    let mut sock = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("connect: {e}"))?; // harness: allow network (engines listed in --engines)
    let _ = sock.set_read_timeout(Some(READ_TIMEOUT));
    let _ = sock.set_write_timeout(Some(READ_TIMEOUT));
    let auth = match key {
        Some(k) => format!("Authorization: Bearer {k}\r\n"),
        None => String::new(),
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\n{auth}Accept: */*\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).map_err(|e| format!("send: {e}"))?;
    let mut raw = Vec::new();
    sock.take(MAX_REPLY).read_to_end(&mut raw).map_err(|e| format!("recv: {e}"))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed reply")?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(format!("status {}", head.lines().next().unwrap_or("?")));
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(kind: Kind) -> Engine {
        Engine {
            name: "e".into(),
            url: "http://x/".into(),
            kind,
            key: None,
            alive: true,
            hit_pct: None,
            hit_hw: None,
            kv_pct: None,
            running: None,
            waiting: None,
            tps: None,
            ttft_s: None,
            phase: None,
            prev: None,
            last_poll: None,
        }
    }

    #[test]
    fn prometheus_counters_become_readings() {
        let text = "# HELP x\n# TYPE x counter\n\
             vllm:prefix_cache_hits_total{model=\"qwen3.8\"} 95.0\n\
             vllm:prefix_cache_queries_total{model=\"qwen3.8\"} 100.0\n\
             vllm:kv_cache_usage_perc 0.27\n\
             vllm:num_requests_running 2.0\n\
             vllm:generation_tokens_total 1000.0\n\
             vllm:time_to_first_token_seconds_sum 1.2\n\
             vllm:time_to_first_token_seconds_count 4.0\n";
        let mut e = engine(Kind::Vllm);
        assert!(e.read_prometheus(text));
        assert_eq!(e.hit_pct, Some(95.0));
        assert!((e.kv_pct.unwrap() - 27.0).abs() < 0.01);
        assert_eq!(e.running, Some(2));
        assert_eq!(e.ttft_s, Some(0.3));
        assert!(e.strip().contains("hit 95.0%"));
    }

    #[test]
    fn strata_status_is_read() {
        let mut e = engine(Kind::Strata);
        assert!(e.read_status(r#"{"busy": true, "queued": 1, "phase": "thinking", "prompt_tokens": 237, "generated": 3}"#));
        assert_eq!(e.running, Some(1));
        assert_eq!(e.waiting, Some(1));
        assert_eq!(e.phase.as_deref(), Some("thinking"));
        assert_eq!(e.strip(), "e · busy · wait 1 · thinking");
        assert!(e.read_status(r#"{"busy": false, "queued": 0, "phase": "thinking"}"#));
        assert_eq!(e.strip(), "e · idle");
    }

    #[test]
    fn down_engines_say_so() {
        let mut e = engine(Kind::Vllm);
        e.alive = false;
        assert_eq!(e.strip(), "e · down");
    }
}
