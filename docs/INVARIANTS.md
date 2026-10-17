# Invariants

What ccorbit promises, why, and what checks it. A promise without a check is marked **review**:
it is kept by reading the change. When one of those breaks once, give it a check.

| Promise | Why | Checked by |
|---|---|---|
| The program never writes a file. Developer outputs (`--snapshot-html`) are the exception, each marked `// harness: allow write`. | People run it on their machine next to their work; it must be safe to leave running. | `tests/rules.rs`: `the_program_never_writes_files` |
| It never runs a command, except `open` in `src/jump.rs` behind `--allow-jump`. | Same; and the one exception is opt-in and explained in the README. | `tests/rules.rs`: `only_jumping_to_a_session_runs_a_command` |
| It never opens a network connection. | Everything it shows is in local files. | `tests/rules.rs`: `the_program_never_opens_a_network_connection` |
| It never opens the `.key` files next to `sessions/*.json`. | They are not ours to read. | **review**: `scan_registry` in `src/source.rs` reads `.json` files only |
| It keeps summaries, counts and times, never message bodies, file contents or edited text. | Transcripts hold people's work. | **review**, and the model tests: `FileStat` holds counts only; texts are cut to their first line |
| `--allow-jump` reads two variables of the session's process environment (`WARP_FOCUS_URL`, `__CFBundleIdentifier`) and drops the rest at once. | The environment can hold secrets. | **review**: the filter in `environ` in `src/jump.rs`; its test reads its own process |
| The data layer does not draw. | One-way flow keeps parsing testable without a terminal. | `tests/rules.rs`: `the_data_layer_knows_nothing_of_the_screen` |
| Two runtime dependencies: `ratatui` and `serde_json`. | Small binary (about 1.2 MB), quick build. | `tests/rules.rs`: `dependencies_stay_few` |
| A hologram frame takes well under its budget (8 ms on average at 250×70). | 30 frames a second with room for the terminal, about 3% CPU. | `scripts/verify.sh`: frame time |
| The release binary stays under 2 MB. | Footprint. | `scripts/verify.sh`: binary size |
| Every screen draws at any size from 30×10 up, without a panic or an overflow. | Terminals come in every size. | `src/ui.rs`: `every_screen_survives_small_and_odd_terminals` (debug build) |
| Screens and readings change only on purpose. | A screen is the product. | `src/ui.rs`: `screens_match_their_golden_copies`, `readings_match_their_golden_copy` |
| `--help`, the README, the module maps and the changelog say what the code does. | Agents and people work from the docs. | `tests/rules.rs`: the three docs rules |
| Formatting and lints are clean. | Diffs stay about the change. | `cargo fmt --check`, `cargo clippy -- -D warnings` |
