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
| `ash-collab` | Identity-scoped delegation, serial groups, receipts, and public chat |
| `ash-workflow` | Workflow profile, builtin skill, and organization assembly tool |
| `ash-tui` | Inline terminal state, rendering, and interaction |
| `ash-cli` | Provider, prompt, tool, collaboration, and UI assembly |

Protocol JSON stays in `ash-protocol`; terminal state stays in `ash-tui`; collaboration is installed
by an embedding application rather than built into the runtime.

## Conversation model

The durable domain is one typed tree:

```text
Conversation
  checkpoint: optional summary
  turns: Arc<Turn>[] after the checkpoint

Turn
  id, input, Arc<Step>[], result, stats

Step
  ordered Item[]

Item
  Text | Thought | ToolCall(arguments + result)
```

`Conversation` contains only completed facts in the current memory window. Committing a checkpoint
drops every covered `Arc<Turn>` and retains only its summary plus later turns. A running turn is private
`TurnRunner` state and is converted to one immutable `Arc<Turn>` at completion. There is no flat
message model or session usage accumulator. `TurnStats` is the durable statistics snapshot: provider
input/output tokens and generation time. The completed tool-call count is never stored; it is always
derived from the structured tool calls in `Turn.steps`, so no record can contradict them. Legacy
records that still carry a count field load unchanged and ignore it.

`ModelContext` is the provider-neutral request view. It combines the conversation checkpoint,
uncovered completed turns, and an optional current input/steps overlay. Provider adapters map that
view directly to their wire format. They do not receive local message IDs or a second message DTO.
Current input and committed steps are shared through `Arc`, so cloning a request for retry does not
copy tool outputs or image attachments. These ownership changes do not change the serialized turn
or step format. The CLI parses `ASH_MODEL_CONFIG` once into `ProviderConfig.model_config`; adapters
apply this explicit configuration rather than reading process-global environment during translation.

## Runtime and session

### Profiles and prompt composition

`ash-agent` embeds `agents/default.md`, `agents/explore.md`, and `agents/review.md`. Markdown
frontmatter has a description and optional comma-separated `tools`; the body is role instructions.
Omitted tools select the regular catalog, an empty string selects none, and invalid metadata or tool
names fail creation. Profiles do not select models. A session's resolved profile is fixed at creation,
including across resume. `ash-workflow` owns its separately installed workflow profile.

`Agent::system_prompt()` composes explicit instructions, role, environment, installed tool instructions,
and repository rules. `Tool::instructions` makes the installed tool name the stable contribution key.
Module installers own their text; CLI only chooses definitions and installers. The composer has no
workflow/review branches or general plugin lifecycle. Creation tools render the actual profile catalog.

`PromptContext` loads the initial project-root-to-cwd `AGENTS.md` chain. Typed `RepositoryInstruction`
values retain file path, directory scope, and content. File tools discover deeper applicable rules
before execution; unseen rules return a deferred result without performing the operation. A subsequent
request may retry. Same-batch calls use the old observed snapshot and cannot bypass the check. Rules
are upserted by path, so changed files replace old versions rather than accumulating contradictory
System blocks. Sibling directory rules are not loaded. Bounds and IO failures remain explicit.
Shell instructions require checking applicable rules; arbitrary shell scripts are not path-analyzed
or sandboxed by this mechanism.

The immutable Agent definition retains a registered tool catalog separately from its selected tools.
A Session owns its current definition snapshot. Skills add registered tools rather than intersecting
profile tools. `ToolOutput` carries typed installed-tool and observed-rule effects; these fields are
not sent as extra provider content. The engine validates all effects against a candidate definition,
commits the step and resulting snapshot together, and only then publishes the next request's tools.
Same-response tool calls use the previous snapshot. Invalid additions do not partially install;
other sessions remain unchanged. Runtime skill reads do not change models. Startup skills retain
existing explicit model overrides. Organization scope removes disallowed collab tools from both
the installed set and the registered catalog; a skill cannot reinstall forbidden creation tools.

`AgentSnapshot` stores resolved profile, explicit prompt, environment, tool names, tool-owned instructions,
and observed repository rules. Restore resolves names against registered implementations and fails
on missing tools instead of silently changing capabilities. Providers and API credentials remain
runtime configuration, not definition fields. Stored environment text is historical context; physical
file/shell working directory still belongs to the current runtime, so resume should use the same cwd.

### Execution

`Agent` is an immutable template copied into each session: model ID, system prompt, tools, context limit, and
tool timeout. `Runtime` owns the model client and the concrete JSONL store. Its root creation paths are:

```text
start(agent)                    -> new root session
start_child(agent, parent)      -> new child session
resume(agent, session_id)       -> existing root session
start_at(agent, identity)       -> organization-owned identity
restore_child(agent, identity)  -> existing child, or explicit missing record
```

`Session` is an actor and the sole writer for its conversation. It serializes a bounded FIFO of
submitted turns and allows view, fork, undo, and manual compaction only while idle. `TurnHandle`
owns cancellation and completion for a submitted turn; `Turn` is only the settled value.
The capacity of 64 counts every accepted turn, including the running turn and the actor's internal
queue. Each queued turn owns one semaphore permit until it finishes. `submit` waits for capacity;
`try_submit` reports `QueueFull` without blocking.

`SessionIdentity` contains `id`, `root_id`, and `parent_id`, and validates those relationships during
construction and deserialization. Collaboration uses instance IDs for routing; group aliases are scoped to their owner.

## Persistence

The only durable records are:

```text
Init { identity, created_at, title, definition }
TurnStart { id, input }
TurnStep { step, definition }
TurnEnd { result, stats, summary }
Checkpoint { summary }
Definition { definition }
```

Each record occupies one JSONL line. `Init` never embeds history. A turn is one `TurnStart`, zero or
more ordered `TurnStep` records, then one `TurnEnd`. Definition includes the selected profile and
installed capabilities, never the provider client. Initial metadata is bounded at 1 MiB, and oversize
creation is rejected before writing. Every completed step is flushed and synced before
the runner accepts it into the next model request. Final provider statistics belong to `TurnEnd`.
`TurnEnd.summary`, when present, summarizes all turns before that record; a standalone
`Checkpoint` summarizes every turn before its position.

Replay first validates complete records and finds the latest compaction boundary, then reconstructs
the conversation from that boundary. Covered turns are not retained. A trailing open turn is not a
settled business result and is truncated from its `TurnStart` when the session reopens. The latest
committed ability snapshot is retained in a standalone `Definition` record, even when that incomplete
turn is removed, so repeated resume cannot lose previously committed tools. Nested starts, orphan steps or
ends, duplicate turn IDs, and checkpoints inside an open turn are corruption.

The writer lazily creates and exclusively locks `<session-id>.jsonl`. A successful append flushes and
syncs file data; first creation also syncs the session directory. A final line without a newline is
treated as an interrupted append and truncated when the session is reopened. Any malformed complete
record, invalid identity, or misplaced `Init` is corruption.

There is deliberately no format version, alternate legacy parser, or dual write. Additive fields may
use a Serde default for optional absent facts; there is no promised migration for obsolete collaboration
or JavaScript workflow records.

Root listing reads only `Init`. Child sessions use the same store but cannot be resumed through the
root path. Tree listing and deletion use `root_id` to include durable descendants.

## Fork and undo

`Conversation::before(turn_id)` is the only history split operation. It returns the turns before the
selected loaded turn plus that turn's input, and preserves the current checkpoint. Turns covered by
the checkpoint are intentionally unavailable as fork points. It is pure and performs no IO.

The session boundary turns a prefix into a new root and seeds its checkpoint before the copied turn
records. A prefix with neither checkpoint nor turns remains in memory until it receives a turn. The
title comes from the first copied turn, or from the selected input for a checkpoint-only prefix. The
original session is unchanged.

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
drops the currently loaded turns; the current runner input and steps remain outside the summary. A
pending automatic summary is used immediately and committed atomically with the final turn.

Manual compaction applies the same plan while idle and writes a standalone `Checkpoint`. It does not
need a runner. Both paths skip compaction when there is no uncovered history or when the resulting
request is not smaller.
`compact_with_cancellation` accepts an external cancellation token. Dropping a compaction request
cancels its model wait, but lets any checkpoint write already in progress settle before the actor
handles the next command. The CLI drops that request when its UI event receiver closes.

Compaction is one independent model call with a fixed system prompt, no tools, and no business-level
`max_tokens`. It receives only the previous summary and newly uncovered turns. Tool text is bounded
while serializing this prompt and attachments are omitted. Prompt construction does not mutate the
conversation; covered turns are released only after the checkpoint commits. Only non-empty text ending
in `EndTurn` succeeds. Tool calls, other stops, truncation, cancellation, transport failure, and an
over-window input fail without writing a checkpoint.

## Stream integrity, stats, and cancellation

Provider EOF without a semantic stop is truncated. Truncated and retryable transport failures retry
with cancellation-aware exponential backoff before any tool side effect. Each request's reported
usage is added directly to the current `TurnStats`, including discarded retry attempts; missing usage
is zero. No session-wide usage is stored or reconstructed.
Before retry backoff, `Retrying` invalidates the uncommitted response preview without changing
committed steps or accumulated usage. Non-interactive output writes committed steps, not text
deltas; completion fills in any steps missed by the event receiver without printing them twice.

After each physical model response that reports non-zero usage, the runner adds that usage to
earlier calls in the same turn and emits one `Activity` snapshot: accumulated `TurnStats` plus the
count of tool calls completed so far, both derived in the runner. A completed tool batch likewise
emits one snapshot with its updated count. Responses without usage stay silent. Consumers replace
their view with this complete value; they never infer statistics from tool lifecycle events. The
request preflight estimates the final outbound context after any compaction and emits a `Context`
event only when that `(tokens, limit)` snapshot changes. Transient events are not persisted.
`StepCommitted` is published only after its `TurnStep` is synced; the final `Turn` remains canonical
and its tool-call count derives from `Turn.steps`.

Cancellation without an executed tool and without a queued successor discards the running turn and
writes no record. If the turn already has a tool result or a later turn depends on it, cancellation is
committed once as `TurnResult::Cancelled`. A process crash with an open turn truncates that unsettled
prefix; unfinished streaming output and unfinished tool work are not invented.

## Tool execution

The working directory is a path-resolution base, not a filesystem sandbox. File tools accept
absolute paths and paths outside that directory; shell commands run with the host process's
permissions. Isolation is an embedding or deployment concern, not a guarantee of the path types.

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

`ash-collab::AgentControl` owns the six tools `agent`, `message`, `group`, `history`, `wait`, and `list`.
It owns organization identity, group membership, minimal execution lines, last results, and unread
receipts. Model loops and personal history remain in Session. Public operations validate caller
identity and direct relationships; profile text and editable chat cannot grant organization access.
Definitions provide optional capability factories, so workflow extends collab without a reverse dependency.
Tool closures use weak control references; the embedding application retains the controller.

`install_root` installs the root's six collab tools. Other instances derive access from their
definition's coordinator capability, membership, and prebuilt children/groups, not profile YAML
permission fields. Ordinary leaves install no collab tools; leaf members get message/history/list.
Workers with prebuilt descendants can manage them, but cannot create more. Only the root can call
agent/group creation; workflow managers use assembly exclusively. Blueprint validation knows all
relationships before a worker's first request. Joining an unstarted instance rebuilds its organization
tools and their instructions without replacing its role or normal capabilities.

Coordination and member instructions are separate module-owned contributions. No leaf receives the
full coordinator manual. Available profiles are rendered on the root agent tool and the manager's
assembly tool; the common prompt composer and CLI do not maintain a second role catalog.

An execution line belongs to one independent agent or one group. A group has at most one active
direct member, one registered successor, and one accepted replacement. A controller lock orders
submission, successor registration, cancellation, finish, and durable state updates. Different lines
execute concurrently; an individual Session still serializes its own turns. Accepted control commands
finish in an owned task even if their caller drops the tool future. Before acceptance, cancellation
rejects the command. No business-success FSM, task queue language, or worktree is introduced.

Parent message starts an idle, received line or replaces running work using Session turn cancellation.
While replacement is settling another parent message is rejected. Same-group message registers one
successor; concurrent registrations have one winner. The sender must stop normally before handoff;
failure/cancellation discards its successor, and parent replacement supersedes it. Switching has no
observable idle gap. Internal execution IDs reject late events, not user-visible round addressing.

Wait targets one direct independent child or owned group. It follows the continuous work line through
handoffs/replacements and selects an immutable last result only when no member is running. Individual
group members, parent, self, and own group are invalid wait targets. Upward help is a final reply,
not a reentrant message; the parent receives it and decides how to continue. Empty targets return null.
A result is the last text item, not all progress/thought/tool output; abnormal termination includes
runtime diagnostics. Stopped means no executor remains, not successful task completion.

Wait also returns a live `pending` snapshot of running/unread descendants below the stopped target,
with each work line's owner and typed target. It does not wait those descendants, rewrite the selected
last message, or acknowledge their results. Group members are not duplicated as independent lines.
The diagnostic list is capped at 32 with `pending_truncated`; its serialized size is subtracted from
the message budget. Top-level list also exposes the caller's pending subtree without acknowledging it.

When the root turn finishes, noninteractive CLI snapshots pending work before closing descendants.
If anything was running or unread, shutdown still completes, but the command returns an error with
the affected owners/IDs rather than success. Interactive CLI emits a warning without dispatching or
cancelling work. ToolStarted identifies an active wait before any result is committed; absence of a
persisted tool result alone is not evidence that wait was never called. No automatic retry loop,
business-success classifier, or change to the group's stopped definition is introduced.

The receipt boundary is:

```text
wait selects stopped result
  -> parent TurnStep + ability snapshot fsync
  -> Tool::committed validates selected message ID
  -> collab unread=false state fsync
  -> next parent model request
```

History/list do not acknowledge. Same-batch wait/message cannot bypass the gate. Cancellation before
commit leaves the result unread; an old receipt cannot clear a newer result. Restore only trusts a
read receipt when the owner's completed personal turn proves that result was delivered; uncertainty
redelivers instead of losing the result. This accommodates personal-store truncation of open turns.

Organization state is atomically saved at `collab/<root>/state.json` under the session directory.
It stores identities, fixed definitions, lines, receipts, and blueprint/assembly associations. Each
group owns `groups/<uuid>/prompt.md` and append-only `chat.jsonl`. Shared prompt is read per member turn;
chat shows public delivery/transition/completion records, not private thoughts or tool transcripts.
Chat is an audit view, never the scheduler: an append alone does not prove accepted or executed work;
state and returned receipt decide acceptance. Failed state persistence does not start unaccepted work.
History uses exclusive anchors, defaults to ten entries, and bounds encoded output without rewriting
original records. Direct file edits do not send messages or alter execution state.

Restore validates the tree and opens child Sessions using saved definitions. Missing completed child
records or missing capabilities fail explicitly. Partial chat tails are repaired; unfinished lines
become readable interrupted diagnostics without replaying messages, shell, or registered successors.
External side effects are not exactly-once and are never implicitly rolled back. Root shutdown cancels
and drains descendants, but a manager's ordinary completion does not stop its descendants. Fork/undo
copy a personal-history prefix, not the organization; previously mentioned IDs do not gain access
in the new root. CLI closes the previous root when switching to a fork/new/undo session.

## Events and TUI

`SessionEvent` has eleven facts: started, retrying, text, thought, activity, context, tool started, tool finished,
step committed, finished, and discarded. Transient events carry `TurnId`; activity is a replaceable
`TurnActivity` snapshot (full `TurnStats` plus the completed tool-call count), and context is a
request-level token estimate plus configured limit. `StepCommitted` carries an ordered, durable
`Arc<Step>`; completion carries the canonical `Arc<Turn>`.
When automatic compaction commits with that turn, completion also carries the summary so every
consumer can apply the same memory boundary.
`Discarded.error` is present only when a completed turn could not be persisted, so event-only
consumers do not mistake storage failure for cancellation. A resumed session replays the current
summary and loaded turns through the same block conversion as a live finish.

Collaboration events carry root, instance, and optional group IDs. The UI keys a group as one line,
not one line per member. Execution generation checks discard stale streams; group status comes from
its controller line rather than an intermediate member finish. Restore and lagged subscriptions
replace rows from controller snapshots, the same source used by list/wait.

The TUI has one business-state owner, `AppState`. Events mutate it directly and produce a small
`RenderPlan` describing terminal IO. `TerminalUi` owns only the terminal surface. Rendering borrows
state and never stores a second conversation, operation, menu, stats, or transcript copy. Each
`StepCommitted` replaces the current preview with canonical blocks and moves them into native
scrollback. On finish, only missed steps and the footer are appended. Resume and checkpoint rebuilds
project the current `Conversation` into the existing history block model. Resize resets scrollback and
replays those same in-memory blocks at the new dimensions; it has no separate persistence or session
loading path.

Submission acknowledgements are pending UI state, polled by the main loop before live events.
The UI continues draining events while waiting for acceptance, so a full event channel cannot
deadlock submission. The controller uses non-blocking `try_submit` to keep cancellation responsive
when the session reaches capacity. Context tokens and their limit are stored as one optional value.

## Workflow organization

`ash-workflow` owns `agents/workflow.md`, builtin `skills/workflow/SKILL.md`, and the short-schema
`workflow(blueprint)` tool. `agent(profile="workflow", prompt=task)` creates an ordinary child with
that definition; `/workflow task` uses exactly this path and tells the external default agent to
wait on the returned manager, not create another. Only manager definitions install the assembly tool,
and they do so before the first model request. Reading the builtin skill changes conversation content,
not the already-installed tools or base System.

The manager analyzes the task and supplies a blueprint with agent names/profiles/parents and group
names/owners/members/prompts. Omitted parents/owners mean that manager; other names reference blueprint
nodes. The tool validates the complete tree and group ownership before creating unstarted resources.
It publishes all instances, shared files, and name-to-ID associations as one organization update.
Failure cleans only resources newly created by that attempt, never existing organization or manager.
A blueprint is bounded to 64 agents and 32 groups; no executable YAML/JavaScript is interpreted.

After assembly, every level uses the collab tools allowed by its organizational position. Ordinary
workers cannot dynamically create nodes, whether grouped or independent, but may manage their
prebuilt direct children. Message starts actual work; the assembly tool never
runs tasks, chooses business flow, or adds a second wait/chat/scheduling implementation. Repeated
manager messages do not automatically rebuild organization. The old JS VM, script tool, and exclusive
workflow lifecycle were removed without aliases or a migration reader.
