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

已实现：

- Anthropic Messages
- OpenAI Chat Completions
- OpenAI Responses

### `ash-tools`

提供工具定义和内置工具。

`path` 模块统一完成路径规范化、工作目录边界和符号链接检查。`bash` 子进程启用
`kill_on_drop`，取消或超时会停止等待并终止进程。

### `ash-agent`

`AgentSession` 是连续对话的唯一历史所有者。每次提交按以下顺序运行：

1. 将用户消息加入历史。
2. 创建协议流并逐项转发文本、thinking 和 usage 事件。
3. 将完整 assistant 消息加入历史。
4. 顺序执行工具，将每个结果加入历史。
5. 如果存在工具调用则继续请求模型，否则结束本轮。

上下文压缩只作用于发送给模型的副本，不破坏会话保存的完整历史。

每个交互会话由一个带日期文件名的 append-only JSONL 保存。`AgentSession` 在用户
消息、完整 Assistant 消息、工具结果和 Turn 结束这些语义边界同步追加并 flush；
首行的配置快照包含恢复模型上下文所需的信息，但不包含 API Key、Token 或自定义
接口地址内容。`/new` 立即生成 Session ID 和创建时间，但会话文件延迟到第一条用户
消息时创建；会话标题由重放后的第一条有效用户消息派生。`/resume` 列出其他已保存
会话的标题和创建时间，选中后按 Session ID 恢复 `Vec<Message>`。Turn rollback 也以 append-only 记录保存；重放 JSONL
时按顺序截断对应用户轮次，不重写已有文件。

提示词由固定基础约定和启动时上下文组合而成。动态上下文只包含环境信息、从外到
内生效的 `AGENTS.md` 和 Skills 元数据；显式启用的 Skill 会追加完整指令，并可覆盖
模型或工具集合。MCP 仍属于独立能力，尚未成为默认交互链路的一部分。

### `ash-tui`

名称保留为 `ash-tui`，实现实际是 inline terminal UI：

- 不使用 alternate screen
- 正常交互不清屏；`/new` 和 `/clear` 使用相同的新会话逻辑
- 当前会话未溢出且宽度未变化时局部擦除 ASH 会话区域；溢出、缩放或布局不确定时
  只清空当前可见屏幕，两条路径都不 Purge shell scrollback
- Ratatui Buffer 统一计算活动内容、状态栏、输入框、补全和底栏的组件布局
- 已完成的历史通过 Crossterm 写入主屏 stdout，让 shell 维护 scrollback；Ratatui
  只负责底部 inline viewport，不使用 alternate screen
- 只原地重绘当前输入行
- Agent 工作期间 `Ctrl-C` 不发送取消命令；状态栏提示 `esc to interrupt`，第一次按
  `Esc` 不展示额外状态
- Agent 工作期间按一次 `Esc` 会取消并回退当前轮，将问题恢复为草稿
- `Esc` 会先在 UI 侧立即移除当前 turn 并恢复 Composer；后端确认回退前到达的旧
  Thinking、工具和文本事件不再参与渲染，确认事件也不会重复清除同一区域
- 回退按本轮已提交的终端行数局部擦除当前轮，不重建整个屏幕，也不 Purge shell
  scrollback；已经滚出当前屏幕的行可能仍由终端保存，但当前轮会从模型上下文移除
- 文本按换行进入 FIFO 队列，正常逐行提交，积压时批量追赶
- 未完成行在响应结束时提交，活动表格在结构稳定前保持为可变尾部
- 流式思考摘要使用固定四行活动区：一行带耗时的 `Thinking` 标题和最新三行正文；
  切换到回答、工具或完成状态时折叠为单行耗时摘要，不把展开正文写入 scrollback
- 工具历史按工具语义生成单行摘要，隐藏内容参数和默认参数，并把绝对路径缩短为
  可辨识的文件名或末级目录；不把原始工具参数 JSON 写入 scrollback
- 历史和 viewport 使用同一套块布局协议：完整块只声明内容高度并管理内部
  padding，父级 Stack 使用 `Flex::Start` 和统一 `spacing` 排列块；输入框与模型、
  路径或补全 footer 组成一个 ComposerBlock，continuation 只表示同一流式块的后续行

模块分工：

- `app`：事件与命令协调
- `block_layout`：完整块、continuation 与 Ratatui Flex spacing
- `input`：Unicode 安全的编辑、历史和可视窗口
- `viewport`：Ratatui 组件、行高、间距和 Buffer 渲染
- `inline`：Crossterm history insertion、scrollback、raw mode 与 viewport 生命周期

输入框的跨进程历史使用独立的 `history.jsonl`；它只负责上下键召回，不参与模型
上下文恢复。终端 scrollback 仍由 shell 保存，恢复会话时 UI 根据语义消息重新渲染
对话，而不是解析终端输出。

### `ash-cli`

负责参数、环境变量、Skill 应用和交互控制器。

控制器保证同一会话同一时间只运行一个 Agent turn，并在 UI 发出取消命令时触发
当前 turn 的 `CancellationToken`。

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

工具调用通过 `ToolContext` 获得当前 Session、Agent 路径、消息快照和运行配置。
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
    └── Event ──→ inline terminal ──→ stdout / shell scrollback
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
