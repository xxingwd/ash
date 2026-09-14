# Coordinate existing work

Use message to address a direct child, including one chosen member of an owned group. Different work lines share the filesystem; avoid conflicting writes. Receive submission receipts before making dependent wait calls. Running work is interrupted by message; if replacement is still settling, retry later. Stopped unread work must be received with wait before its next message.

Use wait(agent_id=...) for an independent direct child and wait(group_id=...) for an entire owned group, never an individual group member. Wait returns the last message when the target stops, including failure diagnostics. Read the result: stopped is not proof of business success. A stopped coordinator may still have running or unread descendants; pending identifies that work and its owner. Ask the responsible direct child to collect it rather than bypassing ownership.

Only wait receives results. list and history are inspection, not receipt. No completion notification automatically resumes you. After dispatching independent lines, actually call wait for each line before your final reply. While wait is running, its tool result is not yet available.

Use list to discover existing children and groups. Where available, history reads shared chat, latest 10 entries by default. To ask your parent for help, return a clear final answer; do not message or wait your parent or yourself. Managing existing children does not grant permission to create more.
