# Ash Design

Ash is an embeddable agent runtime. The CLI and TUI assemble products; the workspace crates own
reusable execution semantics. The design keeps one owner for each piece of mutable state and keeps
provider, persistence, and presentation details at their boundaries.

## Crate boundaries

| Crate | Responsibility |
| --- | --- |
| `ash-core` | Provider-neutral conversation data, model streams, events, tools, errors, and IDs |
| `ash-protocol` | Translation between provider wire formats and `ash-core::ModelContext` |
| `ash-tools` | Filesystem, search, shell, editing, and web tools |
| `ash-agent` | Agent definition, turn execution, sessions, compaction, and JSONL persistence |
| `ash-collab` | Optional named child-agent communication |
| `ash-tui` | Inline terminal state, rendering, and interaction |
| `ash-cli` | Provider, prompt, tool, collaboration, and UI assembly |

Protocol JSON stays in `ash-protocol`; terminal state stays in `ash-tui`; collaboration is installed
by an embedding application rather than built into the runtime.

## Conversation model

The durable domain is one typed tree:

```text
Conversation
  checkpoint: optional summary and covered-turn index
  turns: Arc<Turn>[]

Turn
  id, input, steps, result, stats

Step
  ordered Item[]

Item
  Text | Thought | ToolCall(arguments + result)
```

`Conversation` contains only completed facts. A running turn is private `TurnRunner` state and is
converted to one immutable `Arc<Turn>` at completion. There is no `OpenTurn`, flat message model,
session usage accumulator, or second projection of conversation history. `TurnStats` is the durable
statistics snapshot: provider input/output tokens and generation time. The completed tool-call count
is never stored; it is always derived from the structured tool calls in `Turn.steps`, so no record
can contradict them. Legacy records that still carry a count field load unchanged and ignore it.

`ModelContext` is the provider-neutral request view. It combines the conversation checkpoint,
uncovered completed turns, and an optional current input/steps overlay. Provider adapters map that
view directly to their wire format. They do not receive local message IDs or a second message DTO.

## Runtime and session

`Agent` is immutable behavior shared by sessions: model ID, system prompt, tools, context limit, and
tool timeout. `Runtime` owns the model client and the concrete JSONL store. It exposes three creation
paths:

```text
start(agent)                    -> new root session
start_child(agent, parent)      -> new child session
resume(agent, session_id)       -> existing root session
```

`Session` is an actor and the sole writer for its conversation. It serializes a bounded FIFO of
submitted turns and allows view, fork, undo, and manual compaction only while idle. `TurnHandle`
owns cancellation and completion for a submitted turn; `Turn` is only the settled value.

`SessionIdentity` contains `id`, `root_id`, and `parent_id`, and validates those relationships during
construction and deserialization. Collaboration names are transient routing keys, not identity.

## Persistence

The only durable records are:

```text
Init { identity, created_at, title }
Turn { turn, summary }
Checkpoint { summary }
```

Each record occupies one JSONL line. `Init` never embeds history. A root's first completed turn is
written as `[Init, Turn]`; a non-empty fork is `[Init, Turn, Turn, ...]`. `Turn.summary`, when present,
summarizes all turns before that record. A standalone `Checkpoint` summarizes every turn before its
position. Replay therefore has one rule: read `Init`, then apply each `Turn` or `Checkpoint` in order.

The writer lazily creates and exclusively locks `<session-id>.jsonl`. A successful append flushes and
syncs file data; first creation also syncs the session directory. A final line without a newline is
treated as an interrupted append and truncated when the session is reopened. Any malformed complete
record, duplicate turn ID, invalid identity, or misplaced `Init` is corruption.

There is deliberately no format version, alternate legacy parser, or dual write. Additive fields may
use a Serde default only when the storage boundary can restore their exact value from canonical turn
content; other layout changes require old data to be removed.

Root listing reads only `Init`. Child sessions use the same store but cannot be resumed through the
root path. Tree listing and deletion use `root_id` to include durable descendants.

## Fork and undo

`Conversation::before(turn_id)` is the only history split operation. It returns the turns before the
selected turn plus that turn's input, and clears the checkpoint. It is pure and performs no IO.

The session boundary turns a non-empty prefix into a new root and seeds it one `Turn` record per line,
with every copied summary set to `null`. An empty prefix remains an in-memory session until it receives
a turn. Its title is always derived from the first turn it writes. The original session is unchanged.

`undo` is `before(last_turn_id)`; it is not in-place rollback. Child sessions reject both operations.

## Turn execution and compaction

One turn follows a single control path:

```text
TurnRunner -> optional compact -> model stream -> optional ordered tool batch -> repeat
```

The private stream collector returns one exhaustive `ModelResponse`: stopped, truncated, cancelled,
or failed. `TurnRunner` owns retry policy, tool execution, and final `TurnResult`. Tool calls execute
only after a semantic `EndTurn`; unknown stops, token limits, and truncated responses retain text and
thoughts but discard unconfirmed tool calls.

Before every ordinary model request, the runner estimates the entire outbound request. At 80% of the
configured context window it may summarize all completed turns. A successful checkpoint therefore
always has `tail == conversation.turns().len()`; the current runner input and steps remain outside the
summary. A pending automatic summary is used immediately and committed atomically with the final turn.

Manual compaction applies the same plan while idle and writes a standalone `Checkpoint`. It does not
need a runner. Both paths skip compaction when there is no uncovered history or when the resulting
request is not smaller.

Compaction is one independent model call with a fixed system prompt, no tools, and no business-level
`max_tokens`. It receives only the previous summary and newly uncovered turns. Tool text is bounded
while serializing this prompt and attachments are omitted; the stored conversation is not pruned or
copied. Only non-empty text ending in `EndTurn` succeeds. Tool calls, other stops, truncation,
cancellation, transport failure, and an over-window input fail without writing a checkpoint.

## Stream integrity, stats, and cancellation

Provider EOF without a semantic stop is truncated. Truncated and retryable transport failures retry
with cancellation-aware exponential backoff before any tool side effect. Each request's reported
usage is added directly to the current `TurnStats`, including discarded retry attempts; missing usage
is zero. No session-wide usage is stored or reconstructed.

After each physical model response that reports non-zero usage, the runner adds that usage to
earlier calls in the same turn and emits one `Activity` snapshot: accumulated `TurnStats` plus the
count of tool calls completed so far, both derived in the runner. A completed tool batch likewise
emits one snapshot with its updated count. Responses without usage stay silent. Consumers replace
their view with this complete value; they never infer statistics from tool lifecycle events. The
request preflight estimates the final outbound context after any compaction and emits a `Context`
event only when that `(tokens, limit)` snapshot changes. Transient events are not persisted; the
final `Turn` remains canonical — its tool-call count derives from `Turn.steps`.

Cancellation without an executed tool and without a queued successor discards the running turn and
writes no record. If the turn already has a tool result or a later turn depends on it, cancellation is
committed once as `TurnResult::Cancelled`. A process crash never creates an interrupted turn.

## Tool execution

Tools receive only execution facts:

```rust
pub struct ToolContext {
    pub identity: SessionIdentity,
    pub cancellation: CancellationToken,
    pub deadline: Option<Instant>,
}
```

Calls from one accepted model response execute concurrently in bounded batches of eight. Batch joins
preserve their wire order in the resulting `Step` while preventing an unbounded fan-out. `ToolContext::run`
gives cancellation stable priority over deadlines; the agent timeout applies unless a tool explicitly
disables it.

## Collaboration

`ash-collab` owns named child-session routing, pending counts, and unread completions. It does not own
a scheduler, turn state machine, conversation copy, usage accumulator, or cancellation registry.
Children run the base `Agent`, so they keep independent durable conversations without recursively
inheriting collaboration tools.

Submitting work is synchronous with the controller lock and uses the session's bounded `try_submit`.
A short-lived task waits for each `TurnHandle`, publishes its settled result, and wakes waiters. Waiting
consumes available completions but does not cancel child work.

`AgentControl` subscribes once to each child session and wraps activity events with root ID, child
session ID, and name. A controller broadcast carries those facts to embedding applications. Pending
counts remain the authority for running/idle state; there is no child status query or watch snapshot.

## Events and TUI

`SessionEvent` has nine facts: started, text, thought, activity, context, tool started, tool
finished, finished, and discarded. Transient events carry `TurnId`; activity is a replaceable
`TurnActivity` snapshot (full `TurnStats` plus the completed tool-call count), and context is a
request-level token estimate plus configured limit. Completion carries the canonical `Arc<Turn>`.
`Discarded.error` is present only when a completed turn could not be persisted, so event-only
consumers do not mistake storage failure for cancellation. A resumed session replays every settled
turn through the same block conversion as a live finish, including its footer separator.

The TUI has one business-state owner, `AppState`. Events mutate it directly and produce a small
`RenderPlan` describing terminal IO. `TerminalUi` owns only the terminal surface. Rendering borrows
state and never stores a second conversation, operation, menu, stats, or transcript copy. On finish,
streamed blocks are replaced by blocks derived from the canonical turn; on resume, the same conversion
is applied to `Conversation::turns()`.
