# Provider Usage 统计简化 TODO

目标：token 统计只使用 Provider 返回的事实；本地估算只用于模型请求发送前的
context 容量检查。删除事实与估算的混合路径，不新增新的事件、日志或来源类型。

## 固定语义

```text
ModelEvent::Stop          = 模型请求完成
ModelEvent::Usage         = Provider 返回的 token 事实
estimate_request_tokens   = 发送前的 context preflight
工具已经执行              = Esc 不可回退边界
```

- 收到 `Stop` 才接受当前响应，并允许执行其中的工具调用。
- 完成请求有 usage 时才更新 `in/out`、TPS 和最近一次 input token。
- 完成请求没有 usage 时仍接受响应，但所有 token 统计保持不变。
- 没有 `Stop` 的响应不进入持久化上下文，不执行工具，也不贡献统计。
- 状态栏不展示 token 或 TPS 估算值。
- ContextPolicy 的估算不进入 `Usage`、日志或 TUI。

## 不做

- 不新增 `Completed` 事件，继续使用现有 `Usage` 和 `Stop`。
- 不新增 `UsageSource`、`EstimatedUsage` 或 request 状态类型。
- 不新增 request 级 usage 日志。
- 不增加 context anchor 或 Provider usage 校准逻辑。
- 不根据字符数估算 `in/out` 或 TPS。
- 不改变现有 Session、Turn 和 projector 的所有权边界。

## Core

- [ ] 从 `Usage` 删除 `estimated` 字段及其合并规则。
- [ ] 在 `TurnStats` 增加 `last_input_tokens: Option<u64>`，表示当前 Turn
      最后一次有 Provider usage 的完成请求。
- [ ] `TurnStats::saturating_add` 继续累加 usage 和匹配的 generation time，
      `last_input_tokens` 取右侧最新的 `Some`，否则保留左侧值。
- [ ] 为旧 JSONL 和快照中的缺失 `last_input_tokens` 提供 `serde` 默认值。
- [ ] 更新公开注释：`Usage` 是已确认的 Provider usage 与本地工具计数，不含估算。

## Protocol

- [ ] 保留现有 `ModelEvent::Usage` 和 `ModelEvent::Stop` 协议形状。
- [ ] 保留 request 内 usage chunk 的字段和解，避免累计快照重复计数。
- [ ] 确认所有适配器在语义终止后继续读取尾部 usage。
- [ ] 将各 Provider 的 input token 归一为完整输入量；Anthropic 需要正确处理
      cache read 和 cache creation token，OpenAI cached token 不重复相加。
- [ ] Provider 缺少 usage 不升级为协议错误，完成语义仍由 `Stop` 决定。

## Engine

- [ ] 将 `UsageAccumulator::finish` 改为只返回已收集的 Provider usage，删除
      estimated input/output 参数和本地回填。
- [ ] 删除按输出字符数估算 token 的统计路径。
- [ ] 仅当请求同时收到 `Stop` 和 Provider usage 时，将其 usage 加入 TurnStats。
- [ ] 仅对上述请求累加对应 generation time，保证 TPS 的分子和分母来自同一批请求。
- [ ] 请求有 `Stop` 但无 usage 时接受消息，TurnStats 的 token、TPS 和
      `last_input_tokens` 保持不变。
- [ ] 请求取消、失败或断流且没有 `Stop` 时，丢弃当前请求的消息、usage 和 timing。
- [ ] 未完成请求中解析出的 tool call 不得执行。
- [ ] 工具实际开始执行后继续精确增加 `tool_calls`，不依赖 Provider usage。
- [ ] 自动与手动 compaction 只累计真实 Provider usage，不使用估算兜底。

## Context

- [ ] 保留现有完整 outbound request 估算器。
- [ ] 每次调用 `model.stream` 前运行一次 context preflight，包括首次请求、
      tool result 后续请求和 steering 后续请求。
- [ ] Preflight 结果只用于 compaction 阈值判断和内部裁剪，不发送到 TUI。
- [ ] 删除 context estimate 到 `SessionStats::context_tokens` 的更新路径。
- [ ] TUI context 只在完成请求返回 usage 时更新为该请求的 `input_tokens`。
- [ ] Compaction 自身的模型请求 usage 计入累计 usage，但不作为对话 context gauge。

## Session 与持久化

- [ ] `TurnEnd` 随 TurnStats 持久化 `last_input_tokens`，不新增 LogEntry。
- [ ] Projector 继续只从 durable `TurnEnd` 和 `CompactionUsage` 累加 usage。
- [ ] Session context gauge 取最近一个已提交 Turn 的 `last_input_tokens`。
- [ ] Resume 从日志恢复累计 usage 和最近一次 `last_input_tokens`，不重新估算 TUI context。
- [ ] Rollback 后 context gauge 回到前一个已提交 Turn 的 `last_input_tokens`。
- [ ] Fork 的 context gauge 从 `None` 开始，直到新 Session 首次收到 Provider usage。
- [ ] 没有 usage 的完成请求不覆盖已有 context gauge。

## TUI 与取消

- [ ] Working 状态只显示已经确认并累加的 `in/out`；当前流式请求没有 usage 时数值不变。
- [ ] TPS 只从已确认 output token 和匹配的 generation time 派生。
- [ ] 移除 token/TPS 的 `~`、estimated 分支和相关文案。
- [ ] Footer context 使用最近一次已确认的 input token 百分比；没有历史值时不显示。
- [ ] Esc 丢弃当前未完成模型请求的全部流式输出。
- [ ] 已完成并执行过的工具及其之前内容保留；其后的未完成请求回退。
- [ ] 当前 Turn 没有已完成工具结果时，保留现有整 Turn rollback 和 prompt 恢复行为。
- [ ] 复用现有 staged persistence 和 rollback savepoint，不引入 request commit log。

## 清理

- [ ] 删除不再使用的 `estimated_output_tokens`、输出字符计数和相关辅助函数。
- [ ] 删除 JSONL、快照、TUI 和测试中的 `estimated` 字段。
- [ ] 删除只服务于实时 token 估算的 progress tick；保留 elapsed、工具和其他实时事件所需路径。
- [ ] 更新 `DESIGN.md`：完成、usage、context preflight 和工具回退各有唯一来源。

## 回归测试

- [ ] Provider usage 分片按字段正确和解且只累计一次。
- [ ] `Stop + usage` 更新累计 `in/out`、TPS 和 `last_input_tokens`。
- [ ] `Stop + no usage` 接受响应但不更新任何 token 统计。
- [ ] Usage 后断流且无 `Stop` 时不累计、不持久化消息、不执行工具。
- [ ] 流式生成期间状态栏 token 数保持不变，完成后一次更新。
- [ ] TPS 不包含缺少 usage 请求的 generation time。
- [ ] Resume 恢复累计 usage 和最后一次 context token。
- [ ] Rollback 恢复前一个 context token，fork 从未知 context 开始。
- [ ] Compaction 继续在每次请求发送前触发，但估算不进入 TUI 或 Usage。
- [ ] Esc 保留已完成工具结果，丢弃其后的未完成模型响应。
- [ ] Anthropic cache input 与 OpenAI cached input 的归一化不存在漏算或重复计算。

## 验证

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`

## 完成标准

- `in/out`、TPS 和持久化 usage 中不存在本地估算值。
- 状态栏 token 数只在完成请求返回 Provider usage 后变化。
- TUI context 可从日志稳定恢复，缺失时保持未知。
- ContextPolicy 仍能在每次模型请求发送前避免超过 context window。
- Esc 不提交未完成响应，也不回退已经执行的工具副作用。
- 没有新增公共概念、第二套计数器或 request 级持久化模型。
