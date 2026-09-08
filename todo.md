# Scrollback 增量提交

> 目标：完成事实立即持久化并进入终端原生 scrollback；未完成内容仍可由 `Esc` 撤销；内存 `Conversation` 只保存完整 `Turn`。

## 设计约束

- [x] `Step` 是 Turn 内最小的完成边界：模型内容和对应工具结果全部确定后才成立。
- [x] runner 只持有已经完成的 `Vec<Step>`，不引入 item 级 pending 状态或第二份 transcript。
- [x] `Conversation` 仍只包含不可变的完整 `Turn`，进行中的 Turn 不进入会话历史。
- [x] 副作用集中在 session actor：runner 请求提交，actor 落盘并确认，runner 收到确认后才能继续下一次模型请求。
- [x] TUI 只消费“已持久化 Step”事实，不自行推断工具生命周期是否可以提交。

## 持久化

- [x] JSONL 使用 `TurnStart { id, input } -> TurnStep { step }* -> TurnEnd { result, stats, summary }`，统计只在完整 Turn 封口时进入业务记录。
- [x] 第一条 Step 与 `TurnStart` 同批写入；无 Step 的 Turn 由 `TurnStart + TurnEnd` 同批写入。
- [x] 每次 Step append 都执行 `flush + sync_data`，成功后才回复 runner。
- [x] 文件尾部的开放 Turn 不进入 `Conversation`，恢复时从 `TurnStart` 起截断。
- [x] 拒绝嵌套 start、孤立 step/end、重复 TurnId，以及开放 Turn 内的 checkpoint。
- [x] 保持单一新格式，不增加版本层、双写或旧 `Turn` 记录兼容分支。

## 事件与 TUI

- [x] 新增 `SessionEvent::StepCommitted { turn_id, index, step }`，仅在 Step 同步落盘后发布。
- [x] 发布提交事件前先排空同一 Step 的 live 事件，保证工具完成预览先于 canonical Step。
- [x] 每个 `StepCommitted` 替换本 Step 的流式预览并立即提交到原生 scrollback。
- [x] `Finished` 只补齐广播接收方遗漏的 Step 并追加 footer，不重复渲染已提交 Step。
- [x] 已有提交 Step 时，取消或持久化错误只清理未完成预览，不删除不可逆 scrollback，也不把原输入恢复成可重试状态。
- [x] subagent 活动聚合忽略 Step 载荷；最终 `Finished(Turn)` 仍是会话完成的唯一 canonical 事件。

## 验收

- [x] 覆盖 Step 在 Turn 完成前已落盘并已发布的集成测试。
- [x] 覆盖崩溃后开放 Turn 不显示、不进入业务会话并被截断。
- [x] 覆盖 TUI 已提交 Step 不重复、遗漏 Step 由 Finished 补齐、取消不恢复输入。
- [x] `cargo test --workspace --exclude ash-workflow`
- [ ] `cargo test --workspace`（当前未跟踪的 `ash-workflow` 测试要求 `AgentHandle: Debug`，与本次改动无关）
- [x] 受影响 Rust 文件的 rustfmt 检查
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `git diff --check`
