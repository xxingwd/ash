# Ash 当前架构

本文只描述已经接入主链路的实现。研究材料和未来设想不作为实现规范。

## 设计原则

1. `ConversationLog` 是会话的唯一事实源，完整历史和模型上下文都是投影。
2. Agent 的静态定义、单次运行作用域和运行时依赖分开组合，不以一个万能 context 传递。
3. `ash-core` 与 `ash-agent` 只依赖 provider-neutral 的模型契约。
4. 聊天消息、定时任务、心跳和子 Agent 都先规范化为 `AgentInput`，触发源不进入执行循环。
5. 工具只获得本次调用需要的最小能力；工作目录等产品能力在工具构造时注入。
6. Session、Run 和 Turn 拥有独立 ID，取消和 deadline 一直传递到工具执行。
7. 协议层必须真实流式输出，供应商返回的工具调用 ID 必须原样保留。
8. UI 不拥有 Agent 状态，只消费事件并发送命令。

## Crate 边界

### `ash-core`

共享数据结构和接口：

- `Message`、`ContentBlock`、`Event`、`StopReason`
- `ModelClient`、`ModelRequest`、`ModelStreamEvent`
- `Tool`、最小化的 `ToolContext`、`ToolError`
- `SessionId`、`RunId`、`TurnId`、其他实体 ID 和取消令牌

该层不包含协议枚举、API Key、base URL、Agent prompt、工具注册表或终端状态。

### `ash-protocol`

负责把统一模型请求转换为供应商请求，并把 SSE 事件转换为统一模型流事件。

- `Protocol` 和 `ProviderConfig` 只存在于该层
- 每个 adapter 实现 `ash_core::ModelClient`
- `sse` 模块统一管理连接、错误和禁止自动重放 POST 请求
- 每种协议拥有独立的有状态 decoder
- 分片工具参数在 decoder 内聚合完成后才交给 Agent
- `ASH_MODEL_CONFIG` 在协议生成 body 后以受限的点路径覆盖模型参数，不影响消息、工具和流式结构

已实现：

- Anthropic Messages
- OpenAI Chat Completions
- OpenAI Responses

### `ash-tools`

提供工具定义和内置工具。

`ash_tools::tools(working_dir, enabled)` 在构造阶段绑定 coding workspace；工作目录不会通过
通用 `ToolContext` 暴露给所有扩展工具。
`read`、`write` 和 `edit` 通过已打开的工作目录 capability 执行文件访问，符号链接不能
逃逸到目录外，也不存在校验后重新打开裸路径的竞态。Agent 执行器统一管理工具取消和
超时；`bash` 子进程启用 `kill_on_drop`，执行 Future 被丢弃时会终止进程。
`webfetch` 只请求 HTTP/HTTPS URL，在流式读取阶段限制响应大小，并将 HTML 转为 Markdown。
`glob` 和 `grep` 使用纯 Rust 遍历，搜索范围限制在工作目录内，遵守 ignore 文件且不跟随
符号链接；前者匹配文件路径，后者按正则搜索内容，两者都限制为最多 100 条结果。

### `ash-agent`

该 crate 内保留三个概念边界，但暂不拆成更多 crate：

- Definition：`AgentDefinition` 描述模型、系统提示词、工具和 `ContextPolicy`
- Runtime：`AgentRuntime` 注入 `ModelClient` 和 `ConversationRepository`
- Session/Engine：`AgentScope` 描述工作目录、工具时限和 Agent 路径；`AgentSession`
  管理连续对话，内部 engine 只负责执行一个 turn

这种划分保留了可替换接口，同时避免 harness/core 物理拆分造成大量转发类型。产品可以直接
组合 `(AgentDefinition, AgentScope)`，也可以继续使用完整的 `AgentConfig`。

所有外部触发统一为 `AgentInput`：

- `Trigger` 区分 user、steering、follow-up、scheduled、heartbeat、system 和 child-agent
- `content` 使用统一多模态 `Content`
- `idempotency_key` 用于同一 Session 内拒绝重复投递
- `metadata` 保留渠道或调度器附加信息，不改变模型消息结构

`AgentRuntime::start` 返回 `AgentRun`。它暴露取消句柄和带 `SessionId`、`RunId`、`TurnId`、
单调 sequence、timestamp 的 `RunEvent`，适合聊天网关将事件可靠路由回原会话。连续且持久化的
产品路径使用 `AgentSession::submit_input`；同一个 Session 必须由产品层串行化写入。

`AgentSession` 持有一个 `ConversationLog`。日志记录 accepted input、消息、上下文 checkpoint
和 rollback；完整历史与发送给模型的上下文均由该日志重放得到，不维护第二份可漂移的消息数组。
`ConversationStore::append(expected_revision, entry)` 使用乐观 revision 防止同一 writer 的
陈旧写入，`ConversationRepository` 负责创建、恢复和列举会话。默认实现是 append-only JSONL，
其他产品可以替换为数据库或事件存储；跨进程并发仍需由具体 repository 提供事务或租约。

`ContextPolicy` 负责从会话投影准备模型上下文。默认 `CodingContextPolicy` 会裁剪旧工具输出并在
阈值处生成摘要 checkpoint；`PassthroughContextPolicy` 适合不压缩的产品。长期记忆检索可以通过
新的 policy 或产品组装层加入，不应写死在 Agent engine 中。

每次提交的 engine 顺序为：

1. 原子接受并持久化 `AgentInput`。
2. 通过 `ContextPolicy` 生成本轮模型上下文，必要时追加 checkpoint。
3. 调用注入的 `ModelClient` 并逐项转发文本、thinking 和 usage 事件。
4. 持久化完整 assistant 消息和顺序执行的工具结果。
5. 如果存在工具调用则继续请求模型，否则结束本轮。

默认 repository 为每个交互会话保存一个带日期文件名的 append-only JSONL。`AgentSession`
在 accepted input、完整 Assistant 消息和工具结果这些可恢复的语义边界同步追加并 flush；
首行的配置快照包含恢复模型上下文所需的信息，但不包含 API Key、Token 或自定义
接口地址内容。`/new` 立即生成 Session ID 和创建时间，但会话文件延迟到第一条用户
消息时创建；会话标题由重放后的第一条有效用户消息派生。`/resume` 列出其他已保存
会话的标题和创建时间，选中后按 Session ID 恢复 `Vec<Message>`。`/undo` 列出当前会话
中的真实用户消息，选中后把此前消息复制到新的 Session JSONL，并将所选输入恢复为草稿；
原 Session 保持不变。取消尚未形成响应的当前 turn 时，rollback 仍以 append-only 记录保存；
重放 JSONL 时按顺序投影掉对应用户轮次，不重写已有文件。旧的 user/assistant/tool JSONL
记录仍可读取，新输入则额外保存 trigger、幂等键和 metadata。

提示词由固定基础约定和启动时上下文组合而成。动态上下文只包含环境信息、从 Git 项目根到
当前目录生效的 `AGENTS.md`，以及 `.agents/skills` 中 Skills 的名称与描述；运行时 `skill` 工具按名称注入完整指令、
基础目录和资源文件列表。显式启用的 Skill 会在启动时追加完整指令，并可覆盖模型或
内置工具集合。MCP 仍属于独立能力，尚未成为默认交互链路的一部分。

### `ash-tui`

交互界面是由 Ratatui 管理的 inline TUI：

- 进入时启用 raw mode 和 bracketed paste，不进入 alternate screen，也不捕获鼠标；正常
  退出、错误与 Drop 路径使用同一个幂等 Guard 恢复终端
- UI 持有当前 turn 的 live transcript，以及不含 Ratatui Buffer 的已完成语义历史；
  viewport 按实际内容高度动态伸缩，最大为首屏
- live transcript 使用独立滚动位置并默认跟随底部；已完成历史交给终端原生 scrollback
- `AgentFinished` 是唯一的正常 turn 提交边界：先收缩到 Composer，再在一次同步更新中把
  用户消息、Thought、工具、回答和 Worked 块写到 scrollback，随后转存语义块并释放缓存
- `/new` 和 `/clear` 清空模型历史、可见屏幕与 scrollback；`/resume` 从 JSONL 重放历史
- `/undo` 从历史用户输入中选择 fork 点，创建新 Session 并重放所选输入之前的历史；
  原会话不回退
- resize 先清空可见屏幕和 scrollback，再从语义历史按当前宽度完整重放；重放每次只
  构建一个块的 Buffer，插入后立即释放
- 除全量重放边界外，终端模拟器负责历史滚动、选择与复制
- Agent 工作期间 `Ctrl-C` 不发送取消命令；状态栏提示 `esc to interrupt`，第一次按
  `Esc` 不展示额外状态
- Agent 工作期间按一次 `Esc` 会取消并回退当前轮，将问题恢复为草稿
- `Esc` 会先在 UI 侧立即移除当前 turn 并恢复 Composer；后端确认回退前到达的旧
  Thinking、工具和文本事件不再参与渲染，确认事件也不会重复清除同一区域
- 回退按 turn ID 从 live transcript、已完成语义历史和模型上下文移除当前轮，然后清空并
  重放终端历史
- 流式思考摘要显示带耗时的 `Thinking` 标题和最近五行正文，并随 transcript 向下滚动；
  切换到回答、工具或完成状态时折叠为不可展开的单行耗时摘要
- 工具历史按工具语义生成单行摘要，隐藏内容参数和默认参数，并把绝对路径缩短为
  可辨识的文件名或末级目录；不把原始工具参数 JSON 加入 transcript
- TUI 只聚合连续的原生 `read` 调用；`bash` 始终显示实际命令，不做 Shell 意图猜测，
  也不改变底层调用、结果或持久化数据
- 历史和 viewport 使用同一套块布局协议：完整块只声明内容高度并管理内部
  padding，父级 Stack 使用 `Flex::Start` 和统一 `spacing` 排列块；输入框与模型、
  路径或补全 footer 组成一个 ComposerBlock，continuation 只表示同一流式块的后续行

模块分工：

- `app`：事件与命令协调
- `operation`：互斥操作状态、取消/回退阶段和迟到事件路由
- `block_layout`：完整块、continuation 与 Ratatui Flex spacing
- `input`：Unicode 安全的编辑、历史和可视窗口
- `viewport`：transcript 窗口、Composer 组件、行高、间距和 Buffer 渲染
- `inline`：live turn、提交边界、滚动位置、流状态和 viewport 生命周期
- `inline_surface`：动态 inline viewport、scrollback 插入、raw mode 和终端恢复

输入框的跨进程历史使用独立的 `history.jsonl`；它只负责上下键召回，不参与模型
上下文恢复。恢复会话时 UI 根据语义消息重新渲染对话，不解析终端输出。

### `ash-cli`

负责参数、环境变量、Skill 应用和交互控制器。

控制器独占 `AgentSession`、命令接收端、事件发送端和输入历史存储，保证同一会话同一
时间只运行一个 Agent turn。活动 turn 通过带类型的结束结果返回延迟提交、回退或退出，
不通过可变出参隐式修改外层状态；UI 发出取消命令时，控制器触发当前 turn 的
`CancellationToken`。

### `ash-orchestrator`

提供已经接入默认 CLI 的 Codex 风格子 Agent 管理。

- 内置 `default`、`explorer`、`worker` 三种角色
- 默认采用主动但有边界的委派策略：独立任务同轮并行，简单或紧耦合任务留在本地
- `spawn_agent`、`send_message`、`followup_task`、`interrupt_agent`、
  `list_agents`、`wait_agent` 六个工具
- `ChildAgentFactory` 是创建子 Agent 的唯一边界；默认 factory 捕获父级的 model、definition
  和 scope 模板，Supervisor 本身不读取 provider 配置
- 子 Agent 继承当前模型、工作目录、工具和完整系统上下文
- `fork_turns` 支持不继承、完整继承或只继承最近 N 轮
- 每个子 Agent 保留独立消息历史，可以在完成或中断后继续 follow-up
- 根 Session ID 是 Agent 树的隔离边界；新会话不会看到旧树
- 默认允许三个并发子 Agent，加上根 Agent 共四个执行槽

协作工具只从 `ToolContext` 读取 Session/Run/Turn 标识、deadline，以及 Agent 路径和只读
消息种子；工作目录、模型、prompt 和工具表由 factory 闭包持有。取消和总超时由 Agent
工具执行器统一负责。
子 Agent 在后台运行，主 Agent 通过 list/wait 获取结构化状态和最终消息。普通
`send_message` 只排队，`followup_task` 才会触发下一轮；interrupt 只取消当前轮次，
不会销毁该 Agent 的既有上下文。

## 数据流

```text
chat / CLI / scheduler / heartbeat / child agent
                    │
                    ↓ AgentInput
             product session actor
                    │ one writer per SessionId
                    ↓
               AgentSession ─────→ ConversationStore
                    │                    │
                    │             ConversationLog replay
                    ↓                    ↓
              ContextPolicy ─────→ model context
                    │
                    ↓ ModelRequest
               ModelClient ← injected by AgentRuntime
                    │
       ash-protocol adapter ─────→ provider SSE
                    │
                    ├── RunEvent ─────→ chat gateway / TUI
                    └── ToolContext ──→ tools / ChildAgentFactory
```

聊天软件 adapter、cron 调度器和 heartbeat service 都是 Agent 库外的 trigger producer。
它们负责鉴权、重试、幂等键生成、每 Session 排队和事件投递；Agent 库不启动常驻心跳线程，
也不持有聊天平台 SDK。当前仓库已经提供这些产品所需的输入和运行契约，但尚未实现具体
聊天 connector、持久化调度服务或长期记忆数据库。

## 错误和取消

- 协议错误转换为 `Event::Error`，随后发送 `AgentFinished(Aborted)`。
- 供应商的 token 上限映射为 `StopReason::MaxTokens`。
- Agent 自身轮次上限映射为 `StopReason::MaxTurns`。
- 用户取消映射为 `StopReason::Aborted`。
- 工具执行同时受 deadline、通用 timeout 和 cancellation 控制。
- 会话追加时 revision 不匹配会拒绝陈旧写入；分布式并发冲突由 repository 实现负责。

## 测试重点

- 三种协议的分片工具调用聚合
- 工具调用后的完整消息历史
- ConversationLog 的完整历史/模型上下文投影和 revision 冲突
- AgentInput trigger/idempotency 与 RunEvent 标识、序号
- 自动压缩通过 ContextPolicy 产生 checkpoint
- Unicode 输入编辑和空历史边界
- 工作目录逃逸检查
- 核心类型序列化快照

提交前要求 workspace tests、格式检查和 `clippy -D warnings` 全部通过。

## 产品扩展边界

以下能力不应进入 Agent engine：

- 聊天平台 SDK、webhook 和消息路由
- cron/延时队列、heartbeat 服务和失败重试调度
- 分布式 Session 锁、数据库选型和租约实现
- 用户画像、向量检索和长期记忆策略
- provider 凭据加载与具体协议选择

它们分别通过 `AgentInput`、`ConversationRepository`、`ContextPolicy` 和 `ModelClient` 接入。
新默认行为必须先有行为测试，再进入交互主链。
