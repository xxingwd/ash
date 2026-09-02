# 未提交工作树审查（2026-09-01）

> 口径：只审当前未提交改动，对照被覆盖前工作树里的 A1–A7 计划和已改的 `DESIGN.md`。
> 范围：`git status` 28 文件，+1476 / −544；未跟踪 `crates/ash-collab/src/event.rs`。未跑 `cargo test --workspace`。
> 复核：2026-09-01 二次对照源码，不是只看 diff。§1 / §3 / A4 半截 / `CommandFailed` Display 映射仍成立。A7「Completions 不走 HTTP 错误体」写错；§2 / §5 / P0.6「3 处」过重，已收窄。
> 总判：不是 A1 收尾。已勾完的 A1/A2 上重新打开 live stats；A4 / A7 / P1 错误处理各做了半截。计划文档和实现已分叉。
> 合入前最小集：① 修 `CommandFailed` → 工具结果字符串；② `Removed` 后禁止 Session 事件建行，remove 停 forward；③ 要么改 A1 承认 live `Progress`，要么从这批拿掉 live stats。

## 改动对照计划

| 计划项 | 原清单 | 这批实际做了什么 | 能否勾 |
| --- | --- | --- | --- |
| A1 领域 / 事件 | `[x]` | `SessionEvent` 加回 `Progress`；TUI / collab 再投影 live `TurnStats` | 否。与已勾完的 A1 验收冲突 |
| A2 单一 `AppState` | `[x]` | `turn_stats` / `tool_calls` 进 `AppState`；`ViewportInput` 再摊两字段 | 状态仍一个 owner，但 live stats 又成镜像 |
| A4 `Workspace` | `[ ]` | 工具组构造时打开一次 canonical root + cap-std `Dir` | 第一刀对，A4 没做完 |
| A5 工具并发 | `[x]` | `MAX_PARALLEL_TOOLS = 8` 切批 `join_all` | 行为新增；吞吐是切批而非有界并发 |
| A6 collab 领域方法 | `[ ]` | watch 快照 → broadcast 事件；`AgentGroup::create` 仍未抽 | 扩了 collab 面，没完成 A6 |
| A7 `sse::stream` | `[ ]` | 错误体上限读取 + `Auth`/`RateLimited` 带 message | A7 前半；request-id / retry-after / OpenAI helper 没动。Completions **也走** `sse::stream`，HTTP 错误体路径三家共用 |
| P0.6 路径包含性 | `[ ]` | bash cwd 检查搬进 `Workspace::resolve_dir` | 4 处变 3 处，未关 |
| P1 `Auth`/`RateLimited` | `[ ]` | 变体加 `message: String` | 半完成：无 request-id/retry-after，仍 `Clone` |
| P1 `CommandFailed` | `[ ]` | 新变体 + bash 使用 + 输出限长 | 类型对；持久化/展示用 Display 压扁，合同变了 |
| A3 `Tool`/`FnTool` | `[ ]` | `FnTool` 改私有，空 name/description 拒绝 | 边角，不是 A3 |

原计划「stats/event」目标：`TurnRunner -> Turn.stats`，stats 只在 turn 结束提交一次。A1 删除清单写明：实时 `TurnProgress`、collab `forward_stats`、`SubagentSnapshot.usage`、`SubagentView.usage`，子代理行只保留 name/state。

这批把其中几项加回来，只换了名字：`SessionEvent::Progress`、`SubagentView.stats/tool_calls/active_turn`、controller 转发 child session 事件。`DESIGN.md` 已改成 9 个事实 + 子代理事件广播；A1 验收没改。先统一文档，否则下一刀会按过时验收把 live stats 再拆掉。

---

## P0 — 合入前必须修

### 1. `CommandFailed` 被 `Display` 写进 tool result

- 文件：`crates/ash-agent/src/engine.rs`（`execute_tool_call`）、`crates/ash-core/src/error.rs`、`crates/ash-tools/src/bash.rs`
- 计划原文：增加 `ToolError::CommandFailed { status, output }`，输出限长同时处理其 output；spawn/read/join 仍走执行错误。
- 已做：类型、bash 使用、`limit_tool_result` 限长 `CommandFailed.output`。
- 问题：A1 已定 `ToolCall.result: Result<ToolOutput, String>`。engine 仍是 `limit_tool_result(...).map_err(|error| error.to_string())`。新 Display 是 `command exited with {status}: {output}`。以前失败文本是「stdout/stderr + 空行 + `Command exited with 7`」，现在「状态码在前、整段输出在后」。
- 影响：下一轮模型看到的 tool result（进 `Turn`）；TUI bash 失败块（`is_error` 时把该字符串当 output 画）；任何靠文案解析退出码的下游。
- 修法：`limit_tool_result` 之后显式格式化（输出在前、status 在后，或只把 `output` 放进 `Err(String)`），不要 `error.to_string()`。P1 要的是调用方可分支，不是把 thiserror 文案当领域字符串。不修不能勾 P1 `CommandFailed`。

### 2. `Removed` 之后迟到的 Session 事件会把子代理行复活

- 文件：`crates/ash-tui/src/app.rs`（`update_subagent`）、`crates/ash-collab/src/control.rs`（`remove` / `forward_events`）
- 旧模型：watch 快照。最新一份列表里没有这个名字，行不会回来。
- 新模型：TUI 自己累加事件。`update_subagent` 对任何非 `Removed` 更新都 `push` 新行（默认 `Running`）。`remove` 只发 `Removed`，不停 `forward_events`。
- 正常 remove 前提是 pending 和 unread 都空，所以**通常没有**仍在飞的 Progress/Finished。仍成立的窗口：
  1. `forward_events` 已从 child session 取出、但还没 `publish` 的事件，与 `Removed` 在 controller broadcast 里乱序。
  2. CLI `event_tx`（64）里 `UiEvent::Subagent(Session)` 已排队，TUI 先处理后到的 `Removed`。
  3. 同名重建：旧 session 的迟到 Session 事件 `session_id` 对不上新行，会再 `push` 一行旧 session（默认 Running）。这比「刚 remove 的空闲 agent」更实在。
- 修法仍然对：只在 `StateChanged` 时建行；`Session` 只更新已存在的行；`remove` 时停掉对应 forward task（CancellationToken 或把 stream 在 remove 时 drop）。
- 缺测试：Removed 后再来 Progress/Finished 不得建行；旧 session_id 的 Session 事件不得在新同名 agent 旁边再建一行。

---

## P1 — 设计冲突 / 正确性

### 3. live `Progress` 和已勾完的 A1 打架

- 文件：`crates/ash-core/src/event.rs`、`crates/ash-agent/src/engine.rs`（`ResponseAccumulator::apply`）、`crates/ash-tui/src/app.rs`、`crates/ash-tui/src/viewport.rs`、`DESIGN.md`
- A1 事件集是 7 个事实，明确没有 progress；删除实时 `TurnProgress`；TUI/CLI 只在 turn footer 展示最终 stats。
- 工作树现在是 9 个事实。collector 在 usage 变化时发完整 `TurnStats` 快照（已 settle 的前几次请求 + 当前 `max` 合并），请求预检后的 `Context` 估算只在快照变化时发出。TUI：父会话 `AppState.turn_stats` / `tool_calls`，宽 ≥80 画在 status；子代理 `Started` 清零 → `Progress` 覆盖 stats → `Context` 更新占用 → `ToolStarted` 计数 → `Finished` 用 canonical `Turn` 覆盖。
- 实现本身干净：`progress_is_published_only_when_usage_changes` 把 `max` 合并测清楚了；没有本地估算、没有 timer；最终 `Turn` 仍 canonical。这是改设计，不是完成清单。
- 必须二选一：
  - 承认 live `Progress` 是新产品合同：改 A1 事件列表、删除清单、stats/event 表、完成标准（旧标准「状态栏 token 数只在完成请求返回后变化」与现行 DESIGN 互斥）。
  - 从这批拿掉 `Progress` / 子代理 live metrics，只留 `Workspace` 和协议诊断。
- 不改文档就合，下一轮会按过时 A1 把这条当回退。

### 4. 有界 broadcast 上做投影，丢失后没有权威快照

- 文件：`crates/ash-collab/src/control.rs`、`crates/ash-cli/src/modes.rs`、`crates/ash-agent/src/session.rs`
- session 和 collab 都是 `broadcast(256)`。A1 给父会话 transient 留了 `TurnId`，漏掉 `Started` 后错位 delta 可拒。
- 子代理：`forward_events` 遇到 child lag 是 `continue`；CLI 遇到 controller lag 只打日志。`Started` 丢了，后面的 `Progress`/`ToolStarted` 因 `active_turn` 对不上被忽略；`Finished` 丢了，行停在旧 metrics 上，只靠后来的 `StateChanged(Idle)` 改圆点。
- idle/running 仍以 pending 为准（与 DESIGN 一致）。live tokens/tools 可丢。计划里没有「可丢的 UI 投影」这一层。
- 修法：lag 时清掉该 child 的 metrics；或保留很小的 name/state/stats 快照通道（后者正是 A1 删掉的 watch）。先写进设计，再实现。

### 5. 父 `Progress` 和子代理事件挤进同一个 64 容量 `event_tx`

- 文件：`crates/ash-cli/src/modes.rs`（`InteractiveController::run`）
- controller 对 `UiEvent::Session` 和 `UiEvent::Subagent` 都 `event_tx.send().await`。子代理 usage 一密，**父会话流式输出**会被背压（同一 mpsc 里 Session Text 也要排队）。
- 先前写成「submit/cancel 也会停」过重：`command_rx` 是 `select!` 的另一臂，command 仍会被取出；`session.submit` 走 actor 队列，不经过 `event_tx`。会被拖住的是 UI 事件送达，不是提交本身。
- A2 要求 handler 只改 `AppState`，没有要求 collab 用量和文本 delta 抢有界 mpsc。
- 修法：子代理通道独立，或 progress 可丢（try_send / 合并最新快照）。

### 6. `select_session` 不再按 root 丢掉 subagent 行

- 文件：`crates/ash-tui/src/app.rs`、`crates/ash-tui/src/inline.rs`（`refresh_status`）、`crates/ash-tui/src/viewport.rs`
- 只在 render 时 `filter(|agent| Some(agent.root_id) == state.session_id)`。切 session / NewSession 后，别的 root 的 agent 堆在 `AppState.subagents`。
- `refresh_status` 看未过滤列表：idle 时只要别的 root 还有行就会继续 tick。
- A2 的 `ConversationChanged` 本应一次替换会话相关状态。
- 修法：`select_session` / NewSession 按 `root_id` 丢掉非当前 root 的行；或 `refresh_status` 与 render 用同一过滤。

### 7. 并行工具是切批 join，不是有界并发

- 文件：`crates/ash-agent/src/engine.rs`（`execute_tools`）、`DESIGN.md`
- `pending.drain(..min(8))` + `join_all`。第 9 个要等前 8 个全部结束。一个慢 bash 堵住整批，`ToolStarted` 也被拖住。
- 顺序和上限有测试 `bounds_parallel_tool_execution_without_reordering_results`。测试用 10ms sleep 断言 `maximum == 8`，CI 负载高时可能 `< 8` 抖。
- DESIGN 已写成「bounded batches of eight」。若坚持切批，写明吞吐权衡。否则 semaphore + 一次 `join_all` 同样保 wire order。
- A5「只有 EndTurn 能执行 tool」这批没改，仍成立。

---

## 按计划项逐条

### A1 — 已勾，这批在拆验收

- 原验收：stats 只在一个 turn 内累计并持久化到 `Turn`，不存在 session usage、实时 stats 镜像或分支 usage；公开事件 7 个事实；子代理行只保留 name/state。
- 这批：第 8 个事实 `Progress`，第 9 个事实 `Context`；TUI / 子代理行镜像 live `TurnStats`、变化后的上下文占用和 `tool_calls`。
- 正向：最终 `Turn` 仍 canonical；Progress 不落盘；usage 用 `max` 合并且只在变化时发；session 日志改成 turn id + 长度，不再 `?event` dump 内容。
- 结论：要么改 A1 文档承认 live snapshot，要么撤回 Progress / 子代理 metrics。不能维持「A1 已完成」同时合这批 UI。

### A2 — 已勾，状态 owner 还在，镜像字段回来了

- `AppState` 仍是唯一业务状态。`with_subagent_monitor` / `subagent_rx` 删除，事件从 CLI 进 `UiEvent::Subagent`，符合「handler 只改 AppState」。
- 新增 `turn_stats: Option<TurnStats>`、`tool_calls: usize`，viewport 再从 AppState 摊到 `ViewportInput`。这是本帧布局参数，和 A2「viewport 内部无状态借用视图」相容。
- 不相容的是：这些字段是对 `Progress`/`ToolStarted` 的运行时镜像，turn 结束又清掉，和 A1「不从 items 反算、不驱动 TUI 实时状态」冲突。
- `select_session` 清了父会话 stats，但不清他根 subagents（见 §6）。

### A3 — 未开工

- 仅 `FnTool` 改 `pub struct` → `struct`，`define_tool` 拒绝 trim 后空的 name/description，补了测试。
- 未删公开 `Tool` trait、`Arc<dyn Tool>`、`tool_definitions()` 每次重建、engine 线性按名查找。
- 空元数据拒绝是正向边角，不能当 A3 开工。

### A4 — 第一刀，未完成

目标：一次 `Arc<Workspace>`（canonical root + capability dir）；工具走 `open_read/atomic_write/walk/resolve_dir`；删 `SearchPath`、外部可构造 `WorkspacePath`、两套 `run_blocking`、各工具闭包模板、bash 私有 cwd、散落 `ensure_running`。

已做：

- `tools()` 里 `Workspace::new` 一次 canonicalize + `Dir::open_ambient_dir`
- read/edit/write/glob/grep/bash 捕获 `Arc<Workspace>` 而不是 `Arc<PathBuf>`
- bash cwd → `resolve_dir`；glob/grep → `search_path`
- 测试 helper `WorkspacePath::new` / `SearchPath::new` 转发到 `Workspace`

没做：

- `SearchPath`、`WorkspacePath` 仍是 crate 值对象，各工具仍自己 `open_with` / walker
- `search_path` 和 `resolve_dir` 仍各自 `canonicalize` + `starts_with`
- `run_blocking` / `run_tool_blocking` 还在 `path.rs`，未下沉到 `ToolContext::check` / `run_blocking`
- read/edit/write 闭包里 `ensure_running` 三连还在
- 没有 `Workspace::open_read/atomic_write/walk`

闸门：「只能增加包装而不能删除旧层，停止该项」。当前是 `Workspace` 包着旧路径对象。中间态可合，A4 条目应写成「第一刀：共享 Dir」，不要让人以为路径边界已经唯一。capability `Dir` 实际 open 仍挡 escape，TOCTOU 没削弱。

### A5 — 已勾；这批加切批上限

- 主链 `TurnRunner -> ModelResponse` 未改。
- 新增 8 并行切批，见 §7。
- compact 仍走 `collect_response(..., TurnStats::default())`，不发 Progress（events=None），合同正确。

### A6 — 未完成；做了另一件事

目标：`AgentGroup::create`；create/message 不再重复维护 pending/unread。`submit`/`complete`/`pending: HashSet` 是更早的工作，这批没抽 `create`。

这批实际：

- 删 `SubagentTreeSnapshot` 和 `watch::Sender<Vec<...>>`（对齐「没有 child status query or watch snapshot」）
- 每个 child `session.events()` 转发到 controller `broadcast(256)`
- `activity_event` 丢掉 `Text`/`Thought`/`ToolFinished`，保留 `Started`/`Progress`/`ToolStarted`/`Finished`/`Discarded`
- `AgentEntry::state()` 由 pending 派生，create/message/complete 后发 `StateChanged`

这是新嵌入 API，不是 A6。代价：每个 child 一个永久 forward task，remove 后仍跑到 session 流结束（见 §2）。`list_agents` 仍返回 `SubagentSnapshot { name, state }`，和 TUI 行上 live metrics 不是同一事实源。queue full / 旧 completion / remove 前提这批没碰。create/message 仍复制提交步骤。

同名重建时 TUI 按 `root_id+name` 清旧行、按 `session_id` 认新行——这条是对的。

### A7 — 前半

目标：`sse::stream` 负责 send、错误体上限、request-id/retry-after、typed status、SSE 生命周期；OpenAI `chat_content`/`responses_content` 收 iterator。

已做：

- `read_error_body` 上限 8KB；空 body 用固定文案
- `map_status`：401/403 → `Auth { message }`，429 → `RateLimited { message }`，其余 → `Upstream { status, message }`
- Anthropic / Responses 的 SSE error 同样带 message
- 日志不再 dump SSE data / tool arguments（该留）
- 测试：401 body 保留；decode 失败只记 `event_bytes`

没做：request-id / retry-after；OpenAI content/attachments 成对重复仍在。`ProtocolError: Clone` 完全没动，`Request(error.to_string())` 仍在。A7 不能勾。

先前写成「Completions 不走这套 HTTP 错误体」是错的：`completions.rs:168`、`anthropic.rs:157`、`responses.rs:162` 都调用 `sse::stream`。HTTP 非成功读 body + `map_status` 三家共用。Completions 只是 SSE **decoder** 的 error 映射没改（它走 completions 自己的 `error` 帧，不是 anthropic/responses 那种 `type:error` JSON）。

---

## P0 原清单对照

| 项 | 状态 | 这批 |
| --- | --- | --- |
| 1. collab 锁跨 await | `[x]` | 未改坏 |
| 2. JSONL fsync | `[x]` | 未改 |
| 3. cancel/deadline 竞速 | `[x]` | 未改 |
| 4. 吞持久化失败 | `[x]` | 未改 |
| 5. 非正常 stop 执行 tool | `[x]` | 未改 |
| 6. workdir 包含性 4 处 | `[ ]` | bash 的 canonicalize+starts_with 搬进 `Workspace::resolve_dir`。**仍重复的是同一模式两处**：`search_path` 与 `resolve_dir` 各自 canonicalize + `starts_with(&self.root)`，文案还不同（`path is outside working directory` vs `working directory is outside the session working directory`）。`relative_path` 是逻辑 `..` / `strip_prefix`，不是第三份 canonicalize 检查，不要和前两处算成同一个函数 |
| 7. TUI `unreachable!()` | `[x]` | 未改 |
| 8. 高度累加溢出 | `[x]` | 未改 |

P0.6 不能勾。不要先抽随后又删的 helper，继续跟 A4。

---

## P1 原清单对照

### 用户可见

- TUI Unicode 宽度：未动，仍 `[ ]`
- 输入历史压缩：已 `[x]`，这批未改

### 类型建模

- identity / ToolFinished Result / stop Option / ToolContext::run / ActivityView / ChildSession / `Turn::has_tools`：已 `[x]`，这批未拆
- `tool_display.rs` 硬编码 13 个工具名：未动，仍 `[ ]`，跟 A3

### 错误处理

- `ProtocolError: Clone`：未动，仍 `[ ]`
- `Auth`/`RateLimited` 零载荷：半勾。有 `message`，无 request-id/retry-after。清单应改成「message 已加；元数据仍缺」
- store 可分支错误：已 `[x]`
- bash `CommandFailed`：类型和限长已做；engine `to_string()` 改变失败合同（见 §1）。保持 `[ ]` 直到展示字符串修好

### DRY

- 每请求整体克隆：未动，跟 A3
- ash-tools 打开文件样板 ×3：未下沉到 `Workspace::open_read/atomic_write`，跟 A4
- OverrideBuilder ×3：未动
- `8 * 1024` 字面量：path.rs 已有 `IO_BUFFER_BYTES`，read/bash 是否统一引用这批没清
- 闭包内 `ensure_running` 三连：read/edit/write **仍在**，edit 甚至在进 `run_tool_blocking` 前又 `ensure_running` 一次再构造 `path`
- TUI 折行/省略号/OpenAI content：未动
- collab create/message 重复：未抽 `AgentGroup::create`
- bash 换行计数双实现 / glob-grep 截断提示 / 文案与常量双轨：未动

### P2 / P3

- P2 全未动（无 profiling，正确）
- P3：`SubagentViewState::is_active` 现在用于排序，原「零调用」过时；`accepted: true` / `removed: true` 还在；其余未顺手

---

## 文件级审查

### `DESIGN.md`

- 加了 live Progress 段落；工具改为 bounded batches of eight；collab 改为事件广播、无 watch snapshot；SessionEvent 改为九个事实，并在每次普通模型请求前发出 Context 估算事件。
- 与工作树一致，与 A1 验收不一致。合 live stats 则 A1 文档必须一起改；不合则 DESIGN 这 23 行应回滚。

### `crates/ash-core/src/event.rs`

- 新增 `Progress { turn_id, stats: TurnStats }`。无 serde，符合公开流约定。
- 一旦存在，所有 exhaust match 必须处理。CLI 已加臂；TUI 父会话 / 子代理都消费。漏 match 会编不过，这点安全。

### `crates/ash-core/src/error.rs`

- `Auth { message }` / `RateLimited { message }`：正向。
- `CommandFailed { status, output }`：正向。Display 不应成为 `ToolCall.result` 的序列化格式。

### `crates/ash-core/src/tool.rs`

- `FnTool` 私有；空 name/description 拒绝；测试覆盖。小且干净。

### `crates/ash-agent/src/engine.rs`

- `collect_response` 增加 `settled_stats: TurnStats`，Progress = settled + 当前 accumulator。retry 前先 `saturating_add` 再发下一轮，live 数字含被丢弃的 attempt，与 A5「retry 计入本 Turn」一致。
- usage 用 `max` 再比较 previous，重复帧不发 Progress。测试 `progress_is_published_only_when_usage_changes` 覆盖 (12,0)+(0,3) → (19,2) 然后 (19,5)。
- `execute_tools` 切批 8：见 §7。
- `limit_tool_result` 处理 `CommandFailed`：对；随后 `to_string()`：错，见 §1。
- `MockModel` 改 `VecDeque` 多响应：测试需要，可留。

### `crates/ash-agent/src/mcp.rs`

- 文本块改 iterator `collect::<String>()`，多块仍无分隔（原行为）。这文件这次改过拼接，顺手加 `"\n"` 比留下粘连更值。不是阻塞。

### `crates/ash-agent/src/session.rs`

- `publish` 按 variant 打结构化日志，文本/thought 只记 chars。该留。
- `Progress` 走 debug。频率等于 provider usage 帧，比文本 delta 低，可接受。

### `crates/ash-protocol/src/sse.rs`

- HTTP 非成功先读 body 再 `map_status`，不再丢响应体。上限 8KB，chunk 失败 break，不 panic。
- 日志去内容：decode 失败只记 `event_bytes`；`log_model_event` 不打 Text/Reasoning/tool arguments。该留。
- 测试绑本机 401。无 request-id/retry-after。这条 HTTP 错误体路径三家 adapter 共用（都调 `sse::stream`）。

### `crates/ash-protocol/src/anthropic.rs` / `responses.rs`

- SSE error 的 Auth/RateLimited 带 message。测试断言 `"Slow down"`。Completions 未改。

### `crates/ash-tools/src/path.rs`

- `Workspace { root, dir: Arc<Dir> }`：正确的能力对象。
- `path()` 只做相对清洗，不在这里 canonicalize 文件（symlink 交给 cap-std open）——对。
- `search_path()` / `resolve_dir()` 仍独立 canonicalize+starts_with，P0.6 未关。
- `SearchPath::relative` 对 walker 路径 `strip_prefix(&self.workspace)`：walker 根是 canonicalize 后的 `full_path`，与 `workspace`（canonical root）一致时成立。

### `crates/ash-tools/{read,edit,write,glob,grep,bash,lib}.rs`

- 构造期共享 `Arc<Workspace>`：A4 第一刀。
- bash 非零退出改 `CommandFailed`：类型对；展示合同见 §1。
- bash `None => working_dir.root().to_path_buf()`：root 已 canonical，与旧 `to_path_buf` 在已 canonical 的 Arc 上等价。
- glob/grep 测试 helper 包一层 `Workspace::new`：可接受，避免测工具重复 canonicalize 语义。

### `crates/ash-collab/src/event.rs`（未跟踪）

- `SubagentEvent { root_id, session_id, name, kind }`；`kind`: `StateChanged` / `Session(SessionEvent)` / `Removed`。
- 提交时必须 `git add`。无 serde，符合「公开流无 serde」；若嵌入方要持久化这不是领域记录。

### `crates/ash-collab/src/control.rs`

- `broadcast::channel(256)` 替换 watch。`publish` 变成 `let _ = send`。无订阅者时事件丢——交互模式 CLI 会先 `control.events()` 再 start session，顺序目前安全；纯库调用若先 create 再 subscribe 会丢初始 StateChanged。watch 有最新值，broadcast 没有。DESIGN 已说无 watch snapshot，这是有意的，但嵌入方必须先 subscribe。
- `forward_events`：child lag 打 warn 后 continue，投影出现空洞（见 §4）。
- `activity_event` 过滤 Text/Thought/ToolFinished：减少总线负载，子代理行因此不能显示「正在输出」只能显示 stats/tools。产品合同要写明。
- `futures` 从 dev-dep 升到正式依赖：`Stream` bound 需要，合理。
- 测试 `child_session_progress_is_wrapped_with_its_identity`：覆盖 identity 包装，不覆盖 Removed 复活、不覆盖 lag。

### `crates/ash-collab/src/snapshot.rs` / `lib.rs`

- 删 `SubagentTreeSnapshot`。`SubagentSnapshot` 仍给 `list_agents` JSON。Deserialize 仍无 TUI 消费者（P3 死代码那条部分仍对）。

### `crates/ash-cli/src/modes.rs`

- 删 `map_subagent_monitor` / `subagent_views` 转换层：少一层 DTO，正向。
- `subagent_update` 把 collab 事件译成 TUI 类型：装配层该做的，terminal 类型没进 agent。
- print 模式不订 subagent 事件：对。
- 交互模式 `event_tx` 容量 64 混流：见 §5。
- `SessionEvent::Progress` 在 print 模式被 `_ => {}` 忽略：对，print 不应刷 usage。

### `crates/ash-tui/src/subagent.rs`

- `SubagentView` 增加 `session_id`、`stats`、`tool_calls`、`active_turn`。从「name/state 投影」变成「可累加的小会话」。A1 删除清单明确不要 `SubagentView.usage`。
- `SubagentUpdate` / `SubagentUpdateKind` 是 CLI→TUI 的装配 DTO，可留在 tui，不要进 core。

### `crates/ash-tui/src/app.rs`

- `update_subagent` 建行逻辑：见 §2。同名不同 session_id 时先按 name 清旧行，这条对。
- `update_subagent_session`：错位 turn_id 的 Progress/ToolStarted/Finished/Discarded 走通配忽略，与父会话 TurnId 守卫同构，好。
- 父会话 Progress 要求 `current_turn_id` 匹配且 `accepts_live_output()`：与 Text/Thought 一致。
- 测试 `subagent_events_are_scoped_by_root_and_session` 只 filter 断言，不证明 `select_session` 会丢掉他根行（事实上不会丢）。测试名略夸大。
- `child_session_events_project_live_stats_and_tool_count` 覆盖正向路径，无 Removed、无错位 turn、无 lag。

### `crates/ash-tui/src/viewport.rs`

- 渲染时按 `session_id` 过滤 subagents：画面正确；状态堆见 §6。
- `activity_metrics`：`stats == default && tool_calls == 0` 才空。只有 tool、还没有 usage 时画出 `0 in / 0 out · 3 tools`。宽不够时整段 metrics 丢掉，只留名字。
- `TURN_STATS_STATUS_WIDTH = 80`：测试覆盖 79 vs 80。没测「仅 tool_calls」。
- 子代理行：名字+metrics 放得下才拼，否则只截名字、宁可不显示数字。产品上可接受，应在测试里写明。
- `TurnId` 仅测试用，import 在 `#[cfg(test)]` 里，生产编译没问题。

---

## 正向（应保留）

- SSE / session / model event 日志去内容：不打 body、不打 tool arguments、不打文本。
- HTTP 4xx/5xx 读 body、上限 8KB、Auth/RateLimited 带 message，有测试。
- Progress 只在 usage 实际变化时发；`max` 合并语义有测试。
- 子代理 identity 用 `root_id + session_id`；同名重建先按 name 清旧行。
- `FnTool` 私有、空元数据拒绝。
- 工具组一次打开 Workspace Dir，不再每个工具 `open_ambient_dir`。
- 删 collab watch → CLI `map_subagent_monitor` 转换层。
- `CommandFailed` 类型本身（修好 Display 映射之后）。

---

## 不要勾

- A3、A4、A6、A7
- P0.6 路径包含性
- P1 `ProtocolError: Clone`、P1 Auth 完整版（request-id/retry-after）、P1 `CommandFailed`（直到 §1 修好）
- A1 维持 `[x]` 仅当文档改为承认 live Progress；否则这批是 A1 回退，不能一边勾一边合

## 建议拆提交

1. `Workspace` 共享 Dir（A4 第一刀）+ bash `CommandFailed`（含 Display→tool result 的显式格式化）
2. SSE 错误体 + `Auth`/`RateLimited { message }`（A7 切片，注明还缺 request-id）
3. live `Progress` + 子代理事件（单独产品/设计变更，带着 A1 / DESIGN / 完成标准一起改；含 §2 复活和 §6 切 session 过滤）

## 合入前验证

- [ ] 修 `CommandFailed` 的工具结果字符串，补回归：非零退出的模型可见文本 / TUI 失败预览不得把 Display 整句当 output 主体
- [ ] `Removed` 后 Progress/Finished 不得建行；remove 停 forward；补测试
- [ ] 统一 A1 与 DESIGN 的 stats 语义，或撤回 live Progress
- [ ] `select_session` 与 `refresh_status` 对齐当前 root
- [ ] `git add crates/ash-collab/src/event.rs`
- [ ] 至少跑：`bounds_parallel_tool_execution_without_reordering_results`、`progress_is_published_only_when_usage_changes`、`child_session_progress_is_wrapped_with_its_identity`、`child_session_events_project_live_stats_and_tool_count`、`reports_nonzero_exit_as_a_typed_error`、`preserves_http_error_details_without_unbounded_body_reads`
- [ ] 再跑 `cargo fmt --all -- --check`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`（本审查未跑）
