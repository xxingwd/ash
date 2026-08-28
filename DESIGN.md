# Ash Design

Ash is an embeddable agent runtime. Product adapters such as the CLI and TUI own assembly and
presentation; the workspace crates own reusable execution semantics. The design favors one owner per
piece of state and concrete boundaries over unused extension points.

## Crate boundaries

| Crate | Responsibility |
| --- | --- |
| `ash-core` | Provider-neutral messages, model streams, events, usage, tools, errors, and IDs |
| `ash-protocol` | Translation between provider wire formats and `ash-core` |
| `ash-tools` | Filesystem, search, shell, editing, and web tools |
| `ash-agent` | Agent definition, model loop, context planning, sessions, and JSONL persistence |
| `ash-collab` | Optional named child-agent communication |
| `ash-tui` | Inline terminal projection and interaction |
| `ash-cli` | Provider, prompt, tool, collaboration, and UI assembly |

Protocol translation stays in `ash-protocol`; terminal state never enters `ash-agent`; collaboration
is installed by an embedding application rather than built into the runtime.

## Agent and runtime

`Agent` is the immutable behavior shared by every session that runs it. It owns the model ID, system
prompt, tools, context limit, and tool timeout. Retry constants remain private engine details. There
is no second session configuration that mirrors these fields.

`Runtime` owns exactly two shared effectful dependencies: a model client and a concrete
`JsonlSessionStore`. Its construction paths are:

```text
start(agent)                    -> new root session
start_child(agent, parent)      -> new child session
resume(agent, session_id)       -> existing root session
```

All three use the same JSONL store. Working directory and protocol labels belong to application
assembly and do not enter runtime state.

## Session and turn

`Session` is the concurrent conversation boundary. Its actor owns the append-only log, active turn,
and FIFO queue of submitted turns. `Session::submit` is the only input path; collaboration and CLI
inputs use it alike. Rollback, fork, compaction, and other projection mutations run only while idle.

Each `Turn` has a typed ID, cancellation token, and one completion receiver. `Turn::wait` returns the
canonical `TurnView`; the same view is emitted as `SessionEventKind::TurnCompleted`. Cancellation
targets that turn and does not introduce a second input or steering channel.

`SessionIdentity` contains only `id`, `root_id`, and `parent_id`. A root has `id == root_id` and no
parent; a child receives a fresh ID, preserves the root ID, and records its immediate parent. Names
used by collaboration are routing keys, not durable session identity.

## Log and persistence

`SessionLog` is the semantic source for visible history, compacted model context, settled turn views,
and settled usage. Its entries cover turn start, user/model/tool-result messages,
context checkpoints, turn end, compaction usage, and rollback. Live stream deltas are never
persisted. An open turn found during replay is projected as interrupted, and its partial response is
not exposed as normal history.

`JsonlSessionStore` is the only persistence implementation; there is no storage trait or child-only
strategy. A session lazily acquires one exclusive `SessionWriter` on its first append and reuses it for
later commits. Child sessions are durable under the same rules as roots and forks.

The filename is `<session-id>.jsonl`. The strict format-4 header stores format version, creation time,
session ID, root ID, parent ID, and optional title. It never stores model settings, secrets, working
directory, collaboration names, or prompts. Older formats are rejected rather than migrated.
`list_roots` excludes children, while tree listing and deletion use `root_id` to include all durable
descendants. Only root sessions can be resumed through the root resume path.

## Model loop and context

The engine owns all model and tool IO. One turn repeatedly performs:

```text
pure context plan -> optional compaction call -> model stream -> ordered tool batch -> repeat
```

After a tool batch completes, the engine prepares context and calls the model again until the model
stops or the turn is cancelled. Safe retries remain part of the same model request.

Context planning is synchronous and pure: it prunes eligible old tool output, estimates the complete
outbound request, decides whether compaction is needed, builds a plan, and applies a returned summary.
It has no model client, cancellation token, event sender, async trait, or ephemeral message channel.

At 80% of the configured limit, the planner summarizes older history while retaining recent complete
turns. Full visible history stays in the log; only model context receives a checkpoint. Automatic and
manual compaction use the engine's normal stream collector and accept only non-empty text ending in a
clean `EndTurn`, with no tool call.

Before every model call within a turn, the engine locally estimates the complete outbound input. That
estimate is used only for context occupancy and the compaction decision; it never enters usage.

The stream collector reconciles split or cumulative `ModelEvent::Usage` reports for one request, then
the turn runner adds those request totals across all model calls in the turn. Receipt of usage updates
live progress immediately. Tool-call count increases when an accepted call batch starts execution.
Input/output usage and TPS therefore use provider-reported tokens, while context occupancy remains a
local estimate. Missing provider usage contributes zero rather than falling back to an estimate.
`TurnEnd` persists the completed turn aggregate in JSONL; `CompactionUsage` does the same for a manual
compaction outside a turn.

## Tool execution

Tools receive only execution facts:

```rust
pub struct ToolContext {
    pub identity: SessionIdentity,
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
}
```

They do not receive turn IDs or conversation history. Tool calls from one provider response execute
concurrently, while ordered buffering preserves provider order when results enter model context. The
agent's tool timeout is applied at this boundary unless a tool explicitly disables it; cancellation
always remains effective.

## Collaboration

`ash-collab` is an optional communication layer exposing five tools:

- `agent`: create a named child, submit its first message, and immediately return its turn ID;
- `message_agent`: submit a FIFO follow-up and immediately return its turn ID;
- `wait_agent`: consume available child completions, waiting when work is still pending;
- `list_agents`: read active child state and current session usage;
- `remove_agent`: remove an idle child after all of its results have been consumed.

The controller partitions children by root session ID and addresses active entries by a trimmed name.
Names must be non-empty, contain no control characters, and contain at most 64 Unicode characters.
Only an active duplicate is rejected; removal makes the name immediately reusable.

Each `AgentEntry` owns its `Session`, pending turn count, and unread completions. A short-lived task
waits for each submitted turn, forwards its terminal result to that entry, and wakes `wait_agent`.
A wait drains all unread entries visible under the state lock. It has no queue argument or internal
timeout; cancelling a wait neither consumes results nor cancels child work.

Children use the clean base `Agent`, so they keep their own durable multi-turn history but do not
inherit collaboration tools. Profiles, prompt additions, tool selection, and delegation policy are
application concerns. The CLI adds its orchestration prompt only after installing the communication
tools. Collaboration does not own a second scheduler, execution state machine, completion queue,
usage accumulator, or cancellation registry.

## Events and TUI

`SessionEventKind::Live` carries ephemeral text, reasoning, and tool previews. `TurnStarted`,
`TurnProgress`, `ContextChanged`, `TurnCompleted`, and `ContextCompacted` expose execution boundaries
and replaceable projections. Child-agent snapshots subscribe to the same session stats projection, so
their running usage and tool counts update with the root TUI. Only durable log entries survive resume.

The TUI replaces live preview content with the canonical `TurnView` after completion. Working and
settled usage and child rows display protocol totals; context occupancy displays the local preflight
estimate.

## Stream integrity, retry, and cancellation

A clean model response is accepted only after a semantic provider terminal marker. EOF without one is
truncated, even if partial text or tool-call fragments arrived. A truncated response never executes
tool calls. After retries are exhausted, accumulated text and reasoning are persisted with
`StopReason::Truncated` so the next turn retains that context; tool-call blocks are discarded.

Retry is limited to retryable transport/status failures and truncated streams before any tool side
effect. The engine discards earlier attempts before retrying, retains any usage reported by those
attempts, and uses cancellation-aware exponential backoff. Once a tool has started, retry cannot
repeat that side effect. Cancellation and failure settle through the same turn boundary as normal
completion.
