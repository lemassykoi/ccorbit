# Decisions

Why things are the way they are, including what was tried and rejected. Read this before
proposing a change in the same area; add an entry when you make a choice someone could undo
without knowing why.

## Read-only, by design (2026-10-16)

ccorbit reads the files Claude Code already writes: no hook, no settings change, no command, no
network. Hooks would give richer events, but they change the user's setup and run code on every
session. Everything shown had to be found in the files; when something is not there (network
bytes, the context window size), ccorbit says so instead of guessing silently.

## Rust, ratatui and serde_json only (2026-10-16)

A single small binary (about 1.2 MB) that idles at 2 to 3% CPU. Edition 2024. No async runtime:
the run loop polls files every tick, which is enough at this scale. A new dependency needs an entry
here and the list in `tests/rules.rs`.

## The name: ccorbit (2026-10-17)

`cctop` was taken on GitHub. ccorbit keeps the "cc" of Claude Code and matches the orbiting
hologram; it was free on GitHub and crates.io.

## Pixel mode rejected (2026-10-17)

The hologram was drawn as an image with glow through the kitty graphics protocol (it works in
Warp, under the text with `z=-1` so the mouse keeps working). Rejected after trying it:

- At one image pixel per dot, Warp scales the image up about six times and it looks blurry.
- A sharp image needs the screen's real resolution, about 7 MB a frame; Warp decodes and uploads
  every image it receives, so 30 frames a second is out of reach (performance was already poor at
  180 KB a frame).

The text hologram is sharp at any size and holds 30 frames a second. The code is kept on a local
branch, `pixels-experiment`, never pushed.

## The hologram at 30 frames per second (2026-10-17)

A frame takes about half a millisecond to compute; 30 frames a second cost about 3% CPU and
135 KB/s of terminal output, 60 about 5%. The sweep and the rings always turn, so the rate does not
drop when the graph is calm. `--fps` changes it.

## The compaction ceiling is the largest compaction seen (2026-10-17)

The window size is not in the transcript, and automatic compaction does not fire at a fixed size:
`opus-5-5` compacted near 968k, but also at 204k to 738k in the same sessions. Using the last
compaction as the limit showed nonsense (a session past its "limit"). The ceiling is the largest
automatic compaction seen with the same model in any session, and the UI calls it a ceiling, not a
promise; without one, the window is guessed (200k, or 1M once usage passes 200k).

## Slow-call warnings need 5 minutes and 8 times the usual (2026-10-17)

First thresholds (1 minute, 5 times the median) flagged a 3m43s frontend verification as "111×
usual": shell commands are keyed by their program (`Bash:npm`), and quick runs of the same program
make the median small. A wrong warning is worse than none. Now: at least 5 minutes, 5 earlier
calls of the kind, 8 times the median and 3 times the 90th percentile.

## No notice for failed shell commands (2026-10-17)

A workflow's agents failed 71 quick shell commands in a few minutes (a grep with no match, checks
that exit 1): one notice a second. Shell failures now show in the feed and in red in the
hologram; repeated failures raise their own warning (`failing ×3`). The same notice twice shows
once with a count.

## Prices are built in and called API-equivalent (2026-10-17)

The transcript has no price. `src/price.rs` holds the list prices of 2026-10-17 (cache write and
read rates per model, fast mode doubled). A subscription does not bill this way, so the UI writes
"≈$" and "API-equivalent". Models not in the list are left out of the estimate, not guessed.

## Workflow agents are followed live only for runs in progress (2026-10-17)

Their transcripts can weigh hundreds of megabytes per session. Runs already finished when ccorbit
starts are read from their `wf_*.json` record (agents and timings, no steps).

## Jumping to a tab is opt-in (2026-10-17)

`o` reads the session's process environment for Warp's `WARP_FOCUS_URL` and runs `/usr/bin/open`
with it, without a shell. It is the one feature that runs a command, so it needs `--allow-jump`,
and only two variables of the environment are kept. Other terminals give no focus link: ccorbit
brings the app forward instead.

## Windows: tested by CI, not yet on real sessions (2026-10-17)

The Windows build checks processes with `OpenProcess`/`GetExitCodeProcess`, finds `.claude` under
`%USERPROFILE%`, and reads paths with `\`. CI builds and tests it, but it has not been run against
real Claude Code sessions on Windows; the README says so.

## Golden screens: exact text, tolerant dots (2026-10-17)

The screens are compared with copies in `tests/golden/` at a frozen moment. Text must match
exactly. Braille dots may move in up to 2% of their cells, because `sin` and `cos` are not
bit-identical across systems (macOS, Linux, Windows); a dot that turns into text is still a
failure. The first golden copies caught two busy sessions with identical brainwaves.

## License: MIT OR Apache-2.0 (2026-10-17)

The Rust community's usual pair: people pick either; Apache-2.0 adds a patent grant.
