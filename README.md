# Ash

Ash 是一个 Rust 编写的命令行 coding agent。目前主链路包括：

- Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 三种协议
- 真正的 SSE 增量输出
- 连续对话与工具调用历史
- 默认启用 `read`、`glob`、`grep`、`bash`、`edit`、`write`、`webfetch` 七个内置工具
- 可并行提交、异步收取结果的命名子 Agent
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
这些配置在启动时读取、解析一次；嵌入式调用通过 `ProviderConfig.model_config` 显式传入，
不同 adapter 的配置相互独立，请求转换不再读取进程环境变量。

模型上下文窗口默认是 1M token，可用 `--max-context-tokens` 或
`ASH_MAX_CONTEXT_TOKENS` 覆盖。Ash 使用每 4 个 UTF-8 字节约 1 token 的轻量估算，不引入 tokenizer
依赖；该估算只用于模型调用前的 context preflight 和自动压缩判断。Turn 结束后的 `Worked`
行显示本轮所有模型请求返回的输入、输出 token、生成速度和实际工具调用数；服务端未返回
usage 时不会用本地估算回填。

估算输入达到模型上限的 80% 时，Ash 会在正式请求前自动压缩模型上下文。压缩器使用固定
system prompt，在一次不带工具、未指定业务输出上限的独立请求中摘要全部已完成 Turn；当前
正在运行的 input 和 steps 始终留在摘要外。再次压缩只发送旧摘要之后新增的 Turn。原始
JSONL 和会话标题保持完整；内存 `Conversation` 在 checkpoint 落盘后立即释放其覆盖的 Turn，
只保留摘要与之后的 Turn。摘要 prompt 会限制工具文本且不携带附件；工具刚执行完写入会话前
仍有 64 KiB 的运行时上限。交互模式下可用 `/compact` 随时主动更新 checkpoint。

单次输出模式：

```bash
cargo run -p ash-cli -- run "inspect this project"
cargo run -p ash-cli -- run --log "inspect this project"  # 额外输出 debug 日志到 stderr，便于排查
```

非交互模式按已提交的 step 输出文本，而不是立即输出每个流式 delta，避免网络重试时将被丢弃的
响应混入 stdout。交互模式仍显示实时预览，重试时会清除当前未提交的响应。

每个 Session 最多接收 64 个尚未完成的 turn，包含正在执行的 turn。异步 `submit` 在容量耗尽时
等待，`try_submit` 立即返回队列已满；CLI 使用后者，保持控制命令可响应。

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

交互会话按“一会话一文件”保存为 JSONL。默认目录为 `~/.ash/sessions/`，文件名就是
Session ID：

```text
<session-id>.jsonl
```

普通新会话在第一个 Step 完成或 Turn 结束时创建文件；带非空历史的 fork 会立即写入新文件。
首行是 `Init { identity, created_at, title }`，每轮随后写入一个 `TurnStart { id, input }`、零个
或多个 `TurnStep { step }`，最后由 `TurnEnd { result, stats, summary }` 封口；独立的
`Checkpoint { summary }` 记录手动压缩。每个完成 Step 都会先刷新并同步到磁盘，再用于下一次
模型请求和通知 UI。恢复时单次流式重放，遇到 checkpoint 就丢弃它覆盖的 Turn，
不会把整个 JSONL 长期留在内存；若文件尾部留下开放 Turn，则从 `TurnStart` 起截断，既不显示也
不放入最终会话。旧格式不读取或迁移。恢复会话沿用当前运行配置，
不从 JSONL 恢复模型、协议、工作目录、系统提示词或工具定义，因此 API Key、访问令牌和
自定义接口地址内容不会写入文件。

`/new` 和 `/clear` 使用相同逻辑：清空模型会话历史和终端 scrollback，并建立新的
Session。`/status` 显示当前会话的模型、协议和工作目录。`/exit`（别名 `/quit`）退出
Ash。`/resume` 会用所选 JSONL 的最新摘要和之后的消息重建内存与终端 scrollback。
`/undo` 会把最近一轮（上一条用户输入及其回答）从模型会话历史和终端 viewport 中移除，
并把该输入恢复到输入框，便于修改后重新提交。`/fork` 会列出当前会话的历史用户输入；
选中后创建一个新的 fork Session，继承该输入之前的消息，并把所选输入恢复到输入框。
原 Session 的内存历史和 JSONL 都不会改变。
通过 `/new` 或 `/clear` 建立的 Session 会立即获得 ID，但在第一条用户消息发出前不会
创建文件；会话列表中的创建时间取第一次持久化记录，会话名称取第一条有效用户消息。
`/resume` 和 `/fork` 都会在输入框下方显示选择菜单，使用方向键选择。输入框的跨进程
历史单独保存在 `~/.ash/history.jsonl`。

## 子 Agent

默认交互链路提供五个异步协作工具：

- `agent`：创建命名 Agent、提交初始 `message`，立即返回 accepted 和 Turn ID
- `message_agent`：向现有命名 Agent 提交 FIFO follow-up，立即返回 accepted 和 Turn ID
- `wait_agent`：返回当前全部未读结果；仍有任务但暂无结果时等待下一次完成
- `list_agents`：读取当前根 Session 下 Agent 的 idle/running 状态
- `remove_agent`：删除没有运行任务和未读结果的 Agent

每个 child 自己保存 pending Turn 和未读结果；`wait_agent` 是唯一消费结果的入口，没有队列
参数和内部超时。取消等待不会消费结果，也不会取消 child。一次 wait 会取走当时所有 Agent
的未读结果；同一 child 内保持 FIFO，不承诺不同 child 之间的全局完成顺序。结果消费完且
Agent idle 后可以删除，名字随后可立即复用。

这些工具只安装在主 Agent 上。子 Agent 从干净的基础 Agent 派生，继承当前模型、普通工具、
AGENTS.md 和 Skills 配置，但不继承协作工具或主 Agent 的编排提示，并从空历史开始。名字在
当前 root Session 内有效：trim 后必须非空、不含控制字符且不超过 64 个 Unicode 字符。

child 与 root 使用同一套普通 Session 和 JSONL 持久化规则，保留自己的多轮历史，并通过
`root_id`、`parent_id` 记录 lineage。root 会话列表不展示 child；tree 查询和删除覆盖其下
的 durable child。目前不提供 child picker 或单独 resume 入口。

## 终端行为

交互界面使用 Ratatui inline viewport，不进入 alternate screen，也不捕获鼠标。终端原生
滚动、选择和复制因此保持可用。尚未完成的流式 Thought、工具与回答存在于本轮 live
viewport；它按实际内容高度增长，填满首屏后在首屏内部跟随最新内容。

收到已经持久化的 `StepCommitted` 后，UI 会用完整 Step 替换当前预览，并立即写入终端
scrollback；`Finished` 只补齐可能遗漏的 Step 和本轮 footer，不重复输出已经提交的内容。
完成历史随后释放渲染缓存，不参与逐帧渲染。resume、fork 和压缩都把当前内存
`Conversation`（可选摘要及其后的 Turn）转换成相同的历史块；resize 使用同一历史块渲染器
按新尺寸清空并重放当前内存内容。日常滚动、选择和复制仍由终端模拟器负责。`/new`、
`/clear` 会同时清除语义历史、可见屏幕和 scrollback，`/resume` 和完成的 `/fork`
则从对应 Session 消息重新渲染历史。

模型提供思考摘要时，`Thinking (Xs)` 和完整思考正文会随 transcript 持续向下滚动。
正文、工具调用或本轮结束后，思考区折叠为一行 `• Thought for Xs`。OpenAI Chat
Completions 兼容接口会识别 `reasoning_content`、`reasoning` 和 `thinking` 字段，
Responses 接口只展示 reasoning summary，不展示原始 reasoning text。

工具调用使用语义化单行摘要，不直接打印参数 JSON。折叠状态下没有结果正文的连续同名
工具（如 `read`、`skill`、`agent`、`wait_agent`）会聚成一个摘要；展开后需要显示正文的
工具会恢复为独立调用。`glob`、`grep` 折叠时显示结果数量，`webfetch` 显示 HTTP 状态和
字符数，因此不参与聚合；三者展开时都显示完整结果。`bash` 始终显示实际命令，不猜测
Shell 意图。聚合只影响展示，底层工具调用、结果和会话记录仍保持独立。

工具输出、bash 命令续行和思考区默认只显示少量行（折叠模式，5 行预算，超长时保留
头尾加省略号）。按 `Ctrl-O` 可全局展开全部内容，再次按恢复折叠；展开状态跨会话保持。
`read` 的成功输出在两种模式下都不显示。

- `Enter`：空闲时提交新一轮；任务运行时向同一 Session 队列提交下一轮
- 输入 `/`：显示斜杠命令补全；继续输入会按命令名或别名过滤
- 补全菜单中 `↑` / `↓`（或 `Ctrl-P` / `Ctrl-N`）：切换选择
- 补全菜单中 `Tab`：补全命令；`Enter`：执行当前选择；空闲时 `Esc`：关闭菜单
- `↑` / `↓`：输入历史
- `PageUp` / `PageDown`：按页浏览当前 live turn
- `Ctrl-Home` / `Ctrl-End`：跳到当前 live turn 顶部或底部
- `Ctrl-O`：在所有可见工具输出与思考区之间切换折叠（5 行预览）与完整展开
- 鼠标滚轮和终端原生快捷键：浏览已完成的 scrollback
- 任务运行时按一次 `Esc`：撤回未完成的响应块并中断当前轮；已经提交的 Step 保留在历史中，
  尚无完成 Step 且没有后续排队任务时撤销整轮并把原问题恢复到输入框
- 任务运行且输入框为空时 `Ctrl-C`：取消当前 Turn；有草稿时先清空输入
- 空闲且输入框为空时 `Ctrl-C` 或 `Ctrl-D`：退出
- `exit` / `quit`：退出

运行中提交的消息会创建独立 Turn，由 Session 按接收顺序执行。排队输入只在对应 Turn
真正开始时进入终端历史，不会切断当前流式回答。`Esc` 不会反向撤销已经由
工具写入文件系统的修改。斜杠命令始终显示；运行中输入 `/new`、`/clear`、`/resume`、
`/undo`、`/fork` 或 `/compact` 时，命令不会发出，输入会保留，界面不显示额外状态。
`/status` 同样会被阻止，只有 `/exit` 会直接结束程序。`/undo` 会移除最近一轮并恢复其输入，
但不会撤销工具副作用。`/fork` 不会修改原 Session；它会从所选输入之前的历史创建新
Session，随后重放继承的历史并恢复该输入。

## 开发检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

当前架构和约束见 [DESIGN.md](DESIGN.md)。
