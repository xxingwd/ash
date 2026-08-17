# Session 架构收敛 TODO

## 目标

统一 Ash 的执行模型：

```text
Agent -> Runtime -> Session -> Turn -> ModelCall
```

- `Agent`：不可变行为定义，包括模型、提示词、工具和上下文策略。
- `Runtime`：模型客户端、Session 存储等运行能力。
- `Session`：独立、持久化、串行执行的 Agent 会话。
- `Turn`：Session 中一次完整执行及其结果。
- 根 Agent 和子 Agent 必须走同一套 Session/Turn 执行路径。
- 协作层只维护代理树投影和调度策略，不维护第二套执行队列。
- 不兼容旧 `Thread` 数据和旧协议字段；需要时直接清空本地数据。

## 约束

- 每个阶段独立提交，避免把身份、生命周期、事件和存储改造混在一起。
- 优先修复领域错误，再做命名和 API 精简。
- 不为兼容保留旧类型、旧字段、旧目录或旧协作工具。
- 类型表达真实领域语义，避免用 `String` 同时表示文件路径和代理路径。
- 不把产品层 UI 状态放进 `ash-core` 或 `ash-agent`。
- 保留用户界面的 `chat` 文案；`Session` 是架构词，`chat` 是产品词。

---

## P0：Session 身份与父子关系

### 当前问题

`SessionOptions` 同时包含执行配置和协作身份：

```text
working_dir
tool_timeout
path
tree_id
kind
```

其中 `path` 默认取当前工作目录，但协作层把它当 `/root/task` 形式的代理路径。这会把文件系统路径和代理树路径混在一起。

`TreeId` 本质上又由根 `SessionId` 派生，`SessionKind` 也可以从是否有父 Session 推导。

### 任务

- [x] 新增强类型 `AgentPath`，只表达协作树中的规范路径。
- [x] 新增 Session 身份结构，至少包含：
  - [x] `id: SessionId`
  - [x] `root_id: SessionId`
  - [x] `parent_id: Option<SessionId>`
  - [x] `path: AgentPath`
- [x] 根 Session 的身份规则固定为：
  - [x] `root_id == id`
  - [x] `parent_id == None`
  - [x] `path == /root`
- [x] 子 Session 的身份规则固定为：
  - [x] `root_id == parent.root_id`
  - [x] `parent_id == Some(parent.id)`
  - [x] `path == parent.path / task_name`
- [x] 从 `SessionOptions` 删除 `path`、`tree_id`、`kind`，只保留执行范围配置。
- [x] 删除 `TreeId`，协作树直接用根 `SessionId` 标识。
- [x] 删除 `SessionId -> TreeId` 转换。
- [x] 删除 `SessionKind`，或仅在证明无法由 `parent_id` 推导时保留。
- [x] `ToolContext` 直接携带当前 Session 身份，不再临时拼装树身份。
- [x] JSONL header 持久化 `root_id`、`parent_id`、`path`。
- [x] Session 列表通过 `parent_id.is_none()` 选择根 Session，而不是依赖 `SessionKind`。

### 验收

- [x] CLI 创建的根 Session 路径恒为 `/root`，与工作目录无关。
- [x] 子 Session 有可追溯的 root/parent/path。
- [x] 不存在 `TreeId`、`tree_id`、字符串型裸代理路径。
- [x] 工作目录只影响 IO 工具，不影响代理寻址。
- [x] 重启后能从持久化元数据恢复 Session 父子关系。

---

## P0：Turn 成为完整完成边界

### 当前问题

`Session` 已生成完整 `TurnView`，但 `Turn::wait()` 只返回 `TurnResult`。子代理完成后还要调用 `Session::messages()` 扫描整段历史，才能找到本轮最终回答。

### 任务

- [x] 将 `Turn::wait()` 改为返回 `Result<TurnView, AshError>`。
- [x] actor completion channel 直接发送与 `EventKind::Turn` 相同的 canonical `TurnView`。
- [x] 已落盘的模型执行失败返回 `TurnView { result: Failed(...) }`。
- [x] 只有 actor 关闭、持久化边界损坏等无法形成 TurnView 的基础设施失败才返回 `Err`。
- [x] 子代理直接从 `TurnView.messages` 提取最终 assistant message。
- [x] 删除子代理完成后的 `Session::messages()` 二次查询。
- [x] 审查 `Session::messages()` 的剩余调用；若 `Session::view()` 足够则删除该公共 API。
- [x] CLI print 模式从完成的 `TurnView` 读取 usage，不再额外读取整个 Session view。

### 验收

- [x] `Turn::wait()` 与 `EventKind::Turn` 提供同一份 TurnView。
- [x] 子代理完成不扫描 Session 全历史。
- [x] 模型失败、取消、中断和正常完成各有回归测试。
- [x] 一个 Turn 的结果、消息、usage 和 context tokens 只有一个权威来源。

---

## P0：删除 ash-collab 的第二套执行队列

### 当前问题

`Session` 已拥有 turn queue、inbox、submit、notify、steer 和 cancellation，但 `ash-collab` 又维护：

```text
ChildCommand
command_tx
run_child loop
pending_followups
ChildState lifecycle
```

当前调用链多了一层：

```text
AgentControl queue -> ChildCommand -> run_child -> Session queue -> Turn
```

### 任务

- [x] `ChildRecord` 直接持有 `Session`。
- [x] `message_agent(start_turn = false)` 直接调用 `Session::notify()`。
- [x] `message_agent(start_turn = true)` 直接调用 `Session::submit()`。
- [x] 删除 `ChildCommand` 和每个子代理的 command channel。
- [x] 删除 `run_child` 消费循环。
- [x] 每个已提交 Turn 启动一个只负责观察完成结果的任务。
- [x] 协作层只保存 UI/调度投影：
  - [x] task path
  - [x] profile
  - [x] 当前活动 Turn 的取消能力
  - [x] 最近完成结果
  - [x] completion revision / wait cursor
- [x] Turn 顺序完全交给 Session actor，不在 collab 中再次排队。
- [x] `interrupt_agent` 只取消当前活动 Turn，不关闭 Session。
- [x] 并发限制基于真正处于执行状态的子 Session/Turn，明确排队 Turn 是否占槽。
- [x] 删除可由 TurnView 推导的重复 `ChildState` 分支。

### 验收

- [x] Collab 中不存在第二套消息队列。
- [x] 多个 follow-up 的执行顺序由 Session 测试覆盖。
- [x] notify 不开启 Turn，follow-up 开启 Turn。
- [x] interrupt 后 Session 仍可接收新 Turn。
- [x] wait 不会把中间 follow-up 错判为代理最终完成。

---

## P1：AgentRole 收敛为 AgentProfile

### 当前问题

子代理角色目前是写死的 enum，并分别编码名称、描述、prompt 和工具删减规则。根代理使用 `Agent`，子代理使用 `Agent + AgentRole` 特殊构造，仍不是完全统一的数据模型。

### 任务

- [x] 引入 `AgentProfile`：
  - [x] `name`
  - [x] `description`
  - [x] `prompt_overlay`
  - [x] `tool_policy`
  - [x] 可选 model override
  - [x] 可选 turn/深度限制
- [x] 内置 `default`、`explorer`、`worker` profile。
- [x] 根 Session 也显式使用 profile，不让 profile 成为“仅子代理”概念。
- [x] 工具策略使用 allow/deny 数据结构，不在 match 中硬编码删工具。
- [x] `ChildAgent` 改名为 `ChildSessionSpec`。
- [x] `AgentSpawner` 改名为 `ChildSessionFactory`。
- [x] `SpawnRequest` 改名为 `ChildSessionRequest`。
- [x] profile 解析、工具过滤和 prompt overlay 各自保持单一实现。

### 验收

- [x] 新增 profile 不需要修改 Session 执行引擎。
- [x] explorer 的只读能力由测试验证。
- [x] 子 Session 默认继承父 Agent/Runtime，只应用显式 profile override。

---

## P1：SessionEvent 与 UiEvent 分层

### 当前问题

`ash-core::EventKind` 同时包含运行时事件和 CLI/TUI 命令结果：

```text
运行时：TurnStart / Live / Turn / automatic Compacted
产品层：SessionsListed / ForkPointsListed / Restored / SessionForked / Error
```

CLI 还会自行构造 `EventKind` 发送给 TUI，导致“Session 是唯一事件发布者”的约束不成立，并丢失 event envelope。

### 任务

- [x] `Event` 改名为 `SessionEvent`。
- [x] `EventKind` 改名为 `SessionEventKind`。
- [x] Session 运行事件只保留：
  - [x] `TurnStarted`
  - [x] `Live`
  - [x] `TurnCompleted(TurnView)`
  - [x] `ContextCompacted`
- [x] CLI/TUI 定义独立 `UiEvent`：
  - [x] `Session(SessionEvent)`
  - [x] `SessionsListed`
  - [x] `ForkPointsListed`
  - [x] `SessionRestored`
  - [x] `SessionForked`
  - [x] `RollbackCompleted`
  - [x] `CommandFailed`
- [x] TUI 不再接收裸 `EventKind`。
- [x] Session 事件始终保留 session ID、turn ID、sequence 和 timestamp。
- [x] `ash-core` 删除 session picker、命令失败等产品层事件。

### 验收

- [x] Session actor 是 SessionEvent 的唯一发布者。
- [x] CLI 只负责把 SessionEvent 和命令结果组合成 UiEvent。
- [x] TUI 对 UiEvent 穷尽匹配。
- [x] 命令错误不会伪装成 Session 运行错误。

---

## P1：ToolContext 按 Session 收敛

### 当前问题

`ToolContext` 已有 `session_id`，却又嵌套一个 `AgentToolContext { tree_id, path, messages }`。这些字段描述的是调用 Session 的快照，不是 `Agent` 行为定义。

### 任务

- [x] `AgentToolContext` 改为 `SessionToolContext`。
- [x] `ToolContext.agent` 改为 `ToolContext.session`。
- [x] Session tool context 使用 Session 身份结构，不重复 tree/path 字段。
- [x] Turn 相关字段保持在 ToolContext 顶层或单独 `TurnToolContext`。
- [x] 工具只拿执行所需的只读 Session 快照，不暴露 Runtime、模型密钥和可变 Session。

### 验收

- [x] `context.agent` 不再存在。
- [x] 工具上下文中 Agent 表示行为定义，Session 表示运行实例，语义无混用。
- [x] 普通文件/shell 工具不依赖协作字段。

---

## P1：明确子 Session 的持久化策略

### 当前问题

子 Session 当前写入 JSONL，但被根 Session 列表隐藏，也不能作为根 Session resume；其 parent/root/path/profile 又没有完整持久化，重启后成为隐藏孤儿。

### 决策

默认采用 durable child session：每个子代理 Session 都持久化完整 lineage。暂不采用“写入但不可恢复”的中间状态。

### 任务

- [x] Session header 持久化身份和必要执行元数据（lineage/身份已持久化；profile 未落盘）。
- [x] SessionStore 支持按 `root_id` 查询整棵 Session tree。
- [x] 普通 session picker 默认只列根 Session。
- [x] 删除根 Session 时定义并测试子树清理行为（`delete_tree` 先锁定整棵树，子节点优先、根最后删除）。
- [x] 明确重启后子 Session 是可查看、可恢复执行，还是只读历史：子会话为 durable 只读历史，不可作为根恢复。
- [x] 若未来需要 ephemeral child，使用显式 `PersistencePolicy`，不要通过 `SessionKind` 隐式判断（当前无 ephemeral 需求，不提前引入该枚举）。

### 验收

- [x] 磁盘上不存在无法归属到根 Session 的子 Session。
- [x] 根列表、树列表和单 Session load 的语义分别明确。
- [x] 数据清理按 Session tree 工作。

---

## P2：精简 SessionStore

### 当前问题

`SessionStore` 同时暴露 `create`、`load`、`open`、`list(excluded)`、`open_writer`。其中 `create` 只有测试使用，`list(excluded)` 把调用方过滤策略塞进存储，具体 `SessionWriter` 也被公开导出。

### 任务

- [x] 删除只为测试存在的 `SessionStore::create`。
- [x] 评估并删除生产路径不需要的 `load`，测试改走正式 open/replay 路径（评估后保留 `load` 作为文档化的非加锁读取：活 Session 持有排他 writer，测试/内省仍需无锁路径）。
- [x] `open_writer` 改为语义明确的 `open_new`。
- [x] `list(excluded_session)` 改成 `list(filter)` 或无参数 `list_roots()`。
- [x] `Runtime::sessions()` 改为 `list_sessions()`。
- [x] `SessionWriter` 变为 JSONL backend 私有实现，不从 `ash-agent` 根导出。
- [x] `SharedSessionStore` 若只在 crate 内使用则降为 crate-private。
- [x] 检查 `StoredSession` / `OpenedSession` 是否应成为公开 API。

### 验收

- [x] SessionStore 每个方法都有生产调用者和明确语义。
- [x] 测试不驱动公共 API 膨胀。
- [x] Runtime/Session 不依赖 JSONL 具体 writer 类型。

---

## P2：严格执行唯一新存储格式

### 任务

- [x] 删除 `SessionHeader.kind` 上的 `#[serde(default)]`。
- [x] 所有必需 header 字段缺失时直接拒绝文件。
- [x] `find_session()` 只访问规范文件名 `<session_id>.jsonl`，删除目录 fallback 扫描。
- [x] 保持旧 `thread_*` 字段、目录和 alias 为零。
- [x] 新增严格 header 序列化快照和缺字段失败测试。
- [x] 数据结构升级时采用明确版本字段；当前阶段不做旧格式兼容。

### 验收

- [x] 一个 SessionId 对应唯一规范路径。
- [x] 缺失身份字段不会静默降级为根 Session。
- [x] 存储层不存在兼容性猜测逻辑。

---

## P2：精简 Fork API

### 当前问题

`Fork` 暴露 session、messages、model、protocol、working_dir、prompt，但生产代码只使用 session 和 prompt。

### 任务

- [x] `Fork` 改名为 `ForkedSession`。
- [x] 只保留 `session` 和 `prompt`。
- [x] 删除 `ForkData` 中未使用的 model/protocol/working_dir/messages。
- [x] fork 后的展示状态统一从新 Session 的 `view()` 获得。
- [x] fork 使用新的 Session identity，正确设置 root/parent/path 语义。

### 验收

- [x] Fork 返回值只包含调用方实际需要的领域数据。
- [x] fork 不携带隐式 UI setup。

---

## P2：删除旧协作工具兼容

### 任务

- [x] 删除 `LEGACY_COLLABORATION_TOOL_NAMES`。
- [x] 删除 `send_message`、`followup_task`、`list_agents` 的兼容检测和清理逻辑。
- [x] 只保留当前公开工具：
  - [x] `spawn_agent`
  - [x] `message_agent`
  - [x] `interrupt_agent`
  - [x] `wait_agent`
- [x] 确保 prompt、schema、测试和工具安装逻辑只有一套名称。

### 验收

- [x] 全仓旧协作工具名扫描为零。
- [x] 工具安装不包含兼容分支。

---

## P3：低风险命名清理

- [x] `SessionState` -> `SessionActorState`。
- [x] `Runtime.sessions` -> `Runtime.session_store`。
- [x] `AgentStart` -> `TurnStartOutcome`。
- [x] `agent_started()` -> `turn_started()`。
- [x] tracing 文案 `agent event` -> `session event`。
- [x] `duplicate agent input` -> `duplicate session input`。
- [x] `AgentSnapshot.agent_id: SessionId` -> `session_id`。
- [x] 区分 `AgentSnapshot`（行为/角色视角）和 `SessionSnapshot`（运行实例视角），只保留真正需要的一种。
- [x] 检查所有 `agent_id: SessionId`，按语义改为 `session_id`。

### 不机械修改

- 用户界面的 `chat` / `saved chat` 文案保留。
- `Agent` 作为不可变行为定义保留。
- `spawn_agent` 和 `subagent` 作为产品概念保留。
- 真正表示 Tokio/OS 工作线程的 `worker threads` 保留。

---

## 推荐提交顺序

1. `ash-agent: add typed session identity`
2. `ash-agent: return canonical turn views from wait`
3. `ash-collab: delegate child queues to sessions`
4. `ash-collab: introduce data-driven agent profiles`
5. `ash-core: separate session events from UI events`
6. `ash-core: expose session-aware tool context`
7. `ash-agent: persist session lineage`
8. `ash-agent: simplify session store`
9. `ash-agent: simplify fork results`
10. `ash-collab: remove legacy collaboration tools`
11. `refactor: finish session terminology cleanup`

每个提交都必须执行：

```text
cargo fmt --all -- --check
cargo test <受影响 crate>
cargo check --workspace
git diff --check
```

阶段性完成后执行：

```text
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

若全仓测试或 Clippy 被已有无关问题阻塞，必须记录具体文件、行号和失败信息，不在架构提交中夹带无关修复。
