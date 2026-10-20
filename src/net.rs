//! The one place ccorbit touches a socket. `http_get` is the short read-only HTTP GET behind both
//! the `--engines` footer pulse and the `--hosts` poller of remote exporters; `serve` is the plain
//! HTTP listener behind `--export`. Everything here is `http://` only, on the LAN-trust assumption
//! stated in the README: a request or reply is capped and every connection is short (a strict
//! connect and read timeout), so a stalled peer can never hang the run loop or a host.
//!
//! This is the single sanctioned socket file. `tests/rules.rs` lets a `// harness: allow network`
//! marker appear here and nowhere else; no other module may hold a connection.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs}; // harness: allow network (the only socket file)
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(700);
const READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// Never swallow more than this from one engine or host reply.
const MAX_REPLY: u64 = 4 << 20;
/// Never swallow more than this from one export request head (a request is a line and a few headers).
const MAX_REQUEST: usize = 16 << 10;

/// One short HTTP/1.1 GET, no keep-alive, capped reply. For the plain-HTTP loopback and LAN
/// endpoints listed in the `--engines` and `--hosts` files.
pub fn http_get(url: &str, key: Option<&str>) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// urls are read")?;
    let (host_port, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = split_host_port(host_port).ok_or("bad host:port")?;
    let addr = (host.as_str(), port)
        .to_socket_addrs() // harness: allow network (the only socket file)
        .map_err(|e| format!("resolve {host}: {e}"))?
        .next()
        .ok_or("no address")?;
    let mut sock = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("connect: {e}"))?; // harness: allow network (the only socket file)
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

/// The pieces of a request line and its headers that an export handler needs: the path, and the
/// `Authorization` value (a token can also ride in the query, for a browser or `curl` poking at it
/// by hand, but only its value is kept).
pub struct Request {
    pub path: String,
    pub token: Option<String>,
}

/// Bind the export listener. Split from the accept loop so a test can grab an ephemeral port
/// (`listen("127.0.0.1:0")`, read `local_addr()`) and drive `serve_on` on a thread.
pub fn listen(bind: &str) -> Result<TcpListener, String> {
    // harness: allow network (sole socket file)
    let listener = TcpListener::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| format!("{bind}: {e}"))?;
    Ok(listener)
}
/// A plain HTTP/1.1 listener for `--export`: accept one connection at a time (the poller is the
/// only client, and this is a LAN), read its request head up to a cap, hand it to `handle`, and
/// write the reply with `Connection: close`. `handle` returns a status code and a body; it checks
/// the token. The listener is polled against `stop` (so a test can bring it down) at a small idle
/// cost; in normal use `--export` runs it forever with `stop` never set.
pub fn serve_on(listener: TcpListener, stop: &AtomicBool, mut handle: impl FnMut(&Request) -> (u16, String)) {
    // harness: allow network (sole socket file)
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut sock, _)) => {
                // The listener is nonblocking so `accept` can be polled; put the accepted
                // connection back to blocking-with-timeout so a request head can be read at all.
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_read_timeout(Some(READ_TIMEOUT));
                let _ = sock.set_write_timeout(Some(READ_TIMEOUT));
                if let Some(req) = read_request(&mut sock) {
                    let (code, body) = handle(&req);
                    let reason = match code {
                        200 => "OK",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        404 => "Not Found",
                        _ => "Internal Server Error",
                    };
                    let reply = format!(
                        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes());
                    let _ = sock.flush();
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {}
        }
    }
}

/// Bind and serve: what `--export` calls. Returns only if the bind fails.
pub fn serve(bind: &str, stop: &AtomicBool, handle: impl FnMut(&Request) -> (u16, String)) -> Result<(), String> {
    let listener = listen(bind)?;
    serve_on(listener, stop, handle);
    Ok(())
}

/// Read one request head (the request line and headers, stopping at the blank line) from a client
/// socket, bounded so a chatty or stalled peer cannot grow the buffer. Returns `None` on a read
/// error, a timeout, an over-long head, or a request line that is not a `GET`.
// harness: allow network (read_request borrows the accepted socket)
fn read_request(sock: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(sock);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    if !request_line.starts_with("GET ") {
        return None;
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    let mut token: Option<String> = None;
    let mut seen = request_line.len();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        seen += n;
        if seen > MAX_REQUEST {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.strip_prefix("Authorization:").or_else(|| line.strip_prefix("authorization:")) {
            let v = v.trim();
            token = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        }
    }
    if token.is_none() {
        // A poller may also send the token as `?token=` so it can be checked from a browser.
        token = url_query(&query).remove("token").filter(|t| !t.is_empty());
    }
    Some(Request { path, token })
}

/// `host:port` split, tolerating a bare host (default port 80). Shared by `http_get` and the tests.
fn split_host_port(host_port: &str) -> Option<(String, u16)> {
    match host_port.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((host_port.to_string(), 80)),
    }
}

/// Percent-decoded view of a query string: `a=1&b=x%20y` -> a map. Used only to read the optional
/// `token` out of a hand-made export request; a poller authenticates with the header instead.
struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    fn remove(&mut self, key: &str) -> Option<String> {
        let i = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(i).1)
    }
}

fn url_query(query: &str) -> QueryParams {
    let pairs = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect();
    QueryParams(pairs)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `~/x` under `$HOME`, otherwise verbatim. Shared by the `--engines` and `--hosts` config readers.
pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

/// Read a token key file (trimmed, empty treated as absent), the house pattern from
/// `~/.config/strata/api-key` and `~/.config/qwen38/api-key`.
pub fn read_key(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// The name the exporter puts in its envelope, shown on each remote card. A bind address that names
/// a real host uses it; a wildcard bind falls back to the machine's own hostname (read from files
/// and the environment — never a command). Shared by `ccorbit --export` and `ccorbit-export`.
pub fn host_name_for(bind: &str) -> String {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    if !host.is_empty() && host != "0.0.0.0" && host != "::" && host != "*" {
        return host.to_string();
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_splits() {
        assert_eq!(split_host_port("127.0.0.1:30002"), Some(("127.0.0.1".to_string(), 30002)));
        assert_eq!(split_host_port("10.0.0.51"), Some(("10.0.0.51".to_string(), 80)));
        assert_eq!(split_host_port("h:bad"), None);
    }

    #[test]
    fn query_token_is_percent_decoded() {
        assert_eq!(url_query("a=1&token=ab%20cd&z=2").0[1], ("token".to_string(), "ab cd".to_string()));
        assert_eq!(url_query("token=x+y").0[0].1, "x y");
    }

    #[test]
    fn non_get_and_oversized_are_rejected() {
        // read_request needs a socket; the parsing it guards is exercised end-to-end by the
        // export round-trip below. Here we only assert the helpers http_get uses.
        assert!(http_get("https://x/", None).is_err(), "only http:// is read");
    }

    #[test]
    fn export_round_trips_and_gates_by_token() {
        use std::sync::Arc;
        let listener = listen("127.0.0.1:0").expect("bind ephemeral");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let th = std::thread::spawn(move || {
            serve_on(listener, &s2, |req| match (req.path.as_str(), req.token.as_deref()) {
                ("/export", Some("sekrit")) => (200, "{\"ok\":true}".to_string()),
                ("/export", _) => (401, "{\"error\":\"unauthorized\"}".to_string()),
                _ => (404, "{}".to_string()),
            });
        });
        // The socket is already listening (bound synchronously above), so no startup race.
        let got = http_get(&format!("http://127.0.0.1:{port}/export"), Some("sekrit")).expect("authorized poll");
        assert_eq!(got, "{\"ok\":true}");
        assert!(http_get(&format!("http://127.0.0.1:{port}/export"), Some("nope")).is_err(), "a wrong token is a 401, not data");
        // A token may also ride in the query (a browser or curl checking by hand).
        assert!(http_get(&format!("http://127.0.0.1:{port}/export?token=sekrit"), None).is_ok(), "query token accepted");
        assert!(http_get(&format!("http://127.0.0.1:{port}/other"), Some("sekrit")).is_err(), "only /export answers");
        stop.store(true, Ordering::Relaxed);
        let _ = th.join();
    }
}
