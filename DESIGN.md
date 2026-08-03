# Ash Architecture

Ash is an embeddable agent runtime. Product adapters such as a CLI, TUI, chat gateway,
scheduler, or service own transport and presentation; Ash owns agent execution semantics.

## Crates

| Crate | Responsibility |
| --- | --- |
| `ash-core` | Provider-neutral messages, model, tool, event, ID, error, and cancellation types |
| `ash-protocol` | Model-provider protocol adapters and streaming translation |
| `ash-tools` | Sandboxed file, process, search, and web tools |
| `ash-agent` | Agent definition, runtime, threads, turns, input, context, extensions, and persistence |
| `ash-collab` | Optional child-agent spawning, messaging, lifecycle, and collaboration tools |
| `ash-tui` | Terminal rendering and interaction only |
| `ash-cli` | Binary composition and product command routing |

Dependencies point inward. `ash-core` knows nothing about products, providers, persistence,
terminal state, or collaboration. `ash-agent` does not depend on the CLI or TUI.

## Public Model

The stable execution vocabulary is deliberately small:

- `Agent` is immutable behavior: model, prompt, tools, limits, and context policy.
- `Runtime` owns injected capabilities: model client, thread store, and extensions.
- `ThreadOptions` is per-thread scope: working directory, timeout, agent path, tree ID, and metadata.
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
create(metadata, entries) -> version
load(thread_id) -> stored thread
append(thread_id, expected_version, entries) -> version
list(excluded_thread) -> summaries
```

Entry slices represent one ordered version change. Implementations reject stale versions.
The default `JsonlThreadStore` serializes writes per store, appends entries in one batch, and
can read legacy `session-*`/`session_meta` files while writing `thread-*`/`thread_meta` files.

The runtime's hot write path keeps one open `ThreadWriter` per active thread, so each
append is a single write+flush instead of a directory scan plus full-file re-read. Writes
are batched at commit points: accepted inputs are persisted when the turn starts, and all
turn messages plus `TurnEnd` are flushed together when the turn settles. A crash before
that commit point leaves a turn without `TurnEnd`, which the projection reports as
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
live deltas as a preview and replaces them with the canonical projection on `Turn`.

## Context And Memory

`ContextPolicy` prepares model context and performs automatic compaction without deleting full
history. A checkpoint changes only the model-context projection. Manual and automatic
compaction therefore share the same durable mechanism.

Long-term or cross-thread memory is an extension, not part of the thread log. A memory extension
can retrieve relevant facts in `prepare`, inject ephemeral context, and update its own store in
`complete`. This keeps durable conversation history separate from derived memory indexes.

## Extensions

`Runtime::with_extension` composes any number of `Extension` implementations. Each extension
receives a `TurnContext` containing IDs, accepted inputs, current messages, and thread metadata.

`Extension::prepare` returns a `TurnPatch` with ephemeral context and turn-scoped tools.
`Extension::complete` observes a `TurnOutcome` containing the final status and the complete
durable message projection for that Turn.

This is the intended integration boundary for:

- Codex-style goals and plans;
- Claude Code-style workflows and modes;
- dynamic memory retrieval;
- product policy and approvals;
- per-turn tool exposure;
- tracing and audit integrations.

Extensions compose at `Runtime`; they do not add branches to the model/tool loop.

## Collaboration

`ash-collab` is optional. Its public control vocabulary is:

- `AgentControl` for child-agent lifecycle and communication;
- `AgentSpawner` for constructing a child agent;
- `SpawnRequest` for the typed spawn contract;
- `ChildAgent` for the runtime, agent definition, options, and inherited history.

Every child gets its own `Thread` and executes through the same runtime path as a root agent.
Tree identity and canonical agent path live in `ThreadOptions` and `ToolContext`; collaboration
state does not leak into `ash-core` or terminal state.

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
steering, compaction, extensions, and durable agent history belong to `ash-agent`.
