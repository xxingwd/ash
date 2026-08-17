# Multi-Agent 最小设计

> 状态：已采用
> 范围：`ash-agent`、`ash-collab`、CLI 装配层

## 决策

Ash 保留一个通用的 `Agent` 执行定义。主 Agent 和子 Agent 使用同一条
`Agent + SessionOptions -> RunConfig` 路径，不建立两套配置或执行引擎。

协作能力只在主 Agent 装配时追加：

```text
clean base Agent
├── main Agent  = base + collaboration tools + multi-agent instructions
└── child Agent = profile(base)
```

子 Agent 默认没有以下工具：

```text
spawn_agent
message_agent
interrupt_agent
wait_agent
```

这是装配结果，不是新的权限系统。当前不引入 `AgentScope`、`max_depth`、
`CollaborationPolicy` 或 root/child 专用配置类型。

## 核心约束

### 干净基底

CLI 创建的基础 `Agent` 只包含模型、基础 prompt、业务工具、limits 和 context policy。
它不能包含协作工具或 `<multi_agent_mode>`。

`InheritedSessionFactory` 在主 Agent 增强之前保存该基底。所有 child 都从这个基底克隆。

### 主 Agent 装配

`install_collaboration` 完成一次性装配：

```rust
pub fn install_collaboration(
    base: Agent,
    options: SessionOptions,
    runtime: Runtime,
    max_concurrent_children: Option<usize>,
) -> Result<(Agent, AgentControl), ToolError>
```

函数先用 `base.clone()` 创建 child factory，再向返回的主 Agent 追加协作工具和
`<multi_agent_mode>`。

### 子 Agent 派生

`AgentProfile` 只覆盖工作方式：

- prompt instructions；
- 普通业务工具投影；
- 可选 model；
- 可选 max turns。

`ToolPolicy::Inherit` 只保留基底已有工具。`ToolPolicy` 和 `apply_profile` 不接收
collaboration tools，因此 child 路径无法把它们加回来。

### Prompt

主 Agent 使用基础 prompt 加 `<multi_agent_mode>`。

子 Agent 使用：

```text
base prompt
+ profile instructions
```

不添加 `<subagent_context>`、父路径、任务路径或额外身份说明。父子关系已经由
`SessionIdentity` 持久化，具体任务通过正常 user message 发送给 child。

## 生命周期

每个 child 都是独立 `Session`，通过 `Runtime::start_child` 启动。`AgentControl` 只负责：

- spawn；
- message/follow-up；
- interrupt；
- wait；
- 并发计数和 UI snapshot。

Turn 排队、持久化、取消和上下文管理继续由 `ash-agent` 负责。

## 测试契约

必须覆盖以下行为：

1. 主 Agent 包含四个协作工具和 `<multi_agent_mode>`。
2. `default`、`worker`、`explorer` child 都不包含协作工具。
3. child prompt 不包含 `<multi_agent_mode>` 或 `<subagent_context>`。
4. `default` 和 `worker` 保留继承的业务工具。
5. `explorer` 只保留显式只读 allowlist。
6. profile 的 model 和 max-turns override 保持有效。
7. root 与 child 继续使用同一套 `Runtime -> Session -> Turn` 执行路径。

## 非目标

当前不设计：

- child 创建 grandchild；
- 通用 capability/permission 框架；
- 可配置的协作深度；
- root/child 专用执行引擎；
- TOML/JSON Agent 配置格式。

如果将来需要嵌套协作，应以新的明确需求重新设计，而不是为当前实现预埋抽象。
