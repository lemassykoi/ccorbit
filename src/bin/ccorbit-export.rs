//! `ccorbit-export` — the light, standalone exporter (server mode). It is the same reader the wall
//! uses (`src/opencode.rs`) compiled **without the terminal**: it opens only this box's opencode
//! database (read-only) and answers one `GET /export` with the body-free summaries a central ccorbit
//! polls. No `ratatui`, no screen — just the daemon half, so the machines you only *publish from*
//! carry none of the wall.
//!
//! This is one of the two roles of [remote hosts](../../README.md#remote-hosts): run `ccorbit-export`
//! on every box (a systemd user service is the house style) and `ccorbit --hosts <FILE>` on the one
//! you watch from. `ccorbit --export <ADDR>` does the identical job from the full binary; this
//! separate build exists only to keep the exporter lightweight. All transport lives in `src/net.rs`.

// The reader and model expose the whole API the wall uses; this daemon reaches only a subset of it.
#![allow(dead_code)]

#[path = "../model.rs"]
mod model;
#[path = "../net.rs"]
mod net;
#[path = "../opencode.rs"]
mod opencode;
#[path = "../parse.rs"]
mod parse;
#[path = "../price.rs"]
mod price;
#[path = "../time.rs"]
mod time;

const HELP: &str = "\
ccorbit-export — publish this machine's opencode summaries to a central ccorbit (server mode).

  ccorbit-export [ADDR]     Listen on ADDR (default 0.0.0.0:30002); read-only
  --db <PATH>              Read that opencode database (default: opencode's own)
  -h, --help               Show this help
  -V, --version            Show the version

It opens only the opencode storage database (read-only) and answers GET /export when the central
presents the ~/.config/ccorbit/host-key token. http:// only, on a trusted LAN; it never writes,
never runs a command, and never draws a screen.
";

fn main() {
    let mut bind = "0.0.0.0:30002".to_string();
    let mut db = opencode::default_db();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return;
            }
            "-V" | "--version" => {
                println!("ccorbit-export {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--db" => db = args.next().map(std::path::PathBuf::from).unwrap_or_else(|| usage("--db needs a path")),
            other if other.starts_with('-') => usage(format!("unknown argument: {other}")),
            other => bind = other.to_string(),
        }
    }
    // Same shared LAN key as the central; refuse to serve without one (enforced in export_serve too).
    let key = net::read_key(&net::expand_home("~/.config/ccorbit/host-key"));
    let name = net::host_name_for(&bind);
    if let Err(e) = opencode::Open::export_serve(db, &bind, key.as_deref(), name) {
        eprintln!("ccorbit-export: {e}");
        std::process::exit(1);
    }
}

fn usage(msg: impl std::fmt::Display) -> ! {
    eprintln!("ccorbit-export: {msg}\n\n{HELP}");
    std::process::exit(2);
}
