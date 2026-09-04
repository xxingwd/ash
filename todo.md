# 近期代码修正计划

> 来源：2026-09-04 对当前工作树及最近提交的代码审查。
> 目标：先修复可见的 TUI 正确性问题，再收紧统计状态模型；遵循 `AGENTS.md` 的最小设计、强类型、DRY 和显式数据流原则。
>
> **状态（2026-09-04 第二轮）**：P0.1、P0.2、P0.3、P1.4、P1.5、追加 9、追加 10（文档部分）、P2 的 footer 收紧与 `omitted` 清理已实现并通过 `cargo test --workspace`、`clippy -D warnings`。
> P1.6 降级为不做：`working_dir` 是会话级固定字段（`App::new` 传入），运行期不变，缓存键缺它的实际触发条件不存在，属极端场景防御。
> P1.7、追加 11 的 `Worked.elapsed` 显式变体、追加 8 的 `Step::tool_calls()` 收敛暂缓：收益低，待有真实需求再做。

## 实施原则

- [ ] 保持改动聚焦，不引入新的折行依赖；继续使用现有的 `unicode-segmentation`、`unicode-width` 和 Ratatui 类型。
- [ ] 持久化数据只保留领域事实；临时 UI 活动状态与最终 `Turn` 数据分开建模。
- [ ] 优先使用纯转换函数：文本解析、折行、截断、前缀选择都应输入明确、返回新值；缓存写入和 Ratatui 绘制留在边界层。
- [ ] 不做无收益的“全面函数式重写”。actor、事件循环、流式渲染中的局部可变状态是合理的。
- [ ] 不把 TUI 状态下沉到 `ash-agent`，不通过解析展示字符串恢复领域信息。

## P0：TUI 渲染正确性

### 1. 按最终视觉行截断折叠内容

涉及：`crates/ash-tui/src/live_block.rs`、`crates/ash-tui/src/wrap.rs`

- [x] 将折叠流程统一为“解析 ANSI / 高亮 -> 按可用宽度折行 -> 按最终视觉行做 head/tail 截断 -> 渲染”。
- [x] `render_tool_output` 不再先按 `str::lines()` 截取逻辑行；单个超长行折行后也必须受 `COLLAPSED_MAX_LINES` 限制。
- [x] 多行命令的 continuation 部分先全部折行，再调用截断逻辑；第二个逻辑行很长时不得绕过折叠上限。
- [x] 省略提示显示被省略的视觉行数，计算一次后直接使用，不保留 `let _ = omitted` 之类的无效中间值。
- [x] 展开模式仍显示完整内容，并保留 ANSI 样式、高亮和原有 head/tail 顺序。
- [x] 如一次性折行会放大超长输出的内存占用，将折行结果改为迭代式收集有限的头尾行；不要因此新增通用框架。

完成标准：

- [x] 64 KiB 的单行工具输出在折叠状态下最多占约定的内容行数，展开后内容完整。
- [x] 超长的首条命令和后续命令都遵守相同的 continuation 行上限。
- [x] 中英文、CJK、emoji、ANSI 彩色文本的宽度和样式没有回退。

### 2. 修复极窄终端中的前缀吞字

涉及：`crates/ash-tui/src/wrap.rs`、`crates/ash-tui/src/history_block.rs`、`crates/ash-tui/src/live_block.rs`

- [x] 明确定义 `prefix_width >= width` 时的降级规则，不能在保留完整前缀后假装仍有 1 列内容宽度。
- [x] 推荐规则：当前行放不下“前缀 + 至少一个 grapheme”时，前缀单独成行或省略装饰前缀，正文从下一行零缩进开始。
- [x] 将该规则收敛在共享折行函数中，避免 error、tool title、history 各写一套窄宽度分支。
- [x] 保证任何非空正文在宽度 `1..=10` 时至少有可见字符，continuation 行不能只剩空格。

完成标准：

- [x] 为 `wrap_styled_line_with_prefix` 增加宽度 `1..=10` 的表驱动测试。
- [x] 覆盖前缀等于宽度、前缀大于宽度、双宽字符和超长单词。
- [x] error、tool title、tool output 在窄宽度下均不丢正文且不 panic。

### 3. 多行历史块只显示一次语义前缀

涉及：`crates/ash-tui/src/history_block.rs`

- [x] 用户输入只在第一条显式源行显示 `› `，后续显式行使用等宽 hanging indent 或空前缀。
- [x] info 只在第一条显式源行显示 `• `。
- [x] error 只在第一条显式源行显示 `• Error: `；后续行与正文起始列对齐。
- [x] 软折行和显式换行使用同一 continuation 规则，避免每行重复表达同一个事件类型。

完成标准：

- [x] 分别覆盖单行、多行、空行、首尾空行以及窄宽度渲染。
- [x] 测试直接检查 Buffer 中的可见行，确保前缀只出现一次。

## P1：消除统计状态的双重事实源

涉及：`crates/ash-core/src/conversation.rs`、`crates/ash-core/src/event.rs`、`crates/ash-agent/src/engine.rs`、`crates/ash-agent/src/jsonl.rs`、`crates/ash-agent/src/session.rs`、`crates/ash-cli/src/modes.rs`、`crates/ash-tui/src/`

### 4. 从持久化 `TurnStats` 移除 `tool_calls`

- [x] `TurnStats` 只保留模型返回的统计事实：`input_tokens`、`output_tokens`、`generation_ms`。
- [x] 最终或恢复后的 turn 统一通过 `Turn::tool_calls().count()` 得到工具调用数；必要时增加一个返回饱和值 `u64` 的领域方法，避免各调用方重复转换。
- [x] 删除 engine 完成 turn 后回写 `turn.stats.tool_calls` 的归一化。
- [x] 删除 JSONL 读取时的 `normalize_turn_stats`，不再用可变修补维持两个字段一致。
- [x] 旧 JSONL 中多余的 `stats.tool_calls` 应可兼容读取；新记录不再写该字段，并增加向后兼容回归测试。

设计约束：`Turn.steps` 是已完成工具调用的唯一持久化事实源，不能再构造出 `steps` 与计数互相矛盾的 `Turn`。

### 5. 用独立快照建模实时活动

- [x] 在 `ash-core` 定义小而扁平的临时值类型，例如：

```rust
pub struct TurnActivity {
    pub stats: TurnStats,
    pub tool_calls: u64,
}
```

- [x] `SessionEvent::Stats` 携带完整 `TurnActivity` 快照，或重命名为更准确的 `SessionEvent::Activity`；不要同时传递可累加 delta 和外部镜像计数。
- [x] `TurnRunner` 在一个位置更新活动快照并发布，TUI / collab 收到后只做整体替换。
- [x] 明确 `tool_calls` 的时点语义。当前是在一批工具执行完后增加，若保持该行为应命名/记录为 completed；若产品需要 started 数量，则在 `ToolStarted` 产生时更新快照并测试并发工具场景。
- [x] turn 完成或从历史恢复时，不信任临时快照，改由 canonical `Turn` 生成 footer、CLI usage 和日志数据。
- [x] footer 若同时需要 stats 与工具数，显式接收 `TurnActivity` 或两个有语义的参数，不把计数重新塞回 `TurnStats`。

完成标准：

- [x] 公共构造无法制造“最终工具列表有 N 项但持久化计数为 M”的状态。
- [x] live stats、子代理 stats、最终 footer、恢复后的 footer 和 `--print` usage 显示一致。
- [x] 多轮模型调用、并发工具、取消、截断、失败以及旧 JSONL 恢复均有针对性测试。

## P1：缓存与视觉语义

### 6. 将工作目录纳入 LiveBlock 缓存键（已降级：不做）

涉及：`crates/ash-tui/src/live_block.rs`

> 降级原因：`working_dir` 是 `App::new` 传入的会话级固定字段，会话内不变，同块同宽度换目录渲染的场景不存在；属极端情况防御，收益不抵改动。

- [ ] `RenderCache` 保存并比较规范化后的 `working_dir: Option<PathBuf>`，或使用等价的显式缓存键类型。
- [ ] 缓存复用和 streaming markdown 的增量复用都要检查影响渲染的完整输入。
- [ ] 同一 `LiveBlock`、同一宽度先后用两个工作目录渲染时，工具路径必须随目录变化。

### 7. 恢复文件变更统计的语义颜色（暂缓：低收益外观项）

涉及：`crates/ash-tui/src/live_block.rs`

- [ ] `render_file_change` 使用 styled spans 组装 detail：路径保持默认样式，`+N` 为绿色，`-N` 为红色。
- [ ] 继续走共享 hanging-wrap，不能为了颜色退回手工宽度切割。
- [ ] 增加跨行后的样式断言，保证折行不会丢失增删颜色。

## P2：低风险表达力清理

- [x] `restored_turn_footer` 的所有分支都返回值，将返回类型从 `Option<HistoryBlock>` 收紧为 `HistoryBlock`，同步简化调用方。
- [x] 清除 `render_tool_output` 中重复计算或计算后丢弃的 `omitted`。
- [x] 抽取“折行后截断”（`wrap::truncate_rows`）与“首行前缀”（`wrap::render_prefixed_lines`）两个稳定语义的纯函数。
- [x] 命名保持 Rust 约定，并让名称表达语义时点：`Turn::completed_tool_calls`、`TurnActivity::completed_tool_calls`。

## 建议实施顺序

1. [x] 先完成 P0.1 和 P0.2，建立统一的视觉行与窄宽度语义。
2. [x] 再完成 P0.3，修复历史块前缀表现。
3. [x] 单独提交统计模型修改（P1.4 和 P1.5），便于审查 JSONL 兼容性和跨 crate 数据流。
4. [x] 最后完成 P2 中由前述修改直接暴露的冗余。

## 验收

- [ ] `cargo test -p ash-tui`
- [ ] `cargo test -p ash-core -p ash-agent -p ash-cli -p ash-collab`
- [ ] `cargo test --workspace`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `git diff --check`
- [ ] 对可见 TUI 变更保留宽屏、窄屏和展开/折叠状态的终端截图，供 PR 审查。

## 追加：统计快照改造复审（2026-09-04）

> 来源：对工作树中 `SessionEvent::Stats` / `TurnStats.tool_calls` 改造（未提交）的复审。
> 其中 8、9 与上文 P1.4 / P1.5 指向同一根因：已按 P1.4 移除持久化 `tool_calls`，二者随之解决。
> 10 的文档措辞已同步；11 部分完成。

### 8. 收敛 tool_calls 计数的重复派生（DRY）

涉及：`crates/ash-agent/src/engine.rs:128`、`crates/ash-agent/src/engine.rs:206-219`、`crates/ash-agent/src/jsonl.rs:474-480`、`crates/ash-core/src/conversation.rs`

- [x] 已随 P1.4 解决：`engine.rs:128` 回写与 `jsonl.rs` 的 `normalize_turn_stats` 删除，计数统一由 `Turn::completed_tool_calls` / runner 内单点派生，不再三处重复。

### 9. 消除 tool_calls 的“累计 + 覆盖”双路径

涉及：`crates/ash-agent/src/engine.rs`

- [x] 选择“中间快照也派生”：`record_stats` 不再累计 tool_calls，批次完成后由 `publish_activity` 从 `self.steps` 派生，`run()` 不再覆盖。
- [x] 补多轮模型调用 + 多批工具下“中间 `Activity` 快照 == 最终 turn 派生值”的一致性测试（`activity_snapshots_match_the_final_turn_across_model_rounds`）。

### 10. 明确 Stats 快照发布条件并同步 DESIGN.md

涉及：`crates/ash-agent/src/engine.rs`、`DESIGN.md`

- [x] DESIGN.md "Stream integrity, stats, and cancellation" 一节措辞修正为"reports non-zero usage 时发布"；发布条件保持 `input_tokens > 0 || output_tokens > 0`（具名谓词收益低，未抽取）。
- [x] DESIGN.md "Conversation model" 一节同步：completed tool-call count 不再是持久化字段，始终由 `Turn.steps` 派生，旧记录容忍读取。
- [ ] generation_ms-only 的 delta（累计但不发布）语义用测试或注释显式说明（暂缓）。

### 11. 用显式变体建模恢复态 footer

涉及：`crates/ash-tui/src/history_block.rs`、`crates/ash-tui/src/inline.rs`

- [ ] `Worked.elapsed: Option<String>` 改为 `HistoryBlock::Restored(TurnStats)` 或等价显式状态（暂缓：低收益重构；`worked`/`restored` 构造器已收口该不变量）。
- [x] `restored_turn_footer` 返回类型收紧为 `HistoryBlock`。
- [x] 恢复会话为每个已完成 turn 渲染 footer 分隔行的行为确认保留，并在 DESIGN.md "Events and TUI" 补充说明。
