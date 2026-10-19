# What ccorbit reads from opencode

None of this is a documented interface: it is what opencode 1.19 writes, observed in the live
storage database. When a reading looks wrong, query the file first
(`sqlite3 "file:~/.local/share/opencode/opencode.db?mode=ro" 'select data from part where ...'`).
The database is opened `READ_ONLY`; WAL lets ccorbit read while opencode writes.

All rows ccorbit reads carry `rowid`, `time_created` and `time_updated` (milliseconds). A tool row
is **updated in place** as its state advances (`pending` → `running` → `completed`/`error`), so
`time_updated` is a completion time and rows must be re-checked, not only read once. A streamed
`reasoning` or `text` row keeps growing while the model talks; ccorbit reads it once its
`time_updated` has been quiet for `STREAM_STABLE`, which is exact to within a second.

## `session`

| Column | Used for |
|---|---|
| `id`, `parent_id` | A row with no parent is a session; a row whose `parent_id` names another session is a subagent run (`task` tool), followed as an agent of the parent |
| `directory`, `title` | The card's project and name |
| `model` | JSON `{"id","providerID",...}`: the model chip and the compaction ceiling's key |
| `time_updated`, `time_archived` | Liveness (a session quiet for 12 h drops off the wall); archived sessions are never shown |

## `message`

`data` is JSON: `role` (`user`/`assistant`), `modelID`, `providerID`, `cost`, `tokens`,
`time.created` and `time.completed`. The role tells a text row whether it is a prompt or an answer;
an assistant message with no `time.completed` is the session being busy.

## `part`

One row per piece, `data.type` decides the meaning:

| `type` | Fields used | Becomes |
|---|---|---|
| `tool` | `tool`, `callID`, `state.status`, `state.input`, `state.output` (or `state.metadata.output`), `state.metadata.sessionId` (task), `state.metadata.model` | A `tool_use` record when first seen, a `tool_result` record when it reaches `completed`/`error` |
| `step-finish` | `tokens.{input,output,reasoning}`, `tokens.cache.{read,write}`, `cost`, `reason` | One `usage` record per step; `cost` travels as `usage.ccorbit_cost_micros` for models `price.rs` does not know (every local one) |
| `reasoning` | its timing only — the text is dropped | A `thinking` block (its size is never shown; the trace only knows the time) |
| `text` | `text`, first line only | A prompt (`user` role) or the session's `last_text` (`assistant` role) |
| `compaction` | `auto` | A `compact_boundary`; `preTokens` is the last `step-finish` total |
| `patch`, `file`, `step-start` | not read | — (files come from `edit`/`write` inputs; busyness from the open message) |

### Tool names

`bash read write edit glob grep webfetch task skill question todowrite invalid` are the built-ins,
translated to the transcript grammar's names (`read`→`Read`, `task`→`Task`, `question`→
`AskUserQuestion`, ...). Any other name that starts with a `mcp` server name from
`~/.config/opencode/opencode.json` (`{server}_{tool}`) becomes `mcp__{server}__{tool}`, so the
MCP crown and server labels work as they did.

### What is deliberately never kept

`state.input.oldString`/`newString`/`content` (edit bodies), `reasoning.text` (the chain of
thought), `task`'s `prompt`, and any `state.output` longer than its first line. Edits reach the
code city as counts of added and removed lines (a line-multiset delta of old and new), and tool
results as one clipped line, exactly as the transcript format promises.

## Liveness

opencode leaves no process identity behind, so ccorbit decides alone:

- working = rows of the session (or any of its children) moved in the last 15 s, or the last
  message is an assistant without `time.completed` and rows moved in the last 10 min;
- a tool row is created empty (`pending`, `input: {}`): its command/path arrives in a later
  update, so ccorbit refreshes the step (`Session::set_step_input`) whenever the row moves
  without reaching its end — new lines fill in as the input lands;
- a `running` tool whose message completed, or an open turn silent past 10 min, is a crashed
  process: the calls close at the turn's end with "no result: the turn ended without it", and
  the card reads idle;
- closing a chat writes nothing: an idle session drops off the wall after an hour of silence and
  returns on activity;
- rows frozen longer than `ACTIVE_WINDOW` never even load, so old sessions cost nothing.

## Observed facts

- opencode reports `cache.read`/`cache.write` as 0 for a local OpenAI-compatible engine: the
  engine's own counters (the `--engines` pulse) are the truth about prefix-cache hits.
- A child session appears in `session` a moment after the `task` row that launched it; the link is
  `state.metadata.sessionId`, and the child's own `model` may name a different lane.
- The `question` tool's row stays `running` while the person decides: an open `AskUserQuestion`
  step is the `NEEDS YOU` signal.
- A dismissed question arrives as `status: "error"`; it counts as a failed step, not a hang.
