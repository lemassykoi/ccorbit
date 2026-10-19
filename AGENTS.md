# Working on ccorbit

ccorbit is a terminal view of every opencode session on a machine: a wall of cards, a session
tree, a 3D hologram and a code city, drawn from the rows opencode writes to its storage database.
Rust, three dependencies, read-only. This file is the map; the details live in `docs/`.

## Promises you must not break

- The program never writes a file, never runs a command (except `open` behind `--allow-jump`); it
  opens a socket only for the opt-in `--engines` read-only pulse, in `src/engines.rs`.
- The live source opens opencode's database `READ_ONLY` and never takes a write lock.
- It keeps summaries, counts and times, never message bodies, reasoning text, file contents or edited text.
- The data layer (`source`, `model`, `opencode`, `engines`, `parse`, `insight`, ...) never draws; `ui` and `hud` do.
- Three runtime dependencies; a frame of the hologram well under 8 ms; a binary under 3.8 MB.

Each promise and the check that enforces it: [docs/INVARIANTS.md](docs/INVARIANTS.md).

## Where things are

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): the data flow and every module.
- [docs/OPENCODE_FORMAT.md](docs/OPENCODE_FORMAT.md): the storage tables and row fields the live
  source reads. Undocumented upstream; check real rows before relying on a new field.
- [docs/TRANSCRIPT_FORMAT.md](docs/TRANSCRIPT_FORMAT.md): the internal record format that
  `Session::ingest` parses. The opencode source translates rows into it; `--replay` and `--demo`
  feed it directly. It is the one grammar every screen is built on.
- [docs/DECISIONS.md](docs/DECISIONS.md): why things are this way, and what was tried and
  rejected (pixel mode, early warning thresholds, ...). Read it before proposing a change in the
  same area.
- [docs/VISUAL_QA.md](docs/VISUAL_QA.md): how to look at a screen and judge it.

## Commands

| Command | When |
|---|---|
| `scripts/verify.sh` | Before every commit. Formatting, lints, every test (rules and golden screens included), the build, and the frame and size budgets. CI runs the same script. |
| `cargo test` | While working. |
| `UPDATE_GOLDEN=1 cargo test` | Only when a screen or a reading changed on purpose; then read `git diff tests/golden`. |
| `cargo run --release -- --demo` | To see every screen with made-up sessions. |
| `cargo run --release -- --dump` | To see what ccorbit understood of the real sessions. |
| `cargo run --release -- --demo --bench-hud 250x70` | To time hologram frames. |

On a Mac whose Command Line Tools linker is older than its SDK (`tapi error ... arm64e`), prefix
builds with `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.sdk`.

## How to work

1. Read the map above, then the code you will change and the code around it.
2. Make the change small and match the code around it: names, comment density, idioms. Comments say
   why, not what.
3. Run `scripts/verify.sh`. A failing check says what is wrong and how to fix it: follow it rather
   than silencing it.
4. A screen changed? Look at it (golden diff, `--demo`), and update the golden copies only if the
   change is the one you meant.
5. Update the docs in the same change: README (features, options, keys), CHANGELOG (Unreleased),
   and `docs/` when you change a promise, the format you rely on, or a decision.
6. The commit message says why.

When a mistake happens twice, do not just fix it again: add a check (`tests/rules.rs`, a test, a
budget in `scripts/verify.sh`) or a line in these docs so it cannot happen a third time.

## Never

- Stop processes by name: people run their own copy of ccorbit. Record the exact PID of what you
  start and stop only that.
- Publish a release (a tag, crates.io, the Homebrew tap) without the owner's go: a crates.io
  version can never be taken back. The steps are in the README, under Releasing.
- Add a dependency, a write, a command or a network call to get around a limit; say what is
  missing instead.
- Commit secrets or real session content: tests and screenshots use `--demo`.

English in code, comments and docs.
