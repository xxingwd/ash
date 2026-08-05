# Ash 代码简化与可读性优化 TODO

> 生成日期：2026-08-04
> 范围：对 `crates/` 下全部 8 个 crate 的人工 review 结论，聚焦「减少魔法判断、无用状态、重复样板，用标准数据流转做优雅处理，命名简单易读」。

## 背景

对代码库做了全量人工 review（逐文件通读 + `cargo clippy --workspace --all-targets` 零告警确认），目的是找出**小而安全**的简化点。整体结论：

- 代码库本身已经很精简，模块分层与 `DESIGN.md` 一致，clippy 零告警。
- 「无用状态」这一类基本是空的：没有重复布尔标志、死字段、冗余 Option。
- 真正的价值集中在两类：
  1. **消除漂移风险**——复制粘贴的代码将来必然不同步（协议翻译、timeout 解析、投影谓词、header 转换）。
  2. **消除误导性代码**——死分支、不可达状态、不对称守卫。
- 全部改动合计约省 100~150 行（占 ~2.5 万行的 0.5%），行数不是重点，重点是消除重复与误导。

**原则**（写在这里防止执行时跑偏）：
- 只做局部、低风险改动，不做大重构。
- 函数式风格要克制：`or()` / `map_or` / zip 链不自动等于更可读，凡是「平替甚至更绕」的一律不做（见文末「明确不做」清单）。
- 每批改完跑 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`。

---

## 复核记录（2026-08-04，逐项对照当前代码后更新）

> TODO 生成（14:33）之后，一批条目已被落地（相关文件 mtime 14:43–15:08 晚于 TODO）。
> 本记录区分「已完成 / 待做 / 决定」，正文保留原描述仅作历史记录，执行时以本记录为准。

- ✅ **已落地**：#1、#2、#3（改法）、#5、#7、#8、#10、#11。
- ✅ **#3 验证补齐**：守卫为无条件 `if entries.is_empty()`，且已覆盖 `create(metadata, &[])` 不创建文件的回归测试。
- 📌 **#5B 位置迁移**：engine.rs 的双重 match 已不存在；残余为 thread.rs `submit_inputs` 两处 `match &result`（:706 取 usage、:722 取 result 字段），与 #20 同文件，可随 #20 一并处理。
- 🔧 **#9 修正**：「QueuedTurn 构造 ×4」不实——实际 2 处（submit/enqueue）；空输入文案实为 4 种（含 `"a turn requires at least one input"`，thread.rs:645）。
- 🔧 **#12 修正**：「`result.as_ref().map_or_else(...)` 各出现两次」不实——各 1 次（completions.rs:91、responses.rs:82）。
- 🔧 **#16 修正**：决定**不合并** `MAX_MATCHES`/`MAX_RESULTS`（grep 限制匹配行数、glob 限制结果路径数，语义不同）；其余子项照做。
- 🔧 **#18 决定**：**不做**（3 变体手写直白；strum 改变 trim/错误文案语义，且为 ash-collab 新增依赖）。
- ❌ **#19 决定**：**不做**。工具描述属于发给模型的文本契约；为同步几个限制常量而扩大 `ash-core` 公共接口和多工具改动面，收益不足。
- 📌 **行号提示**：正文多处行号基于生成时快照，已随代码演进偏移（如 responses.rs:376→424、model_config.rs:103-120→92-104），以当前代码为准。

## 实施完成记录（2026-08-04）

- ✅ **已完成**：#1–#17、#20；#3 的空 `create` 回归测试已补齐。
- ✅ **#20 已完成**：`ThreadLog` 由单一 `Projector` 增量维护投影，rollback 使用边界快照恢复；线程完成事件从同一投影取得 `TurnView`，并有旧规则差分、连续 rollback、checkpoint rollback、序列化恢复测试。
- ✅ **全量收敛**：OpenAI 工具结果分组、SSE decoder 收尾、JSONL header/旧文件名、请求应答样板、错误文案与低风险常量项均已按条目实施。
- ❌ **保持不做**：#18、#19；grep/glob 的上限仍保留各自语义常量，不合并。

---

## 第一批：零风险快赢（✅ 已完成）

### 1. `crates/ash-tui/src/ansi.rs`（约 91-96 行）— 删除死分支（✅ 已落地：当前为单分支 + `ANSI_ALPHA_DEFAULT`，`OPAQUE_ALPHA` 已不存在）

- **现状**：`if fg.a == OPAQUE_ALPHA` 的两个分支返回**完全相同**的 `Color::Rgb(...)`。
- **问题**：误导性代码，读者会以为 alpha 参与判断，实际没有。
- **改法**：删掉 if/else 只留一个表达式；`OPAQUE_ALPHA` 常量一并删除。
- **验证**：TUI 渲染测试 + clippy。

### 2. `crates/ash-tui/src/inline.rs`（107-121 行）— 合并相同方法体（✅ 已落地：私有 `set_estimated_tokens` 已抽出，两个公共方法均已委托）

- **现状**：`set_compacted_context` 与 `set_estimated_context` 方法体完全相同。
- **问题**：两个公共语义名指向同一实现，将来改一处漏一处。
- **改法**：统一到一个实现，两个方法都调用它（保留语义名，因为调用点语义不同）。
- **验证**：现有 inline 渲染测试。

### 3. `crates/ash-agent/src/jsonl.rs`（约 252 行）— 修正不对称的空追加守卫（✅ 已完成，含回归测试）

- **现状**：`if entries.is_empty() && self.file.is_some() { return Ok(()); }`
- **问题**：`ThreadStore::create(metadata, &[])` 是公开 API；新 writer（`file.is_none()`）收到空 entries 时**不会提前返回**，会写出一个只有 header 的线程文件。该文件在列表里不可见（`read_summary` 因无 title 返回 None），属于无效写入。
- **改法**：`if entries.is_empty() { return Ok(()); }` —— 意图就是「空追加一律无事发生」。
- **验证**：已补 `create` 传空 entries 的回归测试。

### 4. `crates/ash-agent/src/thread.rs`（168-256 行）— 收敛 7 处「请求-应答」样板（✅ 已完成）

- **现状**：`messages / view / fork_points / rollback / fork_at / compact / steer` 每处都是同样的 4 行：
  ```rust
  let (reply, result) = oneshot::channel();
  self.commands.send(Command::X(reply)).await.map_err(|_| thread_closed())?;
  result.await.map_err(|_| thread_closed())
  ```
- **问题**：7 份复制，命令协议改动要同步 7 处。
- **改法**：抽泛型助手，每个方法缩成 1 行：
  ```rust
  async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Command)
      -> Result<T, ash_core::AshError>
  {
      let (reply, result) = oneshot::channel();
      self.commands.send(make(reply)).await.map_err(|_| thread_closed())?;
      result.await.map_err(|_| thread_closed())
  }
  // 调用：self.ask(|reply| Command::Fork { message_id, reply }).await
  ```
- **验证**：`crates/ash-agent` 现有测试已覆盖所有 7 个方法。

### 5. `crates/ash-agent/src/engine.rs`（280-296、406-423 行）— 去掉 `mut stop_reason`、合并双重 match（✅ A 已落地；B 残余迁至 thread.rs）

- **现状 A**：`let mut stop_reason = None; match ... { 赋值 }` 先声明后赋值。
- **改法 A**：match 直接产出 `Option<StopReason>`（`Failed` 分支提前 return），去掉 `mut`。
- **现状 B（已迁移）**：该双重 match 已不在 engine.rs；残余为 thread.rs `submit_inputs` 两处 `match &result`（:706 取 usage、:722 取 result 字段），`error.to_string()` 现仅计算一次。
- **改法 B**：一次 match 同时产出 usage 与 result 字段；与 #20 同文件相邻，建议随 #20 一并处理。
- **验证**：`run_agent_turn_persisted` 相关测试。

### 6. `crates/ash-protocol/src/sse.rs`（9-38 行）— `DecodeResult` 去掉换皮构造器（✅ 已完成）

- **现状**：`continuing()` / `finished()` 只是 `Self::Continue / Self::Finished` 的换皮；三个 decoder 收尾各自写 `Ok(if finished { sse::DecodeResult::finished(items) } else { sse::DecodeResult::continuing(items) })`（`anthropic.rs:302`、`responses.rs:424`，`completions.rs` 散落 4 处）。
- **改法**：删两个换皮构造器，加 `DecodeResult::finish(items, finished)`，三处收尾各变一行。
- **验证**：协议解码测试。

### 7. `crates/ash-tools/src/bash.rs`（82-88 行）— 超时文案回头重算（✅ 已落地：bash.rs:67-68 已用 `timeout.as_secs_f64()`）

- **现状**：`timeout`（`Duration`，`Copy`）已算好，报错却写 `args.timeout.unwrap_or_default()` 再推导一遍。
- **改法**：`format!("command timed out after {} seconds", timeout.as_secs_f64())`。
- **验证**：bash 工具超时测试。

### 8. 杂项局部整理（各自独立、各自验证）（✅ tui 三处 + mcp 两处已落地；log 部分并入 #20）

| 位置 | 改法 | 状态 |
|---|---|---|
| `crates/ash-tui/src/status_line.rs`（44-56 行） | 两个返回相同元组的分支合并为一个守卫 | ✅ 已落地（status_line.rs:50-52 单一守卫） |
| `crates/ash-tui/src/input.rs`（177-193 行） | `history_next` 两分支重复的 3 行状态重置提到公共尾部 | ✅ 已落地（input.rs:191-192 公共尾部） |
| `crates/ash-tui/src/markdown.rs`（552-570 行） | 手写「去首尾空行 + 折叠连续空行」循环 → 抽清晰 helper（不硬写成密集 fold；现无 `remove(0)`） | ✅ 已落地（markdown.rs:561-580 单遍 helper） |
| `crates/ash-agent/src/mcp.rs`（116、158 行） | 手写 env 循环 → `cmd.envs(env)`；`as Arc<dyn Tool>` 冗余强转 → 删 | ✅ 已落地（mcp.rs:115 `cmd.envs(env)`；强转不存在） |
| `crates/ash-agent/src/log.rs`（93-158 行） | ~~两处投影共享 partial 判定 → 抽 `is_partial`~~ 当前已是单投影单判定，无「两处复用」前提 | 并入 #20：投影重写后该判定自然消失，不单独做 |

---

## 第二批：高价值（需要同步测试/小范围调用点调整）

### 9. `crates/ash-agent/src/thread.rs` — 统一空输入与 inactive 文案（✅ 已完成）

- **现状**：空输入 3 种文案（`"thread input cannot be empty"` ×3、`"steering input cannot be empty"`、actor 侧 `"agent input content cannot be empty"`）；inactive 2 种（`"the target turn is no longer active"` / `"the target turn is not active"`）。
- **问题**：同一错误多种说法，用户困惑、测试脆弱。
- **改法**：统一为各一个文案；`submit/enqueue/notify/steer` 的 `QueuedTurn` 构造重复 4 次 → 抽 `queued_turn(input, completion)` 助手。
  - 🔧 **修正（复核记录）**：`QueuedTurn` 构造实为 **2 处**（submit/enqueue，thread.rs:115/142），notify/steer 走不同命令，助手只能覆盖前两处；空输入文案实为 **4 种**（`thread input cannot be empty` ×3、`steering input cannot be empty`、`agent input content cannot be empty`、`a turn requires at least one input`）。
- **验证**：搜索所有引用并跑 agent 测试。

### 10. `crates/ash-tools/src/lib.rs`（13-45 行）— `tools()` 双重过滤（✅ 已落地：lib.rs:16-30 为 HashSet 版，O(n+m) 单次筛选，并带未知名/显式选择测试）

- **现状**：每个 name 全量扫描 tools 两次（一次判未知、一次筛启用），还先攒一个 Vec 只为判空，O(n²)。
- **改法**：
  ```rust
  let known: HashSet<&str> = tools.iter().map(|t| t.name()).collect();
  let unknown: Vec<_> = names.iter().filter(|n| !known.contains(n.as_str())).collect();
  if !unknown.is_empty() { /* 报错 */ }
  let enabled: HashSet<&str> = names.iter().map(String::as_str).collect();
  Ok(tools.into_iter().filter(|t| enabled.contains(t.name())).collect())
  ```
- **验证**：tools 工具测试。

### 11. `crates/ash-tools/src/bash.rs`（113-121 行）与 `webfetch.rs`（64-76 行）— 共享 `parse_timeout`（✅ 已落地：`crates/ash-tools/src/timeout.rs` 存在，两工具均调用 `parse_positive_seconds`；webfetch 保留 DEFAULT_TIMEOUT/MAX_TIMEOUT 包装）

- **现状**：两个文件各自实现 `parse_timeout`，校验逻辑和错误文案（`"timeout must be a positive finite number of seconds"` / `"timeout is too large"`）逐字相同。
- **问题**：改一边漏一边的典型漂移点。
- **改法**：抽 `crates/ash-tools/src/timeout.rs` 共享核心解析；`webfetch` 保留自己的 default/cap 包装（`DEFAULT_TIMEOUT`/`MAX_TIMEOUT` 是 webfetch 特有）。
- **验证**：两个工具的 timeout 测试。

### 12. `crates/ash-protocol/src/completions.rs`（63-101 行）与 `responses.rs`（62-101 行）— 共享 ToolResult 分组（✅ 已完成）

- **现状**：两个 OpenAI 协议 adapter 里的「连续 ToolResult 消息分组 + attachments 收集」循环几乎逐行重复（仅生成的 json 形状不同），`result.as_ref().map_or_else(...)` 也各出现两次。
  - 🔧 **修正（复核记录）**：`map_or_else` 实为各 **1 次**（completions.rs:91、responses.rs:82）；循环重复本身属实，是协议层最大漂移点。
- **问题**：**两个协议翻译必须同步**，这是全库最大的漂移风险点。
- **改法**：抽共享的 run 分组辅助（遍历 `&[Message]`，对连续 ToolResult 产出 `(id, result, attachments)` 元组序列），两个 adapter 各自消费。
- **验证**：两个协议的请求构建测试。

### 13. `crates/ash-protocol/src/model_config.rs`（103-120 行）— `parse_value` 早退改写（✅ 已完成）

- **现状**：`.ok_or(()).unwrap_or_else(...)` 链式绕路，且 `f64` 分支要求 `Number::from_f64` 成功，语义藏得深。
- **改法**：三个 `if let Ok(n) = ... { return ... }` 早退，最后 fallback 字符串。语义完全等价（i64 优先、u64、f64）。
- **验证**：model_config 测试。

### 14. `crates/ash-protocol/src/responses.rs`（270-279 行）— `item_key` 改 if-let（✅ 已完成）

- **现状**：`.map(Ok).unwrap_or_else(...)` 两层嵌套。
- **改法**：直接 if-let。等价且直白。

### 15. `crates/ash-agent/src/jsonl.rs` — header 转换 4 处收敛 + 遗留文件名判断共享（✅ 已完成）

- **现状**：`StoredHeader` 构造散落 4 处（`Replay::apply` 的 `ThreadHeader`/`LegacyThreadMeta` 两分支 + `read_summary` 两分支）；`is_thread_file` 与 `legacy_filename_matches` 里的遗留文件名前缀判断（`thread-`/`session-` + `.jsonl`）重复。
- **改法**：抽 `stored_header()`（接收 record + timestamp，返回 `Option<StoredHeader>`）；抽 `is_legacy_thread_name(name) -> bool` 供两处复用。
- **验证**：jsonl 回放/摘要测试。

### 16. 命名常量（魔法数字自文档化）（✅ 已完成；grep/glob 各自保留语义常量）

| 位置 | 现状 | 改法 |
|---|---|---|
| `crates/ash-tools/src/path.rs`（约 101 行） | `for _ in 0..100` 临时文件重试次数 | `const MAX_TEMP_FILE_ATTEMPTS: usize = 100`，错误文案引用它 |
| `crates/ash-tools/src/grep.rs`（16 行）/`glob.rs` | `MAX_MATCHES`/`MAX_RESULTS` 都是 100，各自定义 | 🔧 **决定不合并**：grep 限制匹配行数、glob 限制结果路径数，语义不同，各自保留自文档化常量（合并成共享 `MAX_RESULTS` 会让 grep 侧命名变模糊） |
| `crates/ash-cli/src/modes.rs`（76、112、117 行） | 默认模型串、`max_turns: 100`、`Duration::from_secs(120)` | 命名常量，与已有 `DEFAULT_MAX_CONTEXT_TOKENS` 风格统一 |
| `crates/ash-protocol/src/sse.rs`（66-73 行） | 裸状态码 `401 \| 403`、`429` | `reqwest::StatusCode::UNAUTHORIZED` 等关联常量 |
| `crates/ash-agent/src/context.rs`（243-262 行） | `position == 0` 表达「最新一轮」（魔法序号） | `.rev().next()` 显式取出最新一轮，循环内去掉序号判断 |
| `crates/ash-tui/src/viewport.rs`（351-369 行） | 两个 5 元素数组是手工维护的优先级顺序 | 提为 `SHRINK_ORDER`/`REMOVE_ORDER` 常量并注释「顺序即优先级」 |
| `crates/ash-tui/src/app.rs`（约 520-530 行） | Ctrl-C 嵌套 if，状态判断晦涩（当前 app.rs:546-555） | 拆成两个 match 守卫：输入非空 → 清空；空输入 → 再按 busy/exit 分支 |

---

## 第三批：有取舍（改动面广或行为有细微差异，建议单独评估后做）

### 17. `crates/ash-collab/src/control.rs`（189、684 行）— `spawner` 必填化（✅ 已完成）

- **背景**：`spawner: Option<Arc<dyn AgentSpawner>>`，生产路径必传 `Some`（`install_subagent_tools` → `with_spawner`），`None` 只会产生永远无法恢复的运行时错误 `"sub-agent spawner is not configured"`。
- **取舍**：改成必填字段需要同步处理 3 处测试用的 `AgentControl::default()`（control.rs 1344/1469/1502），以及公开 API `AgentControl::new` 的语义。
- **结论**：值得做（让「没有 spawner 就不能 spawn」成为类型层面的事实），但要连带改测试构造器，放最后。

### 18. `crates/ash-collab/src/control.rs`（62-68、114-121 行）— `AgentRole` 用 strum 派生（❌ 决定不做）

- **背景**：`Default/Explorer/Worker` 的字符串映射写了三遍：手写 `FromStr` + `name()` + `serde rename_all = "lowercase"`。
- **取舍**：① 手写版有 `value.trim()`，strum 的 `EnumString` 不 trim（行为差异）；② 错误文案 `unknown agent_type '{other}'` 会变（strum 版为 `no variant found...`）；③ `AgentRole` 的序列化断言可能变化。
- **结论**：❌ **不做**（复核决定）。当前手写 3 个 role 很直白；strum 会改变 `trim`/错误文案语义（`unknown agent_type '{other}'` → 默认 ParseError 文案），且为 ash-collab 新增依赖（workspace 已有 strum 0.28，但该 crate 未引用）。除非 role 快速扩展，否则保留现状。

### 19. 工具描述/截断文案常量化（`ash-tools` + `ash-core`）（❌ 决定不做）

- **背景**：工具描述里写死数字：`bash.rs:28` "2000 lines or 50KB"、`grep.rs:53/164` "100"、`glob.rs:28/84` "100"、`webfetch.rs:46` "5MB"。这些与 `truncate.rs` 的 `DEFAULT_MAX_LINES`/`DEFAULT_MAX_BYTES`、`MAX_RESULTS`、`MAX_RESPONSE_BYTES` 脱钩，改常量后描述会漂移。
- **取舍**：需要把 `define_tool` 的 `description: &str` 改成 `impl Into<String>`，波及 `ash-core` 签名与所有调用点；且**工具描述是发给模型的文本**，属于序列化契约，改前要确认没有测试断言精确描述。
- **结论**：❌ **不做**。价值中、波及面广，而且会改动模型可见文本的构造方式；保留当前字面量。

---

## 明确不做（等价改写或更绕，写在这里防止反复）

| 项 | 原因 |
|---|---|
| `app.rs` 两个 picker 处理器泛化 | 为 2 个调用点引入带 2~3 个闭包参数的泛型函数，可读性大概率变差；35 行重复在两个几乎相同的处理器场景下是可接受的显式 |
| `scrollback.rs` `wrap_text` 与 `input.rs` `wrap_input` 共享 | 需求不同：一个纯文本换行，一个要 char index + 光标定位；强行抽象要加开关参数，得不偿失 |
| `viewport.rs` `ScreenRegion`/`ScreenPart` 合并 | `layout_screen` 的 `Composer if rows.menu > 0` 守卫依赖两者分工，合并有真实风险；加注释说明分工即可 |
| `operation.rs` `is_busy` | 3 变体枚举用 `matches!` 就是 Rust 惯用写法，没有可简化空间 |
| `welcome_card` Title/Logo 样式合并 | 两者语义不同（标题文字 vs 艺术字），样式相同是巧合，保留 |
| `runtime.rs` `first_error` 改 `.or()` | 当前写法已直白，`.or()` 版更密但没更清楚 |
| `context.rs` `user_turns` 改 zip 版 | `zip(skip(1).chain(once(len)))` 并不比占位符回填更易懂 |
| `engine.rs` 双重 match 之外的 match 改写 | ~~现在「先取 Live 字段、再取持久化字段」两阶段有语义分工，不算坏代码~~ 🔧 该双重 match 已不存在（见 #5B 迁移说明），本条自动失效 |

---

## 执行顺序与验证（复核后更新）

实施结果：

1. **✅ 已完成**：#1–#17、#20。
2. **❌ 按决定不做**：#18、#19；grep/glob 上限常量不合并。
3. **✅ 无遗留测试项**：#3 的空 entries 回归测试已补。

每批完成后统一跑：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

#19 已决定不做，因此本轮不再改动工具描述或其构造接口；未来若重新评估，仍需检查 `crates/ash-core/tests/snapshots.rs` 与对应断言，Insta snapshot 变化需人工 review 而非机械接受。

建议落地顺序：
1. 第一批 1-8（零风险）
2. 第二批 9-16（高价值）
3. 第三批 17-19（单独评估）

> ⬆️ 以上原顺序已由「执行顺序与验证（复核后更新）」取代（见前文）。

## 第四批：架构级重构 —— LogEntry 投影收敛（方案 A）

> 来源：2026-08-04 三轮架构审查（发散 5 方案 → 收敛打分 → 落地计划）的最终结论。
> 与前三批不同：这不是局部快赢，而是唯一权威投影的「出口收敛 + 增量维护」重构，风险中低、收益最大。

### 背景

对数据流做了全量梳理：

```
产品层(CLI/TUI) → Input → Thread::submit/enqueue/notify → ActorQueues
  → ThreadState::submit_inputs → LogEntry(TurnStart+Input) 持久化
  → run_agent_turn_persisted → ModelRequest → provider SSE
  → Decoder → ModelEvent 流 → collect_response → CollectedResponse
  → Message 持久化 + LiveEvent → TurnEnd → TurnView → EventKind::Turn
投影链: LogEntry → project_entries → messages/context/turns → ThreadView → TUI
```

审查结论（三轮）：

1. **投影（reducer）是唯一权威，但「视图出口」被平行实现绕过**——`log.rs:139` 的 `project_entries` 是唯一投影实现，但 `thread.rs:720-737` 用局部变量手工装配 `TurnView`（注释明言 "capture them here instead of re-projecting"），`thread.rs:351-361` 用 `turns().pop().filter(...)` 取失败视图，靠注释维护「手工装配 == 投影输出」的不变量，新增 entry 类型时会静默失效。
2. **投影是 4 阶段 3 遍扫描**（`active_entries` → `open_turn_ids` 前瞻 → 主循环 8 状态变量 → 收尾），且 `messages()/model_context()/turns()/turn_messages()/view()`（log.rs:99-133）**每次调用都全量重投影 O(n)**，`turn_messages` 甚至是「投影全部 turns 再 find」。
3. **引擎流收集是隐式状态机**（engine.rs:470-594 的 `thought_started_at` 标志 + `finish_open_thought` 三处调用 + 四层嵌套退出判断）——已作为暂缓方案 B 记录，不并入本批。

### 为什么选它（打分与取舍）

| 方案 | 可读性 | 简单性 | 可扩展性 | 合计 | 风险/波及 |
|---|---|---|---|---|---|
| **A 投影收敛（本批）** | 4.5 | 4 | 5 | 13.5 | 低-中：仅 log.rs + thread.rs |
| B 引擎流显式状态机 | 4 | 4 | 3 | 11 | 低：仅 engine.rs |
| D 协议共享翻译助手 | 3.5 | 3.5 | 4 | 11 | 低：仅 ash-protocol |
| C 类型约束（role 派生） | 3 | 2.5 | 4.5 | 10 | 高：持久化格式、跨 5 crate |
| E Actor 命令矩阵化 | 3 | 3 | 3.5 | 9.5 | 低：仅 thread.rs |

选 A 的理由：

1. **唯一修复「视图权威性」问题的方案**。`thread.rs:720-737` 与 `thread.rs:351-361` 两处手工 `TurnView` 是投影规则的平行实现。**新增一种 LogEntry 变体：现状要改 4 处（主循环 + open_turn_ids + active_entries + thread.rs 手工装配），A 之后只改 1 处**。
2. **顺带解决真实性能问题**：5 个查询入口每次全量重投影 O(n)，A 之后读 O(1)。
3. **风险可控、改动面最小**：公共 API、JSONL 磁盘格式、事件序列全部不变，行为等价由差分测试机器证明。

### 改造前后关键对照

| 维度 | 改造前 | 改造后 |
|---|---|---|
| 投影 | `project_entries`（log.rs:139）4 阶段 3 遍扫描 + 8 个散落局部变量 | `Projector` 单遍折叠：`apply` 逐条 + 结构体状态（`open`/`boundaries`/`checkpoint` 快照） |
| 前瞻 | `open_turn_ids`（log.rs:237）预扫描判断「未结算 turn」 | 无需前瞻：`open` 缓冲 + 结算时 flush（settled → 补全部消息；未结算 → 只补用户消息，语义不变） |
| 查询 | 5 个入口每次全量重投影 O(n) | 增量缓存，读 O(1)；Rollback 走边界快照截断（同一 fold，无双实现） |
| TurnView 出口 | 两处手工装配（thread.rs:720-737、thread.rs:351-361） | 唯一出口 `log.turn_view(id)` / `log.turn_messages(id)` |
| 冗余状态 | `current_turn: Option<TurnId>` 与 `turn: Option<(TurnId, Vec<Message>)>` 冗余 | 仅 `open: Option<OpenTurn>`（id + 缓冲 + 起始下标） |
| 失败回退 | `turns().pop().filter(|v| v.id == id)`（注释承认会 pop 到上一轮视图） | `turn_view(id)` 精确按 id 查，pop 风险消失 |

### 20. 落地计划（分阶段）（✅ 已完成）

**Phase 0 —— 差分测试安全网（纯测试，不改产品代码）**

- 在 `log.rs` tests 模块把现有 `project_entries` 固化为参考实现（改名 `legacy_project_entries`）。
- 固定种子 entry 序列生成器：随机交错 `TurnStart/Input/Message/Checkpoint/TurnEnd/Rollback`（含 turn 内 checkpoint、连续 rollback、legacy 无 TurnStart 的纯 Message 序列），断言 `messages/context/turns` 与参考实现全等。

**Phase 1 —— `log.rs` 单遍折叠 + 增量缓存**

- 新增 `Projector { all, history, context, turns, open: Option<OpenTurn>, boundaries: Vec<Boundary>, checkpoint: Option<ContextCheckpoint> }`，`apply()` 六臂分发：

  ```rust
  impl Projector {
      fn apply(&mut self, e: &LogEntry) {
          match e {
              TurnStart(id) => self.open_turn(*id),          // 顺带把上一个 open 结算为 Interrupted
              Input(i)      => { self.all.push(i.message.clone()); self.open_msg(i.message.clone()); }
              Message(m)    => { self.all.push(m.clone()); self.open_msg(m.clone()); }
              Checkpoint(c) => self.rebuild_context(c),      // context = summary + all[tail..]
              TurnEnd{..}   => self.settle(e),               // 结算: 补 history/context、入 turns
              Rollback      => self.rollback(),              // 边界快照截断，与冷启动同逻辑
          }
      }
  }
  pub struct ThreadLog { entries: Vec<LogEntry>, projection: Option<ThreadProjection> }
  // push 时增量 apply；messages()/turn_view() 等全部读缓存
  ```

- `Boundary` 在 `TurnStart` 记录 `{all_len, history_len, context_len, turns_len, checkpoint}` 快照；`Rollback` = 弹快照截断 + context 从边界 checkpoint 重建；无 TurnStart 时保留 legacy 的「回退到最近 user-turn 消息」规则（log.rs:265-283）。
- `ThreadLog` 增加 `projection: Option<ThreadProjection>`：`push` 时增量 `apply`（Rollback 也走 `apply`，冷启动与增量同一逻辑），读接口在缓存为 `None` 时冷折叠。
- 公共 API 签名不变；`turn_messages(turn_id)` 改为 `turns` 中查找 + `open` 缓冲兜底。
- 删除 `active_entries` / `open_turn_ids` / `rollback_last_turn` 辅助函数。

**Phase 2 —— `thread.rs` 视图出口收敛**

- `submit_inputs`（thread.rs:640-770）：删除 `accepted_messages`（:658）、`context_before`（:696）、`turn_messages`（:712）、`view_messages`（:718）四个局部变量；`view.messages = self.log.turn_messages(turn_id)`（`result/usage` 仍由本地提供，投影不知道这两个值）；`complete_turn` 观察与 terminal 写入的顺序、失败修正逻辑保持不变。
- `run_turn` 失败路径（thread.rs:351-361）：`state.log.turn_view(id)`——prepare 失败场景 log 已含 `TurnEnd(Failed)`；什么都没写的场景返回 `None`，落到手工空 Failed 视图。

**Phase 3 —— 清理与文档**

- 确认 `turn_messages` 其余调用点（现为 O(1)，可保留为公共 API）。
- DESIGN.md 的 *one reducer direction* 一节补一句「视图唯一出口、增量维护」。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 全量通过。

### 每阶段测试

| 阶段 | 新增/修改测试 |
|---|---|
| Phase 0 | `legacy_projection_matches_reference_on_generated_corpora`（差分/属性测试，固定种子） |
| Phase 1 | `incremental_apply_matches_cold_fold`（逐条 push 的缓存结果 == 冷启动折叠）；`rollback_after_mid_turn_checkpoint_restores_context`；`rollback_without_turn_start_falls_back_to_user_turn`（legacy 规则）；现有 log.rs 测试原样通过 |
| Phase 2 | `submitted_turn_view_equals_log_projection`（含 steering + 工具调用 + 扩展 patch 的 turn：提交的 `TurnView.messages` == `log.turn_messages(id)`，且不含扩展 patch 消息）；现有 `failed_turn_that_wrote_nothing_emits_its_own_failed_view` 保持通过 |
| Phase 3 | 无新增；全量 workspace 测试 + clippy + fmt |

### 行为变化

**无变化（外部）**：事件序列、JSONL 磁盘格式、`ThreadView` 内容与顺序、全部公共 API 签名——差分测试机器证明投影输出等价。

**有变化（内部）**：

1. 查询复杂度 `messages()/model_context()/turns()/turn_messages()/view()` 从每次 O(n) 全量重投影 → O(1) 缓存读；Rollback 后首次读 O(n) 重建（比现状每次读都 O(n) 更优）。
2. `run_turn` 失败路径不再依赖 `turns().pop()` 的顺序假设——thread.rs:351-361 注释里的「pop 到上一轮视图」的坑从根上消失。
3. 内存画像：常驻一份投影缓存（消息 clone 一次），但现状每次查询都全量 clone，峰值内存反而下降。
4. `turn_messages(未结算 turn)` 的返回从「投影收尾时生成的 Interrupted 视图消息」变为「open 缓冲直读」，内容一致。

### 取舍与风险（诚实说明）

- **折叠的 flush 语义是全文最微妙处**：结算时补 `history/context` 的「checkpoint 段规则」（只补最近一次 checkpoint 重建之后的消息，避免重复）比现状的「逐条 push + 前瞻」难读。这是「单遍 + 无前瞻」的必然代价，用 Phase 0 差分测试 + 详尽注释兜底。
- **Rollback 的边界快照**（`Boundary` 含 checkpoint 引用）增加少量结构，换来「Rollback 走同一 fold、无双实现」——避免「增量路径与冷启动路径分叉」的关键决策。
- **与已有条目关系**：本方案吸收第一批 #8 的 log.rs 部分（投影重写后 `is_partial` 谓词自然消失）；与 #4（thread.rs 的 ask 助手）同文件但内容独立，可并行。若先做 #8 再做本批，log.rs 的谓词抽取会被替换，属正常迭代。

### 暂缓/不做的备选方案（供未来参考）

- **方案 B（engine.rs 流收集显式状态机）**：可作独立后续 PR，改动面不重叠。收益：`finish_open_thought` 三处调用消失；局限：新增 `ModelEvent` 仍需审计所有 `(event, phase)` 组合。
- **方案 D（协议层共享翻译助手）**：纯减负，新增第 4 个协议时复制量从约 150-200 行降到约 40 行；responses.rs:225-314 的 key 归一化是协议专属语义，无法共享。注意与第二批 #12 有重叠（ToolResult 分组），若先做 #12 则 D 的该部分被吸收。
- **方案 C（role 从消息内容派生）**：触碰已持久化 JSONL 格式与 9 处 `MessageContent` 三分支调用点，与「不修改无关代码」约束冲突，建议单独立项评估。
- **方案 E（Actor 命令矩阵化）**：矩阵宽而浅，收益最小，搁置。

---

## 当前状态（复核后更新）

- [x] 第一批 1-8（含 #3 回归测试）
- [x] 第二批 9-16（#16 grep/glob 按决定不合并）
- [x] 第三批 #17（#18、#19 按决定不做）
- [x] 第四批 20（LogEntry 投影收敛与增量维护）
- [x] 第五批 21-27（协议终止、ephemeral context、协作队列、存储能力边界、线程关闭、bash cwd 与消息投影）

> 细粒度状态见「复核记录」与各条目标题标注。

## 第五批：跨模块契约收敛（2026-08-05，已完成）

### 21. `ash-protocol` —— 分离语义终止与 wire 终止

- Chat Completions 在 `finish_reason` 后继续读取 trailing usage，并把唯一 `Stop` 放到最后。
- `[DONE]`/EOF 只表示 wire 结束；缺少 provider terminal marker 时统一产生 `Truncated`。
- Anthropic/Responses 在 clean completion 前拒绝未闭合的工具调用。
- 新增跨 chunk usage、`[DONE]`、EOF 和 unfinished tool-call 回归测试。

### 22. `ash-agent` —— ephemeral context 与 durable context 类型化分离

- `ContextRequest`/`PreparedContext` 分别携带 durable messages 和 `ephemeral_context`。
- 两者共同参与 token 预算和模型请求，自动压缩只总结并 checkpoint durable messages。
- 压缩模型必须返回 clean `EndTurn`；截断或无 terminal marker 的摘要拒绝落盘。

### 23. `ash-collab` —— follow-up 从布尔态改为精确计数

- 删除只能表达一个待办的 `RunningThenPending`。
- `pending_followups` 跟踪所有已接受但未结算的 follow-up；最后一个结束前不发布 completion revision。

### 24. `ash-agent` —— `ThreadStore` 写能力真正 storage-neutral

- 新增对象安全的 `ThreadAppender`；`open/open_writer` 返回 backend-owned append handle。
- runtime 不再依赖 JSONL `ThreadWriter`，自定义数据库/远端 store 可保留自己的事务或 lease。

### 25. `ash-agent` —— 关闭命令通道后禁用 active select 分支

- 活跃 turn 中 `commands.recv() == None` 时只取消一次并关闭该分支，避免立即就绪造成忙轮询。

### 26. `ash-tools` —— bash cwd 防 symlink escape

- canonicalize 工作区根和候选目录，再验证 canonical 前缀。
- 外部 symlink 明确拒绝，解析到工作区内部的 symlink 保持可用。

### 27. `ash-protocol` —— provider 前统一验证并投影消息语义

- 三个 adapter 共用一条 role/content 校验路径，非法组合在网络请求前返回 `InvalidRequest`。
- `Message::system` 与请求级 system 合并后进入 provider 的 system/instructions 字段，不再被 content 形状误降级为 user。
- 继续保留现有 JSONL 与 `MessageContent` 格式，避免把 provider 翻译规则泄漏到持久化层。
