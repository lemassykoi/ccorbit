# Decisions

Why things are the way they are, including what was tried and rejected. Read this before
proposing a change in the same area; add an entry when you make a choice someone could undo
without knowing why.

## The live source is opencode, not the upstream monitor (2026-10-19, fork)

This fork runs on a machine whose sessions are `opencode` talking to locally served models; the
upstream monitor's reader was dropped entirely (`--replace entirely`, the owner's word). The record
grammar of `Session::ingest` stayed: the new source is a *translator* from opencode's storage
rows to those records, so `model`, `insight`, `ui` and `hud` did not move and the golden screens
survived (only the help screen's DATA line changed). Rejected then: polling `opencode serve` over
HTTP as a live source (the zero-network promise), and a sidecar that rewrote rows into
transcript-shaped files (a second writer to keep in step for no fidelity gain). The database is
opened `READ_ONLY`; the replay reader and the demo still feed records directly, in the upstream grammar.
*(The serve point is revisited and corrected under "Remote opencode hosts": a deliberately started
serve does read the shared database — it was rejected for other reasons, not that one.)*

## rusqlite, the third dependency, and why the binary grew (2026-10-19, fork)

Reading SQLite needs a SQLite reader. The bundled build keeps one static binary across the three
platforms; the alternative (hand-rolled page parsing) is a hundred times the code. The binary
grew from 1.2 MB to about 3.3 MB, and the size budget in `scripts/verify.sh` moved to 3.8 MB to
match. The frame budget and the CPU/footprint promises did not change.

## The engine pulse is the only socket (2026-10-19, fork)

A local model reports no per-session cache share (opencode zeroes `cache.read`), so the true
prefix-cache hit rate, KV fill and decode rate come from the engine itself — but reading them is
a network call, against the house rule. Kept honest: off unless `--engines` names a JSON file of
addresses, `http://` only, one short GET per engine per two seconds, replies capped, counters
only; the marked lines live in `src/engines.rs` and nowhere else, which the rule test enforces
line by line. Sessions and readings never depend on it: with no `--engines` ccorbit is exactly
the zero-network tool it was.
- *(In part superseded 2026-10-19 by v0.4.0: all transport code — this client and the new `--export`
  listener — moved into `src/net.rs`, so `src/engines.rs` no longer holds a socket of its own; it calls
  `net::http_get`, as does the new `--hosts` poller. The engine pulse is no longer the only socket, but
  it is still the only one the local-only tool ever opens, and the only socket file is now `src/net.rs`.
  See "Remote opencode hosts" below.)*

## Remote opencode hosts: the exporter, not the serve (2026-10-19, v0.4.0)

The tool followed one machine's sessions; the user runs several opencodes across the LAN and wants
one central ccorbit to show them all at full fidelity. This **adds a network surface to the
product's main feature**, which is a bigger deal than the footer pulse, so the founding promises
("reads files, never a command, network only the opt-in engine pulse") are renegotiated here rather
than quietly bent.

**Decided: B — ccorbit is its own agent.** `ccorbit --export <ADDR>` on each host runs the *local*
reader (`src/opencode.rs`) against that box's `opencode.db` and publishes only the translated
**summary records** it already emits, plus a per-session status header, over one token-gated
`http://` endpoint. The central ccorbit (`--hosts <FILE>`) polls those and feeds the records through
the same `Session::ingest` that `--replay` uses. Bodies never leave the box: the wire carries the
grammar ccorbit already keeps in memory (`map_tool` drops `oldString`/`newString`/`content`/`prompt`;
reasoning is timing-only; text is clipped to its first line), so the one-line-summary promise holds
*structurally*, not by a filter added on the server.

**A (`opencode serve` over HTTP) was seriously reconsidered, not dismissed.** The v0.4 design note
assumed a serve "may only expose its own instances and be dead on arrival" for TUI sessions. That is
**false**, and was checked against the live box before choosing: the `opencode serve` the OTG runs on
`127.0.0.1:4097` lists the **shared** `opencode.db` — it returned the very session this decision was
written in, and the build session that wrote the design note, neither of which it owns — and its
`time.updated` matched the database byte-for-byte (live). `GET /session` is even **summary-only**
(`id, parentID, directory, title, model.id, cost, tokens{…}, time{…}, summary{additions,deletions,files}`,
default page 100, `?limit=` works; the serve listed 167 of the database's 202). A was still rejected
for two reasons that survive that finding:

1. **A fidelity/privacy fork.** The body-free `/session` list can drive only the wall's headline
   numbers. The session tree, the brainwave and the code city need `GET /session/:id/message`, which
   returns message parts verbatim — the `text` prompt, the `reasoning` chain of thought, and tool
   inputs. So A either ships bodies across the LAN (breaking the summary-only promise on the main
   feature) or shows a wall of cards that cannot open.
2. **A needs a second source-side translator.** The serve hands back message-shaped `{info, parts[]}`,
   not the `part` rows with `state.status` transitions that every hard-won quirk in `opencode.rs` is
   written against (in-place tool-row updates, empty-input first sight, `metadata.exit`, a question
   staying `running`, children by `parent_id`/`state.metadata.sessionId`). A would reuse the *model*,
   not the ~1000-line reader — a second source to build, test and keep in step, for less fidelity.

**Rejected outright:** **C — ssh + remote `sqlite3`.** The program never runs a command; an ssh
client is a command, and a remote shell is the widest surface this tool could open. **D — an
NFS/SMB-mounted `opencode.db`.** SQLite WAL over a network filesystem is a corruption footgun and a
reader can see torn state; the read-only promise depends on WAL behaving, which it does not across a
mount. **E — hybrid (B everywhere, A opportunistically where a public serve exists).** Rejected for
v0.4: two transport paths mean two test suites and two failure modes for one feature. The verified
serve behaviour above is recorded (OPENCODE_FORMAT.md) so E can be revisited honestly later if a body-
free serve message API ever appears.

**Design choices that defuse the traps:**
- **Liveness is decided on the host, the verdict shipped.** BLIP/DEAD/ACTIVE_WINDOW run where the
  database and the correct clock are; the central only displays `status`/`alive`/`archived`/`last_act`.
  NTP-skewed boxes can no longer flip busy/idle — skew then only nudges an "x ago" label. The envelope
  carries the host's `now_ms`; the central derives `offset = host_now − arrival_local` and shifts
  record timestamps before ingest.
- **Identity is host-qualified at the source** (`host:ses_…`). Because the HUD already keys nodes on
  `s.id` and the wall keys `toast_seen`/`waiting`/`warned`/`compacted`/`heights`/selection on `s.id`,
  qualifying it once in the poller makes all of them correct for free. Cards and titles gain a host
  chip; `short_tag()` is not a sufficient disambiguator (77 sessions share `/home/clement` on one box,
  and three boxes share a username).
- **File and collision keys are host-qualified.** `insight::collisions` and the code city group by the
  raw `path`; two hosts editing `/home/clement/app.ts` are different files and must not read as a
  collision or merge into one building.
- **A dead host degrades alone.** Reuse the shared `net::http_get` (700 ms connect / 1.5 s read),
  back off on failure (1→2→5→10 s), mark the host `stale` with its age, drop its cards after a policy
  timeout; never block the other hosts or the local source. `.49` boots Windows and goes absent; this
  is the normal case the UI must absorb.
- **Realtime.** The exporter inherits the reader's 1 s database tick; the central polls each host every
  1 s (the footer pulse's 2 s cadence is the throttle on failure, not on the happy path). The 3 s
  streamed-text settle (`STREAM_STABLE`) is the floor — identical to local — so remote matches local
  plus one poll and one LAN hop. A push channel (long-poll/SSE) is deliberately deferred to keep one
  protocol for v0.4.
- **Version skew is loud, not silent.** The envelope carries a wire `format` version *and* the host's
  opencode version; the central refuses a mismatch rather than mis-parsing. This matters for `.51`,
  which runs **opencode v2** (a different major with no proven-read schema): it is deferred out of
  v0.4.0 rather than half-supported.

**The promise, restated honestly.** ccorbit still never writes a file and never runs a command. It now
opens a socket for **three** opt-in surfaces, but *all* transport code — the HTTP client and the export
listener alike — is confined to a **single** file, `src/net.rs`, so the promise stays checkable line by
line and by file. The shared client `fn http_get` backs both `--engines` and the `--hosts` poller
(`src/engines.rs` and `src/hosts.rs` hold no socket themselves; they call `net::http_get`), and
`fn serve` is the `TcpListener` the `--export` mode runs through (the export *logic* stays in
`src/opencode.rs`). `tests/rules.rs` allows a `harness: allow network` marker in `src/net.rs` and
nowhere else. Without `--hosts`/`--export` ccorbit is exactly the v0.3.0 tool. `http://` only, on a
LAN-trust assumption stated like the engine pulse's; one shared key file
(`~/.config/ccorbit/host-key`) gates the export endpoint. No new dependency: the hand-rolled transport is
shared and std-only, so the binary stays on `ratatui`, `serde_json`, `rusqlite`. The "about 3 MB / 8 ms
frame" claims are re-measured under the extra hosts rather than inherited, and the per-host session cap
is lowered if they slip.

## Read-only, by design (2026-10-16)

ccorbit reads the files opencode already writes: no hook, no settings change, no command, no
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

## Jumping to a tab was opt-in (2026-10-17), removed by the fork (2026-10-19)

Upstream's `o` read a session process's environment for Warp's `WARP_FOCUS_URL` and ran
`/usr/bin/open` with it, without a shell: the one feature that ran a command, kept honest by
`--allow-jump` and a filter that held only two environment variables. The fork removed it
outright: opencode sessions share one server process, so there is no per-session pid to find a
tab by, and the flag had become a promise it could never keep. The command rule now has no
exception, and `tests/rules.rs` checks it without one.

## Windows: tested by CI, not yet on real sessions (2026-10-17)

The Windows build reads opencode's storage on Windows and paths with `\`. CI builds and tests it,
but it has not been run against real opencode sessions on Windows; the README says so. (An earlier
upstream version used the Windows process API to find the owning session by pid under `%USERPROFILE%`;
that went away with the process-liveness check — see the entry above, and the live source now being
opencode's database.)

## Golden screens: exact text, tolerant dots (2026-10-17)

The screens are compared with copies in `tests/golden/` at a frozen moment. Text must match
exactly. Braille dots may move in up to 2% of their cells, because `sin` and `cos` are not
bit-identical across systems (macOS, Linux, Windows); a dot that turns into text is still a
failure. The first golden copies caught two busy sessions with identical brainwaves.

## License: MIT OR Apache-2.0 (2026-10-17)

The Rust community's usual pair: people pick either; Apache-2.0 adds a patent grant.
