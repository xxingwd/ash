# Workflow

Use the `workflow` tool for bounded work that benefits from fan-out, parallel
execution, or a runtime-discovered number of items. Keep ordinary conversation,
one-off edits, and small delegation in the normal agent tools.

## Contract

- The script is JavaScript executed by the workflow runtime.
- `root_id` is the current session. Omit the parent argument to create a child
  of the current workflow root.
- `agent(prompt, parent_id)` creates one child session, waits for it, and
  returns `{ id, output }`.
- A child may be used as the parent of another child by passing its `id`.
- Keep prompts bounded and make every child return a concise structured result.
- Use `Promise.all` for independent work and a normal loop for ordered work.
- Do not create dynamic agent types, goals, or new workflow mechanisms in the
  script. Dynamic values are items and child instances only.
- Do not use workflow recursively. Child sessions receive the base agent, not
  the workflow tool.

## Shape

Prefer a small number of explicit phases:

```js
const analysis = await Promise.all([
  agent("Analyze the request. Return JSON with an `items` array."),
  agent("Analyze the request independently. Return JSON with an `items` array."),
  agent("Analyze the request from a third angle. Return JSON with an `items` array."),
]);

const items = analysis.flatMap(({ output }) => JSON.parse(output).items);
const results = await Promise.all(
  items.map((item) => agent(`Process this item and return JSON: ${JSON.stringify(item)}`)),
);

return { items: results };
```

Do not invent a static diagram for runtime-created items. The runtime snapshot
and session events are the source of truth for the live parent-child graph.

## Persistence

- Each agent conversation remains in the existing session JSONL. Do not copy
  transcripts into workflow storage.
- Each workflow has a small append-only control log containing its script,
  spawned instance ids and parent ids, labels, terminal states, and final result.
- Live snapshots are projections of control events. A cold snapshot marks work
  without a terminal event as `interrupted`.
- A trailing incomplete JSONL record is ignored. A malformed complete record or
  sequence gap is corruption and must not be silently accepted.
- Workflow execution is not resumed after a process restart. Recovery restores
  inspection state; a new invocation decides whether to retry interrupted work.
