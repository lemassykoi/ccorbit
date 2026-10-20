# Invariants

What ccorbit promises, why, and what checks it. A promise without a check is marked **review**:
it is kept by reading the change. When one of those breaks once, give it a check.

| Promise | Why | Checked by |
|---|---|---|
| The program never writes a file. Developer outputs (`--snapshot-html`) are the exception, each marked `// harness: allow write`. | People run it on their machine next to their work; it must be safe to leave running. | `tests/rules.rs`: `the_program_never_writes_files` |
| It never runs a command. (Upstream's `--allow-jump` was dropped in the fork: opencode sessions share one server process, so there is no per-session pid to jump to.) | Same. | `tests/rules.rs`: `the_program_never_runs_a_command` |
| It opens no network connection, except three **opt-in** surfaces: the engine pulse (`--engines`), the remote-hosts poller (`--hosts`), and the summaries exporter (`--export`). *All* transport code — the HTTP client and the export `TcpListener` — is confined to one file, `src/net.rs`; `src/engines.rs`, `src/hosts.rs` and `src/opencode.rs` call it and hold no socket themselves. | Everything it shows is read from local files; each socket is a deliberate, `http://`-only exception off unless its flag is passed. The exporter ships only the body-free summary records ccorbit already keeps in memory, so the summary-only promise holds on the wire too (see DECISIONS.md, "Remote opencode hosts"). | `tests/rules.rs`: `the_program_never_opens_a_network_connection` (a `harness: allow network` marker is honored only in `src/net.rs`) |
| The live source opens opencode's SQLite storage `READ_ONLY`; it never takes a write lock and the WAL lets it read while opencode writes. | The database is opencode's; ccorbit must not disturb the session it is watching. | **review**: `Open::connect` in `src/opencode.rs` passes only `SQLITE_OPEN_READ_ONLY` |
| It keeps summaries, counts and times, never message bodies, reasoning text, edited text or file contents — and the same holds on the `--export` wire, not just in memory. | The rows hold people's work. | **review**, and the model and source tests: `FileStat` holds counts only; `map_tool` drops `oldString`, `newString`, `content`, `prompt`; texts are cut to their first line. `src/opencode.rs`: `bodies_never_cross_the_wire` seeds a database with sentinels and asserts they never appear in the serialized export |
| The data layer does not draw. | One-way flow keeps parsing testable without a terminal. | `tests/rules.rs`: `the_data_layer_knows_nothing_of_the_screen` |
| Three runtime dependencies: `ratatui`, `serde_json`, `rusqlite` (bundled SQLite). | A single static binary (about 3.3 MB), quick build. | `tests/rules.rs`: `dependencies_stay_few` |
| A hologram frame takes well under its budget (8 ms on average at 250×70). | 30 frames a second with room for the terminal, about 3% CPU. | `scripts/verify.sh`: frame time |
| The release binary stays under 3.8 MB. | Footprint. | `scripts/verify.sh`: binary size |
| Every screen draws at any size from 30×10 up, without a panic or an overflow. | Terminals come in every size. | `src/ui.rs`: `every_screen_survives_small_and_odd_terminals` (debug build) |
| Screens and readings change only on purpose. | A screen is the product. | `src/ui.rs`: `screens_match_their_golden_copies`, `readings_match_their_golden_copy` |
| `--help`, the README, the module maps and the changelog say what the code does. | Agents and people work from the docs. | `tests/rules.rs`: the three docs rules |
| Formatting and lints are clean. | Diffs stay about the change. | `cargo fmt --check`, `cargo clippy -- -D warnings` |
