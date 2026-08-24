# Agent 管理与统计实施清单

本文记录最终采用的方案。设计目标是让统计只有一个事实来源，让子 Agent 管理与真实
Session 生命周期解耦，同时保持现有 TUI footer 不变。

## 最终数据流

```text
Provider usage chunks
        |
        | 单 request 内每个字段取最大值，缺失值用本地估算
        v
    Request Usage
        |
        | Turn 内所有 request、retry 和自动 compaction 求和
        v
      TurnStats ----------------> TurnProgress ----------------> Working
        |
        | TurnEnd 成功持久化
        v
 SessionLog projector ----------> Session::stats() -----------> Agent snapshot
```

- `Usage` 只保存可累计的 input、output、tool calls 与 estimated。
- `TurnStats` 保存当前 Turn 的 Usage 与 generation time，TPS 由两者派生。
- `SessionStats` 保存已持久化的累计 Usage、当前 active Turn 快照和 context token gauge。
- Engine 是 active Turn 统计的唯一所有者。
- SessionLog projector 是 settled Session Usage 的唯一累计者。
- TUI 和 collaboration 只替换快照，不做二次累加。

## Core 与 Engine

- [x] `Usage` 增加 `tool_calls`、饱和加法和 `total_tokens`。
- [x] 将 `generation_ms` 移入非可选的 `TurnStats`。
- [x] 增加 `SessionStats { settled_usage, active_turn, context_tokens }`，context 不进入 Usage。
- [x] `TurnView` 保存 final `TurnStats`，`SessionView` 保存 `SessionStats`。
- [x] 单 request 内 provider usage chunk 按字段取最大值，避免重复快照重复计数。
- [x] Turn 内多次 request/retry 按实际请求求和；失败重试不回退已发生 usage。
- [x] 每次执行工具前精确增加一次 `tool_calls`。
- [x] 首个模型输出立即发送完整 `TurnProgress`，之后最多每 250ms 更新一次。
- [x] `TurnProgress` 使用非阻塞发送；丢帧后下一份完整快照自动收敛。
- [x] provider 最终 usage 校准本地估算；没有 provider usage 时保留 `estimated`。
- [x] generation time 从首个 reasoning/text/tool-call 开始，不包含工具执行和 retry backoff。
- [x] 自动 compaction usage 合入触发它的 Turn。
- [x] 自动 compaction 失败时仍将已发生 usage 带入该 Turn 的最终统计。
- [x] 普通请求与 compaction 共用 request usage accumulator。

## Session 与持久化

- [x] `TurnEnd` 持久化非可选 `TurnStats`，磁盘继续使用兼容的 `usage` key。
- [x] 旧日志缺少 `tool_calls` 时按零读取；`usage: null` 读取为默认 TurnStats。
- [x] 手动 compaction usage 通过单用途 `CompactionUsage` 记录持久化。
- [x] 手动 compaction 失败时仍持久化已发生 usage，并同步刷新 Session stats 快照。
- [x] projector 只在 durable `TurnEnd`/`CompactionUsage` 上累计 Session usage。
- [x] rollback 不倒扣已经发生的 usage，只重建 history/context。
- [x] resume 从 JSONL 重建 usage，并从当前 model context 重新估算 context tokens。
- [x] fork 的 usage 从零开始，context 根据复制后的消息估算。
- [x] Session 内部使用私有 `watch<SessionStats>`，对外提供同步 `Session::stats()` 快照。
- [x] `ContextChanged` 是 prepared context 的唯一实时 gauge 来源。
- [x] Session actor 在 `TurnStarted`/`TurnProgress` 更新 active Turn，并在 durable commit 后原子清除。
- [x] `SessionStats::total_usage()` 是 settled 与 active 的唯一合并入口。

## 子 Agent 管理

```rust
struct AgentTree {
    agents: HashMap<AgentPath, AgentEntry>,
    removed: HashSet<AgentPath>,
    completions: VecDeque<QueuedCompletion>,
}
```

- [x] `AgentEntry` 不缓存 Turn 进度；usage 直接读取 `session.stats().total_usage()`。
- [x] `SubagentSnapshot`/`SubagentView` 显示 settled Session usage 与 active Turn usage 的投影和。
- [x] child Session 的 stats watch 驱动 collaboration watch 更新，不由 TUI 定时轮询。
- [x] active 到 settled 的切换由 Session actor 原子发布，collaboration 不维护 baseline。
- [x] `list_agents` 独立读取当前快照，不消费 unread completions。
- [x] `remove_agent` 在一次 tree lock 内摘除 entry、写入 path tombstone、清理该 path 的 completion。
- [x] 解锁后取消未完成 Turn 并 drop entry，断开消息与 completion 路由。
- [x] completion settle 和 fallback 入队前都要求 path 仍在 active table。
- [x] tombstone 名称在当前 tree 生命周期内不可复用，因此不需要额外 instance ID。
- [x] remove 不调用 SessionStore 删除接口，子 Session JSONL 保留。
- [x] `message_agent` 只能找到 active entry，removed Agent 无法再接收消息。

协作工具共五个：`agent`、`message_agent`、`list_agents`、`remove_agent`、`wait_agent`。

## TUI

- [x] footer 保持 model/path/protocol 布局，上下文占用只显示百分比，不显示字符进度条。
- [x] 宽屏 Working 行持续显示当前 Turn 的 input/output/tools/TPS，宽窄模式只由终端宽度决定。
- [x] Working/Thinking 状态行使用静态圆点，移除点动画和中断提示，elapsed 紧跟状态文案。
- [x] Worked 使用 durable `TurnView.stats`，并增加工具调用数。
- [x] TUI 只接受与 current turn ID 匹配的 `TurnProgress`。
- [x] child Agent 的 Idle 和 Running 状态都持续显示。
- [x] Running 排在 Idle 前，同状态按名称稳定排序。
- [x] 状态只用前置颜色圆点表达，不显示 `running`/`idle` 文本。
- [x] 行内固定显示 name、input/output 和 tools；profile 不显示。
- [x] latest task 放在末尾，按 Unicode 显示宽度截断，窄屏优先隐藏。
- [x] 最多显示四个 Agent，多余项显示 `+N more`。
- [x] 子 Agent 行实时显示当前 Session 累计 usage，完成后收敛到 durable SessionStats。

目标格式：

```text
● inspect_glob  12.0k in / 2.2k out · 8 tools · inspect current usage flow…
○ fix_bash       5.4k in / 700 out · 3 tools · fix cancellation handling…
```

## 验证

- [x] `cargo fmt --all -- --check`
- [x] `cargo test --workspace`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] 覆盖 80 列宽档与 79 列窄档，实时数值变化不会触发布局切换或隐藏 stats。
- [x] 覆盖长 Agent 名称、末尾任务截断、四行上限和 footer 布局。
- [x] 覆盖 remove 后 Agent 立即消失、迟到 completion 不出现且原 Session JSONL 仍存在。
