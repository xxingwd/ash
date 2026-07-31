# Ash

Ash 是一个 Rust 编写的命令行 coding agent。目前主链路包括：

- Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 三种协议
- 真正的 SSE 增量输出
- 连续对话与工具调用历史
- 默认启用 `read`、`glob`、`grep`、`bash`、`edit`、`write`、`webfetch` 七个内置工具
- Codex 风格的 `default`、`explorer`、`worker` 子 Agent
- 保留终端原生 scrollback 的 inline TUI

## 运行

在进程环境中提供配置，然后直接运行 CLI：

```bash
export ASH_PROTOCOL=openai-responses
export ASH_MODEL=gpt-5
export ASH_BASE_URL=https://api.example.com
export ASH_API_KEY=...
export ASH_MODEL_CONFIG='reasoning.effort=high;temperature=0.2'
cargo run -p ash-cli
```

也可以只对单次命令设置环境变量：

```bash
ASH_PROTOCOL=openai-responses \
ASH_MODEL=gpt-5 \
ASH_API_KEY=your-key \
ASH_MODEL_CONFIG='reasoning.effort=high' \
cargo run -p ash-cli
```

CLI 参数优先于对应的环境变量。

Anthropic：

```bash
export ASH_API_KEY=...
cargo run -p ash-cli -- --model claude-sonnet-4-20250514
```

OpenAI Responses：

```bash
export ASH_API_KEY=...
cargo run -p ash-cli -- \
  --protocol openai-responses \
  --model gpt-5
```

兼容服务可以通过 `--base-url` 指定地址。

`ASH_MODEL_CONFIG` 用 `;` 分隔 `key=value` 覆盖项，`.` 表示 body 中的嵌套对象。`true`、
`false`、`null`、整数和小数会保留为 JSON 标量，其余值为字符串。例如 OpenAI Responses
使用 `reasoning.effort=high`，Anthropic 可使用
`thinking.type=adaptive;output_config.effort=high`。Ash 不解释这些供应商字段，只在发送前
写入请求 body；`model`、`messages`、`input`、`tools`、`stream`、`system` 和
`instructions` 等请求结构字段不能覆盖。

模型上下文窗口默认是 200K token，可用 `--max-context-tokens` 或
`ASH_MAX_CONTEXT_TOKENS` 覆盖；底栏会按该上限显示上下文百分比。Ash 使用每 4 个字符约
1 token 的轻量估算，不引入 tokenizer 依赖。每轮结束后的 `Worked` 行显示输入、输出
token 和生成速度；服务端没有返回 usage 时使用带 `~` 的本地估算值。

估算输入达到模型上限的 80% 时，Ash 会在正式请求前自动压缩模型上下文，并持久化一条
隐藏的 compaction checkpoint。原始消息、终端 scrollback 和会话标题保持不变。压缩器
保留最近两个完整用户轮次，让同一个模型在禁用工具的独立请求中把更早历史整理为结构化
摘要；再次压缩会更新已有摘要。摘要输入中的旧工具输出最多保留 2,000 字符，图片只保留
媒体类型和大小。模型调用前还会保护最近两个轮次及约 40K token 的近期工具结果；更旧
结果累计可释放超过 20K token 时会先清理，清理后仍达到 80% 才生成摘要。工具刚执行完
写入会话前仍有 64 KiB 的运行时上限。交互模式下可用 `/compact` 随时主动更新隐藏的
模型上下文。

单次输出模式：

```bash
cargo run -p ash-cli -- --print "inspect this project"
```

## 项目上下文

Ash 启动时会构建一份精简的系统上下文，包含：

- 内置的 coding agent 工作约定
- 当前目录、shell、日期、时区、操作系统和架构
- 从 Git 项目根目录到当前目录依次生效的 `AGENTS.md`
- 项目和用户目录中可用的 Skills，以及通过 `--skill` 显式启用的 Skill 指令

更深目录中的 `AGENTS.md` 指令优先级更高。找不到 Git 项目根时，只读取当前目录。

项目 Skill 使用 `.agents/skills/<name>/SKILL.md`，用户 Skill 使用
`~/.agents/skills/<name>/SKILL.md`。项目 Skill 优先于同名用户 Skill，更深目录中的项目
Skill 优先于上层目录中的同名 Skill：

```markdown
---
name: review
description: Review changes for correctness
tools:
  - read
  - bash
---
Review the relevant code and report concrete findings.
```

使用 `--skill review` 会在启动时加载完整 Skill 指令，并应用可选的模型和工具覆盖。Skill
未写 `tools` 时默认启用全部七个内置工具；写了 `tools` 时则只启用名单中的内置工具。

未显式启用 Skill 时，系统上下文只提供可用 Skills 的名称和描述。模型在任务
匹配时调用 `skill({"name":"review"})`，再获得完整指令、Skill 基础目录和最多 10 个资源
文件路径。`skill` 是 Agent 层的运行时工具，不受 Skill 的内置工具名单影响，也不会动态
修改模型或底层工具配置。

## 会话历史

交互会话按“一会话一文件”保存为 JSONL。Linux 默认目录为
`~/.local/share/ash/threads/`，文件名直接包含本地日期和时间：

```text
thread-2026-07-14T16-30-25.123-<thread-id>.jsonl
```

普通新会话的文件在第一次提交消息时才会创建；带继承历史的 fork 会立即写入新文件。
首行保存格式版本、Session ID、模型、协议、工作目录、最终系统提示词、工具定义和上下文
限制等配置快照；后续按顺序追加完整用户消息、Assistant 消息、工具结果和 Turn 结束状态。
压缩时只追加摘要和近期历史起点；恢复会话后，UI 重放完整消息，模型请求则使用该
checkpoint 构造压缩上下文。API Key、访问令牌和自定义接口地址内容不会写入文件。

`/new` 和 `/clear` 使用相同逻辑：清空模型会话历史和终端 scrollback，并建立新的
Session。`/resume` 会用所选 JSONL 重建模型上下文，并把完整消息重放到终端 scrollback。
`/undo` 会列出当前会话的历史用户输入；选中后创建一个新的 fork Session，继承该输入
之前的消息，并把所选输入恢复到输入框。原 Session 的内存历史和 JSONL 都不会改变。
通过 `/new` 或 `/clear` 建立的 Session 会立即获得 ID 和创建时间，但在第一条用户消息
发出前不会创建文件；会话名称取第一条有效用户消息。
`/resume` 和 `/undo` 都会在输入框下方显示选择菜单，使用方向键选择。输入框的跨进程
历史单独保存在 `~/.local/share/ash/history.jsonl`。

## 子 Agent

默认交互链路提供六个 Codex 风格的协作工具：

- `spawn_agent`：启动一个有独立上下文的后台 Agent
- `send_message`：向现有 Agent 追加消息，但不主动开启新一轮
- `followup_task`：复用现有 Agent 的上下文并开启后续任务
- `interrupt_agent`：只中断目标 Agent 当前一轮，之后仍可继续复用
- `list_agents`：查看当前根会话中的 Agent、状态和最终结果
- `wait_agent`：等待状态变化；完成状态会携带最终消息

内置类型与 Codex CLI `0.144.3` 对齐：

- `default`：继承当前配置的通用 Agent
- `explorer`：用于明确、窄范围、只读的代码库问题
- `worker`：用于实现、修复、测试和重构，需要明确文件或模块所有权

Codex 源码中仍保留 `awaiter` 配置，但该版本已从可用角色中临时移除，因此 Ash
也不对外暴露它。子 Agent 继承当前模型、协议、工作目录、工具、AGENTS.md 和 Skills
上下文；默认最多同时运行三个子 Agent，加上根 Agent 共四个并发槽。`fork_turns`
支持 `none`、`all` 或最近 N 轮。子 Agent 树按根 Session ID 隔离，执行 `/new` 或
`/clear` 后不会混入旧会话的 Agent。

ASH 会主动寻找真正能并行推进的工作：多个独立问题通常在同一轮交给多个
explorer，边界清晰的代码改动优先交给 worker。简单任务和紧耦合的即时阻塞仍由
当前 Agent 自己完成；委派后当前 Agent 会继续处理不重叠的工作，而不是立即等待。

## 终端行为

交互界面使用 Ratatui inline viewport，不进入 alternate screen，也不捕获鼠标。终端原生
滚动、选择和复制因此保持可用。当前用户消息、流式 Thought、工具与回答只存在于本轮 live
viewport；它按实际内容高度增长，填满首屏后在首屏内部跟随最新内容。

收到 `AgentFinished` 后，UI 才在一次同步更新中把整轮语义块依次写到终端 scrollback，
随后从 live transcript 移入轻量语义历史并释放渲染缓存。完成历史不参与逐帧渲染；只有
resize 会先清空可见屏幕与 scrollback，再按当前宽度从头重放。重放逐块渲染并立即释放
Buffer，不会同时缓存整段历史。日常滚动、选择和复制仍由终端模拟器负责。`/new`、
`/clear` 会同时清除语义历史、可见屏幕和 scrollback，`/resume` 和完成的 `/undo` fork
则从对应 Session 消息重新渲染历史。

模型提供思考摘要时，`Thinking (Xs)` 和完整思考正文会随 transcript 持续向下滚动。
正文、工具调用或本轮结束后，思考区折叠为不可展开的一行 `• Thought for Xs`。OpenAI Chat
Completions 兼容接口会识别 `reasoning_content`、`reasoning` 和 `thinking` 字段，
Responses 接口只展示 reasoning summary，不展示原始 reasoning text。

工具调用使用语义化单行摘要，不直接打印参数 JSON。连续的原生 `read` 调用会聚合为
一个摘要；`glob`、`grep` 和 `skill` 分别显示匹配模式、正则和 Skill 名称；`bash` 始终
显示实际命令，不猜测 Shell 意图。文件内容、编辑前后文本、默认参数、工作目录和绝对
路径不会进入终端历史。聚合只影响展示，底层工具调用、结果和会话记录仍保持独立。

- `Enter`：提交
- 输入 `/`：显示斜杠命令补全；继续输入会按命令名或别名过滤
- 补全菜单中 `↑` / `↓`（或 `Ctrl-P` / `Ctrl-N`）：切换选择
- 补全菜单中 `Tab`：补全命令；`Enter`：执行当前选择；`Esc`：关闭菜单
- `↑` / `↓`：输入历史
- `PageUp` / `PageDown`：按页浏览当前 live turn
- `Ctrl-Home` / `Ctrl-End`：跳到当前 live turn 顶部或底部
- 鼠标滚轮和终端原生快捷键：浏览已完成的 scrollback
- 任务运行时按一次 `Esc`：取消并撤销当前一轮，将原问题恢复到输入框
- 任务运行时状态栏显示 `esc to interrupt`，第一次按 `Esc` 不展示额外状态
- 任务运行时 `Ctrl-C` 不取消当前请求
- 空输入时 `Ctrl-C` 或 `Ctrl-D`：退出
- `exit` / `quit`：退出

按下 `Esc` 会从模型会话历史和 live viewport 中移除当前轮，并把问题恢复到输入框；它不会
反向撤销已经由工具写入文件系统的修改。`/undo` 也不会撤销工具副作用或修改原 Session；
它会从所选输入之前的历史创建新 Session，随后重放继承的历史并恢复该输入。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

当前架构和约束见 [DESIGN.md](DESIGN.md)。
