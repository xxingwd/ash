# Ash 极简回归计划

> 生成日期：2026-08-05
> 目标：把 Ash 拉回「极简 CLI coding agent」的定位。TUI 方向已确认：**inline 原生 scrollback**（不进 alternate screen、不捕获鼠标）为最终方向，不再摇摆。
> 依据：2026-08-05 对全部 8 个 crate 的三路细粒度功能审计（ash-agent+ash-collab / ash-tui / ash-cli+ash-protocol+ash-tools+ash-core）。
> 原则：
> - 删除只动「无消费者」的代码，不动任何被 CLI/TUI/collab 真实调用的路径。
> - 每批改完跑 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`。
> - 先删后简：无争议的删除先落地，涉及取舍的由决策点拍板后落地。

---

## 复核记录

- ✅ 2026-08-05：基线提交 `6cf0883`（当前工作区全部改动入库，fmt/test/clippy 通过）。本计划基于该基线。
- ✅ 2026-08-05：**Step 1（A 类）已落地并提交**。A1–A5 全部删除，附带同步：engine.rs 的 `RateLimited { .. }` 匹配与测试构造、TODO.md #17 的过时 `with_spawner` 表述。`RateLimited` 变体由带字段改为 unit 变体（错误文案同步简化）。
- ✅ 2026-08-05：**Step 2（C1）已落地并提交**。删除鼠标交互整套（从未启用 `enable_mouse_capture`，属不可达代码）：`selection.rs` 模块（267 行）、viewport.rs 的 `SelectableText` 字段/构建/5 个方法/4 个测试、inline.rs 的 `TextSelection`/`selection` 状态/`scroll_lines_up/down`/`start/drag/finish_selection`、app.rs 的 `handle_mouse`/`picker_mouse_action`/`scroll_picker`/`MOUSE_SCROLL_ROWS`、inline_surface.rs 的 `copy_to_clipboard`/`osc52_sequence` + 测试，并移除 ash-tui 的 base64 依赖（`transcript_area` 字段随之无读，一并删除）。净 −633 行。
- ✅ 2026-08-05：**Step 3（C2 复核）已落地并提交**。确认 Ctrl+O 展开是设计意图（README「思考区不可展开」为文档错误，AGENTS.md 无此描述）。保留 50 行逻辑，合并复杂度：统一 3 组 5/50 常量为 `ansi.rs` 的 `COLLAPSED_MAX_LINES`/`EXPANDED_MAX_LINES`；提取泛型 `split_with_ellipsis`（head+ellipsis+tail），`truncate_command_lines` 复用（删 30 行手写 drain 逻辑），`render_tool_output` 与 reasoning 预览引用共享常量。TUI 行为不变（142 测试全绿）。README 修正「不可展开」并补充 Ctrl-O 文档。

---

## A. 纯死代码 — 无任何消费者，可直接删

| # | 项 | 位置 | 说明 |
|---|---|---|---|
| A1 | `AgentId` | ash-core/src/message.rs | 全仓零调用（含 FromStr/Display/Default 全套 impl），唯一确定的死代码 |
| A2 | `Turn::interrupt()` | ash-agent/src/thread.rs | 零消费者（含测试）；CLI Esc 与 collab interrupt_agent 都走 `cancellation_token().cancel()` |
| A3 | `PassthroughContextPolicy` | ash-agent/src/context_policy.rs | 无消费者（仅 lib.rs re-export） |
| A4 | `AgentControl::with_spawner` | ash-collab/src/control.rs | 与 `new` 完全重复，无消费者 |
| A5 | `ProtocolError::RateLimited.retry_after` 字段 | ash-core/src/error.rs | 恒为 `None`，从未有真实值（map_status 不填充） |

## B. 框架先于需求 — 有设计意图但产品零消费，需决策

| # | 项 | 位置 | 说明 | 决策 |
|---|---|---|---|---|
| B1 | Extension 机制全套（`Extension::prepare/complete`、`Runtime::with_extension`、`TurnPatch`、thread.rs 中 ~60 行失败/投影/合并支撑） | ash-agent/src/extension.rs、runtime.rs、thread.rs | 仅测试消费（GoalExtension/FailingPrepareExtension）；DESIGN.md 列为预期集成边界 | ⬜ |
| B2 | `Thread::enqueue` + `InputSource::Schedule/Heartbeat/Custom` | ash-agent/src/thread.rs | 仅测试；为「调度器/心跳」预留，产品无此场景 | ⬜ |
| B3 | `Turn::steer` + steering 通道 + engine `apply_steering` | ash-agent/src/thread.rs、engine.rs | 仅测试；chat gateway 场景。删除需动 run_turn select 循环与 engine 调用点（非纯删除） | ⬜ |
| B4 | idempotency_key 校验链 | ash-agent/src/thread.rs、log.rs | 仅测试；为「外部触发」设计 | ⬜ |
| B5 | `mcp.rs` 全模块（`McpManager`/`McpToolAdapter`/`load_mcp_tools`） | ash-agent/src/mcp.rs | 无产品入口：CLI 没有任何配置 MCP 的途径，等于不可用的库能力 | ⬜ |

## C. 隐藏功能 — README 未描述甚至矛盾，建议删

| # | 项 | 规模 | 位置 | 说明 |
|---|---|---|---|---|
| C1 | 鼠标拖拽选择 + OSC52 复制整套 | ~500 行（含测试） | selection.rs + viewport.rs/inline.rs/app.rs/inline_surface.rs 相关 | **不可达代码**：从未调用 `enable_mouse_capture`，crossterm 不会投递鼠标事件。删除后与 README「不捕获鼠标、复制交给终端」自洽 |
| C2 | ~~Ctrl+O 全局展开/折叠~~（✅ 已复核：**保留功能**，合并重复实现） | 贯穿 5 个模块 | app.rs / inline.rs / live_block.rs / stream_state.rs / ansi.rs | 决策变更：功能是设计意图，README「思考区不可展开」为文档错误（已修正）。真正的复杂度是 3 处重复的 head/ellipsis/tail 截断（工具输出 / bash 命令 / reasoning 预览）+ 3 组 5/50 常量，已合并 |
| C3 | bash 命令语法高亮（syntect + two_face 完整语法集常驻内存） | ansi.rs 高亮部分 | crates/ash-tui/src/ansi.rs | README 未提；只为标题行配色引入两个重依赖。降级为纯文本/ANSI 解析可删依赖 |
| C4 | `/status` 命令 | slash_command.rs + inline.rs | 输出 model/protocol/directory，与底栏信息完全重复 | |
| C5 | welcome card ASCII logo（三级回退） | welcome_card.rs | 纯装饰，可简化成单行标题 | |
| C6 | edit/write diff 着色预览 | live_block.rs `render_change_preview` | **与 README 隐私承诺矛盾**：「文件内容、编辑前后文本不会进入终端历史」；建议删以守隐私 | |

## D. 重复实现 — 技术债，非功能问题

| # | 项 | 位置 | 建议 |
|---|---|---|---|
| D1 | `/new` 与 `/clear` 逻辑完全相同 | slash_command.rs | 二选一（或保留双入口，直觉性） |
| D2 | session_picker / fork_picker 环绕导航逐行同构 | session_picker.rs / fork_picker.rs | 合并为一个泛型 picker |
| D3 | 菜单视图结构两份（`MenuView` 借用 / `RenderedMenu` owned 副本） | menu.rs / inline.rs | 去掉副本层 |
| D4 | `sanitize_single_line` 两份拷贝 | scrollback.rs / tool_display.rs | 合并 |
| D5 | 字符级换行 4 份实现 | scrollback.rs / input.rs / markdown.rs / ansi.rs | 各自带样式，合并收益低，缓 |
| D6 | HOME 路径缩写两份 | status_line.rs / welcome_card.rs | 合并（C5 落地后自然消掉一份） |
| D7 | 「stable+gap+tail」行索引算法两份 | markdown.rs / live_block.rs | 合并或注释对齐 |

## E. 可降档 — 保留但简化

| # | 项 | 位置 | 说明 |
|---|---|---|---|
| E1 | 重试 `max_retries=5` | ash-agent/src/agent.rs | 每次重试是完整重新生成，5 次成本高；建议 2-3 次（保留退避与安全策略） |
| E2 | bash `timeout` 参数与工具级 `DEFAULT_TOOL_TIMEOUT=120s` 双层超时 | ash-tools/src/bash.rs / ash-cli/src/modes.rs | 语义重叠，可收敛为一层 |
| E3 | 图片链路（read 附件 → 三协议 base64 转发，约 200+ 行） | ash-tools/src/read.rs + 三个协议适配器 | 保留但知成本；砍则不能读图 |
| E4 | `ASH_MODEL_CONFIG` 点号嵌套 DSL | ash-protocol/src/model_config.rs | 用户可见文档化功能；极简视角可换配置文件，需决策 |
| E5 | 摘要策略二级规则（40K/20K 预剪枝、skill 特例、2K–8K 保留预算） | ash-agent/src/context.rs | 用户已表态「摘要规则不是复杂点」——**默认不动**，仅记录位置 |

## F. 决策点（拍板后按结果执行）

| # | 问题 | 影响 |
|---|---|---|
| F1 | MCP 近期要接吗？ | B5 去留；AGENTS.md/DESIGN.md 相关表述同步 |
| F2 | chat gateway / 调度器 / 心跳场景还要吗？ | B2/B3/B4 去留（enqueue/steer/idempotency） |
| F3 | Extension 机制有真实需求吗（goals/记忆/审批/审计）？ | B1 去留；DESIGN.md 的 Extensions 章节同步 |
| F4 | `/new` 与 `/clear` 合并吗？ | D1 |
| F5 | 图片读取要保留吗？ | E3 |
| F6 | `ASH_MODEL_CONFIG` DSL 保留还是换配置文件？ | E4 |
| F7 | 重试次数降到几次？ | E1（建议 2-3） |

## G. 分步执行计划

每步完成即提交，保证基线可回退。

- **Step 1**：删除 A 类死代码（A1–A5）。纯删除，风险最低。
- **Step 2**：删除 C1 鼠标选择+OSC52 不可达代码（~500 行）。
- ~~**Step 3**：删除 C2 Ctrl+O 展开模式~~ ✅ 2026-08-05 已改向：**保留功能**，合并 3 处重复的截断实现与 3 组 5/50 常量，修正 README 描述。
- **Step 4**：删除 C3 bash 语法高亮，去掉 syntect/two_face 依赖。
- **Step 5**：删除 C4 `/status`、C5 welcome logo（降级单行）、C6 diff 预览（守隐私）。
- **Step 6**：合并 D 类重复实现（D2/D3/D4，D5/D7 视收益）。
- **Step 7**：按 F 决策结果处理 B 类（B1–B5）。
- **Step 8**：E 类降档（E1 重试次数、E2 双层超时）。
- **Step 9**：文档同步——README 与代码行为对齐（隐藏功能删除后自然一致），DESIGN.md 删除不再成立的章节（Extensions/Collaboration 按决策调整），AGENTS.md 修正（ash-orchestrator 不存在、MCP 归属按 F1 调整）。

## 预期收益

- 代码量：Step 1–5 合计约 −1,500 ~ −2,000 行 + 2 个重依赖。
- 行为：主链路（提交 → 流式 → 工具 → 收束 → 持久化）零变化；全部改动不触碰任何被产品消费的路径。
