# Ash Architecture

Ash is an embeddable agent runtime. Product adapters such as a CLI, TUI, chat gateway,
scheduler, or service own transport and presentation; Ash owns agent execution semantics.

## Crates

| Crate | Responsibility |
| --- | --- |
| `ash-core` | Provider-neutral messages, model, tool, event, ID, error, and cancellation types |
| `ash-protocol` | Model-provider protocol adapters and streaming translation |
| `ash-tools` | Sandboxed file, process, search, and web tools |
| `ash-agent` | Agent definition, runtime, threads, turns, input, context, and persistence |
| `ash-collab` | Optional child-agent spawning, messaging, lifecycle, and collaboration tools |
| `ash-tui` | Terminal rendering and interaction only |
| `ash-cli` | Binary composition and product command routing |

Dependencies point inward. `ash-core` knows nothing about products, providers, persistence,
terminal state, or collaboration. `ash-agent` does not depend on the CLI or TUI.

## Public Model

The stable execution vocabulary is deliberately small:

- `Agent` is immutable behavior: model, prompt, tools, limits, and context policy.
- `Runtime` owns injected capabilities: model client and thread store.
- `ThreadOptions` is per-thread scope: working directory, timeout, agent path, tree ID,
  and typed `ThreadKind`.
- `Thread` is the durable concurrent conversation boundary and the sole input-queue owner.
- `Turn` is one submitted unit of execution. It can be awaited, interrupted, or steered.
- `Input` carries content, source, metadata, and an optional idempotency key.
- `Event` routes an `EventKind` with thread ID, optional turn ID, sequence, and timestamp.

`Runtime::start`, `start_with_history`, and `resume` are the only thread construction paths.
All products and child agents execute through the `Thread` input APIs.

## Thread Ownership

Each `Thread` is backed by one actor task. The actor owns:

- the ordered queue of submitted turns;
- the active turn and its cancellation state;
- the durable log and current version;
- rollback, fork, compaction, and read operations;
- monotonic event sequencing.

Products submit immediately. They may mirror pending prompts for display, but they must not
decide when queued work starts. This gives chat gateways, CLI/TUI clients, schedules,
heartbeats, and child agents identical ordering and cancellation behavior.

`Thread::submit` returns a `Turn` handle. `Thread::enqueue` creates fire-and-forget work for
external triggers. `Thread::notify` attaches a message to the next submitted turn without
starting work, which is useful for child-agent mailboxes. Schedulers and heartbeat timers remain
product infrastructure; they create typed `Input` values and call `enqueue` instead of bypassing
the thread.

## Submit And Steer

`submit` creates a new turn and appends it to the thread queue. Multiple submissions are
executed in acceptance order.

`Turn::steer` targets only its currently active turn. The input is sent through the active
execution channel, durably recorded with the same turn ID, appended to model context, and used
by the next model iteration. It is not converted into an anonymous follow-up turn. Steering an
inactive turn returns an explicit error.

## Durable Log

`ThreadLog` is the durable append-only log and the only source for full user-visible
history, compacted model context, and turn views. Its entries are:

- `TurnStart`;
- `Input`;
- model and tool-result messages;
- context checkpoints;
- `TurnEnd { result, usage }`;
- rollback markers.

A turn is the scrollback/replay boundary: `TurnEnd` carries the terminal `TurnResult`
(completed, failed, or interrupted) plus the optional aggregated `Usage`. A turn left open
when a session ends is projected as `Interrupted` and never exposed as normal history.
Live streaming deltas are never persisted.

All inputs in a submitted turn are validated before persistence. Empty input, a previously used
idempotency key, or duplicate keys within one batch reject the whole turn before `TurnStart`
is written.

`ThreadStore` is the only public persistence boundary:

```text
create(metadata, entries)
load(thread_id) -> stored thread
open(thread_id) -> stored thread + locked writer
open_writer(metadata) -> locked writer for a new thread
list(excluded_thread) -> summaries
```

Locked writers are exposed through the storage-neutral `ThreadAppender` capability. A backend may
hold a file lock, database transaction, or remote lease in that handle; runtime code never depends
on the JSONL writer type.

The default `JsonlThreadStore` stores new threads as `{thread_id}.jsonl`. Its compact first record
contains only list metadata, so listing does not replay thread logs. Loading addresses files
directly by id and only replays the selected thread.

`ThreadMetadata` carries the typed `ThreadKind` (`Root` or `Subagent`) used by persistence and
session listing. Runtime code derives that value from `ThreadOptions.kind`.

The runtime's hot write path keeps one exclusively locked `ThreadAppender` per active thread. Resume
replays the selected file through that same handle, so later appends are a single write+flush with
no second read. Writes are batched at commit points: accepted inputs are persisted when the turn
starts, and all turn messages plus `TurnEnd` are flushed together when the turn settles. A crash
before that commit point leaves a turn without `TurnEnd`, which the projection reports as
`Interrupted` and never exposes as normal history.

## Events And Projection

`EventKind` distinguishes live deltas from durable facts and derived views:

- `Live(LiveEvent)` carries ephemeral streaming deltas for the active turn's preview only.
- `Turn(TurnView)` is emitted when a turn settles and carries its canonical messages, result,
  and usage; clients use it to commit scrollback.
- `Restored(ThreadView)` / `ThreadForked` carry the full projection after resume or fork.
- `Compacted` reports a context-checkpoint change; `TurnRolledBack` reports a rollback.

All memory state is derived from the log through one reducer direction:
`LogEntry -> ThreadView -> messages / context / turns -> transcript blocks`. The TUI draws
live deltas as a preview and replaces them with the canonical projection on `Turn`. `ThreadLog`
maintains that projection incrementally as entries arrive, and thread events obtain settled turn
views from the same reducer rather than assembling a parallel view in the execution path.

## Context And Memory

`ContextPolicy` prepares model context and performs automatic compaction without deleting full
history. A checkpoint changes only the model-context projection. Manual and automatic
compaction therefore share the same durable mechanism.

Durable messages and ephemeral context remain separate throughout preparation.
Both count toward the request budget and are sent to the model, but compaction summarizes and
checkpoints only durable messages. A compaction stream must end with a clean `EndTurn`; a missing or
truncated terminal marker rejects the summary instead of persisting partial context.

Before provider-specific serialization, `ash-protocol` validates every message's role/content pair
and projects system-role text into the provider's privileged system or instructions field. This
keeps system context at system priority without changing the durable message schema; invalid
role/content combinations and system images fail locally as invalid requests.

## Collaboration

`ash-collab` is optional. Its public control vocabulary is:

- `AgentControl` for child-agent lifecycle and communication;
- `AgentSpawner` for constructing a child agent;
- `SpawnRequest` for the typed spawn contract;
- `ChildAgent` for the runtime, agent definition, options, and inherited history.

Every child gets its own `Thread` and executes through the same runtime path as a root agent.
Tree identity and canonical agent path live in `ThreadOptions` and `ToolContext`; collaboration
state does not leak into `ash-core` or terminal state.

The controller counts every accepted follow-up until it settles. A child with queued follow-ups
stays active and receives no completion revision until the final queued turn finishes, so waiters
cannot observe an intermediate result as the agent's terminal state.

## Stream Integrity And Error Handling

A model stream is only "clean" when the provider signals a terminal state:
`finish_reason` (Chat Completions), `message_stop` (Anthropic), or
`response.completed` (Responses). A stream that reaches EOF without one of
those markers is truncated, not finished.

Semantic termination is distinct from wire termination. Chat Completions continues draining after
`finish_reason` so a trailing usage chunk is preserved, then emits exactly one `Stop` as the final
model event. `[DONE]` and EOF only close the wire; neither can replace a missing semantic terminal
marker. Provider completion is also rejected while a tool call is still incomplete.

Detection is layered so no client can silently treat a cut stream as a clean
stop:

1. `ash-protocol` adapters emit `ModelEvent::Stop(StopReason::Truncated)` when
   the SSE stream ends without a terminal marker (`sse::stream` tracks whether
   the decoder ever reported `Finished`).
2. `ash-agent`'s `collect_response` applies the same rule as a fallback: a
   stream that runs out without any `Stop` event is `Truncated`, never
   `EndTurn`.

Retries follow a strict safety policy. A single model call may be re-issued up
 to `RunConfig::max_retries` times (default 5), and only when:

- the failure is transport-level and retryable: `Request` (network/EOF),
  upstream 5xx, or `RateLimited`; or the stream was `Truncated`; and
- no tool call has been executed yet in this call, so re-issuing the request
  cannot repeat side effects.

Between attempts the runner waits an exponential backoff (`RetryBackoff`,
base 1s doubling to a 10s cap, i.e. 1s, 2s, 4s, 8s, 10s), and a cancellation
during the wait aborts the turn instead of retrying.

Non-retryable failures (`Auth`, `InvalidRequest`, `ContextTooLong`,
`InvalidResponse`, upstream 4xx) fail the turn immediately and surface as
`TurnResult::Failed`. On retry, the partial assistant message is discarded both
from memory and from the staged log (`ThreadPersistence::rollback_to`), so a
successful retry leaves no trace of the failed attempt; when the retry budget
is exhausted the final partial output is kept and the turn ends with
`StopReason::Truncated`, visible to the user as an incomplete response.

## Product Boundary

The TUI owns drafts, menus, viewport state, and a display projection of queued prompts. The CLI
routes UI commands and continuously forwards thread events. Neither layer owns execution order.

A chat integration follows the same pattern:

```text
chat message / timer / webhook
            |
         typed Input
            |
  submit / enqueue / notify
            |
      core queue + log
            |
       routed Events
            |
      product transport
```

Transport reconnection, authentication, delivery retries, timer persistence, and platform rate
limits belong to the product adapter. Conversation ordering, idempotency, cancellation,
steering, compaction, and durable agent history belong to `ash-agent`.
