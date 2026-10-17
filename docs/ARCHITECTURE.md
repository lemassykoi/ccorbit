# Architecture

ccorbit turns the files Claude Code writes into screens. Data flows one way:

```
~/.claude files ──▶ source ──▶ model (Session::ingest, one record at a time) ──▶ insight ──▶ ui / hud
   sessions/          tails, replay,      steps, agents, tokens, phases,          readings      draw frames
   projects/          demo generator      files, compactions, workflows           across        from the
                                                                                  sessions      state
```

- **source** finds sessions and follows their files. It knows paths and offsets, nothing about
  what a record means.
- **model** reads records into state. It knows the transcript format (see
  [TRANSCRIPT_FORMAT.md](TRANSCRIPT_FORMAT.md)), nothing about screens.
- **insight** computes readings that need several sessions at once: alerts, collisions, races,
  compaction ceilings.
- **ui** and **hud** draw. They read the state and never change what was read; the only state they
  own is what is on screen (selection, camera, notices).

The first three are the data layer. They must not import `ratatui`, `crossterm`, `crate::ui` or
`crate::hud`; `tests/rules.rs` checks this.

## Modules

| File | Layer | Role |
|---|---|---|
| `src/main.rs` | entry | Options (`Opts`), startup, hidden developer options, terminal setup |
| `src/source.rs` | data | `World`: live discovery (registry every 2 s, folders every 1 s, tails every tick), replay of a transcript, the demo, workflow history, process liveness |
| `src/tail.rs` | data | Follows an append-only file by byte offset, holding back a partial last line |
| `src/model.rs` | data | `Session::ingest`: steps, agents, background tasks, workflows and their live agents, tokens split by cache, phases (brainwave), files, compactions, durations by kind, price |
| `src/parse.rs` | data | Small tolerant readers: tool kinds, summaries, workflow meta, call signatures and duration keys, paths |
| `src/insight.rs` | data | Readings across sessions: slow and looping calls, collisions, workflow races, compaction ceilings and forecasts |
| `src/price.rs` | data | API list prices and the cost of one call |
| `src/time.rs` | data | Timestamps (parse and format), durations, compact numbers |
| `src/demo.rs` | data | The made-up sessions of `--demo`, generated as transcript records and fed to `ingest` |
| `src/jump.rs` | data | Opt-in: reads a session process's focus link and runs `open` (the only command ccorbit runs) |
| `src/ui.rs` | screen | The run loop, boot, header and footer, wall, skyline, feed, session view, notices, help, keys and mouse, snapshots and dump |
| `src/ui/vitals.rs` | screen | Instruments: brainwave, cache reactor, context gauge, race lanes |
| `src/ui/city.rs` | screen | The code city |
| `src/hud.rs` | screen | The hologram: layout in 3D, camera, packets, hit tests, target panel |
| `src/hud/director.rs` | screen | The director: picks shots, captions, the inset |
| `src/theme.rs` | screen | Palettes, gradients, fog, the 256-color fallback |

## The run loop

`ui::run` ticks the world every 250 ms (80 ms in replay), turns new events into notices, and
draws when something changed or the frame interval passed: 30 frames a second in the hologram
(`--fps`), about 11 on the other screens while something moves, 2 when nothing does. A frame of
the hologram takes about half a millisecond to compute (`--bench-hud`).

## Key types

- `Session` (model): one Claude Code session. `agents[0]` is the main agent; subagents and
  workflow agents follow. Items are steps (tool calls) and prompts.
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
