# ccorbit

[![CI](https://github.com/pcangemi/ccorbit/actions/workflows/ci.yml/badge.svg)](https://github.com/pcangemi/ccorbit/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ccorbit.svg)](https://crates.io/crates/ccorbit)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A live terminal view of every Claude Code session running on your machine. It shows sessions,
subagents, workflows, skills, MCP servers, token flow, cost and context, as a wall of cards, a
live tree per session, a 3D hologram you can turn with the mouse, and a city of the files being
worked on.

It is a single Rust binary of about 1.2 MB. It only reads the files Claude Code already writes: it
never writes, never runs a command and never opens a network connection (the one opt-in exception
is [jumping to a session's tab](#jump-to-a-session)). It uses 2 to 3% CPU and
9 to 13 MB of memory, the 3D hologram included.

![ccorbit in demo mode: the wall of sessions, a session's vitals, the 3D hologram, the director and the code city](docs/demo.gif)

<sub>Recorded with `ccorbit --demo`: made-up sessions, nothing real.</sub>

## Install

On macOS or Linux, the prebuilt binary:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/pcangemi/ccorbit/releases/latest/download/ccorbit-installer.sh | sh
```

With Homebrew:

```sh
brew install pcangemi/tap/ccorbit
```

With Cargo (Rust 1.85 or newer):

```sh
cargo install ccorbit
```

Then run it:

```sh
ccorbit              # the wall of sessions
ccorbit --hud        # start on the 3D hologram
ccorbit --director   # the hologram with the camera running itself, for a second screen
ccorbit --demo       # made-up sessions: try it without Claude Code running
```

It is built and tested on macOS and Linux, in any terminal with mouse support. Gradients and fog
look best with 24-bit color (`COLORTERM=truecolor`, which Ghostty, iTerm2, WezTerm, kitty and Warp
set); other terminals get the nearest 256-color shades.

### From source

```sh
git clone https://github.com/pcangemi/ccorbit.git
cd ccorbit
cargo build --release
cp target/release/ccorbit ~/.local/bin/
```

### If the build fails on macOS

`tapi error: malformed file ... unknown architecture arm64e.x1-macos` means the Command Line Tools
linker is older than the default macOS SDK. Build against an SDK it can read:

```sh
ls /Library/Developer/CommandLineTools/SDKs/
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.sdk cargo build --release
```

## Screens

Every session keeps one color everywhere: on its card, its tower, its feed lines, its planet in
the hologram and its district in the city. Two sessions in the same folder get a suffix
(`webapp·63`).

### Wall (the start screen)

One card per running session, whatever its project:

- **Status:** BUSY, IDLE, ENDED, or a blinking NEEDS YOU when a question or a plan waits for your
  answer.
- **Current step:** it shimmers while it runs. "model · thinking or writing the next step" means
  the session is generating with no tool running.
- **Chips:** collisions (`✖`), warnings (`⚠`), the workflow race (`△`), agents, background tasks,
  skills and MCP servers.
- **Token flow** over the last minute, and the context gauge against the compaction ceiling.
- **Brainwave**, cache reactor, estimated price and model (see Readings).
- **Last prompt.**

Below the cards:

- **Skyline:** one isometric tower per session, its height following tokens per minute.
- **Signal feed:** the latest events of all sessions.

### Session (Enter on a card)

- **Vitals:**
  - three brainwave traces (thinking, writing, tools) over the last five minutes;
  - tokens sent and generated;
  - the prompt cache with its hit ratio and the estimated price;
  - the context against the compaction ceiling, with how many calls are left at the current pace.
- **Tree:** prompts and steps, subagents under the call that started them, and workflows with
  their phases. The agents of a running workflow appear live, each with its own steps. Slow or
  looping calls are marked `⚠`.
- **Detail** of the selected row. A workflow shows its race against an earlier run: two lanes on
  one time scale, ahead or behind, and the estimated time left.
- **Connections:** MCP servers and skills.

### HUD (`g`): the hologram

- **Layout:** the core sits in the middle. Sessions orbit it as planets on a tilted floor, and
  their agents and workflows circle them as moons. The MCP servers and skills they share hang on a
  crown around the edge.
- **Depth:** far things fade and shrink.
- **Packets** run along the links as work happens:
  - prompts come in from the core;
  - calls go out to agents, workflows and servers;
  - results come back, green when they succeed and red when they fail;
  - token traffic streams between each session and the core.
- **Events:** new nodes fly out from their parent, a rebuilt prompt cache flares, and two sessions
  changing the same file are joined by a flashing red link.
- **Mouse:**
  - drag to turn the view (across to spin, up and down to tilt);
  - wheel to zoom around the pointer;
  - click to lock a target, which dims what it is not linked to and opens a panel with its details;
  - double-click to open a session, or to fly to any other node.
- **Frame rate:** 30 frames per second, sharp at any size, for about 3% CPU; `--fps 60` for
  smoother still.
- **`v`** folds the view flat (seen from above) and back.
- **`d`** starts the director: the camera runs itself like a live broadcast.
  - It flies to whatever just happened (a session waiting for you, a collision, a failure, a slow
    or looping call, an agent or a workflow finishing) and holds the shot under a caption.
  - Between events it slowly circles the graph while the caption tours the sessions, and an inset
    follows the busiest session.
  - It also starts by itself after two minutes without input. Any key or click hands the camera
    back.

### Code city (`c`)

- **Districts and buildings:** every file the sessions read or changed is a building, grouped by
  project. A building's height follows the number of lines changed, and its color is the session
  that touched it last.
- **Signals:** fresh edits glow, windows light up while a file is being read, and a file two
  sessions change at once flashes red.
- **Side panel:** the selected file (who, when, lines added and removed) and the files that
  changed most.
- **Keys:** `[` and `]` change the time window (hour, 6 hours, 24 hours, everything loaded), and
  Enter opens the session that touched the selected file last.

### Jump to a session

Start ccorbit with `--allow-jump`, point at a session (a card on the wall, the open session, a
locked target in the hologram, a building in the city) and press `o`: the terminal tab where that
session runs comes to the front.

- **Warp:** ccorbit brings forward the exact tab. Warp gives every tab a focus link, and the
  session's process inherits it in its environment as `WARP_FOCUS_URL`.
- **Other macOS terminals:** ccorbit brings the terminal app forward, not the exact tab.

This is off by default because it is the one thing that goes beyond reading files:

- ccorbit reads the environment of that session's own process, keeps only `WARP_FOCUS_URL` and
  `__CFBundleIdentifier`, and never stores or shows them;
- it runs `/usr/bin/open` with the link, without a shell.

Without the option, `o` only says how to turn it on.

**Help (`?`)** lists every key, glyph and color. **Toasts** in the bottom right (the same one
again shows once, with a count) announce:

- an agent or workflow finished, with the race result;
- a failed step (not shell commands: they fail too often to be news; the feed shows them, and
  repeated failures raise a warning);
- a session that needs you;
- a compaction;
- a slow or looping call;
- a collision.

## Readings

| Reading | What it is | Where it comes from |
|---|---|---|
| Brainwave | Time spent thinking, writing and waiting for tools, summed over all agents | The gaps between transcript records: Claude Code writes each thinking block, text and tool call when it is complete, and each tool result when the tool returns. Thinking text is not in the transcript; only its size and timing are used. |
| Cache reactor `◉ 97%` | Share of the input served from the prompt cache over five minutes; `✺` flares when the cache is rebuilt | The `usage` of each API call |
| Price `≈$12.40` | What the tokens would cost on the Anthropic API | Built-in list prices (October 2026), including cache write and read rates and fast mode. A subscription does not bill this way. Models missing from the list are left out. |
| Compaction ceiling | Context against the largest automatic compaction seen with the same model, and calls left at the current pace | `compact_boundary` records. Claude Code can compact earlier than the ceiling. |
| `⚠ 9× usual` | A call running far longer than calls of its kind | Durations of finished calls, by tool, by program for shell commands and by agent type. A warning needs at least 5 minutes and 5 earlier calls, and 8 times the usual duration. |
| `⚠ ×4 in a row` | An agent repeating the exact same call, or failing with the same tool again and again | Tool name and input |
| `✖ collision` | A file changed by two sessions (or by two agents of one session at work at the same time) within 15 minutes | Edit and write results |
| `△ 8m behind · ETA 12m` | A running workflow against the last earlier run of the same workflow, or of one with the same phases | Earlier runs' `wf_*.json` records |

## Keys

| Where | Keys |
|---|---|
| Everywhere | `?` help · `g` HUD · `c` code city · `o` jump to the session's tab (with `--allow-jump`) · `t` theme · `x` effects on or off · `m` mouse capture off (to select text) · `q` quit |
| Wall | arrows or `hjkl` select · Enter open · mouse: click selects, click again opens, wheel moves |
| Session | arrows or `jk` move · Enter fold · `e` expand all · `f` follow the newest step · `Tab` next session · `Esc` back |
| HUD | `Tab` or arrows cycle targets · Enter open · `f` fly to the target · `+` `-` zoom · `0` reset the view · `v` flat or 3D · `d` director · space ping · `Esc` drop the target, then back |
| Code city | arrows move · Enter open the session · `[` `]` time window · `Esc` back |
| Replay | `p` pause · `<` `>` speed |

## Options

| Option | Effect |
|---|---|
| `--hud` | Start on the hologram |
| `--demo` | Show made-up sessions instead of yours: to try it, or to record it without showing your work |
| `--fps <N>` | Frames per second of the hologram, default 30 (60 costs about 5% CPU) |
| `--allow-jump` | Let `o` bring forward the terminal tab of a session (runs `open`; see [Jump to a session](#jump-to-a-session)) |
| `--director` | Start on the hologram with the director at the camera |
| `--replay <FILE>` | Play back a recorded transcript (`~/.claude/projects/<project>/<session>.jsonl`) |
| `--speed <N>` | Replay speed, default 20 (idle stretches are skipped) |
| `--theme <NAME>` | `phosphor` (default), `amber` or `neon` |
| `--claude-dir <DIR>` | Claude Code data directory, default `$CLAUDE_CONFIG_DIR` or `~/.claude` |
| `--no-boot` | Skip the boot animation |
| `--dump` | Print every session and its readings as plain text, then exit |

## What it reads

Only files Claude Code already writes, under `~/.claude`. No hook, no settings change, no command,
no network. The exception, off by default, is `--allow-jump` (see above).

| Path | Used for |
|---|---|
| `sessions/<pid>.json` | Which sessions run now. The `.key` files beside them are never opened. A session counts as running while its process exists (checked with signal 0, which sends nothing). |
| `projects/<folder>/<session>.jsonl` | Prompts, steps, results, token usage, compactions, background task notices, title |
| `projects/<folder>/<session>/subagents/` | Subagents, and the agents of running workflows (`subagents/workflows/<run>/`) |
| `projects/<folder>/<session>/workflows/wf_*.json` | Finished workflow runs: phases, agents, durations |

Transcripts hold your work. ccorbit keeps one-line summaries, counts and times in memory: never
message bodies, file contents or edited text. It never writes anything anywhere.

## Limits

- The transcript format is not a documented interface. This was built against Claude Code 2.1 and
  may need changes when the format moves.
- Token rates are counted when each response is written, so a long reply shows as a burst.
- "Sent" counts every input token the model processes, cache reads included, which is why it is
  much larger than "generated".
- Network bytes are not in any file; the session view shows how fast its transcripts grow instead.
- Agents of a workflow that had already finished when ccorbit started come from the run's record,
  without their steps.
- A background task launched before the current Claude Code process started is shown as lost.

## Development

```sh
cargo test                     # parsing, lifecycles, readings, prices, the demo, HUD hit tests
cargo run --release -- --dump  # check the readings against your own sessions
```

| File | Role |
|---|---|
| `src/main.rs` | Options and startup |
| `src/source.rs` | Live discovery and file following, replay, workflow history |
| `src/tail.rs` | Follows an append-only file by byte offset, holding back partial lines |
| `src/model.rs` | Session state built from transcript lines: steps, agents, tokens, phases, files, compactions |
| `src/insight.rs` | Readings across sessions: slow and looping calls, collisions, workflow races, compaction ceilings |
| `src/price.rs` | API list prices |
| `src/parse.rs` | Tolerant readers for the pieces of records shown on screen |
| `src/ui.rs` | Boot screen, wall, skyline, session view, keys and mouse, plain-text dump |
| `src/ui/vitals.rs` | Brainwave, cache reactor, context gauge, race lanes |
| `src/ui/city.rs` | The code city |
| `src/hud.rs` | The hologram: layout, camera, animation, packets, hit tests, target panel |
| `src/hud/director.rs` | The director |
| `src/demo.rs` | The made-up sessions of `--demo`, written as transcript records |
| `src/jump.rs` | Jumping to a session's terminal tab (opt-in) |
| `src/theme.rs` | Palettes, gradients, fog and the 256-color fallback |

`docs/demo.tape` records `docs/demo.gif` with [vhs](https://github.com/charmbracelet/vhs). Releases
are built by [dist](https://axodotdev.github.io/cargo-dist/): pushing a version tag (`v0.2.0`)
builds the binaries, the installer and the Homebrew formula.

Two hidden options help check the layout without a terminal:

- `--snapshot WxH` prints one frame of each view as text.
- `--snapshot-html WxH DIR` writes colored HTML frames you can open in a browser.

## License

Copyright (c) 2026 Pierre CANGEMI. Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT),
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
ccorbit by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
