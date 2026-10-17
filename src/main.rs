//! ccorbit: a retro, read-only live view of every Claude Code session on this machine.

mod demo;
mod hud;
mod insight;
mod jump;
mod model;
mod parse;
mod price;
mod source;
mod tail;
mod theme;
mod time;
mod ui;

use std::path::PathBuf;

use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use std::process::ExitCode;

const HELP: &str = "\
ccorbit: live view of every Claude Code session on this machine

USAGE:
    ccorbit [OPTIONS]

OPTIONS:
    --replay <FILE>      Play back a transcript (.jsonl) instead of watching live sessions
    --speed <N>          Replay speed multiplier (default 20)
    --theme <NAME>       phosphor (default), amber or neon
    --claude-dir <DIR>   Claude Code data directory (default: $CLAUDE_CONFIG_DIR, or .claude in
                         your home folder)
    --hud                Start on the HUD (the live graph) instead of the wall
    --director           Start on the HUD with the director at the camera
    --demo               Show made-up sessions instead of yours (to try it, or record it)
    --allow-jump         Let `o` bring forward the terminal tab of a session (runs `open`)
    --fps <N>            Frames per second of the HUD (default 30)
    --no-boot            Skip the boot animation
    --dump               Print the current state as plain text and exit
    -h, --help           Show this help
    -V, --version        Show the version

ccorbit only reads files: it never writes, never runs a command and never opens a network
connection. The one exception is --allow-jump, which runs `open` when you press `o`.
";

pub struct Opts {
    pub claude_dir: PathBuf,
    pub replay: Option<PathBuf>,
    pub speed: f64,
    pub theme: usize,
    pub boot: bool,
    pub dump: bool,
    pub snapshot: Option<(u16, u16)>,
    pub hud: bool,
    pub director: bool,
    pub demo: bool,
    pub allow_jump: bool,
    pub fps: u32,
    pub snapshot_html: Option<(u16, u16, PathBuf)>,
}

fn parse_args() -> Result<Opts, String> {
    let claude_dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        // `HOME` on macOS and Linux, `USERPROFILE` on Windows.
        None => {
            PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).ok_or("neither HOME nor USERPROFILE is set")?).join(".claude")
        }
    };
    let mut o = Opts {
        claude_dir,
        replay: None,
        speed: 20.0,
        theme: 0,
        boot: true,
        dump: false,
        snapshot: None,
        hud: false,
        director: false,
        demo: false,
        allow_jump: false,
        fps: 30,
        snapshot_html: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("ccorbit {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--replay" => o.replay = Some(args.next().ok_or("--replay needs a transcript path")?.into()),
            "--speed" => o.speed = args.next().and_then(|s| s.parse().ok()).filter(|s: &f64| *s > 0.0).ok_or("--speed needs a positive number")?,
            "--theme" => {
                let name = args.next().ok_or("--theme needs a name")?;
                o.theme = theme::index(&name).ok_or_else(|| format!("unknown theme: {name}"))?;
            }
            "--claude-dir" => o.claude_dir = args.next().ok_or("--claude-dir needs a path")?.into(),
            "--no-boot" => o.boot = false,
            "--hud" => o.hud = true,
            "--director" => o.director = true,
            "--demo" => o.demo = true,
            "--allow-jump" => o.allow_jump = true,
            "--fps" => o.fps = args.next().and_then(|s| s.parse().ok()).filter(|n: &u32| (1..=120).contains(n)).ok_or("--fps needs a number from 1 to 120")?,
            "--jump-check" => {}
            "--bench-hud" => {
                args.next();
            }
            "--dump" => o.dump = true,
            // Undocumented: prints one frame of each view, for checking the layout without a terminal.
            "--snapshot" => {
                let size = args.next().ok_or("--snapshot needs WIDTHxHEIGHT")?;
                let (w, h) = size.split_once('x').ok_or("--snapshot needs WIDTHxHEIGHT")?;
                o.snapshot = Some((w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?));
            }
            // Undocumented: writes colored HTML frames of every view, for screenshots.
            "--snapshot-html" => {
                let size = args.next().ok_or("--snapshot-html needs WIDTHxHEIGHT and a folder")?;
                let (w, h) = size.split_once('x').ok_or("--snapshot-html needs WIDTHxHEIGHT")?;
                let dir = args.next().ok_or("--snapshot-html needs a folder")?;
                o.snapshot_html = Some((w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?, dir.into()));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(o)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ccorbit: {e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    let mut world = match &opts.replay {
        Some(path) => match source::World::replay(path, opts.speed) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("ccorbit: cannot read {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None if opts.demo => source::World::demo(),
        None => source::World::live(opts.claude_dir.clone()),
    };
    if let Some((w, h, dir)) = &opts.snapshot_html {
        world.run_to_end();
        if let Err(e) = ui::snapshot_html(&world, &opts, *w, *h, dir) {
            eprintln!("ccorbit: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }
    // Undocumented: times the drawing of the hologram, to check the frame budget.
    if let Some(pos) = std::env::args().position(|a| a == "--bench-hud") {
        let size = std::env::args().nth(pos + 1).unwrap_or_default();
        let (w, h) = size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((170, 50));
        world.run_to_end();
        match ui::bench_hud(&mut world, &opts, w, h, 300) {
            Ok(report) => println!("{report}"),
            Err(e) => eprintln!("ccorbit: {e}"),
        }
        return ExitCode::SUCCESS;
    }
    if let Some((w, h)) = opts.snapshot {
        world.run_to_end();
        print!("{}", ui::snapshot(&world, &opts, w, h));
        return ExitCode::SUCCESS;
    }
    // Undocumented: says how `o` would reach each running session, without doing it.
    if std::env::args().any(|a| a == "--jump-check") {
        world.run_to_end();
        for s in world.sessions.iter().filter(|s| s.alive) {
            let how = match jump::target(s.pid) {
                Ok(jump::Target::Tab(url)) => format!("its exact Warp tab ({}…)", &url[..url.len().min(24)]),
                Ok(jump::Target::App(app)) => format!("its terminal app only ({app})"),
                Err(why) => why.to_string(),
            };
            println!("{} (pid {}): {how}", s.project(), s.pid);
        }
        return ExitCode::SUCCESS;
    }
    if opts.dump {
        world.run_to_end();
        print!("{}", ui::dump(&mut world));
        return ExitCode::SUCCESS;
    }
    let mut terminal = ratatui::init();
    // Mouse reporting is on while ccorbit runs; it is switched off again on exit and on a panic.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let restore_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        restore_hook(info);
    }));
    let result = ui::run(&mut terminal, world, &opts);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ccorbit: {e}");
            ExitCode::FAILURE
        }
    }
}
