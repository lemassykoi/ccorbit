# Changelog

## 0.4.0 - 2026-10-20

- **Remote opencode hosts** (`--hosts <FILE>` + `--export <ADDR>`): one central ccorbit watches
  several machines. Each host runs `ccorbit --export <host:port>` (server mode), which reads only
  that box's `opencode.db` (read-only) and publishes the same **body-free summary operations** the
  wall keeps in memory — never message bodies, reasoning text, edited text or file contents. The
  central lists the peers in a JSON file and folds their sessions into the wall, tree, HUD and code
  city, keyed by a host-qualified id and marked with a host chip. Sessions are decided busy/idle
  **on the host** (so NTP skew cannot flip them); a host that is down degrades alone (stale marker,
  backoff, then drops), and one that disagrees on the wire or opencode version is shown as an error
  rather than misread. All transport is `http://` only through the new `src/net.rs` (the shared
  `http_get` client and the export `TcpListener`) — the engine pulse is no longer the only socket,
  but every socket still lives in that one file, which `tests/rules.rs` now enforces by file.
  Opt-in: without `--hosts`/`--export` ccorbit is exactly v0.3.0. See README "Remote hosts" and
  docs/DECISIONS.md.
- **Rebranded as an opencode monitor:** the leftover Claude-Code language is gone from the README,
  the module docs, code comments and the `--demo` sessions (its chips now show local model names,
  with the demo carrying its own cost the way the reader does). The Anthropic pricing table and the
  `claude-` / `claude_ai_` handling stay — they correctly price and parse real Anthropic models and
  MCP servers that opencode sessions may use.

## 0.3.0 - 2026-10-19

- **The live source is opencode** (fork): sessions, prompts, steps, subagents, skills, MCP
  servers, tokens, compactions and the code city are read from opencode's storage database,
  opened read-only (`$XDG_DATA_HOME/opencode/opencode.db`, or `--opencode-db`). The Claude Code
  reader is gone; `--replay` and `--demo` keep the record grammar. `rusqlite` (bundled) joins the
  two dependencies; the size budget moved to 3.8 MB.
- **The engine pulse** (`--engines <FILE>`): one read-only footer strip per listed local LLM
  engine — prefix-cache hit share, KV fill, running and waiting requests, decode tokens per
  second, mean time-to-first-token — from a vLLM Prometheus endpoint or a llama.cpp-style
  `/status`. Off unless asked for, `http://` only; the only network the tool may touch
  (`docs/DECISIONS.md`).
- **Liveness for a source without process identity**: a session is busy while its rows move (or a
  tool-free turn is open and recent), not while history says so. Tool rows created empty refresh
  their step when the input lands (new lines no longer sit blank); calls left `running` by a dead
  process close as lost; a session quiet for an hour leaves the wall and returns on activity; a
  question waiting on the person is never abandoned.
- **Price falls back to what opencode bills per step** when `price.rs` has no list (every local
  model); with neither, tokens stay unpriced as before.
- **A harness for working on ccorbit with coding agents:** `AGENTS.md` (the map) and `docs/`
  (architecture, promises and their checks, transcript format, decisions, visual QA);
  `scripts/verify.sh` runs formatting, clippy, every test and the footprint budgets, in CI too;
  golden copies of every screen and of the readings; rule tests that keep the read-only promise,
  the layering and the docs true.
- Two busy sessions no longer show the same brainwave.

## 0.2.0 - 2026-10-17

- **Jump to a session** (`--allow-jump`, then `o`): brings forward the terminal tab where the
  pointed session runs. In Warp the exact tab (its focus link); elsewhere on macOS the terminal
  app. Off by default: it runs `open` and reads the session's process environment.
- **The hologram runs at 30 frames per second**, sharp at any size, for about 3% CPU; `--fps`
  changes it (`--fps 60` costs about 5%).
- **Quieter notices:** failed shell commands no longer raise one each (the feed shows them, and
  repeated failures raise a warning); the same notice again is counted (`×4`) instead of stacking;
  notices slide in whole.
- **Windows:** a Windows build and a PowerShell installer; sessions are found under opencode's
  storage on Windows, paths with `\` are read everywhere, and Windows Terminal gets 24-bit color.
  Tested by CI, not yet against real sessions.
- Hidden `--jump-check` and `--bench-hud` options for diagnostics.

## 0.1.0 - 2026-10-17

First release.

- Wall of session cards with skyline and signal feed; session tree with vitals and the live
  agents of running workflows; 3D hologram HUD with a flat view and the director; code city.
- Readings: brainwave of thinking, writing and tool time; prompt cache hit ratio; API-equivalent
  price; context against the compaction ceiling; slow and looping calls; files changed by two
  sessions at once; workflow runs raced against earlier ones.
- `--demo` mode with made-up sessions.
- Install by script, Homebrew (`pcangemi/tap`) or Cargo; builds for macOS and Linux, arm and x86.
