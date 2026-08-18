# Ash Architecture

Ash is an embeddable agent runtime. Product adapters such as a CLI, TUI, chat gateway,
scheduler, or service own transport and presentation; Ash owns agent execution semantics.

## Crates

| Crate | Responsibility |
| --- | --- |
| `ash-core` | Provider-neutral messages, model, tool, event, ID, error, and cancellation types |
| `ash-protocol` | Model-provider protocol adapters and streaming translation |
| `ash-tools` | Sandboxed file, process, search, and web tools |
| `ash-agent` | Agent definition, runtime, sessions, turns, input, context, and persistence |
| `ash-collab` | Optional named child agents and collaboration tools |
| `ash-tui` | Terminal rendering and interaction only |
| `ash-cli` | Binary composition and product command routing |

Dependencies point inward. `ash-core` knows nothing about products, providers, persistence,
terminal state, or collaboration. `ash-agent` does not depend on the CLI or TUI.

## Public Model

The stable execution vocabulary is deliberately small:

- `Agent` is immutable behavior: model, prompt, tools, limits, and context policy.
- `Runtime` owns injected capabilities: model client and session store.
- `SessionOptions` is per-session execution scope: working directory and tool timeout.
- `SessionIdentity` is durable lineage: session ID, root ID, optional parent ID, and a typed
  canonical `AgentPath`.
- `Session` is the durable concurrent conversation boundary and the sole input-queue owner.
- `Turn` is one submitted unit of execution. It can be awaited, interrupted, or steered.
- `Input` carries content, source, metadata, and an optional idempotency key.
- `SessionEvent` routes a `SessionEventKind` with session ID, optional turn ID, sequence, and
  timestamp.

`Runtime::start`, `start_child`, and `resume` are the session construction paths. Root and child
sessions execute through the same `Session` input APIs.

## Session Ownership

Each `Session` is backed by one actor task. The actor owns:

- the ordered queue of submitted turns;
- the active turn and its cancellation state;
- the durable log and current version;
- rollback, fork, compaction, and read operations;
- monotonic event sequencing.

Products submit immediately. They may mirror pending prompts for display, but they must not
decide when queued work starts. This gives chat gateways, CLI/TUI clients, schedules,
heartbeats, and child agents identical ordering and cancellation behavior.

`Session::submit` returns a `Turn` handle. `Session::enqueue` creates fire-and-forget work for
external triggers. `Session::notify` queues a message without starting work; the inbox is prepended
when the next queued turn starts, including a turn that was already submitted while another turn
was active. This is useful for child-agent mailboxes. Schedulers and heartbeat timers remain product
infrastructure; they create typed `Input` values and call `enqueue` instead of bypassing the session.

## Submit And Steer

`submit` creates a new turn and appends it to the session queue. Multiple submissions are
executed in acceptance order.

`Turn::steer` targets only its currently active turn. The input is sent through the active
execution channel, durably recorded with the same turn ID, appended to model context, and used
by the next model iteration. It is not converted into an anonymous follow-up turn. Steering an
inactive turn returns an explicit error.

## Durable Log

`SessionLog` is the durable append-only log and the only source for full user-visible
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

`SessionStore` is the only public persistence boundary:

```text
open_new(identity) -> locked appender
open(session_id) -> stored session + locked writer
load(session_id) -> stored session
list_roots() -> root summaries
tree(root_id) -> root and child summaries
delete_tree(root_id) -> deleted session count
```

Locked writers are exposed through the storage-neutral `SessionAppender` capability. A backend may
hold a file lock, database transaction, or remote lease in that handle; runtime code never depends
on the JSONL writer type.

The default `JsonlSessionStore` writes sessions to its configured data directory's `sessions/`
subdirectory as `{session_id}.jsonl`. New records use `session_header` / `session_id`; old storage
formats are not read. The strict header persists the complete `SessionIdentity`, so roots and
children remain attributable after restart. Its compact first record contains only list metadata,
so listing does not replay session logs. Loading addresses files directly by id and only replays the
selected session. The header must be valid; later malformed records are skipped with a warning so
valid records after a damaged line can still be recovered.

Root listings select `parent_id == None`; tree listings select a shared `root_id`. Durable children
are available as tree history but cannot be resumed as root sessions. Tree deletion accepts only a
root ID, locks every valid session in the tree before removing anything, and removes the root last.

The runtime's hot write path keeps one exclusively locked `SessionAppender` per active session. Resume
replays the selected file through that same handle, so later appends are a single write+flush with
no second read. Writes are batched at commit points: accepted inputs are persisted when the turn
starts, and all turn messages plus `TurnEnd` are flushed together when the turn settles. A crash
before that commit point leaves a turn without `TurnEnd`, which the projection reports as
`Interrupted` and never exposes as normal history.

## Events And Projection

`SessionEventKind` contains only runtime events emitted by the session actor:

- `TurnStarted` marks the accepted turn beginning execution.
- `Live(LiveEvent)` carries ephemeral streaming deltas for the active turn's preview only.
- `TurnCompleted(TurnView)` is emitted when a turn settles and carries its canonical messages, result,
  and usage; clients use it to commit scrollback.
- `ContextCompacted` reports an automatic context-checkpoint change.

Product command results are separate `UiEvent` values owned by the CLI/TUI boundary, including
session lists, restore/fork results, rollback results, and command failures. They never masquerade
as session runtime events.

All memory state is derived from the log through one reducer direction:
`LogEntry -> SessionView -> messages / context / turns -> transcript blocks`. The TUI draws
live deltas as a preview and replaces them with the canonical projection on `TurnCompleted`.
`SessionLog` maintains that projection incrementally as entries arrive, and `Turn::wait` and the
settled session event receive the same canonical `TurnView` rather than assembling parallel views.

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

`ash-collab` is optional. `AgentControl` owns the collaboration tree projection and exposes the
`agent`, `message_agent`, and `wait_agent` tools. `agent` creates one named child with its initial
message; `message_agent` submits a new message to an existing child. Both wait for that turn by
default and return its `TurnResult` plus final response. With `wait=false` they return immediately, and
`wait_agent` later drains unread background completions. A synchronous result is consumed once; if
its caller is cancelled or times out, the result falls back to the unread completion queue.

Child construction uses the clean base `Agent`, `Runtime`, and `SessionOptions` held directly by the
controller. Built-in profiles (`default`, `explorer`, and `worker`) apply prompt and tool-policy
overrides without changing the session engine. A child starts with empty conversation history and
keeps its own history for later `message_agent` calls.

The controller retains the unmodified base `Agent`. Only the main agent receives collaboration
tools and brief orchestration instructions; every child derives from the clean base and therefore
cannot delegate further. Child prompts contain only the base prompt and selected profile instructions.
The main instructions require an independence check before non-trivial work but leave synchronous
versus background execution to each tool call.
Every child gets its own `Session` and executes through the same runtime path as the main agent. Tree
identity and canonical agent path live in `SessionIdentity`; tools receive a read-only snapshot
through `ToolContext.session`. Collaboration state does not leak into terminal state or create a
second execution queue.

Collaboration is an assembly result, not a permission system. `install_collaboration` takes a clean
base `Agent` and is the only assembly path; an already-enhanced agent is rejected. There is no
configurable delegation depth, collaboration concurrency limit, history fork, model-visible session
ID, list operation, or removal operation. Named agents live for their root session. Their display
state is only `idle` or `running`; completion, failure, and interruption belong to `TurnResult`.
`message_agent(interrupt=true)` cancels unfinished child turns before submitting the replacement
message. The controller mirrors Turn handles for waiting and cancellation, while the child `Session`
remains the sole execution-queue owner.

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
  upstream 500/502/503/504/520-524/529, or `RateLimited`; or the stream was `Truncated`; and
- no tool call has been executed yet in this call, so re-issuing the request
  cannot repeat side effects.

Between attempts the runner waits an exponential backoff (`RetryBackoff`,
base 1s doubling to a 10s cap, i.e. 1s, 2s, 4s, 8s, 10s), and a cancellation
during the wait aborts the turn instead of retrying.

Non-retryable failures (`Auth`, `InvalidRequest`, `InvalidResponse`, other upstream
statuses) fail the turn immediately and surface as
`TurnResult::Failed`. On retry, the partial assistant message is discarded both
from memory and from the staged log (`SessionPersistence::rollback_to`), so a
successful retry leaves no trace of the failed attempt; when the retry budget
is exhausted the final partial output is kept and the turn ends with
`StopReason::Truncated`, visible to the user as an incomplete response.
Context preparation happens before the retry snapshot, so a compaction checkpoint
created for the request remains durable across failed attempts.

## Product Boundary

The TUI owns drafts, menus, viewport state, and interaction feedback. While idle, Enter submits a
new turn; while a turn is running, Enter submits another turn to the same Session queue. Pending
prompts are mirrored only for display and enter the transcript when their `TurnStarted` event
arrives. Escape discards unfinished streamed output and cancels the active Turn. When no later Turn
is queued, an interrupted turn with no completed tool result is rolled back and its prompt returns
to the composer. When later work is already queued, the interrupted result is committed so execution
can advance without attempting a rollback against a busy Session. Commands
remain visible while work is active, but session-mutating commands are silently rejected locally
and remain in the composer; `/status` and invalid commands are also rejected while a turn is
running. The CLI retains accepted `Turn` handles in submission order, routes UI commands, and
continuously forwards session events. Cancellation targets the queue head. Neither layer owns
execution order.

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
