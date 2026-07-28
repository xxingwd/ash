# Ash 当前架构

本文只描述已经接入主链路的实现。研究材料和未来设想不作为实现规范。

## 设计原则

1. 一个交互会话只有一个消息历史所有者。
2. 协议层必须真实流式输出，不能先缓存完整响应。
3. 供应商返回的工具调用 ID 必须原样保留。
4. UI 不拥有 Agent 状态，只消费事件并发送用户命令。
5. 文件工具默认只能访问当前工作目录。
6. 取消和超时从会话一直传递到工具进程。

## Crate 边界

### `ash-core`

共享数据结构和接口：

- `Message`、`ContentBlock`、`Event`
- `ModelId`、`ProviderConfig`、`StopReason`
- `Tool`、`ToolContext`、`ToolError`
- ID 和取消令牌

该层不依赖任何具体 LLM 协议或终端实现。

### `ash-protocol`

负责把统一消息转换为供应商请求，并把 SSE 事件转换为统一 `StreamItem`。

- `ProtocolAdapter` 返回 `ProtocolStream`
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

`read`、`write` 和 `edit` 通过已打开的工作目录 capability 执行文件访问，符号链接不能
逃逸到目录外，也不存在校验后重新打开裸路径的竞态。Agent 执行器统一管理工具取消和
超时；`bash` 子进程启用 `kill_on_drop`，执行 Future 被丢弃时会终止进程。
`webfetch` 只请求 HTTP/HTTPS URL，在流式读取阶段限制响应大小，并将 HTML 转为 Markdown。
`glob` 和 `grep` 使用纯 Rust 遍历，搜索范围限制在工作目录内，遵守 ignore 文件且不跟随
符号链接；前者匹配文件路径，后者按正则搜索内容，两者都限制为最多 100 条结果。

### `ash-agent`

`AgentSession` 是连续对话的唯一历史所有者。每次提交按以下顺序运行：

1. 将用户消息加入历史。
2. 创建协议流并逐项转发文本、thinking 和 usage 事件。
3. 将完整 assistant 消息加入历史。
4. 顺序执行工具，将每个结果加入历史。
5. 如果存在工具调用则继续请求模型，否则结束本轮。

上下文压缩只作用于发送给模型的副本，不破坏会话保存的完整历史。

每个交互会话由一个带日期文件名的 append-only JSONL 保存。`AgentSession` 在用户
消息、完整 Assistant 消息和工具结果这些可恢复的语义边界同步追加并 flush；
首行的配置快照包含恢复模型上下文所需的信息，但不包含 API Key、Token 或自定义
接口地址内容。`/new` 立即生成 Session ID 和创建时间，但会话文件延迟到第一条用户
消息时创建；会话标题由重放后的第一条有效用户消息派生。`/resume` 列出其他已保存
会话的标题和创建时间，选中后按 Session ID 恢复 `Vec<Message>`。Turn rollback 也以 append-only 记录保存；重放 JSONL
时按顺序截断对应用户轮次，不重写已有文件。

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
- resize 与 `/undo` 先清空可见屏幕和 scrollback，再从语义历史按当前宽度完整重放；重放
  每次只构建一个块的 Buffer，插入后立即释放
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
- 子 Agent 继承当前模型、协议、工作目录、工具和完整系统上下文
- `fork_turns` 支持不继承、完整继承或只继承最近 N 轮
- 每个子 Agent 保留独立消息历史，可以在完成或中断后继续 follow-up
- 根 Session ID 是 Agent 树的隔离边界；新会话不会看到旧树
- 默认允许三个并发子 Agent，加上根 Agent 共四个执行槽

工具调用通过 `ToolContext` 获得当前 Session、Agent 路径、消息快照和运行配置；
取消和总超时由 Agent 工具执行器统一负责。
子 Agent 在后台运行，主 Agent 通过 list/wait 获取结构化状态和最终消息。普通
`send_message` 只排队，`followup_task` 才会触发下一轮；interrupt 只取消当前轮次，
不会销毁该 Agent 的既有上下文。

## 数据流

```text
shell input
    ↓ UiCommand
ash-cli controller
    ↓
AgentSession ── LlmRequest ──→ ProtocolAdapter ──→ SSE
    │                                  │
    │                                  ↓ StreamItem
    ├── tool execution ──→ ash-orchestrator ──→ child Agent turns
    │
    └── Event ──→ inline TUI ──→ Ratatui viewport / terminal scrollback
```

## 错误和取消

- 协议错误转换为 `Event::Error`，随后发送 `AgentFinished(Aborted)`。
- 供应商的 token 上限映射为 `StopReason::MaxTokens`。
- Agent 自身轮次上限映射为 `StopReason::MaxTurns`。
- 用户取消映射为 `StopReason::Aborted`。
- 工具执行同时受通用 timeout 和 cancellation 控制。

## 测试重点

- 三种协议的分片工具调用聚合
- 工具调用后的完整消息历史
- Unicode 输入编辑和空历史边界
- 工作目录逃逸检查
- 核心类型序列化快照

提交前要求 workspace tests、格式检查和 `clippy -D warnings` 全部通过。

## 暂不扩展

以下能力在主链稳定前不继续增加：

- 更多协议
- 全屏 TUI
- 复杂 Markdown 渲染
- 大型配置框架

新功能必须先有行为测试，再进入默认交互路径。
