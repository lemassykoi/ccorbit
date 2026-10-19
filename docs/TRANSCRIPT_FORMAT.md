# The record format ccorbit reads (origin: Claude Code)

`Session::ingest` speaks one record grammar. `--replay` plays transcript files and `--demo`
fabricates them directly in this grammar; the live source (`src/opencode.rs`) translates opencode
rows into these same records (see [OPENCODE_FORMAT.md](OPENCODE_FORMAT.md)). The grammar is kept
verbatim from the upstream view of the Claude Code format, which `--replay` still reads.

None of this is a documented interface: it is what Claude Code 2.1 writes, observed in real
files. When a reading looks wrong, check the files first (`--dump` prints what ccorbit understood).
All paths are under `~/.claude` (`$CLAUDE_CONFIG_DIR` when set; `%USERPROFILE%\.claude` on Windows),
for `--replay`; the live source ignores paths and reads the tables in `OPENCODE_FORMAT.md`.

## The registry: `sessions/<pid>.json`

One file per running Claude Code process: `pid`, `sessionId`, `cwd`, `name` (`projects-63`),
`status` (`idle`, `busy`, `shell`, a value containing `wait` when it waits for the person),
`startedAt` (ms). A session is running while its process exists. The `.key` files beside them are
never opened.

## Transcripts: `projects/<folder>/<sessionId>.jsonl`

`<folder>` is the session's working directory with every character other than letters and digits
turned into `-`; when the guess fails, every project folder is searched for the session id. One
JSON record per line, appended as the session works. Fields used:

| Record | Fields used |
|---|---|
| every record | `type`, `timestamp` (ISO 8601, UTC), `isSidechain` (in the main transcript: a subagent's line, skipped), `isMeta` |
| `assistant` | `message.id`, `message.model` (`<synthetic>` is ignored), `message.stop_reason` (`end_turn` ends a subagent), `message.usage`, `message.content[]` |
| `user` | `message.content` (a prompt string, or blocks: `tool_result` with `tool_use_id`, `is_error`, `content`), `toolUseResult` |
| `system` with `subtype: compact_boundary` | `compactMetadata.trigger` (`auto` or `manual`), `compactMetadata.preTokens` |
| `queue-operation`, `attachment` | `content` or `attachment.prompt` holding `<task-notification>` blocks: `<task-id>`, `<tool-use-id>`, `<status>` |
| `ai-title`, `agent-name` | `aiTitle`, `agentName`: the session's title |

**One API call is split over several `assistant` records**, one per content block, each repeating
the same `message.id` and the same final `usage`. Count usage once per `message.id`.

**Each record is written when its block is complete**: a thinking block when the thinking ends, a
tool call when its input is written, a tool result when the tool returns. The gaps between records
give the brainwave (thinking, writing, tools).

`usage` fields: `input_tokens` (fresh), `cache_creation_input_tokens` (cache writes; the split
`cache_creation.ephemeral_1h_input_tokens` / `ephemeral_5m_input_tokens` sets the price),
`cache_read_input_tokens`, `output_tokens`, `output_tokens_details.thinking_tokens`, `speed`
(`fast` doubles the price). There is no price in the transcript.

Content blocks: `thinking` (its text is empty in the file: only its size and timing are known),
`text`, `tool_use` (`id`, `name`, `input`), `server_tool_use`, and `*_tool_result` blocks of server
tools.

`toolUseResult` (on the `user` record carrying a tool result):

| Tool | Fields used |
|---|---|
| `Agent` / `Task` launched in the background | `isAsync`, `agentId` |
| `Bash` in the background | `backgroundTaskId` (or the text `running in background with ID:`) |
| `Workflow` | `runId`, `taskId`, `workflowName` (and the text `Task ID:`) |
| `Edit`, `MultiEdit`, `Write` | `structuredPatch[].lines[]` (counted by their `+` and `-` prefixes, never kept), `type` (`create`), `content` (only its line count) |

## Subagents: `projects/<folder>/<sessionId>/subagents/`

- `agent-<id>.jsonl`: the subagent's transcript, same format.
- `agent-<id>.meta.json`: `agentType`, `description`, `toolUseId` (the call that started it).
- `workflows/<runId>/agent-<id>.jsonl` and `.meta.json`: the agents of a workflow run, written
  while the run works; the meta has `agentType: workflow-subagent`, `description` (the label) and
  `workflowPhase` (the phase title). Runs already finished when ccorbit starts are not read (they
  can weigh hundreds of megabytes); their record is.

## Workflow runs: `projects/<folder>/<sessionId>/workflows/`

- `wf_<runId>.json`, written when the run ends: `runId`, `taskId`, `workflowName`, `status`,
  `startTime`, `durationMs`, `totalTokens`, and `workflowProgress[]`: `workflow_phase` (`index`,
  `title`) and `workflow_agent` (`agentId`, `label`, `phaseIndex`, `state`, `startedAt`,
  `durationMs`, `model`, `lastToolName`, `lastToolSummary`).
- `scripts/<name>-<runId>.js`: the script, written at launch. Its `export const meta` literal gives
  the name and the phases (see `parse::workflow_meta`).

## Observed facts

- Automatic compaction of `claude-opus-5-5` tops out near 968k tokens (a 1M window), but it often
  fires earlier, at sizes with no visible pattern (204k to 738k). Hence the ceiling reading uses
  the largest compaction seen, never a fixed number.
- A session started in another folder than its transcript's is found by its id.
