# Ash

Ash 是一个 Rust 编写的命令行 coding agent。目前主链路包括：

- Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 三种协议
- 真正的 SSE 增量输出
- 连续对话与工具调用历史
- `bash`、`read`、`write`、`edit`、`grep`、`find` 内置工具
- Codex 风格的 `default`、`explorer`、`worker` 子 Agent
- 非全屏 inline 终端

## 运行

推荐使用启动脚本：

```bash
export ASH_PROTOCOL=openai-responses
export ASH_MODEL=gpt-5
export ASH_BASE_URL=https://api.example.com
export ASH_API_KEY=...
./start.sh
```

也可以直接写入项目根目录的 `.env`：

```dotenv
ASH_PROTOCOL=openai-responses
ASH_MODEL=gpt-5
ASH_BASE_URL=https://api.example.com
ASH_API_KEY=your-key
```

CLI 会自动加载该文件，命令行参数优先于 `.env`。`.env` 已被 `.gitignore`
排除。

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

单次输出模式：

```bash
cargo run -p ash-cli -- --print "inspect this project"
```

## 项目上下文

Ash 启动时会构建一份精简的系统上下文，包含：

- 内置的 coding agent 工作约定
- 当前目录、shell、日期、时区、操作系统和架构
- 从文件系统根目录到当前目录依次生效的 `AGENTS.md`
- `skills/` 中可用的 Skills，以及通过 `--skill` 显式启用的 Skill 指令

同一目录存在 `AGENTS.override.md` 时，它会替代该目录的 `AGENTS.md`。更深目录的
指令优先级更高；处理子目录文件前，Agent 仍会检查是否存在更具体的说明。

Skills 支持 `skills/name.md` 和 `skills/name/SKILL.md` 两种布局：

```markdown
---
name = "review"
description = "Review changes for correctness"
tools = ["read", "grep", "find"]
---
Review the relevant code and report concrete findings.
```

使用 `--skill review` 会加载完整 Skill 指令，并应用可选的模型和工具覆盖；未显式
启用时，模型仍会收到可用 Skills 的名称、描述和文件位置，可按任务需要读取。

## 会话历史

交互会话按“一会话一文件”保存为 JSONL。Linux 默认目录为
`~/.local/share/ash/sessions/`，文件名直接包含本地日期和时间：

```text
session-2026-07-14T16-30-25.123-<session-id>.jsonl
```

文件第一次提交消息时才会创建。首行保存格式版本、Session ID、模型、协议、工作
目录、最终系统提示词、工具定义和上下文限制等配置快照；后续按顺序追加完整用户
消息、Assistant 消息、工具结果和 Turn 结束状态。API Key、访问令牌和自定义接口
地址内容不会写入文件。

`/new` 和 `/clear` 使用相同逻辑：清空模型会话历史并建立新的 Session。终端已输出的
稳定历史会保留在 scrollback 中；ASH 只移除当前输入区或菜单，再追加新的欢迎区。`/resume`
同样保留已有 scrollback，在新的欢迎区后完整重放所选 JSONL。新 Session 会立即获得 ID
和创建时间，但在第一条用户消息发出前不会创建文件；会话名称取第一条有效用户消息。
`/resume` 会在输入框下方列出其他已保存会话的名称和创建时间，使用方向键选择。输入框的
跨进程历史单独保存在 `~/.local/share/ash/history.jsonl`。

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

交互界面不进入 alternate screen。模型输出直接进入 shell 的历史，输入提示符始终
出现在最新内容的下一行。`/new` 和 `/clear` 在会话未溢出时局部清理，溢出或布局
不确定时清空当前可见屏幕；终端 scrollback 始终保留。

底部可变区域使用 Ratatui 组件统一布局，包括活动内容、状态栏、输入框、命令补全
和底栏；完成后的用户消息、Thought、工具与回答仍由 Crossterm 写入主屏 stdout，
从而进入原生 shell scrollback。Ratatui 不接管 alternate screen，也不保存一份虚拟
全屏历史。历史区和底部区域共享块间距规则：完整块只负责内容和内部 padding，父级
Stack 使用 `Flex::Start` 与统一 spacing 排列；输入框和模型、路径或补全 footer 是
同一个 ComposerBlock。

模型文本以完整换行作为提交边界。正常情况下逐行展示；当等待队列积压时会自动
批量追赶。未完成的半行会保留到下一次换行或本轮响应结束，表格则会暂存在可变
区域，避免流式过程中列宽反复跳动。

模型提供思考摘要时，活动区固定显示最多四行：首行为 `Thinking (Xs)`，下面三行
行内滚动并始终保留最新内容。正文、工具调用或本轮结束后，思考区折叠为一行
`Thought for Xs`；展开期间的思考正文不会写入 shell scrollback。OpenAI Chat
Completions 兼容接口会识别 `reasoning_content`、`reasoning` 和 `thinking` 字段，
Responses 接口只展示 reasoning summary，不展示原始 reasoning text。

工具调用使用语义化单行摘要，不直接打印参数 JSON。读、写、编辑仅显示短文件名；
搜索和列目录只保留关键词及一个短目录；Shell 调用只显示命令本身。文件内容、编辑
前后文本、默认参数、工作目录和绝对路径不会进入终端历史，失败调用也使用相同的
精简格式。每个工具调用仍是独立历史块，块之间统一保留一行间距。

- `Enter`：提交
- 输入 `/`：显示斜杠命令补全；继续输入会按命令名或别名过滤
- 补全菜单中 `↑` / `↓`（或 `Ctrl-P` / `Ctrl-N`）：切换选择
- 补全菜单中 `Tab`：补全命令；`Enter`：执行当前选择；`Esc`：关闭菜单
- `↑` / `↓`：输入历史
- 任务运行时按一次 `Esc`：取消并撤销当前一轮，将原问题恢复到输入框
- 任务运行时状态栏显示 `esc to interrupt`，第一次按 `Esc` 不展示额外状态
- 任务运行时 `Ctrl-C` 不取消当前请求
- 空输入时 `Ctrl-C` 或 `Ctrl-D`：退出
- `exit` / `quit`：退出

按下 `Esc` 会从模型会话历史中移除当前轮，只擦除当前屏幕中属于该轮的用户消息和
回复，并在原位置把问题恢复到输入框；它不会重建整个屏幕，也不会反向撤销已经由
工具写入文件系统的修改。ASH 不会 Purge shell scrollback，
因此已经滚出当前可见屏幕的本轮内容可能仍由终端保留，但不会继续存在于模型上下文。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

当前架构和约束见 [DESIGN.md](DESIGN.md)。
