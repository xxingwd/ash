# Pi Agent 架构分析

> 研究归档：本文用于理解 Pi，不是 Ash 的实现规范。Ash 的当前结构以
> `DESIGN.md` 和实际测试为准。

> 仓库：https://github.com/earendil-works/pi.git  
> 本地路径：`/tmp/opencode/pi-repo`

---

## 一、项目总览

Pi 是一个 TypeScript monorepo，用于构建 AI 驱动的编码 Agent。5 个包按依赖关系分层：

```
@earendil-works/pi-tui        (终端UI库，无内部依赖)
       ↓
@earendil-works/pi-ai         (统一LLM API抽象，30+ Provider)
       ↓
@earendil-works/pi-agent-core (通用Agent框架：Agent类 + AgentHarness)
       ↓
@earendil-works/pi-coding-agent (完整的编码Agent：CLI/SDK/扩展/会话管理)
       ↓
@earendil-works/pi-orchestrator (实验性编排器，管理多Agent进程)
```

### 各包定位

| 包 | 说明 |
|---|---|
| **pi-tui** | 终端 UI 库，差分渲染、Markdown 渲染、键盘绑定管理、选择列表、编辑器、图片显示、Kitty/iTerm2 终端图片协议 |
| **pi-ai** | 统一 LLM API，支持 30+ Provider (Anthropic、OpenAI、Google、DeepSeek 等)，自动模型发现、API Key/OAuth 认证、凭据存储 |
| **pi-agent-core** | 通用 Agent 框架，传输抽象、状态管理、附件支持、可插拔 Agent 循环。含 `AgentHarness` 做会话管理、压缩、分支、事件钩子系统 |
| **pi-coding-agent** | 完整编码 Agent CLI (`pi` 命令)，含 read/bash/edit/write 等工具、JSONL 会话管理、扩展系统、技能/提示词模板、模型注册表、三种运行模式 (TUI/批处理/RPC) |
| **pi-orchestrator** | 实验性编排器，管理多个 coding-agent RPC 进程，通过 Unix socket IPC 通信，与 Radius 云服务集成 |

---

## 二、pi-agent-core — 核心 Agent 框架

### Agent 类 (`packages/agent/src/agent.ts`)

中心抽象，管理对话 Agent 的生命周期。

| 方法 | 说明 |
|---|---|
| `subscribe(listener)` | 订阅生命周期事件，返回取消订阅函数 |
| `state` | 返回 `AgentState`：systemPrompt、model、tools、messages、isStreaming |
| `steer(message)` | 向中间轮次注入消息 |
| `followUp(message)` | 在 Agent 停止后执行的消息队列 |
| `abort()` | 终止当前运行 |
| `waitForIdle()` | 等待当前运行+监听器结束 |
| `reset()` | 清空转录、运行时状态和队列 |
| `prompt(input, images?)` | 开始新提示 |
| `continue()` | 从当前转录继续 |

### Agent 事件类型 (`AgentEvent`)

```
agent_start / agent_end          → 生命周期
turn_start / turn_end            → 轮次生命周期（含 toolResults）
message_start / message_update / message_end  → 消息生命周期
tool_execution_start / _update / _end → 工具执行
```

### Agent 循环 (`packages/agent/src/agent-loop.ts`)

```
runAgentLoop():
  1. emit agent_start, turn_start
  2. streamAssistantResponse()
     → transformContext → convertToLlm → streamFn → 流式返回
  3. 有 tool_call？验证参数 → beforeToolCall hook
     → 执行工具(顺序/并行) → afterToolCall hook
     → 结果放回 context，继续循环
  4. 无 tool_call → emit turn_end → prepareNextTurn
  5. 检查 steering/followUp 队列 → 继续或结束
```

### AgentHarness (`packages/agent/src/harness/agent-harness.ts`)

会话托管的 Agent 包装器，提供：
- 会话持久化（消息、模型变更、thinking level 变更、工具变更）
- 钩子系统（订阅特定事件类型）
- 压缩支持（自动上下文窗口管理）
- 分支摘要
- Steering/follow-up 队列管理

---

## 三、pi-ai — LLM API 抽象层

### 核心接口

| 类型/接口 | 说明 |
|---|---|
| `Provider<TApi>` | Provider 接口：`id`, `name`, `baseUrl`, `auth`, `getModels()`, `stream()`, `streamSimple()` |
| `Models` / `MutableModels` | Provider 集合，自动解析认证 |
| `Model<TApi>` | 模型元数据 (id, name, api, provider, cost, contextWindow, maxTokens) |
| `Message` | `UserMessage | AssistantMessage | ToolResultMessage` |
| `Tool<T>` | 工具定义 (name, description, parameters/TypeBox  schema) |
| `StreamOptions` | 流式选项 (temperature, maxTokens, signal, apiKey, headers, timeout) |
| `CredentialStore` | 凭据存储接口 |

### 工厂函数

| 函数 | 说明 |
|---|---|
| `createModels()` | 创建 `MutableModels` 实例 |
| `createProvider()` | 从 parts 构建 Provider |
| `builtinProviders()` | 返回 33+ 内置 Provider |
| `envApiKeyAuth()` | 标准环境变量认证构建器 |
| `lazyOAuth()` | 懒加载 OAuth 包装 |
| `calculateCost()` | 计算 token 成本 |
| `getSupportedThinkingLevels()` | 获取支持的推理级别 |

### 内置 Provider 清单

Anthropic、OpenAI、Amazon Bedrock、Google、DeepSeek、Mistral、Groq、Cerebras、OpenRouter、GitHub Copilot、xAI、Fireworks、Together、HuggingFace、NVIDIA、Moonshot AI、Minimax、Kimi Coding、OpenCode、Vercel AI Gateway、Cloudflare Workers AI 等 33+。

---

## 四、pi-coding-agent — 编码 Agent

### AgentSession (`packages/coding-agent/src/core/agent-session.ts`)

核心高层会话类（~3200 行），所有运行模式共享。

| 方法 | 说明 |
|---|---|
| `prompt(input, options?)` | 发送用户提示，处理模板展开、图片转换、流式行为 |
| `cycleModel(direction)` | 切换模型 |
| `setModel(model, thinkingLevel)` | 设置模型和推理级别 |
| `abort()` | 终止当前轮次 |
| `retry()` | 重试上次失败的助手回复 |
| `compact()` | 触发上下文压缩 |
| `subscribe(listener)` | 订阅 AgentSessionEvent |
| `exportHtml(filePath)` | 导出为 HTML |

### SessionManager (`packages/coding-agent/src/core/session-manager.ts`)

管理会话文件（JSONL 格式，带版本号），树结构，压缩感知，上下文构建。

### ModelRegistry (`packages/coding-agent/src/core/model-registry.ts`)

管理内置和自定义模型、API key 解析、OAuth 认证、模型发现。

### 内置工具 (`packages/coding-agent/src/core/tools/`)

| 工具 | 说明 |
|---|---|
| `read` | 读文件，支持行/字节限制 |
| `bash` | 执行 shell 命令，含文件变更队列集成 |
| `edit` | 搜索替换文件编辑，生成 diff |
| `write` | 写文件 |
| `grep` | 正则搜索文件内容 |
| `find` | glob 模式搜索文件 |
| `ls` | 列出目录内容 |

工厂：`createCodingTools()` → [read, bash, edit, write]，`createAllTools()` → 全部 7 个。

### 扩展系统 (`packages/coding-agent/src/core/extensions/`)

| 导出 | 说明 |
|---|---|
| `Extension` | 扩展接口 (hooks, tools, commands, shortcuts, UI) |
| `ExtensionRunner` | 扩展生命周期管理，事件派发，工具/命令注册 |
| `ExtensionAPI` | 提供给扩展的 API（会话控制、工具注册、UI 等） |
| `discoverAndLoadExtensions()` | 从文件系统发现和加载扩展 |
| `defineTool()` | 工具定义辅助函数 |

80+ 扩展事件类型：`BeforeProviderRequestEvent`、`ReadToolCallEvent`、`BashToolCallEvent` 等。

### 运行模式

| 模式 | 文件 | 说明 |
|---|---|---|
| `InteractiveMode` | `modes/interactive/` | 完整 TUI 模式 |
| `runPrintMode` | `modes/print-mode.ts` | 非交互批处理模式 |
| `runRpcMode` | `modes/rpc/rpc-mode.ts` | JSON-line RPC 服务端 |
| `RpcClient` | `modes/rpc/rpc-client.ts` | RPC 客户端 |

---

## 五、pi-orchestrator — 编排器

| 类 | 说明 |
|---|---|
| `OrchestratorSupervisor` | 管理多 Agent 实例生命周期 (spawn/stop/handleRpc) |
| `RpcProcessInstance` | 子进程管理 (send/handleUiResponse/onEvent) |
| `RadiusPresence` | 云端注册 + 心跳 |

IPC 协议 (`packages/orchestrator/src/ipc/protocol.ts`)：
- 请求类型：spawn, list, status, stop, rpc, rpc_stream
- 响应类型：spawn_result, list_result, error 等

---

## 六、pi-tui — 终端 UI

| 组件 | 说明 |
|---|---|
| `TUI` | 主引擎：组件树、按键分发、聚焦管理、overlay 支持 |
| `Container` | 布局容器 |
| `Box` | 带边框的盒子 |
| `Text` / `TruncatedText` | 文本显示 |
| `Input` / `Editor` | 输入框/文本编辑器 |
| `Markdown` | Markdown 渲染（基于 marked） |
| `SelectList` | 可选择列表（含模糊匹配） |
| `Loader` / `CancellableLoader` | 加载动画 |
| `Image` | 终端图片显示 (Kitty, iTerm2) |
| `KeybindingsManager` | 键盘绑定管理 |

---

## 七、TUI 与 Agent 的通信与解耦

### 总体架构：三层事件管道

```
┌─────────────────────────────────────────────────┐
│  InteractiveMode (TUI层)                         │
│  - AgentSession.subscribe() 接收事件             │
│  - 事件驱动 UI 渲染                              │
│  - AgentSession.prompt()/abort() 等命令调用      │
└─────────────────────┬───────────────────────────┘
                      │ subscribe(AgentSessionEvent)
┌─────────────────────▼───────────────────────────┐
│  AgentSession (桥接层)                           │
│  - 持有一个 Agent 实例                           │
│  - Agent.subscribe(_handleAgentEvent)           │   ← 内部订阅
│  - 将 AgentEvent 翻译为 AgentSessionEvent       │
│    (加入 compaction/retry/session 等上层事件)     │
│  - 管理持久化、自动重试、压缩、扩展系统           │
│  - 提供 subscribe() 给外部监听                   │
└─────────────────────┬───────────────────────────┘
                      │ subscribe(AgentEvent)
┌─────────────────────▼───────────────────────────┐
│  Agent (核心 Agent 循环)                         │
│  - Agent.subscribe(listener) 发射原始事件        │
│  - Agent.prompt()/continue() 驱动循环           │
│  - 事件：agent/start/end, turn, message, tool等  │
└─────────────────────────────────────────────────┘
```

### 事件流（Agent → Session → TUI）

```
Agent._emitAgentEvent()                           ← agent-loop.ts
  → AgentSession._handleAgentEvent()              ← agent-session.ts:515
    → _emitExtensionEvent()                       ← 同时派发给扩展系统
    → _emit(event)                                ← 派发给所有 AgentSession 订阅者
      → InteractiveMode.handleEvent(event)        ← interactive-mode.ts:2753
        → switch(event.type):
            case "message_start"   → addMessageToChat()
            case "message_update"  → streamingComponent.updateContent()
            case "message_end"     → finalize component
            case "tool_execution_*"→ ToolExecutionComponent
            case "compaction_*"    → CompactionStatusIndicator
            case "auto_retry_*"    → RetryStatusIndicator
```

关键连接代码：
- `agent-session.ts:356` — 构造函数中 `this.agent.subscribe(this._handleAgentEvent)`
- `agent-session.ts:729` — `subscribe(listener)` 对外暴露
- `interactive-mode.ts:2748` — `session.subscribe((event) => handleEvent(event))`
- `agent-session.ts:539-542` — `_emitExtensionEvent()` 先于 `_emit()` 执行

### 命令流（TUI → Session → Agent）

```
用户在编辑器输入 → onSubmit()                      ← interactive-mode.ts:2560
  → session.prompt(text)                          ← 调用 AgentSession
    → agent.prompt(input)                         ← AgentSession 调用 Agent
      → runAgentLoop()                            ← Agent 内部循环
```

### 接口解耦边界

| 边界 | 接口/类型 | 作用 |
|---|---|---|
| TUI ↔ Session | `AgentSessionEvent` 联合类型 | 约定所有可能的事件 |
| TUI ↔ Session | `AgentSession.prompt()/abort()/etc` | 命令接口 |
| Session ↔ Agent | `AgentEvent` 联合类型 | 核心事件契约 |
| Session ↔ Extensions | 80+ `ExtensionEvent` 类型 | 扩展系统事件 |
| Extensions ↔ TUI | `ExtensionUIContext` 接口 | UI 操作抽象 |

### 不直接持有引用

`InteractiveMode` 通过 `AgentSession` 间接访问 Agent，从不直接操作核心 Agent：

```typescript
// interactive-mode.ts:390-395
private get session(): AgentSession { return this.runtimeHost.session; }
private get agent() { return this.session.agent; }
```

### AgentSessionEvent 完整类型

```typescript
type AgentSessionEvent =
  | AgentEvent (排除 agent_end)
  | { type: "agent_end"; messages; willRetry }
  | { type: "queue_update"; steering; followUp }
  | { type: "compaction_start"; reason }
  | { type: "compaction_end"; reason; result; aborted; willRetry }
  | { type: "entry_appended"; entry }
  | { type: "session_info_changed"; name }
  | { type: "thinking_level_changed"; level }
  | { type: "auto_retry_start"; attempt; maxAttempts; delayMs }
  | { type: "auto_retry_end"; success; attempt }
```

---

## 八、解耦模式总结

TUI 与 Agent 的通信遵循 **单向事件流 + 命令调用** 模式，这是典型的 **事件驱动架构**：

| 方向 | 机制 | 说明 |
|---|---|---|
| **Agent → TUI** | `subscribe/emit` 事件流 | Agent 通过事件向上通知状态变更，TUI 被动响应渲染 |
| **TUI → Agent** | 方法调用 | TUI 通过 `AgentSession` 暴露的方法发送命令 |
| **第三方扩展** | `ExtensionRunner + UIContext` | 扩展通过接口与两侧解耦 |

核心设计原则：
1. **Agent 不感知 UI** — 核心 Agent 循环只发射事件，不知道谁在监听
2. **AgentSession 作为翻译层** — 将核心事件增强为上层事件（加 compaction/retry/session 逻辑）
3. **TUI 只订阅不侵入** — TUI 只响应事件和调用公开 API，不修改内部状态
4. **分层契约化** — 每层通过 TypeScript 类型（联合类型/接口）明确定义通信协议
