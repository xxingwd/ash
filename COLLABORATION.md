# Ash 协作模型

> 实现说明。行为契约与细粒度验收状态以 `todo.md` 的 C01–C12 为准，执行与存储细节见 `DESIGN.md`。

## 1. 最小模型

- **Profile**：固定角色定义，Markdown 正文加 `description`、可选 `tools` 字符串；不是实例，不配置模型。
- **Agent**：持续存在的独立会话。一次创建，后续通过实例 ID 通信，保留自己的个人历史。
- **Group**：共享背景和公共聊天的一条串行工作线，不是共享个人会话，也不是业务任务状态机。
- **Workflow 管理者**：选择 workflow profile 的普通子 agent，负责根据任务规划组织、委派和接收结果。
- **Runtime 执行侧**：执行确定性动作，负责身份、能力、调度、存储和取消；不替模型判断业务成功。

普通委派不强制建组。需要共同交流和往返协作时才建立 group；需要事先组织多层结构时才使用 workflow。
首版没有 worktree、分支、自动合并、独立 stop 工具或通用脚本语言。不同工作线可并行，但共用真实文件系统，
不能把 group 串行误认为所有文件写入都有隔离。

## 2. 模块和入口

| 模块 | Rust 入口 / 核心类型 | 模型工具 | 职责 |
| --- | --- | --- | --- |
| ash-core | Tool、ToolOutput、RepositoryInstruction、SessionIdentity | 无 | 中立工具契约、能力效果、具作用域的规则及身份 |
| ash-agent | Profile、PromptContext、AgentSnapshot、Runtime、Session、install_skills | skill | 角色、提示词组合、会话能力、模型执行和个人持久化 |
| ash-tools | tools | read、write、edit、glob、grep、bash、webfetch | 常规操作；文件工具在执行前发现适用 AGENTS.md |
| ash-collab | AgentControl、Definition、WeakControl | agent、message、group、history、wait、list | 身份关系、串行组控制、结果接收及公共聊天 |
| ash-workflow | definition | workflow，仅管理者 | 内置角色、内置 skill、蓝图校验与组织装配 |
| ash-cli / ash-tui | 配置安装、/workflow、状态事件 | 不新增模型工具 | 创建管理者、交互和同源轨迹展示 |

ash-workflow 单向依赖 ash-collab；定义提供能力安装函数，不要求 collab 识别 workflow 的提示词正文。
工具使用控制器弱引用；应用保留强控制器，避免 Session 和工具互相持有导致资源永不释放。

## 3. 六个通用工具

参数均为 JSON 具名字段；ID 是 runtime 返回的实例身份，不是 profile 或蓝图局部名称。

| 函数 | 参数 | 返回 / 作用 |
| --- | --- | --- |
| agent | profile 可选，默认 default；prompt 可选 | 返回 agent_id、profile、parent_id；省略 prompt 只创建，有 prompt 才运行 |
| message | agent_id、message | 开始、改派或登记后继；返回消息标识及接受行为，不是最终答复 |
| group | group_id、agent_id 可选 | 查找或新建自己的组，可幂等加入未运行的直属实例；返回稳定 ID 与文件路径 |
| history | group_id 可选、before 可选、limit 默认 10 | 最新公共聊天，含 next_before 和截断标志；不接收结果 |
| wait | agent_id / group_id 恰好一个 | 等该工作线完全停止，返回最后消息及执行状态；从未运行返回 null |
| list | group_id 可选 | 直属子 agent 和组，或指定可见组详情；不接收结果 |

所有工具名和字段用 snake_case。没有 run_group、next、ack、group_history、wait_agent 或 message_agent。

六工具是模块提供的能力全集，不是每个实例都安装六个。组织能力由真实位置派生，不在 profile 中新增权限配置：

| 实例位置 | 安装的协作工具 |
| --- | --- |
| 主 agent | agent、group、message、wait、list、history |
| Workflow 管理者 | workflow、message、wait、list、history；组织创建只走蓝图 |
| 无下级、无组归属的普通子代理 | 无协作工具；通过最后答复交付 |
| 无下级的组员 | message、history、list；没有 wait |
| 有预建下级 / 组的普通代理 | 增加管理已有下级所需的 message、wait、list；拥有组时有 history，但没有创建能力 |

是否组员与是否拥有下级分别判断。组员拥有预建下级时可以 wait 下级，但仍不能 wait 自己所属的组。
主 agent 由 `install_root` 安装；其他实例在创建 / 蓝图装配、未启动实例入组时确定工具，不在每轮反复切换。

### 创建与身份

`agent({"profile":"review"})` 创建尚未运行的直属实例。创建后角色固定；group 入组也不能换角色。
`group({"group_id":"implementation"})` 可先建空组，随后逐个传 `agent_id` 加入。名称在 owner 范围内查找；
返回的 group_id 是 UUID，文件路径只使用 runtime ID，不使用调用者名称拼路径。

一个实例最多归属一个组，必须是 owner 的直属实例且尚未启动；已在同组的重复调用幂等。
组运行期间不能改变成员关系。首次创建共享文件，查询已有组不会清空文件。

### 通信与改派

| 关系 / 状态 | 行为 |
| --- | --- |
| parent → 直属独立 agent / 自己组的成员，未启动或结果已读 | 启动工作 |
| 同上，正在运行 | 接受替代输入，取消旧 turn，收束后启动替代工作 |
| 同上，已接受替代但还在收束 | 拒绝第二次改派，提示稍后重发；不覆盖已接受输入 |
| 同上，停止且结果未读 | 拒绝，提示先 wait 对应 agent / 整组 |
| 当前组员 → 同组另一成员 | 登记唯一后继，暂不启动 |
| 组员 → 自己预建的直属下级 | 正常委派 / 等待，不占用同组后继槽位 |
| 子 agent → parent | message 拒绝；以最终答复求助，由 parent wait 接收 |
| 自己、无关实例或非直属后代 | 拒绝 |

同批工具可能并行调用。控制器原子裁决登记和改派，不以模型输出顺序假定工具依赖。
改派只复用 Session 的取消机制，不复用 FIFO 队列语义；取消不回滚文件或 shell 副作用。
接受后的控制动作由 runtime 完成，即使调用方的工具 future 已被取消；接受前取消仍可拒绝。

### 完成与 Wait

Group 同时至多有一个直接成员在执行。当前成员正常结束并登记了后继，则保持运行，启动后继。
没有后继、失败或取消时停止；parent 改派优先于旧后继。切换间隙不对外报告为空闲。

**完成只表示无人运行，不等于业务成功。** 预建下级属于独立工作线；上层协调者停止并不自动停止后代。
协调者应先接收自己委派的下级结果，再宣称工作完成。后续 turn 可以继续查询和接管。

wait 等连续工作线，不要求轮次 ID，也不让旧 turn 的取消提前结束整组等待。返回最后文本，
不拼接此前进度、工具输出或思考。失败、取消、截断、无正文或启动失败以 runtime 诊断交付。
组必须整体等待；wait 自己、parent、所属组或组内单独成员都报错，避免自锁。

接收提交时序：

```text
目标停止 → wait 选定最后结果
         → parent 工具结果与能力快照落盘
         → committed 钩子按消息 ID 清除未读并落盘
         → parent 下一次模型请求
```

list/history 不改变未读。取消 wait 不取消目标。选定后重复 wait 可返回相同结果；旧结果的迟到提交
不能清除更新结果的未读。wait 与 message 放在同批不能形成隐式先后关系，应在下一次请求继续。
恢复无法证明交付时重新提供结果，而不是错误解锁新工作。

`wait` 另返回 `pending`：已停止目标仍遗留的运行中 / 未读后代，包含 kind、目标 ID、owner_id 和 state。
最后消息与结束状态仍按原规则固定；pending 是当前观察快照，不修改最后答复，也不要求这些后代停止才返回。
诊断最多返回 32 项，更多时标记 pending_truncated；为诊断预留输出预算，最后消息仍可截断并保留完整个人 / 公共记录。
`list` 的顶层 pending 观察调用者下方的遗留工作；查看不会接收结果。不把未启动、从未产生结果的实例算作未收尾。

非交互 `run` 在根 turn 结束时检查协作树。仍有运行中或未读工作时，关闭并收束后代，给出明确诊断并非零退出，
而不是将模型说“正在等待”当完成。交互模式给出提醒但不自动取消、续跑或派发。
wait 发起时显示具体目标；工具结果只在返回后提交，没有已提交结果不能推出“没调用 wait”。
运行中目标可能正在工作或等待其下级，不据此判断死锁，也不把任务测试通过当成 runtime 的结束判据。

## 4. 提示词和能力

System 顺序为显式基础 / 角色指令、环境、已安装工具所属模块说明、适用的 AGENTS.md。
角色正文只放到对应实例。可选 profile 的名称和描述由创建工具自动生成；普通会话不加载完整 workflow 正文。
工具说明以工具名为稳定键，模块安装工具时带入，移除工具也移除其说明，不做中心化插件框架。

Profile 文件内置打包：

```text
ash-agent/agents/default.md
ash-agent/agents/explore.md
ash-agent/agents/review.md
ash-workflow/agents/workflow.md
```

省略 tools 等于不限制初始常规目录；explore/review 初始无 write/edit，但保留 bash，不是只读沙箱。
Skill 的 tools 是追加已注册能力，不是限制器。安装先全部校验，再随当前 Session 的工具结果提交，
下一次模型请求才曝光新工具；同批工具不能抢先使用，新能力不影响其他 Session。未知工具使安装失败。
所有普通子代理都不安装创建能力，管理者只安装蓝图创建入口；创建 / 装配入口同时检查身份。
不允许的组织工具从实例可用目录一并移除，Skill 不能重新激活；可编辑提示词也不能绕过入口限制。
无协作能力的叶子不加载角色选择清单或协作使用说明；组员只加载组内说明，拥有下级才加载接收 / 调度说明。
主 agent 的 profile 清单由 agent 工具提供；管理者的清单改由 workflow 工具提供，CLI 不重复维护。

AGENTS.md 初始从项目根到 cwd 加载；文件工具首次访问更深作用域先把规则提供给模型，并延后本次操作。
规则按文件路径保留 scope 并去重，更新同一来源时替换旧版。不会扫描全仓或把兄弟目录规则当全局规则。
超长规则标记截断，编码 / 读取失败明确报错。任意 bash 脚本不做自动路径分析，应主动读取适用规则。

组共享 prompt、消息和关系是 turn 的执行上下文，不是静态角色或权限配置。每个成员 turn 开始时取 prompt.md
快照，并注入可见成员、直属下级和组 ID；重试和单个 turn 中的多次请求不反复追加这份动态输入。

## 5. 公共聊天

```text
<session-directory>/
  <agent-id>.jsonl
  collab/<root-id>/
    state.json
    groups/<group-id>/
      prompt.md
      chat.jsonl
```

个人 JSONL 保留每个 agent 的独立会话。公共聊天记录投递 / 登记、启动与最终消息，带发送者、接收者、消息 ID
和执行关联；不公开个人思考或工具 transcript。不使用 @ 语法解析，接收者由 message 参数决定。

chat 只是可观察记录，不是调度数据库：单条追加不能证明投递已可靠接受或执行，接受以控制状态和回执为准。
写入失败如实报错；崩溃后不根据聊天重发任务。parent 可查多个可见组，必须传组 ID；组员默认查所属组。
共享提示词路径可用于读取 / 修改，但改聊天文件不会触发消息。

history 默认最新 10 条，按时间顺序返回；before 为排他锚点，追加新消息不改变已选旧页。
最大 100 条且编码输出小于 64 KiB；单条太长有截断标志，原始文件不被改写。需要全文可读取实际聊天文件。

## 6. Workflow

```text
用户 /workflow 任务
  → 普通创建路径：agent(profile="workflow", prompt=任务)
  → 管理者读取内置 workflow skill，分析并规划
  → workflow(blueprint) 校验和装配完整未启动组织
  → 返回真实 ID / 路径
  → 管理者 message 委派，组员登记后继，各级 wait 接收
  → 外部 default wait 管理者，必要时 message 继续
```

Workflow 工具从管理者首次请求就存在；读取 skill 只加载用法，不临时改变工具列表或基础 System。
蓝图是管理者针对任务产生的 JSON 组织数据，不需要用户选择静态 YAML，也不是复杂执行语言。

```json
{
  "blueprint": {
    "agents": [
      {"name": "implementer"},
      {"name": "reviewer", "profile": "review"},
      {"name": "researcher", "profile": "explore", "parent": "implementer"}
    ],
    "groups": [
      {"name": "implementation", "members": ["implementer", "reviewer"], "prompt": "实现修改并交叉复核"}
    ]
  }
}
```

parent / owner 省略表示实际管理者，填写时引用蓝图内的 agent 名称。profile 缺省 default。
装配先校验默认角色、重复引用、parent 无环、group owner 与成员直属关系，再创建所有未启动资源。
一次最多 64 个 agent、32 个 group；成功返回名称到真实 ID 的映射、路径和装配关联。
失败只清理本次真正新建的资源，不删除已有文件或管理者；没有成员会因装配而自动执行。

普通代理无论是否入组都不能动态创建，但可以管理预建下级。管理者不混用 agent/group 创建入口；后续 message 复用实例，只有显式 workflow 调用才装配新组织。
跨组顺序与并行由管理者决定，组内由成员决定，不增加另一套 scheduler、wait、聊天或任务进度系统。

## 7. 恢复、关闭与展示

个人记录保存解析后的 profile、工具名与配套说明、提示词及已观察规则。Step 与能力快照一起提交；
未封口 turn 的对话截去后，已提交的能力仍保留。组织快照保留身份、归属、执行线、最终结果、未读、蓝图及装配关联。
恢复缺少工具实现或已运行子会话记录时明确报错，不静默换角色或创建空白替身。

中断中的工作停止为可读取的 interrupted 诊断；不重放消息、shell 或后继。读取证明不足时允许重复交付，
不承诺外部副作用恰好执行一次。用户应通过 parent 检查真实文件状态后决定是否发送“继续”。

退出 / 切换新根会话由 CLI 关闭并收束旧根下工作；普通 manager turn 结束不关闭后代。
fork/undo 复制个人历史与能力，不克隆整个协作图，也不撤销文件修改。恢复应使用原工作目录。

GUI 消费 collab 同源事件，group 是一条线路，显示当前成员、运行 / 空闲、异常和未读标志。
恢复或事件落后时替换为 runtime 快照，不从成员局部结束事件推断整组结束，不把空闲显示为业务成功。

## 8. 验证范围

自动化测试覆盖角色解析、System 组合、作用域规则、Skill 会话隔离和恢复、六工具、串行流转、并发改派、
未读提交、历史分页和输出上限、多组并行、组织装配及恢复诊断。测试使用受控模型响应，不依赖真实模型猜测时序。
2026-09-10 的 Grok 实测已覆盖三角色、Skill、目录规则、委派复用及单 / 双组 Workflow；最后双组接收闭环通过。
具体通过与失败样本见 `GROK_EVALUATION.md`；不能把小样本通过当稳定质量保证。
交互终端视觉验收以及强杀后恢复 / 磁盘故障组合仍须分别验收，不能用单元测试或非交互 run 冒充。
