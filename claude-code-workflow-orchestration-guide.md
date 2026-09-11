# Claude Code Workflow 编排完全指南

> 版本基准:2026-09 本会话的 Workflow 工具规范。
> 本文是"事无巨细"版:既覆盖 Workflow 本身的每一处语义细节,也把它放到 Claude Code 整个 agent 体系、以及业界通用编排概念(goal 驱动 / 流程驱动、DAG、scatter-gather、map-reduce 等)的坐标系里做对照。

---

## 0. 信息来源与置信度声明

| 置信度 | 内容 |
|---|---|
| 高(工具规范原文) | Workflow 的调用参数、脚本语法、API 语义、限额、缓存/resume、opt-in 规则、各模式描述 |
| 高(本会话配置) | workflow 规模 guideline = medium(≤15 agents),可通过 `/config` 的 "Dynamic workflow size" 调整 |
| 中(一般行为推断) | subagent 继承会话权限模式、journal/agent-*.jsonl 的具体字段格式 |
| 概念级(外部框架对照) | LangGraph / OpenAI Agents SDK / CrewAI / AutoGen / Temporal / Airflow / GitHub Actions 等描述为范式级对比,不保证与某个具体版本的行为逐条一致 |

文中所有代码示例均为 Workflow 脚本语言(纯 JS,无 TS 类型),可直接作为骨架改写。

---

## 1. 全景:Workflow 在 Claude Code 中处于什么位置

Claude Code 的"扩展执行能力"大致是一个分层结构:

```
┌─────────────────────────────────────────────────────────┐
│ 主循环(你 + Claude 对话)                                 │
│   ├─ 单个工具调用:Bash/Read/Edit/Grep…                    │
│   ├─ Agent 工具:派 1 个 subagent(可 fork 继承上下文)      │
│   ├─ Agent Teams:多个有名字的 teammate,SendMessage 互通    │
│   ├─ Workflow:一段确定性 JS 编排 几~几十个 subagent  ★本文   │
│   ├─ Skills:打包的指令集(/xxx 加载进上下文)               │
│   ├─ Hooks:settings.json 里的确定性拦截器(非模型驱动)      │
│   ├─ Cron / ScheduleWakeup:时间驱动的自我唤醒             │
│   └─ /loop:固定/动态间隔的循环任务                         │
└─────────────────────────────────────────────────────────┘
```

它们的本质区别在"**谁决定下一步**":

| 机制 | 决策者 | 确定性 | 适用 |
|---|---|---|---|
| 主循环工具调用 | 模型边看边决定 | 低 | 交互式、小任务 |
| Agent(单个 subagent) | 模型派发一次,subagent 内部自主 | 低-中 | 一个可独立完成的子任务 |
| Agent Teams | 模型 + teammate 各自自主 | 低 | 需要持续协作/互相发消息的角色分工 |
| **Workflow** | **你写的 JS 脚本(预先确定)** | **高(流程)/ 低(每个 agent 内部)** | 大规模扇出、需要可复现结构的工作 |
| Hooks | 配置文件 | 完全确定 | 强制规则(如"每次编辑后跑 fmt") |
| Cron / loop | 时间表 | 完全确定(触发时机) | 周期任务 |

**Workflow 的独特卖点**:

1. **规模**——单上下文装不下的工作(几十个文件、几百个调用点、几十个待验证的 finding),拆给几十个 agent,每个 agent 只装自己那一小块。
2. **确定性结构**——"先找、再去重、再验证、再汇总"这种控制流由 JS 写死,不依赖模型临场发挥"接下来该干嘛",因此可缓存、可恢复、可审计。
3. **扇出并行**——CPU 允许范围内多个 agent 同时跑,墙钟时间 ≈ 最慢链,而不是串行累加。
4. **成本/质量可调**——每个 agent 可以指定模型档位和推理力度(effort),机械活用 low,关键验证用高档。

---

## 2. 设计哲学:确定性外壳 + 非确定性内核

理解 Workflow 的钥匙是这一句:**编排层是普通程序,执行单元是大模型。**

- **编排层**(你的脚本):控制流、数据流、去重、计数、预算检查、缓存命中——全部是确定性 JS,和写普通程序一样有 bug、可以单步想清楚。
- **执行层**(每个 `agent()`):prompt 进去,一个完整 Claude Code agent 在自己的独立上下文里跑(自己 Read/Grep/Bash),结果出来。agent 内部是非确定性的。

这个分层带来几个直接推论:

1. **能用纯代码做的绝不要派 agent**。例:去重不该让 agent "帮我看哪些重复",应该在脚本里 `seen.add(key(b))`。规范原文给出的 dedup 示例特意标注 "纯代码去重,不需要 agent"。
2. **agent 的返回值是数据,不是给人看的 prose**。规范明确:subagent 被告知"你的最终文本就是返回值,不是人类可读消息,所以返回原始数据"。这就是为什么配 schema 是最佳实践——把"自由文本 → 结构"这个易错环节搬到工具调用层的强校验里。
3. **脚本要为"部分失败"而写**。任何一个 agent 都可能返回 `null`(用户中途跳过、API 终态错误重试后仍失败),所以所有消费结果的地方都要 `.filter(Boolean)`。
4. **可复现性是特性不是巧合**。禁 `Date.now()/Math.random()` 就是为了让"同脚本+同参数"100% 命中缓存,从而支持中断续跑。

---

## 3. 使用门槛:opt-in 规则(逐字级别)

Workflow 会派生大量 agent、消耗大量 token,所以它是**纯 opt-in** 工具。以下五种情况才算合法调用:

1. 用户 prompt 里出现 `ultracode` 关键字,且你会看到 system-reminder 确认它生效;
2. 会话级开启了 ultracode(同样有 system-reminder 确认);此时它变成**常设授权**:默认每个有分量的任务都该用 workflow 编排,目标是"最详尽、最正确的答案",token 成本不作为约束;只有纯对话轮次或琐碎机械修改才 solo;
3. 用户**用自己的话**明确要求了编排——"用个 workflow"、"跑个 workflow"、"fan out agents"、"用 subagent 编排这个"。注意判定标准是用户的原话,而不是"这个任务看起来很适合并行";
4. 用户调用了一个其指令明确要求调用 Workflow 的 skill / 斜杠命令;
5. 用户点名要跑某个具名的/已保存的 workflow。

**除上述之外——即使任务"明显能从并行中受益"——也不许调用。** 此时正确动作是二选一:
- 用 Agent 工具派**单个** subagent(不需要 opt-in);
- 或者向用户**描述**一个多 agent workflow 大概能做什么、**粗略要花多少 token**,问用户要不要跑。

⚠️ 最常见的违规是第 3 条的"善意推断":用户说"帮我彻底审查这些改动"≠ 要求编排。可以说"这适合跑一个约 N 个 agent 的 workflow,要不要?"——问,而不是直接跑。

**规模 guideline(本会话:medium)**:单次 workflow 控制在 **15 个 agent 以内**。这是 guideline 不是硬限,除非用户的 prompt 明确要求更大规模。用户可在 `/config` 里用 "Dynamic workflow size" 放开或收紧。

**规模应服从请求的口气**:
- "随便找找有没有 bug" → 少量 finder + 单票验证;
- "彻底审计一下" → 更大的 finder 池 + 3~5 票对抗验证 + 汇总阶段;
- 研究/评审/审计类天然偏向"彻底";快查类偏向"精简"。

---

## 4. 调用接口(工具参数全解)

| 参数 | 类型 | 说明 |
|---|---|---|
| `script` | string(≤524288 字符) | 内联脚本。**不要**先 Write 到文件再传路径——直接内联。每次调用自动把脚本持久化到 session 目录,并在返回值里给路径 |
| `scriptPath` | string | 改跑已持久化的脚本。**迭代修改的标准姿势**:对返回的路径用 Edit/Write 改文件,然后用 `{scriptPath}` 重跑,不要重发全文 |
| `name` | string | 运行具名/预定义/已保存的 workflow(内置的,或 `.claude/workflows/` 下的),脚本从名字解析 |
| `args` | any | 透传给脚本全局 `args`。**必须传真 JSON 值**,见 §6.7 |
| `resumeFromRunId` | string(`^wf_[a-z0-9-]{6,}$`) | 从上次 run 恢复,见 §12。恢复前必须先 `TaskStop` 停掉旧 run |
| `description` / `title` | string | **被忽略**——展示信息一律以 `meta.description`/`meta.title` 为准 |

### 4.1 执行模型:后台 + 通知

- 调用**立即返回** task ID,workflow 在后台跑;
- 完成时会收到 `<task-notification>`;
- 期间**绝不臆测/预测**尚未完成的结果——用户提前来问,就如实说"还在跑";
- 用 `/workflows` 看实时进度树(每个 phase 一个分组,agent 挂在 `▸ 名称` 下)。

### 4.2 常见的"跨轮次串联"用法

Workflow 不是必须一次做完所有事。规范推荐把大工程拆成**每轮一个 well-scoped 的 fan-out**,中间人(你)留在环里看结果、决定下一步:

| 阶段 | workflow 形态 |
|---|---|
| Understand | parallel readers 分头读子系统 → 结构化地图 |
| Design | judge panel:N 个独立方案 → 评审打分 → 综合 |
| Review | 分维度找问题 → 对抗验证 |
| Research | 多模态扫描 → 深读 → 综合 |
| Migrate | 发现所有改造点 → 逐点转换(worktree 隔离)→ 验证 |

---

## 5. 脚本解剖

### 5.1 骨架

```js
export const meta = {
  name: 'review-changes',           // 必填,短横线命名
  description: '多维度评审并对抗校验', // 必填,显示在授权对话框里
  phases: [                          // 可选,每项 { title, detail, model? }
    { title: 'Review', detail: '按维度找问题' },
    { title: 'Verify',  detail: '对抗性验证每个 finding' },
  ],
}
// ---- 从这里开始是脚本体 ----
```

**meta 的硬规则**:
- 必须是**纯字面量**:不允许变量、函数调用、展开、模板插值。写错直接解析失败;
- `meta.phases[].title` 必须与脚本里 `phase('…')` 的字符串**逐字相同**(大小写、空格都算);对不上时不会报错,但进度树会出现一个游离的分组;
- phases 条目里可以加 `model` 字段,表示该阶段整体用某个模型档位覆盖。

### 5.2 语言与运行环境

- **纯 JavaScript**。任何 TypeScript 特性——类型标注(`: string[]`)、interface、泛型——都会**解析失败**;
- 脚本体运行在 async 上下文里,顶层可以直接 `await`;
- 可用标准 JS 内置:`JSON`、`Math`(除随机)、`Array`、`Object`、`Set`/`Map`、字符串/正则方法等;
- **不可用**:文件系统、任何 Node.js API(`fs`/`path`/`process`/`require`/`import` 模块加载);
- **不可用的三个时间/随机源**(调用即抛错,为了可 resume):
  - `Date.now()`
  - `Math.random()`
  - 无参 `new Date()`
  - 替代方案:时间戳由 `args` 传入,或 workflow 返回后再打戳;要"随机感"就用 **index 变化** prompt/label(确定性伪随机)。

### 5.3 执行顺序心智图

```
Workflow 调用
  └─ 校验 meta / 解析脚本
  └─ 按 code 顺序执行(遇 await 挂起)
       ├─ agent(...)  → 排队 → 拿到并发槽 → 跑 subagent → 返回结果或 null
       ├─ parallel([...]) → 全部 thunk 结束才返回(barrier)
       ├─ pipeline(items, …) → 每个 item 独立流水(无 barrier)
       ├─ phase('X') → 切换当前进度分组
       ├─ log('…') → 输出一行叙述给用户
       └─ return 值 → 成为 workflow 的最终结果
```

---

## 6. 运行时 API 完整参考

### 6.1 `agent(prompt, opts?)` —— 派发一个 subagent

```js
const r = await agent('在这个 crate 里找未处理的 unwrap', {
  label:  'find:unwrap',      // 进度树里的显示名
  phase:  'Find',             // 显式指定进度分组(推荐,防竞态)
  schema: BUGS_SCHEMA,        // JSON Schema → 强制结构化返回
  model:  'sonnet',           // 少用;默认继承会话模型
  effort: 'low',              // 推理力度:'low'|'medium'|'high'|'xhigh'|'max'
  isolation: 'worktree',      // 只在并行改文件时用
  agentType: 'Explore',       // 自定义 subagent 类型
})
```

**返回值规则**:
- 无 `schema` → 返回 agent 的**最终文本**(string)。subagent 被告知"你的最终文本就是返回值",所以拿到的会是原始数据而非客套话;
- 有 `schema` → subagent 被强制在结尾调用 StructuredOutput 工具,返回**已通过校验的对象**;不符合 schema 时工具层会拒绝并让 agent 重试——这是它比"让它输出 JSON 再自己 parse"可靠的原因;
- 返回 **`null`** 的两种情况:用户中途跳过该 agent;subagent 在 API 终态错误、重试后仍死亡。**所有消费处都要 `.filter(Boolean)`**。

**opts 逐项说明**:

| opt | 语义 | 使用建议 |
|---|---|---|
| `label` | 显示标签 | 建议带维度/文件名,如 `verify:alarms.rs`;要"随机感"时按 index 变化 label/prompt |
| `phase` | 进度分组 | 显式传比依赖全局 `phase()` 状态更稳:并行分支同时调 `phase()` 会互相覆盖(全局状态竞态)。规范原话:"opts.phase explicitly assigns this agent to a progress group (use this inside pipeline()/parallel() stages to avoid races on the global phase() state)" |
| `schema` | JSON Schema | 强烈建议用于一切要被程序消费的结果 |
| `model` | 模型覆盖 | **默认不传**(继承主循环模型,几乎总是对的);只有明确知道低档够用时才降档 |
| `effort` | 推理力度 | 机械 stage(纯扫描、格式转换)用 `low`;最难的对 refute/judge stage 用高档 |
| `isolation` | `'worktree'` | 见 §10。贵:每 agent 约 200-500ms 启动 + 磁盘占用;只在**并行写文件**时用 |
| `agentType` | 自定义类型 | 如 `'code-reviewer'`、`'Explore'`;与其系统提示拼接 StructuredOutput 指令,可与 schema 组合 |

**subagent 能做什么**:它是一个完整的 Claude Code agent,有自己的工具(Read/Grep/Bash/…),并且**能通过 ToolSearch 按需加载本会话连接的所有 MCP 工具**(schema 按 agent 懒加载)。注意:交互式登录的 MCP server(如 claude.ai)在 headless/cron 环境下可能缺席。

**subagent 上下文是全新的**:它**不继承**你的对话历史(区别于 `subagent_type: 'fork'` 的 Agent 工具,那个会继承全部上下文)。所以 prompt 必须**自包含**:任务背景、要读的路径、输出要求,一个都不能少。

### 6.2 `pipeline(items, stage1, stage2, …)` —— 无 barrier 流水线(默认首选)

```js
const results = await pipeline(
  FILES,                                        // 任意数组
  (file) => agent(`分析 ${file}…`, {schema: A}),  // stage 收到 (item)
  (analysis, file, i) => agent(                  // 后续 stage 收到 (上一stage结果, 原item, index)
    `校验 ${file} 的结论: ${JSON.stringify(analysis)}`, {schema: B}),
)
```

语义要点:
- **每个 item 独立穿过全部 stage**,item A 在 stage 3 时 item B 可能还在 stage 1;
- stage 之间**没有 barrier**——这是它与 `parallel` 的本质区别,也是它成为默认首选的原因;
- **墙钟时间 ≈ 最慢的单条 item 链**,而不是各 stage 耗时之和;
- stage 回调签名:`(prevResult, originalItem, index)`——后续 stage 靠 `originalItem` 把上下文带下去,不必让 stage1 把 item 塞进返回值;
- **某个 stage 抛异常**:该 item 立即变成 `null` 落地,并**跳过它的剩余 stage**;其他 item 不受影响,整条 pipeline 不 reject。

### 6.3 `parallel(thunks)` —— 有 barrier 的并发

```js
const all = await parallel(FINDERS.map(f => () =>
  agent(f.prompt, {phase: 'Find', schema: BUGS_SCHEMA})))
const found = all.filter(Boolean).flatMap(r => r.bugs)
```

语义要点:
- 参数是 **thunk 数组**(`() => Promise`),不是 Promise 数组——这保证进入时就排队,而不是立刻全部起飞;
- **barrier**:等所有 thunk 结束才返回;
- **永不 reject**:某个 thunk 抛异常/agent 出错,该项 resolve 为 `null`,其余照常。所以用法永远是先 `.filter(Boolean)`;
- 传 100 个 item 也没问题:超出的排队,同一时刻只有 `min(16, CPU-2)` 个在真跑,槽位释放自动补位。

**什么时候才配用 barrier**(规范给出的合法场景):
1. stage N 需要 stage N-1 的**全量结果做跨 item 计算**——典型是"全量去重后再进入昂贵的验证";
2. 典型 "find → 全量 dedup → verify" 模式(见 §15.3)。

判断口诀:**下游消费的是"每 item 一份结果"就用 pipeline;消费的是"整个集合的聚合视图"(去重、排序、全量对比)才用 parallel。** 反例记忆:一个中间 transform 只对每项独立处理时,把它塞进 pipeline stage,别为了"看起来整齐"加 barrier——"barrier latency is real",它会浪费先完成的那批 agent 的空闲时间。

### 6.4 `phase(title)` —— 进度分组

`phase('Review')` 之后派发的 agent 落进 "Review" 分组。规则:
- title 与 `meta.phases[].title` **逐字匹配**才算对上;匹配不上的照常运行,只是进度树里自成一个组;
- 在 `pipeline`/`parallel` 的 stage 回调里**不要依赖全局 `phase()`**(并行分支会互相踩),改用每个 `agent()` 的 `opts.phase`。

### 6.5 `log(message)` —— 叙述输出

在进度树上方打一行"旁白"。两个规定用途:
- 关键节点播报(如 `log(\`${bugs.length}/10 found\`)`);
- **禁止静默截断**:凡是用 top-N、采样、不重试等方式**限制了覆盖面**,必须 `log()` 出丢了什么——"悄悄截断读起来就像覆盖了全部"。

### 6.6 `workflow(nameOrRef, args?)` —— 内嵌子 workflow

```js
const sub = await workflow({scriptPath: p}, {files})   // 或 workflow('saved-name', {…})
```

- `args` 成为**子 workflow 的 `args` 全局**;
- 子 workflow **共享**父 run 的:并发槽、agent 计数器、abort 信号、token 预算;
- **嵌套深度只有一层**:在子 workflow 里再调 `workflow()` 会抛错;
- 名字不存在 / scriptPath 读不到 / 子脚本语法错 → 抛错,需要 `try/catch` 优雅处理。

### 6.7 `args` 全局

- 值为调用时传入的 `args`,**原样(verbatim)**;
- **头号陷阱**:必须传真 JSON 值——`args: ["a.ts","b.ts"]` ✅;`args: '["a.ts","b.ts"]'` ❌(整个数组变成一个字符串,后面 `args.filter`/`args.map` 直接抛 TypeError);
- 时间戳、配置、目标清单都从这里进,替代被禁的 `Date.now()` 等。

### 6.8 `budget` —— token 预算

```js
budget.total        // number | null:用户给了 "+500k" 类指令则为数值,否则 null
budget.spent()      // 本轮已产出的 output tokens(主循环 + 所有 workflow 共享一个池,不按 workflow 分)
budget.remaining()  // max(0, total - spent());无 target 时为 Infinity
```

- 预算是**硬上限**:一旦 `spent()` 达到 `total`,后续 `agent()` 直接抛错;
- 用它做循环护栏时,**必须防 `Infinity`**:

```js
while (budget.total && budget.remaining() > 50_000) { … }   // ✅
while (budget.remaining() > 50_000) { … }                   // ❌ 无 target 时死循环到 1000 agent 上限
```

---

## 7. 结构化输出(schema)最佳实践

```js
const FINDINGS_SCHEMA = {
  type: 'object',
  required: ['findings'],
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        required: ['title', 'file', 'severity', 'evidence'],
        properties: {
          title:    {type: 'string'},
          file:     {type: 'string'},
          severity: {enum: ['critical','major','minor']},
          evidence: {type: 'string', description: '具体代码位置与片段'},
        },
      },
    },
  },
}
```

要点:
- schema 在**工具调用层**强制校验,agent 不合规会被要求重试——比自己 parse 文本可靠得多;
- 字段设计直接服务于下游**纯代码**消费:去重 key 用什么字段、验证 prompt 要引用哪些字段、最终 return 什么,都应在 schema 里一次想清;
- 验证/judge 类 schema 常带"默认悲观"语义字段,如 `refuted: boolean` + prompt 里写 "不确定时默认 refuted=true";
- `evidence` 类字段务必要求(具体文件:行号、代码片段),这是后面对抗验证的输入,也是压模型幻觉的主要手段。

---

## 8. 并发、限额与规模(全部硬数字)

| 限制 | 值 | 说明 |
|---|---|---|
| 同时真跑的 agent | `min(16, CPU核数 - 2)` | 每个 workflow 独立计;超出的排队,槽位释放自动补 |
| workflow 生命周期 agent 总数 | **1000** | 防失控兜底;预算循环写坏才会摸到 |
| 单次 `parallel`/`pipeline` item 数 | **4096** | 超了是**显式报错**,不是静默截断 |
| 会话 guideline(medium) | **15 个 agent / workflow** | 用户 prompt 可放大;`/config` → "Dynamic workflow size" 可调 |
| 脚本长度 | 524288 字符 | `script` 参数上限 |
| workflow 嵌套深度 | 1 层 | 子 workflow 里再调 `workflow()` 抛错 |

推论:100 个文件的迁移可以直接 `pipeline(100 files)`,排队机制保证全部完成;真正要设计的是**每个 agent 的任务粒度**(太粗→单 agent 上下文爆,太细→调度与 token 开销大)。

---

## 9. 确定性约束与被禁项(及原因)

| 禁用 | 原因 | 替代 |
|---|---|---|
| `Date.now()` | 破坏 resume 缓存命中(同脚本应得同结果) | `args` 传时间戳 / 返回后打戳 |
| `Math.random()` | 同上 | 按 index 变化 prompt/label |
| 无参 `new Date()` | 同上 | 同上 |
| `fs`/Node API | 脚本沙箱没有 Node | 让 subagent 用自己的 Read/Write/Bash;文件级持久化交给 workflow 的自动持久化 |
| TS 类型标注 | 解析器只吃纯 JS | 删掉类型 |
| `require`/`import` 模块 | 无模块加载 | 工具函数直接写在脚本里 |

**为什么"可复现"值这个代价**:`resumeFromRunId` 的缓存按 `(prompt, opts)` 匹配——脚本里任何非确定性都可能让"本该命中的调用"错开,导致恢复时大面积重跑(贵)。确定性是 §12 整个机制的的地基。

---

## 10. 隔离:`isolation: 'worktree'`

- 给该 agent 一个**独立 git worktree**(仓库的干净副本 + 独立分支),它在里面改文件不影响别人、也不影响你的工作区;
- **未改动则自动清理**;
- 代价真实:每 agent 约 200-500ms 创建 + 磁盘占用;
- **判定标准一句话:多个 agent 会并行写同一仓库的文件 → 必须用;只读(找 bug、评审、研究)→ 不用。**
- 典型场景:迁移类 workflow——"发现所有改造点 → 每个点一个 agent 在 worktree 里转换 → 验证",没有隔离的话并行写文件会互相踩。

---

## 11. 错误模型(写脚本前必须内化)

| 情形 | 行为 | 你的对策 |
|---|---|---|
| `agent()` 出错(用户跳过 / API 终态失败) | 返回 `null` | 消费前 `.filter(Boolean)`;关键路径可显式判空补救 |
| pipeline 某 stage 抛异常 | 该 item → `null`,跳过其剩余 stage | 下游 stage 判空跳过;最后统计 null 率并 `log()` |
| `parallel` 某 thunk 抛异常 | 该项 resolve `null`,整体**不 reject** | `.filter(Boolean)` |
| `workflow()` 名字/路径/语法错 | 抛错 | `try/catch` 优雅降级 |
| 预算耗尽 | 后续 `agent()` 抛错 | 循环条件检查 `budget.total && budget.remaining() > N` |
| 超 4096 items / 嵌套超 1 层 / 脚本超长 | 显式报错 | 拆分或改结构 |
| `Date.now()` 等被禁调用 | 抛错 | §9 替代方案 |

通用纪律:**`null` 是常态输入而不是异常**。一个健壮 workflow 的返回里通常带统计——`{confirmed, dropped: nullCount, note}`——并 `log()` 掉队情况。

---

## 12. Resume:runId、缓存与 100% 命中

- 每次 Workflow 调用返回一个 **runId**(`wf_` 前缀);
- 中断(超时、手动停、出错)后:`Workflow({scriptPath, resumeFromRunId})` 恢复;
- **缓存判定:已完成过的 `agent()` 调用,若 `(prompt, opts)` 与上次完全一致 → 结果即时返回(不重跑)**;只有被你编辑过/新增的调用才真跑;
- **同脚本 + 同 args ⇒ 100% 缓存命中**(这就是 §9 禁随机/时间的回报);
- 恢复前必须先 `TaskStop` 停掉旧 run;
- **同会话限定**:缓存不跨 session。

实操循环:

```
第 1 版脚本 → 跑 → 发现 stage 2 的 prompt 不好
  → TaskStop(旧 run)
  → Edit 已持久化的 scriptPath 文件(只改 stage 2 prompt)
  → Workflow({scriptPath, resumeFromRunId})
  → stage 1 全部秒回(缓存),stage 2 重跑
```

这是一个非常省钱的调试手段:**改哪段,重跑哪段**。

---

## 13. 调试

**第一步永远是读 journal**:`<transcriptDir>/journal.jsonl` —— 逐条记录每个 agent 的**实际返回值**。规范特别提醒:恢复前别想当然认为缓存结果非空,以 journal 为准。

journal 不存在时的 fallback:读目录下的 `agent-<id>.jsonl`(每个 agent 的完整对话转录),手工拼一个续跑脚本。

常见症状 → 病因:

| 症状 | 病因 |
|---|---|
| 返回 `[]` / 空 | ① 所有 agent 都返回 null 没过滤统计;② prompt 写成了"给人看的汇报"而非数据;③ schema required 写太严,agent 交不出被反复拒绝。读 journal 定位 |
| 脚本解析失败 | meta 不是纯字面量;混入 TS 类型;用了 `import` |
| `args.filter is not a function` | args 传成了 JSON 字符串 |
| `Date.now is not allowed` 类抛错 | 用了被禁 API |
| 进度树分组乱了 | `phase()` title 与 `meta.phases` 不逐字一致;或并行分支共用全局 phase 状态(改用 opts.phase) |
| 缓存不命中 | prompt 里有时间戳/随机数;opts 对象每次构造不一致;改过脚本前段 |
| 恢复后大量重跑 | 同上;或没先 TaskStop |
| token 失控 | 预算循环没防 `Infinity`;或每个 agent 任务粒度太大 |

---

## 14. 成本与性能模型(心里要有这本账)

**Token 成本 ≈ Σ(每个 agent 的输入上下文 + 输出)**。每个 subagent 从零读文件,所以:

- 扇出 N 个 agent 读 N 个文件,N 倍上下文——但换来的是**主上下文保持干净**(只收回结论),长任务不爆窗口;
- `budget.spent()` 是**主循环 + 全部 workflow 共享池**——别只盯 workflow 自己的开销;
- **对抗验证的票数是最大杠杆**:单票 = 1×,3 票 = 3×。按请求口气选票数,而不是永远最高配;
- 让 finder 返回**带证据的结构化 finding**,verify 才不必重读全部材料——这是省 token 的结构性手段;
- wall-clock:用 `pipeline` 时 ≈ 最慢 item 链;用 `parallel` 时 = 最慢单项 + 已完成者的空闲浪费。这也是"pipeline 默认"的性能理由;
- worktree 隔离的 200-500ms/agent 是纯开销,只买"并行写安全"。

---

## 15. 模式库(Cookbook,含完整脚本)

以下模式是规范原文点名 + 展开的八种组合件,以及它们的完整可用写法。

### 15.1 评审 → 对抗验证(规范内嵌的 canonical 示例)

find 完一个维度立刻开始验证,不等其他维度——pipeline 让 Review/Verify 两阶段交叠:

```js
export const meta = {
  name: 'review-changes',
  description: 'Review changed code across dimensions, verify each finding',
  phases: [
    { title: 'Review', detail: 'per-dimension finders' },
    { title: 'Verify', detail: 'adversarial verification per finding' },
  ],
}

const DIMENSIONS = [
  {key: 'bugs', prompt: '在本次 diff 中找正确性 bug…'},
  {key: 'perf', prompt: '在本次 diff 中找性能问题…'},
]

const results = await pipeline(
  DIMENSIONS,
  d => agent(d.prompt, {label: `review:${d.key}`, phase: 'Review', schema: FINDINGS_SCHEMA}),
  review => parallel(review.findings.map(f => () =>
    agent(`对抗性验证:${f.title}`, {label: `verify:${f.file}`, phase: 'Verify', schema: VERDICT_SCHEMA})
      .then(v => ({...f, verdict: v}))
  )),
)

const confirmed = results.flat().filter(Boolean).filter(f => f.verdict?.isReal)
return {confirmed}
```

### 15.2 对抗验证(Adversarial verify)+ 多视角验证

**对抗**:每个 finding 派 N 个独立"怀疑者",prompt 明确要求**反驳**,拿不到多数反驳就杀掉——防止"听起来合理但错误"的 finding 混进结论:

```js
const votes = await parallel(Array.from({length: 3}, (_, i) => () =>
  agent(`尝试反驳这个 finding(视角 #${i}):${claim}。不确定时默认 refuted=true。`,
        {schema: VERDICT_SCHEMA})))
const survives = votes.filter(Boolean).filter(v => !v.refuted).length >= 2   // ≥多数才存活
```

**多视角**:当 finding 可能以**不止一种方式**出错时,给每个验证者**不同的镜头**(correctness / security / perf / does-it-reproduce),而不是发 N 个一样的怀疑者——"diversity catches failure modes redundancy can't"。

### 15.3 Find → 全量 dedup → verify(barrier 正当性的标准示例)

```js
const found = (await parallel(FINDERS.map(f => () =>
  agent(f.prompt, {phase: 'Find', schema: BUGS_SCHEMA})
))).filter(Boolean).flatMap(r => r.bugs)

const seen = new Set(), fresh = []
for (const b of found) {
  const k = `${b.file}:${b.line}:${b.title}`   // 纯代码去重,不派 agent
  if (!seen.has(k)) { seen.add(k); fresh.push(b) }
}
log(`raw=${found.length} deduped=${fresh.length}`)

const judged = await parallel(fresh.map(b => () =>
  parallel(['correctness','security','repro'].map(lens => () =>
    agent(`用 ${lens} 视角判断 "${b.desc}" 是否真问题`, {phase: 'Verify', schema: VERDICT})))
    .then(vs => ({b, real: vs.filter(Boolean).filter(v => v.real).length >= 2}))
))
return {confirmed: judged.filter(Boolean).filter(j => j.real).map(j => j.b)}
```

### 15.4 Loop-until-dry(找到"连续 K 轮无新发现"为止)

简单计数器会漏掉长尾,所以以"干燥轮数"为停机条件:

```js
const bugs = [], seen = new Set()
let dry = 0
while (dry < 3) {
  const r = await agent('在这个 codebase 里找 bug,已知发现:' + JSON.stringify([...seen]),
                        {schema: BUGS_SCHEMA})
  const fresh = (r?.bugs ?? []).filter(b => !seen.has(key(b)))
  if (!fresh.length) { dry++; continue }
  dry = 0
  fresh.forEach(b => { seen.add(key(b)); bugs.push(b) })
  log(`${bugs.length} found so far`)
}
```

### 15.5 预算护栏版循环

```js
while (budget.total && budget.remaining() > 50_000) {
  const r = await agent('…', {schema: BUGS_SCHEMA})
  if (!r || !r.bugs.length) break
  bugs.push(...r.bugs)
}
```

### 15.6 Judge panel(方案空间大时的设计法)

N 个 agent 从**不同视角**独立出方案(如 MVP-first / risk-first / user-first)→ parallel judges 打分 → 从赢家综合,**并嫁接次优方案的亮点**。比"一个方案反复迭代"强在方案空间真的宽时。

```js
const angles = ['MVP-first', 'risk-first', 'user-first']
const proposals = (await parallel(angles.map(a => () =>
  agent(`从 "${a}" 视角设计 X,给出方案与理由`, {phase: 'Propose', schema: PROPOSAL_SCHEMA})
))).filter(Boolean)

const scored = (await parallel(proposals.map(p => () =>
  agent(`按正确性/复杂度/可维护性给这个方案打分:…`, {phase: 'Judge', schema: SCORE_SCHEMA})
    .then(s => ({p, s}))
))).filter(Boolean).sort((a,b) => b.s.total - a.s.total)

const merged = await agent(
  `以方案 A 为主,吸收以下亮点并说明取舍:${JSON.stringify(scored.slice(1))}`,
  {phase: 'Synthesize', schema: FINAL_SCHEMA})
return merged
```

### 15.7 多模态扫描(Multi-modal sweep)

多个 agent 各按一种**正交的检索轴**搜(按容器 / 按内容 / 按实体 / 按时间),彼此看不见对方的结果——单轴搜索有盲区,合起来才覆盖全。适用于"盘点存量""全面调研"。

### 15.8 完整性批评者(Completeness critic)

收尾前派一个 agent 只回答一个问题:"**还缺什么?**——没跑的模态、没验证的声明、没读的源?"它找出的缺口就作为下一轮 workflow 的输入。这是把"自我感觉覆盖全了"变成"被独立检查过覆盖全了"。

### 15.9 组合拳(规范点名的完整流水)

> find → dedup vs seen → 多视角 panel → loop-until-dry

以及反向链:completeness critic → 缺口进下一轮。

---

## 16. Barrier vs Pipeline:决策框架

```
下游 stage 消费什么?
├─ 每 item 一份独立结果(转换/ enrich / 逐项验证)
│    → pipeline(item, stageA, stageB)     // 无 barrier,墙钟≈最慢链
└─ 整个集合的聚合视图
     ├─ 全量去重 / 排序 / top-N / 跨 item 对比
     │    → parallel(收集) → 纯代码聚合 → 再 fan out
     └─ 只是"全部完成后打印个汇总" → 也用 parallel(汇总在脚本里做)
```

反例(规范点名批评的形态):中间 transform 对每项独立处理却用 `parallel(a) → transform → parallel(b)`——白白引入 barrier,先完成的那批 agent 的等待时间是纯浪费("barrier latency is real")。

---

## 17. 与 Claude Code 其他编排机制的对比与选型

| 需求 | 用什么 | 为什么不用 Workflow |
|---|---|---|
| 一个可独立完成的子任务(如"查一下 X 的实现") | Agent 工具(单个 subagent) | 一个 agent 就够,编排无意义 |
| subagent 需要你的全部对话上下文 | Agent + `subagent_type: 'fork'` | fork 继承上下文;Workflow 的 agent 全是新上下文 |
| 多个**长生命周期**角色持续协作、互发消息 | Agent Teams + SendMessage | Teams 是"对话式协作";Workflow 是"一次性确定性扇出",无消息通道 |
| 强制性规则(提交前跑 fmt、编辑后检查) | Hooks | Hooks 不经模型、必然执行;Workflow 的 agent 是概率性的 |
| 固化的一套流程指令(/deploy) | Skills | Skill 是"给模型的指令包";若 skill 指令要求,它**可以**再调 Workflow(合法 opt-in 之一) |
| 周期性触发(每 10 分钟查 CI) | CronCreate / `/loop` / ScheduleWakeup | 这些管"什么时候跑",Workflow 管"跑的时候怎么编排" |
| 需要用户先批准实现方案 | EnterPlanMode | 计划获批后的执行阶段才可能上 Workflow |
| 2-3 个文件的小改动 | 直接干 | 编排开销 > 收益 |
| 几十个文件审计/迁移/全面评审/深度研究 | **Workflow** | 唯一满足"多 agent + 确定结构 + 可恢复"的组合 |

注意混杂场景:**单一 subagent 有独立工作可做时,并行发多个 Agent 工具调用即可**,不需要 Workflow——Workflow 的价值在于**结构**(分阶段、验证、预算、恢复),不是"能并行"本身。

---

## 18. 与业界编排概念的坐标系

### 18.1 两大范式

- **Flow-driven(流程驱动)**:拓扑先写死,执行单元填进去。GitHub Actions、Airflow、Temporal、LangGraph、**Workflow** 都属此类。优点:可审计、可恢复、可缓存;缺点:表达不了"边跑边改计划"。
- **Goal-driven(目标驱动)**:给一个目标 + 工具 + 反馈回路,系统自己规划路径。AutoGPT 类、单 agent 长循环、Agent Teams 属此类。优点:灵活;缺点:不可预测、难恢复、难审计。
- Workflow 的立场很清晰:**外层 flow-driven,内层 goal-driven**——每个 agent 内部仍是自主 agent,但"谁在什么时候跑"是写死的。这也是它区别于"纯 swarm"的关键。

### 18.2 概念对照表

| 通用编排概念 | Workflow 里的对应物 |
|---|---|
| DAG(Direct Acyclic Graph) | 脚本的 await 依赖链;`parallel` = 同层节点,`await` 顺序 = 边 |
| Fork-Join / barrier 同步 | `parallel()` |
| Pipeline parallelism | `pipeline()`(每 item 流水,非 stage 串行) |
| Scatter-Gather | `parallel` 收集 + 脚本聚合 |
| Map-Reduce | `pipeline(items, mapStage)` + 脚本内 reduce(去重/合并);需要模型参与 reduce 时再派 agent |
| Supervisor / Worker | 主循环(你)= supervisor,`agent()` = worker;Workflow 把 supervisor 的调度逻辑代码化 |
| Blackboard(黑板架构) | 脚本里的共享变量(`seen`/`bugs`),各 agent 读写后再喂给后续 agent |
| Quorum / 多数表决 | 对抗验证 ≥2/3 票存活 |
| Ensemble / Judge panel | §15.6 |
| Generator-Critic(生成-批判) | Review → Verify;Completeness critic |
| Saga / 补偿事务 | 脚本 try/catch + 显式补救;workflow 不提供自动回滚(worktree 未改动自动清理算最接近的) |
| Idempotency & resume(幂等/断点续跑) | runId 缓存,按 `(prompt, opts)` 命中 |
| Rate limiting / concurrency cap | `min(16, CPU-2)` 槽位排队 |
| Work stealing / queue | parallel/pipeline 内建排队 + 槽位补位 |
| determinism(确定性重放) | §9 禁随机/时间换 100% 缓存命中 |

### 18.3 与具体框架的粗粒度对比(概念级)

| 框架 | 核心抽象 | 与 Workflow 的异同 |
|---|---|---|
| GitHub Actions | YAML job 矩阵(`matrix:`) | 最像的部分是"矩阵扇出";但 actions 跑的是 shell 步骤,Workflow 的单元是**智能 agent**,且支持阶段间数据流与验证投票 |
| Temporal | 持久化 workflow code + activity,语言级 SDK | 同样"代码即编排 + 可恢复";Temporal 的持久化是事件溯源级(跨天/跨机),Workflow 的 resume 是会话内、按调用粒度 |
| Airflow / Dagster | 数据管道 DAG,调度为中心 | DAG 相似;Airflow 不含智能执行单元,重试是整个 task 级,Workflow 可在脚本级做更细的票选/去重 |
| LangGraph | 图 + 共享 State + 条件边 | 同为流程驱动;LangGraph 的 state 是图间共享对象,Workflow 的状态就是脚本变量(更朴素);LangGraph 常驻服务,Workflow 一次性 |
| OpenAI Agents SDK / Swarm | Agent + handoff(把对话转交给另一个 agent) | handoff 是"控制权转移",串行的;Workflow 是"并行分工 + 聚合",没有对话转交语义 |
| CrewAI | 角色(Crew/Task, role/goal/backstory) | 角色剧本式协作,顺序感强;Workflow 无角色持久性,每个 agent 用完即弃,分工靠 prompt |
| AutoGen | 多 agent 会话/群聊 | 会话驱动,流程由对话涌现;Workflow 反其道,流程写死 |
| Ray / Dask / Celery | 通用并行任务执行 | 只有执行层;没有"结果即数据、带 schema 校验的智能单元"和预算/验证模式 |

一句话定位:**Workflow ≈ "GitHub Actions 的矩阵扇出" + "Temporal 的可恢复代码编排" + "AI 原生的执行单元与验证模式",但作用域是一次会话内的一个 fan-out,而不是常驻平台。**

---

## 19. 反模式清单(每条都见过真实踩法)

1. ❌ 未获 opt-in 就跑(用 Agent 工具或先问用户);
2. ❌ 该用纯代码的去重/过滤/排序,派 agent 做——又贵又不确定;
3. ❌ 消费 agent 结果不 `.filter(Boolean)`——一个 null 毒死全链;
4. ❌ `args` 传 JSON 字符串;
5. ❌ 循环不检查 `budget.total`,无预算时烧到 1000 agent 上限;
6. ❌ 中间 stage 用 parallel 造无谓 barrier;
7. ❌ 并行写文件不加 worktree 隔离(互相覆盖)/ 只读任务加 worktree(纯浪费);
8. ❌ 用 top-N/采样限制覆盖面却不 `log()`;
9. ❌ 把 pipeline stage 回调写成只收一个参数——后续 stage 拿不到 `originalItem/index` 就把 item 塞进返回值,越传越臃肿;
10. ❌ 并行分支里依赖全局 `phase()`(竞态),应传 `opts.phase`;
11. ❌ agent prompt 不自包含(subagent 看不见你的对话);
12. ❌ 让 agent 输出自然语言再自己 parse,而不给 schema;
13. ❌ 脚本里藏时间戳/随机数导致 resume 缓存大面积失效;
14. ❌ 忘了"先 TaskStop 再 resume";
15. ❌ 返回空结果就下结论"没有问题"——先查 journal,区分"确实没有"和"全都 null 了"。

---

## 20. 从零写一个 workflow:操作手册

1. **侦察(inline,不派 agent)**:搞清工作清单是什么——哪些文件、哪些维度、多少个 finding。规范原话:"你不需要在任务开始前知道形状,只需要在编排这一步之前知道。"
2. **问自己三个问题**:
   - 每个执行单元的任务是什么?粒度多大?(一个文件?一个维度?一个改造点?)
   - 结果怎么被程序消费?→ 设计 schema;
   - 需要哪几个阶段?每阶段消费上一阶段的**单项结果**(→pipeline)还是**全量结果**(→parallel)?
3. **写 meta**:name、description、phases(title 之后要与 `phase()` 逐字对应);
4. **写 body**:constants → phase → 扇出 → 聚合(纯代码)→ (必要时)验证 fan-out → return;
5. **加保险**:所有消费点 `.filter(Boolean)`;预算循环防 `Infinity`;覆盖面限制处 `log()`;
6. **内联提交**(不要先写文件);记下返回的 scriptPath 与 runId;
7. **看结果**:不理想 → TaskStop → Edit scriptPath → `resumeFromRunId` 续跑(改哪段重跑哪段);
8. **收尾**:确认结论基于 confirmed(验证存活)而非 raw(原始 finding);把 null/丢弃统计如实带进汇报。

---

## 21. 针对 iot-rust 的实战示例(可直接跑的骨架)

背景:仓库当前有大量未提交修改(config/domain/runtime/server 多个 crate)。下面是一个"对未提交改动做三维评审 + 对抗验证 + 汇总"的 workflow。

```js
export const meta = {
  name: 'iot-rust-diff-review',
  description: '对未提交改动做 bugs/perf/rust-idiom 三维评审并对抗验证',
  phases: [
    { title: 'Scout',   detail: '确定改动文件与 diff 范围' },
    { title: 'Review',  detail: '每维度一个 finder' },
    { title: 'Verify',  detail: '每 finding 三视角表决' },
  ],
}

const FINDINGS = {
  type: 'object', required: ['findings'],
  properties: { findings: { type: 'array', items: {
    type: 'object',
    required: ['title', 'file', 'line', 'severity', 'evidence', 'failure_scenario'],
    properties: {
      title: {type: 'string'}, file: {type: 'string'}, line: {type: 'integer'},
      severity: {enum: ['critical','major','minor']},
      evidence: {type: 'string'}, failure_scenario: {type: 'string'},
    } } } },
}
const VERDICT = {
  type: 'object', required: ['refuted', 'reason'],
  properties: { refuted: {type: 'boolean'}, reason: {type: 'string'} },
}

phase('Scout')
const scout = await agent(
  '在仓库根目录跑 `git diff --name-only` 与 `git diff --stat`,返回修改过的 .rs 文件清单(数组,仓库相对路径)。只返回数据。',
  {label: 'scout:diff', phase: 'Scout',
   schema: {type: 'object', required: ['files'], properties: {files: {type: 'array', items: {type: 'string'}}}}})
if (!scout || !scout.files.length) return {confirmed: [], note: '无改动或侦察失败'}
log(`改动文件 ${scout.files.length} 个`)

const DIMS = [
  {key: 'bugs',  prompt: '只看这些文件的未提交改动(git diff),找正确性 bug:错误处理遗漏、锁/生命周期问题、协议解析边界。文件:' + scout.files.join(',')},
  {key: 'perf',  prompt: '只看未提交改动,找性能问题:热路径分配、锁竞争、不必要的 clone。文件:' + scout.files.join(',')},
  {key: 'idiom', prompt: '只看未提交改动,找 Rust 惯用法问题:应使用标准 API、多余 unsafe、错误类型不一致。文件:' + scout.files.join(',')},
]

phase('Review')
const perDim = await pipeline(
  DIMS,
  d => agent(d.prompt + '。每条 finding 给出 file/line/evidence/failure_scenario。',
             {label: `review:${d.key}`, phase: 'Review', schema: FINDINGS, effort: 'high'}),
  (review, d) => {
    if (!review || !review.findings.length) return []
    return parallel(review.findings.map(f => () =>
      parallel(['correctness', 'does-it-reproduce', 'rust-semantics'].map(lens => () =>
        agent(
          `视角:${lens}。请尝试【反驳】以下 finding(仓库根目录可自行读代码核实)。` +
          `不确定时 refuted=true。\n${JSON.stringify(f)}`,
          {label: `verify:${lens}:${f.file}`, phase: 'Verify', schema: VERDICT})))
        .then(votes => {
          const v = votes.filter(Boolean)
          const refuted = v.filter(x => x.refuted).length
          return {...f, dimension: d.key, refuted, of: v.length,
                  confirmed: v.length > 0 && refuted < Math.ceil(v.length / 2)}
        }))
  },
)

const all = perDim.filter(Boolean).flat().filter(Boolean)
const confirmed = all.filter(f => f.confirmed)
log(`raw=${all.length} confirmed=${confirmed.length} refuted=${all.length - confirmed.length}`)
return {confirmed, rejected: all.filter(f => !f.confirmed), filesScanned: scout.files.length}
```

按本会话 guideline(≤15 agents),这个骨架在 finding 不多时合规;若 finding 很大,先在脚本里按 severity 截断并 `log()` 丢弃数,或分多个 workflow 跨轮次跑。

---

## 22. FAQ

**Q:Workflow 和同时发多个 Agent 工具调用有什么区别?**
A:后者只有"并行",没有结构——没有阶段、没有共享的去重状态、没有预算控制、没有 runId 恢复。任务能在一个 agent 内独立完成时用后者;需要"找→去重→验证→汇总"这类结构时用 Workflow。

**Q:subagent 能看到我们的对话吗?**
A:不能。Workflow 的 agent 全新上下文, prompt 必须自包含。(需要继承上下文的是 Agent 工具的 `fork` 类型,另一回事。)

**Q:能自己写文件吗?**
A:能(agent 有全套工具),并行写同一仓库就要 `isolation: 'worktree'`;只读任务不用。

**Q:结果为空怎么办?**
A:读 journal 区分"真没有"和"全 null"。这也是为什么返回值里建议带 raw/confirmed/丢弃统计。

**Q:一次 workflow 最多多少 agent?**
A:生命周期 1000 硬上限、单次调用 4096 items、会话 guideline 15 个(medium)。工程上受 token 预算约束远早于碰这些数。

**Q:skill 里能用吗?**
A:能——"用户调用了指示调用 Workflow 的 skill"本身就是合法 opt-in 之一。

**Q:iterating 时每次都要重发脚本吗?**
A:不要。Edit 持久化的 scriptPath 文件,`{scriptPath, resumeFromRunId}` 续跑,未改动的调用全部命中缓存。

---

## 23. 速查卡片

```
何时用      用户 opt-in(ultracode / 原话要求 / skill 指示 / 点名 workflow)+ 任务是"多 agent 扇出"
调用        Workflow({script}) → 拿 scriptPath+runId → 迭代用 {scriptPath, resumeFromRunId}
脚本头      export const meta = {name, description, phases:[{title,detail}]}  // 纯字面量
默认结构    pipeline(items, find, verify)   // stage 间无 barrier,墙钟≈最慢链
barrier 时机 下游需要全量聚合(去重/排序/对比)才 parallel
agent 返回  无 schema→文本;有 schema→对象;失败/跳过→null(记得 filter(Boolean))
必须过滤    null;必须 log() 截断;必须防 budget.total 为 null(Infinity)
禁用        Date.now / Math.random / new Date() / fs / Node API / TS 类型 / 嵌套>1层
限额        并发 min(16,CPU-2);总 1000;单次 4096;本会话 guideline 15
恢复        同 (prompt,opts) 秒回缓存;同脚本+同参数=100%命中;先 TaskStop
调试        journal.jsonl(每个 agent 的实际返回)→ agent-<id>.jsonl 兜底
状态确认    agent 结果没到之前绝不臆测;/workflows 看进度
```

---

## 24. 一句话总结

> Workflow = 把"多 agent 协作"写成一段**确定性 JS**:你自己侦察出工作清单 → `pipeline` 逐 item 流水(默认)→ 需要跨 item 聚合时才 `parallel` barrier → 结果一律带 schema 结构化 → 结论靠"多视角投票反驳"对抗验证 → 不足就 loop-until-dry,超支靠 `budget`,断了靠 runId 续跑;外层是流程驱动(可缓存、可恢复、可审计),内层每个 agent 仍是目标驱动的自主 agent。
