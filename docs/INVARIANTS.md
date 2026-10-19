# Invariants

What ccorbit promises, why, and what checks it. A promise without a check is marked **review**:
it is kept by reading the change. When one of those breaks once, give it a check.

| Promise | Why | Checked by |
|---|---|---|
| The program never writes a file. Developer outputs (`--snapshot-html`) are the exception, each marked `// harness: allow write`. | People run it on their machine next to their work; it must be safe to leave running. | `tests/rules.rs`: `the_program_never_writes_files` |
| It never runs a command. (Upstream's `--allow-jump` was dropped in the fork: opencode sessions share one server process, so there is no per-session pid to jump to.) | Same. | `tests/rules.rs`: `the_program_never_runs_a_command` |
| It opens no network connection, except the read-only engine pulse, which is off unless `--engines` lists the addresses. Only `src/engines.rs`, on its marked lines, may hold a socket. | Everything it shows is read from local files; the pulse is a deliberate, opt-in, http-only exception. | `tests/rules.rs`: `the_program_never_opens_a_network_connection` |
| The live source opens opencode's SQLite storage `READ_ONLY`; it never takes a write lock and the WAL lets it read while opencode writes. | The database is opencode's; ccorbit must not disturb the session it is watching. | **review**: `Open::connect` in `src/opencode.rs` passes only `SQLITE_OPEN_READ_ONLY` |
| It keeps summaries, counts and times, never message bodies, reasoning text, edited text or file contents. | The rows hold people's work. | **review**, and the model and source tests: `FileStat` holds counts only; `map_tool` drops `oldString`, `newString`, `content`, `prompt`; texts are cut to their first line |
| The data layer does not draw. | One-way flow keeps parsing testable without a terminal. | `tests/rules.rs`: `the_data_layer_knows_nothing_of_the_screen` |
| Three runtime dependencies: `ratatui`, `serde_json`, `rusqlite` (bundled SQLite). | A single static binary (about 3.3 MB), quick build. | `tests/rules.rs`: `dependencies_stay_few` |
| A hologram frame takes well under its budget (8 ms on average at 250×70). | 30 frames a second with room for the terminal, about 3% CPU. | `scripts/verify.sh`: frame time |
| The release binary stays under 3.8 MB. | Footprint. | `scripts/verify.sh`: binary size |
| Every screen draws at any size from 30×10 up, without a panic or an overflow. | Terminals come in every size. | `src/ui.rs`: `every_screen_survives_small_and_odd_terminals` (debug build) |
| Screens and readings change only on purpose. | A screen is the product. | `src/ui.rs`: `screens_match_their_golden_copies`, `readings_match_their_golden_copy` |
| `--help`, the README, the module maps and the changelog say what the code does. | Agents and people work from the docs. | `tests/rules.rs`: the three docs rules |
| Formatting and lints are clean. | Diffs stay about the change. | `cargo fmt --check`, `cargo clippy -- -D warnings` |
