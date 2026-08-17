# Multi-Agent 统一配置与 Root-Only 协作设计

> 状态：提案（未实施）  
> 范围：`ash-agent`、`ash-collab`、CLI 装配层  
> 目标：统一 root agent 与 child agent 的配置派生，并规定只有 root agent 能编排子 agent。

## 1. 背景与问题

Ash 已经把绝大多数执行配置放在两个稳定抽象中：

- `ash_agent::Agent`：模型、系统提示词、工具、最大轮次、上下文策略等跨 session 的不可变行为；
- `ash_agent::SessionOptions`：工作目录、工具超时等会话范围；
- `Runtime`：模型客户端与 session store；
- `SessionIdentity`：root ID、parent ID、路径组成的持久化层级身份。

root 与 child 最终都通过 `RunConfig::new(&Agent, &SessionOptions)` 执行，因此模型调用、上下文管理、超时、工具执行和持久化并不存在两套运行时。

当前协作层正在收敛为 `ProfileName` / `AgentProfile` / `ToolPolicy`（见 `crates/ash-collab/src/control.rs`）。该方向将角色的描述、提示词覆盖、工具投影和可选模型/轮次覆盖集中起来，是正确的基础。

但仍有一个要明确的产品边界：

> `default`、`worker`、`explorer` 是**工作 profile**，不是调度权限。

如果 child 继承 `spawn_agent`、`message_agent`、`interrupt_agent`、`wait_agent`，便可以继续创建孙 agent。这样会带来：

1. 嵌套树和并发配额更难预测；
2. child 的任务边界被自行扩张；
3. root 的任务拆分与状态管理不再是唯一协调点；
4. child prompt 可能同时要求“专注完成任务”和“继续委派”，产生歧义；
5. 角色和协作权限耦合，后续增加 profile 时容易遗漏工具或 prompt 分支。

本提案采用**root coordinator + leaf workers**模型：root 负责编排，所有 child 都是叶子执行者。

---

## 2. 设计目标

### 2.1 必须达成

1. root 和 child 使用同一条 `Agent + SessionOptions -> RunConfig` 执行路径。
2. `default` 是 root 的默认 profile，也是 child 可选的通用工作 profile。
3. profile 可以统一定义：
   - 名称与供 `spawn_agent` 使用的说明；
   - prompt overlay；
   - 普通业务工具的投影规则；
   - 可选模型、max turns 等执行覆盖。
4. root 是唯一可见且可调用协作工具的 session。
5. 所有 child（包括 `default`、`worker`、`explorer`）都不能 spawn、message、wait 或 interrupt agent。
6. 权限不只依赖 prompt：工具装配阶段隐藏，工具执行阶段再次拒绝。
7. child 仍然继承根的基础模型、环境、AGENTS.md、技能信息、工作目录、超时、上下文策略和业务工具基底；profile 再作有限覆盖。

### 2.2 非目标

本提案不做以下事情：

- 不新增独立的 `MainAgentConfig` / `SubAgentConfig` 大型镜像结构；
- 不改变 `Runtime`、`Session` actor 或 `RunConfig` 的共用执行模型；
- 不在这一步设计 TOML/JSON 自定义 profile 配置格式；
- 不支持 child-to-child 的直接通信；如有需要，由 root 读取结果后再分发；
- 不支持多层嵌套 child。将来如果重新开放，应作为明确产品策略，而非 profile 的副作用。

---

## 3. 概念模型

配置应分成三个相互独立的维度：**执行基底**、**工作 profile**、**协作权限/会话层级**。

```text
                ┌────────────────────────────────┐
                │ Base Agent                       │
                │ model / base prompt / base tools │
                │ limits / context policy          │
                └───────────────┬────────────────┘
                                │ clone
          ┌─────────────────────┴─────────────────────┐
          │                                           │
┌─────────▼──────────┐                     ┌──────────▼─────────┐
│ Root materialize    │                     │ Child materialize  │
│ profile: default    │                     │ profile: selected  │
│ scope: Root         │                     │ scope: Child       │
│ + collaboration     │                     │ - collaboration    │
│ + multi-agent prompt│                     │ + leaf prompt      │
└─────────────────────┘                     └────────────────────┘
```

### 3.1 执行基底（base definition）

基底就是现有 `Agent`：

```rust
Agent {
    system_prompt,
    tools,
    model,
    max_turns,
    max_context_tokens,
    context_policy,
}
```

CLI 继续负责构造它：发现 skills、构建基础系统提示词、创建内置工具和 skill 工具、应用 active skill 覆盖。

**关键约束：基底不能含协作工具，也不能含 `<multi_agent_mode>`。**

否则 child 从基底克隆时会带上 root-only 的工具或调度提示。当前 `install_subagent_tools` 在 root `Agent` 上原地追加协作工具和提示词，然后又将该 Agent 克隆给 child；迁移时应消除这个泄漏路径。

### 3.2 工作 profile

profile 只描述“此 agent 怎样完成自己的工作”，不描述是否能调度其他 agent。可沿用正在形成的 `AgentProfile`：

```rust
struct AgentProfile {
    name: &'static str,
    description: &'static str,
    prompt_overlay: &'static str,
    tool_policy: ToolPolicy,
    model: Option<ModelId>,
    max_turns: Option<u32>,
}

enum ToolPolicy {
    // 从不含 collaboration tools 的基础业务工具集合继承。
    Inherit,
    // 只保留显式许可的基础业务工具。
    Allow(&'static [&'static str]),
}
```

内置 profile 的建议语义：

| Profile | 普通业务工具 | Prompt 职责 | 推荐用途 |
| --- | --- | --- | --- |
| `default` | 继承基础业务工具 | 直接完成一个自包含任务 | 通用调查、分析、实现 |
| `worker` | 继承基础业务工具 | 在已分配文件/模块范围内实现和验证 | 代码或生产任务 |
| `explorer` | 只读 allowlist：`read`、`glob`、`grep`、`webfetch`、`skill` | 只读检查并返回证据 | 独立代码库问题 |

`worker` 可以写文件，`explorer` 不可以；两者都不能调度子 agent。

### 3.3 Session scope / capability

协作权限来自 session 的位置，而不是 profile。最小实现无需持久化额外字段，因为已有：

```rust
SessionIdentity::is_root() -> bool
```

概念上可表达为：

```rust
enum AgentScope {
    Root,
    Child,
}

impl AgentScope {
    fn from(identity: &SessionIdentity) -> Self {
        if identity.is_root() { Self::Root } else { Self::Child }
    }

    const fn can_manage_agents(self) -> bool {
        matches!(self, Self::Root)
    }
}
```

是否真的引入 `AgentScope` 类型取决于代码量：

- **最小改动**：在协作工具入口直接使用 `context.session.identity.is_root()`；
- **更强类型的未来扩展**：引入私有 `AgentScope`，让工具装配与 prompt 组装共用该策略。

因为产品规则明确是“仅 root”，推荐先使用已有 `is_root()`，不要为了一个布尔含义再复制身份状态。

---

## 4. 工具装配规则

协作工具的名字必须被视为保留名字：

```rust
const COLLABORATION_TOOL_NAMES: [&str; 4] = [
    "spawn_agent",
    "message_agent",
    "interrupt_agent",
    "wait_agent",
];
```

### 4.1 两阶段 materialization

应将 Agent 组装收敛为一个概念操作：

```rust
fn materialize_agent(
    base: Agent,
    profile: AgentProfile,
    is_root: bool,
    collaboration_tools: &[Arc<dyn Tool>],
) -> Agent
```

推荐顺序：

1. 从 `base` 删除所有保留协作工具；
2. 将 profile 的普通工具规则应用到剩余基础工具；
3. 应用 model、max turns 等 profile 覆盖；
4. 只有 `is_root == true` 时追加协作工具。

伪代码：

```rust
fn materialize_agent(
    base: Agent,
    profile: AgentProfile,
    is_root: bool,
    collaboration_tools: &[Arc<dyn Tool>],
) -> Agent {
    let agent = base.without_tools(&COLLABORATION_TOOL_NAMES);
    let agent = profile.apply(agent); // 只处理基础工具和执行覆盖

    if is_root {
        agent.pushing_tools(collaboration_tools.iter().cloned())
    } else {
        agent
    }
}
```

重点是 `AgentProfile::apply` / `ToolPolicy::apply` **不再接收 collaboration tools**。这样类型签名就避免了“不小心为 child 加回协作工具”。

### 4.2 Root 工具

root 的最终工具集合：

```text
base business tools
+ skill tool
+ collaboration tools
```

root 可以 spawn child、向 child 发送消息、等待 child 结果、中断 child。

### 4.3 Child 工具

child 的最终工具集合：

```text
tool_policy(profile, base business tools)
```

不含任何 collaboration tool。

因此：

- child `default`：继承 read/write/edit/bash/web 等业务工具；
- child `worker`：同上；
- child `explorer`：只读 allowlist；
- 所有 child：没有 `spawn_agent` / `message_agent` / `interrupt_agent` / `wait_agent`。

---

## 5. Prompt 规则

工具权限与 prompt 不能冲突。

### 5.1 Root prompt

仅 root 添加：

```xml
<multi_agent_mode>
...
</multi_agent_mode>
```

该区块只讲 root 的编排职责：何时拆分、profile 如何选择、并行写入范围、何时 wait、如何审查 child 结果。

它不应该被复制到 child。

### 5.2 Child prompt

child 应从不带 multi-agent 内容的基础 prompt 派生，并追加 profile overlay 和叶节点边界：

```xml
<subagent_context>
You are `/root/task_name`, a `worker` sub-agent spawned by `/root`.
You share the workspace with the parent agent and other child agents.

You are a leaf sub-agent. Complete the assigned task directly.
You cannot create, message, wait for, or interrupt other agents.
Return your findings, changed files, and validation results to your parent.

[profile-specific instructions]
</subagent_context>
```

需要同步修改现有 profile 文案：

- `default` 中删除“delegate independent subparts”；
- `worker` 中删除“coordinate independent side questions through sub-agents”；
- `explorer` 保持只读和不修改文件的要求。

模型看不到工具，同时 prompt 也明确说明不能使用，是可解释且一致的行为。

---

## 6. 运行时防线

只从 child 的工具列表中移除协作工具还不够。协作工具闭包会持有 `AgentControl`，未来的装配错误、恢复/重放兼容问题或自定义工具注册都可能再次暴露它们。

所有协作入口均需统一调用：

```rust
fn require_root(context: &ToolContext) -> Result<(), ToolError> {
    if context.session.identity.is_root() {
        Ok(())
    } else {
        Err(ToolError::Execution(
            "collaboration tools are available only to the root agent".to_string(),
        ))
    }
}
```

调用位置：

- `AgentControl::spawn`；
- `AgentControl::message_agent`；
- `AgentControl::interrupt`；
- `AgentControl::wait`。

这是 defense in depth：

| 防线 | 效果 |
| --- | --- |
| 工具装配 | 模型不会看到 child 不允许使用的工具 |
| Child prompt | 模型理解其叶节点职责 |
| 执行时 `require_root` | 即使工具意外暴露也不能越权 |

`SessionIdentity` 已持久化 `root_id`、`parent_id` 和路径，`ToolContext` 已携带 `SessionToolContext.identity`，所以不需要向 tool API 新增参数。

---

## 7. 推荐代码边界

### `ash-cli`

职责不变：创建基础 `Agent`、`SessionOptions` 和 `Runtime`，然后把基础定义交给协作层。

建议入口语义改为更精确的名字，例如：

```rust
install_root_collaboration(base_agent, options, runtime, max_concurrent_children)
```

现有 `install_subagent_tools` 容易被理解为“给所有 agent 装子 agent 工具”，而目标实际是“为 root 安装协调能力，并注册 child factory”。这只是可读性改进，不是必要 API 破坏。

### `ash-collab`

职责：

- 保存不含协作工具/提示词的基础 `Agent`；
- 构造 root 的 materialized agent；
- 在 spawn 时构造 child 的 materialized agent；
- 管理 child 生命周期、并发限制、消息和 UI snapshot；
- 提供 root-only 协作工具与运行时权限检查。

建议的内部拆分：

```text
profiles.rs      ProfileName / AgentProfile / ToolPolicy
materialize.rs   base + profile + root/child -> Agent
control.rs       spawn/message/wait/interrupt 与生命周期
prompt.rs        root multi-agent prompt 与 child context prompt
```

当前模块规模仍可先保留一个 `control.rs`；只有 profile 配置继续增长时再拆文件。不要为了本提案创建空模块层级。

### `ash-agent`

无需增加 root/child 专用执行引擎。继续使用：

```text
Runtime::start       -> root session
Runtime::start_child -> child session
RunConfig::new       -> shared engine configuration
```

### `ash-core`

无需保存单独的“协作权限”字段。`SessionIdentity::is_root()` 已能实现 root-only 策略。

---

## 8. 迁移步骤

建议拆成可审查的小提交。

### 阶段 1：完成 profile 收敛

- 保留/完成 `ProfileName`、`AgentProfile`、`ToolPolicy`；
- 确保 model、max turns 和基础工具投影均通过一个 `apply_profile` 实现；
- 明确 `default` 是默认 profile；
- 保留 explorer 的只读工具 allowlist。

验证：现有 profile/工具投影单元测试。

### 阶段 2：协作工具从 profile 中剥离

- `ToolPolicy::apply` 删除 `collaboration_tools` 参数；
- `apply_profile` 不再可能添加协作工具；
- 在 materialization 的 root 分支最后追加协作工具；
- child 一律以“已移除保留协作工具”的基础定义开始。

验证：default、worker child 的工具列表不含四个协作工具。

### 阶段 3：分离 root 与 child prompt

- child factory 保存干净的 base prompt / base Agent；
- root materialization 后添加 `<multi_agent_mode>`；
- child prompt 只添加 `<subagent_context>` 和 leaf 约束；
- 修正 default/worker 的旧文案，删除继续委派的描述。

验证：child system prompt 不含 `<multi_agent_mode>`，且含叶节点约束。

### 阶段 4：添加运行时授权检查

- 对四个协作工具入口调用 `require_root`；
- 将错误文案设为稳定且可测试的用户可见契约。

验证：手工构造 child `ToolContext`，即便直接调用控制器方法也返回授权错误。

### 阶段 5：文档和兼容性清理

- 更新 `README.md`：说明 root 才能编排，child 是叶节点；
- 更新 `DESIGN.md` 的协作段落；
- 若公开过嵌套 child 行为，标记为有意变更；
- 移除 “Codex style” 这类实现导向的测试名，改成行为导向名称。

---

## 9. 测试矩阵

### 9.1 Agent materialization 单元测试

| 用例 | 期望 |
| --- | --- |
| root default | 基础工具 + skill + 四个协作工具 |
| child default | 基础业务工具，不含四个协作工具 |
| child worker | 基础业务工具，不含四个协作工具 |
| child explorer | 只读 allowlist，不含写工具和协作工具 |
| child 有自定义/MCP 工具 | profile 按策略保留或移除；协作工具始终移除 |
| profile model override | 只影响该 child 的模型 |
| profile max turns override | 只影响该 child 的轮次上限 |
| base 工具同名协作工具 | root 的控制器工具覆盖；child 最终不含保留名字 |

### 9.2 Prompt 单元测试

| 用例 | 期望 |
| --- | --- |
| root | 恰有一个 `<multi_agent_mode>` |
| child default/worker/explorer | 不含 `<multi_agent_mode>` |
| child | 含 `<subagent_context>`、路径、profile 名和 leaf 限制 |
| 多次安装 root collaboration | multi-agent 区块不重复 |
| 子 prompt | 不包含“delegate / spawn sub-agents”一类不具备能力的指令 |

### 9.3 授权和集成测试

| 用例 | 期望 |
| --- | --- |
| root 调用 `spawn_agent` | child 成功创建、持久化 parent/root/path lineage 正确 |
| child 直接执行 `spawn_agent` handler | 返回 `collaboration tools are available only to the root agent` |
| child 直接执行 message/wait/interrupt handler | 同样被拒绝 |
| child 尝试创建孙 agent | 不产生 session、不会占用并发配额 |
| root message/wait/interrupt child | 保持现有行为 |
| explorer child | 请求里的 tool definitions 没有协作工具或写工具 |
| default/worker child | 请求里的 tool definitions 没有协作工具 |
| 多个 root session | 每个 root 只管理其 own tree |

### 9.4 回归测试

- session JSONL 中 root/child identity 可以重放；
- 子 session 对 session list 隐藏的行为不变；
- TUI 子 agent snapshot 继续显示；
- 并发上限按 root tree 继续计算；
- fork history 和 follow-up turn 的语义不变。

---

## 10. `<multi_agent_mode>` 与 `tmp/codex` 的关系

### 已确认的事实

`<multi_agent_mode>` 标签在 `tmp/codex` 的 OpenAI Codex 源码中是正式协议常量：

- `tmp/codex/codex-rs/protocol/src/protocol.rs`
- `MULTI_AGENT_MODE_OPEN_TAG = "<multi_agent_mode>"`

Codex 也将它用于 developer-context 的多 agent 模式说明：

- `tmp/codex/codex-rs/core/src/context/multi_agent_mode_instructions.rs`

Codex 当前的内置模式主要表达“显式请求才委派”与“主动委派”两种策略。例如 proactive 模式的文本是：

```text
Proactive multi-agent delegation is active. ...
Use sub-agents when parallel work would materially improve speed or quality.
```

因此，**标签、术语和多 agent 机制显然与 Codex 有直接关联**。

### 不应作出的结论

在 `tmp/codex` 中没有找到 Ash 当前长提示词的逐字副本。Ash 的以下团队协作规则：

```text
Split work where parallelism pays ...
Use explorer ... Prefer worker ...
```

与 Codex 的当前模式文案不同。仓库 git blame 还显示该长文本早于后来把它包进 `<multi_agent_mode>` 的改动而存在于 Ash 历史中。

因此更准确的表述是：

> Ash 参考了 Codex 的 `<multi_agent_mode>` 标签和多 agent 产品模型；当前详细的团队协作提示是 Ash 自己的文本，不是从 `tmp/codex` 当前源码逐字复制。

### Codex 对本提案的启发

Codex 源码中同时存在不同代际/模式的多 agent 设计：部分路径允许嵌套和深度控制，另一些 V2 测试明确断言 leaf worker 不应收到 collaboration tools。不能把 Codex 的任何单一路径当作唯一规范。

本提案选择 root-only 是 Ash 的产品决策，原因是其实现更简单、行为更可预测，并与“root 编排、child 执行”的职责边界一致。

---

## 11. 备选方案与取舍

### 方案 A：仅通过 prompt 告知 child 不要继续委派

**拒绝。** 模型指令不是权限系统；child 若仍收到协作工具，便仍可调用。

### 方案 B：仅从 child 工具表移除协作工具

**不充分。** 正常模型路径有效，但工具意外注册或未来重构时可能绕过。仍需 handler 的 root 检查。

### 方案 C：让 explorer 是 leaf，default/worker 可以嵌套

**不推荐作为默认。** profile 语义被混入调度权限；worker 之间的文件冲突与树级资源控制会变复杂。若未来确实需要 planner/manager role，应显式新增 `Coordinator` capability，而不是隐含在 `worker` 中。

### 方案 D：新建 `MainAgentConfig` 与 `SubAgentConfig`

**拒绝。** 会复制 `Agent`、`SessionOptions`、`Runtime` 和 `RunConfig` 已经清晰表达的内容，导致两套配置漂移。应该用“同一基底 + profile + scope”派生。

---

## 12. 最终决策建议

采用以下规则：

1. 保留 `Agent`、`SessionOptions`、`Runtime`、`RunConfig` 的现有职责；
2. 保留并完成 `AgentProfile` 的统一抽象；
3. 协作工具不属于 profile，而只属于 root session；
4. child 不继承 `<multi_agent_mode>`，全部使用 leaf child context；
5. root-only 同时通过工具装配和运行时身份检查保证；
6. `default` 只是默认通用 profile，不意味着拥有协作权限；
7. 不为当前需求创建额外的大配置结构或通用权限框架。

这能用最少的新抽象，得到清晰的能力边界：

```text
Root default = coordinator + default work profile
Child default = leaf + default work profile
Child worker  = leaf + implementation work profile
Child explorer = leaf + read-only research profile
```

这也是后续若要增加 `reviewer`、`tester`、`planner` 等 profile 时最稳定的扩展点。
