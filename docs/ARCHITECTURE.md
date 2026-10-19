# Architecture

ccorbit turns the rows opencode writes into screens. Data flows one way:

```
opencode.db ──▶ source ──▶ model (Session::ingest, one record at a time) ──▶ insight ──▶ ui / hud
 (read-only)     opencode     steps, agents, tokens, phases,                   readings     draw frames
                 translator,  files, compactions, workflows                    across       from the
                 replay,      (translate rows to records; replay               sessions     state
                 demo         and demo feed records directly)
engine urls ──▶ engines ─────────────────────────────────────────────────────────────────▶ footer
 (opt-in)       read-only pulse
- **model** reads records into state. It knows the transcript format (see
  [TRANSCRIPT_FORMAT.md](TRANSCRIPT_FORMAT.md)), nothing about screens.
- **insight** computes readings that need several sessions at once: alerts, collisions, races,
  compaction ceilings.
- **ui** and **hud** draw. They read the state and never change what was read; the only state they
  own is what is on screen (selection, camera, notices).

All of these are the data layer. They must not import `ratatui`, `crossterm`, `crate::ui` or
`crate::hud`; `tests/rules.rs` checks this.

## Modules

| File | Layer | Role |
|---|---|---|
| `src/main.rs` | entry | Options (`Opts`), startup, hidden developer options, terminal setup |
| `src/source.rs` | data | `World`: the three sources (live opencode, replay, demo) behind one `tick`, and the clock |
| `src/opencode.rs` | data | The live source: opens opencode's storage database read-only, follows rows per session, translates them into transcript records; subagent children hang under their task call |
| `src/engines.rs` | data | The opt-in engine pulse: one short http GET per listed engine per 2 s, vLLM Prometheus counters and llama.cpp-style status into footer numbers |
| `src/model.rs` | data | `Session::ingest`: steps, agents, background tasks, workflows and their live agents, tokens split by cache, phases (brainwave), files, compactions, durations by kind, price |
| `src/parse.rs` | data | Small tolerant readers: tool kinds, summaries, workflow meta, call signatures and duration keys, paths |
| `src/insight.rs` | data | Readings across sessions: slow and looping calls, collisions, workflow races, compaction ceilings and forecasts |
| `src/price.rs` | data | API list prices and the cost of one call |
| `src/time.rs` | data | Timestamps (parse and format), durations, compact numbers |
| `src/demo.rs` | data | The made-up sessions of `--demo`, generated as transcript records and fed to `ingest` |
| `src/ui.rs` | screen | The run loop, boot, header and footer, wall, skyline, feed, session view, notices, help, keys and mouse, snapshots and dump |
| `src/ui/vitals.rs` | screen | Instruments: brainwave, cache reactor, context gauge, race lanes |
| `src/ui/city.rs` | screen | The code city |
| `src/hud.rs` | screen | The hologram: layout in 3D, camera, packets, hit tests, target panel |
| `src/hud/director.rs` | screen | The director: picks shots, captions, the inset |
| `src/theme.rs` | screen | Palettes, gradients, fog, the 256-color fallback |

## The run loop

`ui::run` ticks the world every 250 ms (80 ms in replay), polls the engines (throttled to 2 s
inside), turns new events into notices, and
draws when something changed or the frame interval passed: 30 frames a second in the hologram
(`--fps`), about 11 on the other screens while something moves, 2 when nothing does. A frame of
the hologram takes about half a millisecond to compute (`--bench-hud`).

## Key types

- `Session` (model): one opencode session (or one transcript in replay, one made-up one in demo).
  `agents[0]` is the main agent; subagents (opencode child sessions) and workflow agents follow.
  Items are steps (tool calls) and prompts.
- `Step`: one tool call, from its `tool_use` to its `tool_result`, plus a background task when the
  call launched one (async agent, background shell, workflow).
- `Activity`: 64 buckets of 5 seconds, the base of every rate and trace.
- `Workflow`: a run, its phases, the agents followed live (`live`) and those read from the run
  record when it ended (`agents`).
- `World` (source): every session, the clock (`now`, frozen in tests) and finished workflow runs.
- `App` (ui), `HudState` (hud), `CityState` (ui/city): what each screen shows and where.

## Tests

- Unit tests sit next to the code they test (`#[cfg(test)] mod tests`).
- `src/ui.rs` holds the golden screens: every screen of the demo at a frozen moment compared with
  `tests/golden/`.
- `tests/rules.rs` checks the promises and the docs on the source itself.
- `scripts/verify.sh` runs all of it, plus the budgets.
