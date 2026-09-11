# Ash

Ash 是一个 Rust 编写的命令行 coding agent。目前主链路包括：

- Anthropic Messages、OpenAI Chat Completions、OpenAI Responses 三种协议
- 真正的 SSE 增量输出
- 连续对话与工具调用历史
- 默认启用 `read`、`glob`、`grep`、`bash`、`edit`、`write`、`webfetch` 七个内置工具
- 独立子 Agent、串行协作 Group 和动态规划组织的 Workflow 管理者
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

非交互 `run` 结束时若仍有运行中或未接收结果的协作工作，会列出目标及 owner、关闭后代并非零退出。
这只检查执行与接收事实，不判断业务成功。交互模式提醒未收尾工作，不自动推进；wait 开始时会显示正在等待的目标。

每个 Session 最多接收 64 个尚未完成的 turn，包含正在执行的 turn。异步 `submit` 在容量耗尽时
等待，`try_submit` 立即返回队列已满；CLI 使用后者，保持控制命令可响应。

## 项目上下文

Ash 启动时会构建一份精简的系统上下文，包含：

- 内置的 coding agent 工作约定
- 选定角色的职责和工作原则（`--profile`，默认 `default`）
- 当前目录、shell、日期、时区、操作系统和架构
- 已安装工具所属模块提供的说明，包括协作与 Skills 的可选项
- 从 Git 项目根目录到当前目录依次生效的 `AGENTS.md`

更深目录中的 `AGENTS.md` 指令优先级更高。初始只加载项目根到 cwd；文件工具首次进入新作用域时先返回适用规则，本次不执行操作，模型阅读后重试。规则按文件路径去重，同一文件更新替换旧内容，不加载兄弟目录规则。找不到 Git 项目根时从 cwd 开始。此检查不是文件沙箱，bash 仍需主动检查适用规则。

### Agent 角色

内置角色位于 `crates/ash-agent/agents/{default,explore,review}.md`，以 Markdown 正文定义
角色指令，frontmatter 只声明非空 `description` 和可选的逗号分隔 `tools` 字符串，不配置模型。
这些文件编译进二进制，当前不从用户 / 项目目录覆盖或热加载。

- `default`：通用开发，默认七个常规工具。
- `explore`：代码定位与关系梳理，初始工具去掉 `write` / `edit`。
- `review`：核实正确性与回归风险，初始工具与 `explore` 相同。

例如 `cargo run -p ash-cli -- --profile review run "review the current changes"`。
角色的工具选择不是安全沙箱：`explore` / `review` 仍可使用 `bash`，显式启用的 Skill 也可增加工具。

普通委派调用 `agent({"profile":"review","prompt":"review the changes"})`；省略 `profile`
使用 `default`，省略 `prompt` 只创建、不执行。后续通过返回的 `agent_id` 调用 `message` 和
`wait`。角色定义在创建时固定，恢复也不替换为新版同名角色。

`ash-workflow` 另内置 `workflow` 角色：只在创建这种子代理时安装组织装配工具与专用说明。
可选角色名称与用途由主 agent 的 `agent` 工具或管理者的 `workflow` 工具根据实际定义自动注入，不在主模块维护另一份名单。
普通子代理默认没有创建下级的工具；组员只获得组内交流工具，拥有蓝图预建下级时才获得对应的管理工具。
Workflow 管理者只通过蓝图创建组织，不混用 `agent` / `group` 创建入口；Skill 不能重新开放被组织身份禁止的工具。

### Skills

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

使用 `--skill review` 会在启动时加载完整 Skill 指令，并保留可选的模型覆盖行为。
Skill 的 `tools` 现在是对角色初始工具集的追加，不再限制已有工具；省略或空列表不增加工具，
重复名称复用既有实现，未知名称使装配失败。启动 Skill 同样应用于本次 CLI 装配的普通子代理定义；
Workflow 管理者由专用模块安装 skill，可按需加载项目 skill。

未显式启用 Skill 时，系统上下文只提供可用 Skills 的名称和描述。模型在任务
匹配时调用 `skill({"name":"review"})`，再获得完整指令、Skill 基础目录和最多 10 个资源
文件路径。对启动时已激活的 Skill，工具只返回资源信息与已加载提示，不重复正文。
`skill` 也是 Agent 层的运行时工具。调用会为当前 Session 追加声明的已注册工具；工具结果持久化后，下一次模型请求才看见新工具。同批调用不能提前使用它们，其他 Session 不受影响。重复激活幂等，未知工具使整次安装失败。运行中不修改模型；能力快照随个人记录恢复，缺少实现时明确报错。

## 会话历史

交互会话按“一会话一文件”保存为 JSONL。默认目录为 `~/.ash/sessions/`，文件名就是
Session ID：

```text
<session-id>.jsonl
```

普通新会话在第一个 Step 完成或 Turn 结束时创建文件；带非空历史的 fork 会立即写入新文件。
首行 `Init` 保存身份、标题和初始角色 / 提示词 / 能力快照。每轮写入 `TurnStart`、零个或多个
`TurnStep` 和 `TurnEnd`；`TurnStep` 将工具结果与更新后的能力快照一起同步落盘，然后才用于下一次
模型请求。`Checkpoint` 保存压缩摘要。恢复遇到未封口的 Turn 时截去其对话内容，但通过
`Definition` 记录保留已经提交的能力快照，避免再次恢复时丢失已安装工具。

恢复使用保存的角色正文、工具名单、模块说明和已观察目录规则；工具实现仍由当前运行时提供，
缺失实现会报错。模型、协议、密钥和供应商配置由当前启动配置提供，不自动写入定义快照；
但提示词、工具输出及聊天属于持久化内容，不应将敏感数据放入其中。环境上下文保留创建时快照，
实际文件 / shell 操作仍使用当前运行时的工作目录，恢复时应使用原工作目录。旧协作 / JS workflow
没有兼容入口或迁移层。

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

## 协作与 Workflow

普通委派不要求建组。模型可用六个通用协作工具，名称统一为小写：

| 工具 | 参数 | 作用 |
| --- | --- | --- |
| `agent` | `profile="default"`, `prompt?` | 创建固定角色的直属实例；有 prompt 才启动 |
| `message` | `agent_id`, `message` | 向直属实例 / 所有组成员委派或打断；当前组员向同组成员登记后继 |
| `group` | `group_id`, `agent_id?` | 按 owner 范围查找或新建组，幂等加入尚未运行的直属实例 |
| `history` | `group_id?`, `before?`, `limit=10` | 查询公共聊天；组员可省略所属组 ID，parent 需要明确组 |
| `wait` | `agent_id?`, `group_id?`，恰选一个 | 等待目标完全停止，接收最后消息或异常诊断 |
| `list` | `group_id?` | 查询直属子代理、组或指定可见组的状态，不接收结果 |

`group_id` 创建时可用名称，返回值是稳定 ID；后续建议使用返回的 ID。一个组同时最多运行一个
直接成员。组员可调用 `message` 登记唯一后继，只有当前成员正常停止后才启动下一位；没有后继
就停止。不同组与独立 agent 可并行，但首版共用工作目录，**不提供 worktree 或并发写入隔离**。

目标运行时的 `message` 采用取消旧 turn 后启动替代输入，不排 FIFO；已接受的改派尚在收束时再次
改派会报错，不默默丢中间输入。目标已停止但最后结果未接收时，必须先 `wait`，才能再次发消息。
`wait` 在父会话工具结果持久化后才解除未读门禁；取消等待不取消目标。组必须整体 wait，不能单独
wait 组员。停止不是业务成功，失败、取消、截断也返回，parent 决定是否继续。

组员可读共享聊天，但不共享个人历史或思考。每组在
`~/.ash/sessions/collab/<root-id>/groups/<group-id>/` 下保存 `prompt.md` 和 `chat.jsonl`。
`group` / `list` 返回具体路径。共享提示词每个成员 turn 开始时读取；编辑聊天文件不触发调度。
历史默认最新 10 条，`before` 是排他分页锚点，返回上限 100 条 / 64 KiB，原记录不因展示截断而改写。

### Workflow 入口

```text
/workflow 分析需求，实现修改，并组织复核
```

快捷命令与 `agent({"profile":"workflow","prompt":"任务"})` 走同一创建路径；外部主 agent 仍为
`default`。Workflow 管理者读取内置 skill，针对任务生成蓝图，再调用其专有的 `workflow(blueprint)`
工具。蓝图描述 `agents[{name,profile?,parent?}]` 和 `groups[{name,owner?,members,prompt?}]`；
名称只用于蓝图内部引用，省略 parent / owner 表示管理者。无需预先准备 YAML 文件。

装配器先校验引用、归属和无环 parent 树，再创建整套未启动实例及共享文件，返回真实 ID 和路径。
执行时仍只用 `message` / `wait` / `history`，不增加脚本语言或第二套调度器。组员没有动态创建工具，
但能管理蓝图预建的直属下级。管理者提前答复不等于后代停止，后续 turn 可查询并接管。

恢复重建组织和子会话；中断中的工作交付 `interrupted` 诊断，不自动重发消息、重跑 shell 或重启后继。
接收是否持久化不确定时允许重复交付，不承诺外部副作用恰好执行一次。退出、新建、fork / undo 切换
会由 CLI 关闭旧根组织；fork 复制个人历史和能力，不克隆协作组织，也不回滚文件修改。

模块边界与完整时序见 `COLLABORATION.md`、`DESIGN.md`；契约与验收记录见 `todo.md`。

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
工具（如 `read`、`skill`、`agent`、`wait`）会聚成一个摘要；展开后需要显示正文的
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
