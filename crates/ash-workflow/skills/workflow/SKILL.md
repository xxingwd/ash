---
name: workflow
description: Build a task-specific agent organization and coordinate it with message and wait
---
# Organization blueprint

Use an ordinary child for a small bounded delegation. For a task needing several cooperating lines, first inspect the task and choose the smallest useful organization. Call the preinstalled workflow tool with a blueprint, not executable code. Every agent in the blueprint starts idle; roles default to default.

Example arguments:

```json
{
  "blueprint": {
    "agents": [
      {"name": "implementer"},
      {"name": "reviewer", "profile": "review"},
      {"name": "researcher", "profile": "explore", "parent": "implementer"}
    ],
    "groups": [
      {"name": "implementation", "members": ["implementer", "reviewer"], "prompt": "Implement the requested behavior and independently verify it."}
    ]
  }
}
```

Names are blueprint-local labels. Missing parent or group owner means you, the manager. A group's members must be direct children of its owner. Agents may belong to at most one group; parent cycles and missing references fail validation. Ordinary workers, whether grouped or independent, cannot dynamically create children. Put any needed lower-level helpers in the blueprint in advance; their parent gets tools to manage them, not to create more. Your workflow tool supplies the available profile catalog and is your only organization creation entry.

The response maps local names to actual instance IDs and includes group IDs, prompt paths and chat paths. Use these IDs for all later calls. Update group prompt.md for shared task background, then message one chosen member to start. Within a group that member can message one peer to register its successor; no successor means the group stops.

Different groups and independent agents may run concurrently. A group is strictly serial. Send work to every independent line you intend to run before waiting. Wait for a group using group_id, never a member's agent_id. Use history(group_id=...) to inspect shared communication, with before for older pages. Reading history does not receive the final result.

An accepted message is not a completed task. Receive stopped work with wait before sending its next task. A message to running work cancels that work and starts the accepted replacement after cancellation settles. If another replacement is already settling, retry later. Cancellation does not undo edits. Failures and help requests arrive as final results; inspect them and decide the next message.

Each coordinator must collect its own direct children's work. Repeated manager messages reuse the organization; only explicit workflow calls create more nodes. Do not assume that the manager or a group becoming idle also stops all descendants.

For two parallel groups: message the first member of each group, receive both submission receipts, then wait once per group_id. The two waits may run together. Before your final reply, list your work lines to check that none you started is running or unread. If a stopped line is unread, call wait for it; history/list do not clear unread. Do not ask the outer parent to collect groups it does not own.
