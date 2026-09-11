# Grok 真实模型验收

日期：2026-09-10。使用当前环境配置的 `grok` 模型别名与 OpenAI Responses 协议；不推断别名对应的供应商版本。

首轮执行 11 类场景、17 次真实模型 CLI 运行（含复测），不是 17 次底层 API 请求。首轮最后一次双组复测通过，前序失败仍保留。
随后追加订单服务的 3 次复杂场景 / 对照运行、3 次针对性复测、能力收紧后的 2 次验证及本轮 2 次验证，累计 27 次 CLI 运行。
追加结果见后文：既有真实闭环成功，也有提前退出和限时未完成，尚不能认为协作稳定。

## 测试方式

```bash
ash -m grok --protocol openai-responses --max-context-tokens=500000 run "任务"
```

角色对比额外传 `--profile default|explore|review`，Workflow 任务以 `/workflow` 开头。
测试使用当前源码重新构建的 `target/debug/ash`，通过临时 PATH 暴露为 `ash`；没有覆盖 `~/.cargo/bin/ash`。
在源码目录以外的非 Git 临时项目运行，每个案例有独立 HOME、个人记录、组织状态和共享聊天。
沿用现有连接配置，报告和仓库不保存密钥、接口地址或原始会话日志。

测试目录：`/tmp/ash-grok-eval-9EtPaCEL/`。
驱动脚本：`/tmp/ash-grok-evaluate.py`；记录分析脚本：`/tmp/ash-grok-analyze.py`。
每个案例保存 prompt、stdout/stderr、实际文件、独立 unittest 结果和 JSONL / 组织状态；临时文件不作为源码提交。

### 独立判断结果，不照抄模型结论

- 发票模块有数量遗漏、百分比未除以 100 两个缺陷，对应 4 个 unittest。
- 运费模块没有对每开始一个公斤向上取整，对应另外 4 个 unittest。
- 修复类任务由外部测试再次验证；审查类任务要求保留缺陷并给证据，不要求把测试改绿。
- 检查原始文件摘要，确认角色、组成员没有修改测试或不属于自己职责的源码。
- 检查实际 tool calls、子会话数量与轮次、group chat、最终 unread，不把模型口头承诺当完成。
- 只处理发票的案例保留运费缺陷，因此全套 8 项测试的退出码仍为非零，这是预期，不是发票修复失败。

## 已完成案例

| 案例 | 实测结果 | 耗时 |
| --- | --- | --- |
| 连通性 | 无工具调用，回复 `ASH_GROK_OK` | 单独 smoke |
| default | 仅修改 invoice.py，发票 4 项测试通过 | 39.56 秒 |
| explore | 不改源码，定位两个问题并复现 3 个失败用例 | 45.87 秒 |
| review | 不改源码，给出位置、触发输入、实际 / 期望值及审查边界 | 52.42 秒 |
| Skill 动态扩展 | explore 调用 skill → 新增 write → read，写出指定标记；原源码不变 | 45.62 秒 |
| 深层 AGENTS.md | 第一次 write 延后，下一次 write 加入目录要求的 docstring，read 验证 | 28.84 秒 |
| 独立委派复用（最新已完成复测） | 一个 review 实例，两次 message、三次 wait（含空等）、一次 list；没有多建实例 | 88.08 秒 |
| 普通串行组 | 实施者修复 → message 登记复核者 → 复核结束；parent 整组 wait 后再次复用复核者 | 188.09 秒 |
| 单组 Workflow | 动态装配两个成员；管理者实际 wait 整组；发票 4 项通过，组最终已读 | 153.22 秒 |
| 最小 Workflow 接收 | 管理者创建一组一成员，message → wait(group_id) → list；外部 parent wait 管理者 | 40.61 秒 |
| 双组 Workflow（最后复测） | 两组分别实施 / 复核，管理者实际 wait 两组并运行全部测试；8 项通过，两组已读 | 157.68 秒 |

这些是小型受控任务的单次观测，且部分案例并发运行；耗时不是稳定性能基准，不据此计算模型成功率。
原始失败和中间复测没有丢弃，见后文。双组最后复测的额外证据见本文末尾。

### 串行组与复用证据

普通组的公共记录顺序是：

```text
accepted → started → registered → completion → started → completion
accepted → started → completion
```

第一段对应实施者到复核者的接续；第二段复用同一个 review 实例。
parent 两次 `wait(group_id)`，最终 execution 为空且 unread=false；review 的个人记录包含两个完整 turn。
两名成员和 parent 都实际读取过公共 history，而不是复制同一份个人 transcript。

最小 Workflow 中，管理者只调用一次 skill、workflow、message、wait、list，外部 parent 只调用一次 wait。
成员不调用工具，只回复指定标记；管理者等待的是自己拥有的 group，而非自身所属的 group。

## 暴露的问题及调整

### 1. 可选 ID 的 schema 表达不利于填参

初版 delegation 中，模型连续提交 `wait({"agent_id":null})`；初版 Workflow 的外部 parent 还把管理者 ID
填到了 group_id。错误已经反馈，但模型仍重复错误调用，因此主动停止这两次请求。

原来的可选 SessionId schema 是：

```json
{"anyOf":[{"$ref":"#/$defs/SessionId"},{"type":"null"}]}
```

这不是非法 JSON Schema，但在本次配置下实际填参表现很差。现在给 SessionId 添加 `schemars(inline)`，
让必填 ID 暴露为 string、可选 ID 暴露为 `type:["string","null"]`，Rust 内部仍保留强类型及 UUID 反序列化校验。
新增回归覆盖 message / group / wait 的 ID schema，不修改运行时目标校验或增加工具。

后续独立委派和单组 Workflow 已成功调用 wait。不能据此断言所有模型或所有 `$ref` 都有问题。

### 2. 创建成功却返回 accepted=false 容易误解

无 prompt 的创建实际成功，但原回执同时出现 created=true、accepted=false。一次复测中模型多创建了一个
未运行的 review 实例。现在回执明确使用 created=true、started=false，创建与提交工作不再混在一个 accepted 字段中。
配套说明要求保存实例 ID，不因为尚未运行就重复创建；相关回归同时检查没有旧 accepted 字段。

最新委派复测的调用序列恰好是：

```text
agent → wait(空结果) → message → wait → message → wait → list
```

### 3. “自己的组”存在角色歧义

共同提示词曾写“不能等待自己的组”。这对正在执行的组员成立，但对拥有下级组的管理者不成立。
已改为明确区分：

- 组员不能等待自己作为成员所属的组，否则等到自己，产生自锁。
- 管理者可以并且应当等待自己拥有的组。

同时强调 history/list 只是查看，不是接收；即使已经 idle 且聊天中有答案，只要 unread=true 就还需要 wait。
没有自动恢复当前会话的完成通知，message 回执也不是完成结果。

Workflow 的普通完成规则改为先接收所有已委派工作，再向 parent 汇总；只有 parent 明确要求提前交回时才返回未完成工作。
外部 launcher 的协调正文移入 ash-workflow，CLI 只调用 `manager_input`，不再维护这段模块业务提示词。

### 4. 复杂任务出现提前结束和空转，不能只靠测试变绿验收

一次双组任务修复了两个模块，外部 8 项测试通过，但管理者没有调用 wait，两个组仍 unread=true。
一次后续复测中，外部 parent 不断要求管理者继续，管理者多次宣布“将等待”却结束 turn；测试驱动在 420 秒停止它。
该次代码仍正确，但协作闭环失败，不能标成通过。

另一次独立委派只创建实例就结束了，进程退出码仍为 0。这也按场景失败记录，而不是按退出码判断成功。
因此需要同时看业务结果和真实协作轨迹，不能因为模型说“完成”或测试全绿就忽略未接收结果。

为区分模型没有发工具调用与适配器漏解析，Responses 完成事件新增 debug 级结构记录，只记 output 类型和工具名，
不记录思考、正文、工具参数或认证信息。诊断轮的实际工具调用与完成结构可对照；这不是对所有失败原因的证明。

### 5. 环境探测有可见开销

若干案例先尝试不存在的 python，随后改用 python3；复核者也尝试过非 Git 目录下的 git diff。
这些操作如实报错，模型能继续，但多消耗工具调用和模型请求。没有为通过测试安装依赖、增加全局 alias 或创建 Git 仓库。

## 当前边界

- 三种基础角色、能力扩展、目录规则、独立会话复用和单组协作已经有真实模型通过样本。
- 复杂双组存在明确的失败样本；首轮澄清提示词后的复测通过，但随后更复杂订单任务再次失败，不能承诺稳定成功率。
- 强化提示词不是新增业务状态机，runtime 不根据 TODO、消息内容或测试结果自动推进工作。
- 未测试交互 TUI 的 Esc、真实终端展示、强杀后 resume、磁盘故障组合，也未验证大规模真实仓库任务。
- 500000 是测试使用的上下文配置，不表示本次把窗口填满，也不证明 KV cache 命中率或费用表现。
- 对一两个局部修改优先使用 default 或一个子 agent；只有确实需要独立复核、共享讨论或多线工作时才使用 group/workflow。
- 已有实例恢复时保留创建时的角色 / 说明快照；需要新建会话与管理者来试用本次提示词更新。

## 首轮最后双组复测

案例 `workflow-parallel-verified` 使用澄清“成员所属组 / 管理者拥有组”后的提示词和同一双模块任务，157.68 秒完成：

- 一个外部 default、一个 workflow 管理者、两个 default 实施者、两个 review 复核者，共 6 个实际会话。
- 管理者一次装配生成两组，随后给两组发送任务；每个实施者登记本组复核者接续，没有跨组修改文件。
- 管理者在同一个 turn 中实际调用两次 wait，各自使用正确 group_id；最终 list 显示两组 execution 为空、unread=false。
- 管理者运行全部 unittest，外部再次运行确认 8 项通过；仅 invoice.py 和 shipping.py 发生修改。
- 外部 default 只调用一次 wait(agent_id=管理者)，没有越级查询组或替管理者实施，也没有反复催办。
- 个人记录统计为 44 次工具调用、167514 input tokens、8338 output tokens。这里的 input 含多次请求的重复上下文，不等于独立上下文大小或计费成本。

该结果支持继续沿用当前最小原语，不需要为本次失败增加状态机、自动推进工具或工作流语言。
重要调整是叶子 ID schema、创建回执含义、成员与 owner 的区别，以及查看与接收的区别。

代码回归：`cargo test --workspace`（491 个）、Clippy `-D warnings`、rustfmt 检查及 `git diff --check`。

## 追加：跨模块订单服务

实测时间：2026-09-10 夜间至 2026-09-11 凌晨，Asia/Shanghai。本节使用相同 Grok 参数与当前编译产物，
没有为通过实验修改 Ash 的运行逻辑、角色提示词或工具 schema。只新增临时测试夹具及本报告 / TODO 记录。

### 任务与验收

不是大规模真实仓库，而是专门构造的、无外部依赖的 Python 小型订单服务，涉及三个模块：

- `pricing.py`：按数量计价、整单折扣 half-up、超大整数精度、严格参数校验、输入不变。
- `inventory.py`：重复 SKU 汇总、库存不足和后续无效行的原子拒绝、返回新库存副本。
- `checkout.py`：组合两个模块、JSON 持久化、重启恢复、幂等重试、同 key 不同请求拒绝、失败不占用 key、防御性拷贝。

预先提供完整 SPEC 和 30 个 unittest 方法（计价 8、库存 8、集成 14，部分包含 subtests）。
三次都从相同错误代码重新开始；初始测试输出为 failures=42、errors=17，这是包含 subtests 的计数，不是 59 个测试方法。
所有原有文件都有 SHA-256 基线，外部驱动独立复跑测试，不使用模型自己的报告作为通过依据。
并发进程写入、崩溃恢复、任意磁盘 IO 故障不在这个业务夹具的契约内。

Workflow 目标组织为两个可并行组，每组实施者 → 复核者，以及一个独立集成者；先完成两个模块，再集成，
最后复用两个 review 会话审查跨模块边界。只有真实发现缺陷才返工，不为演示人为制造审查问题。

### 三次结果

| 运行 | 耗时 | 代码验收 | 协作结果 |
| --- | --- | --- | --- |
| Workflow，详细指令 | 285.29 秒后由测试者停止 | 计价 / 库存各 8 项通过；checkout 未修改，整套失败 | 两组确实并行，组内实施 → 复核成功；管理者不接收组结果，集成者未启动 |
| Workflow，精简指令对照 | 18.19 秒 | 没有文件修改，整套仍为初始失败 | 外部主 agent 只说正在等待，未调用任何工具；正常退出时取消尚在运行的管理者 |
| 单 default 直接实施对照 | 181.97 秒 | 30 项全部通过；另有 100 组固定种子随机组合全部通过 | 无委派；仅改三个目标源码，测试 / 规范 / 规则未改 |

单代理对照只取消组织方式要求，业务契约和测试保持不变；它不是“两组 Workflow 修复成功”的替代证据。
随机组合使用 seed=20260910，覆盖重复 SKU、大整数计价、重启后重试、忽略额外键、冲突拒绝、失败后重用 key 和结果隔离；
验收脚本放在项目外，未提供给被测模型。100 组组合也不等于独立的 100 次模型运行。

### 实际执行轨迹

详细指令运行中，一份蓝图正确创建 5 个未启动工作实例。连同外部 default 和 workflow 管理者，共 7 个实例，
其中集成者始终未启动，只有 6 个实例产生个人执行记录。

- 外部状态采样观察到两组同时 running；各组聊天均为 `accepted → started → registered → completion → started → completion`。
- 两个 review 都实际调用 history 并独立跑各自模块测试；没有越界修改 checkout 或测试文件。
- 管理者结束了 7 个 turn，但实际工具轨迹只有两次工作下发，没有任何 wait，也没有集成任务下发。
- 外部主 agent 实际 wait 管理者 7 次、发送继续消息 6 次，仍未推动管理者接收组结果。
- 最终两组都 idle 且 unread=true；管理者已经通过 list 看见该状态，仍只回复“正在并行 wait 两个组”。
- 识别到重复空转后测试者终止进程，退出码为 -15；不是碰到 720 秒上限，也不是业务成功。

精简指令运行则更早失败：外部主 agent 没有调用 wait 就输出等待中的文本，CLI 返回 0；
管理者只来得及读取 skill / SPEC / 项目结构，随后因根会话关闭被取消，尚未创建蓝图。
这再次说明进程退出码 0 和 turn 正常结束均不能证明任务完成。

单代理对照实际读取代码、先跑失败测试、修改三个模块，再跑全套测试；共 16 次工具调用。
它在输出前也清理了自己初稿中不必要的异常处理。外部验收确认最终代码符合本夹具契约，
但不因此宣称满足生产级存储可靠性；夹具明确排除了任意 IO 故障和并发写入。

### 结论与下一步

此次能确认双组并行、组内转交、共享 history 和叶子实现可以工作，但无法确认多阶段集成、复用审查和返工闭环：
两个 Workflow 样本都没有进入这些阶段。不能把计划中的步骤写成已验证结果。

失败直接表现为模型以文字承诺代替 wait 工具调用，而非 wait 已调用后卡住；完成事件结构日志也出现只有 message、没有 function_call 的响应。
当前证据不支持把问题归因于 UUID 校验或工具执行竞态，也不足以推断模型别名对应供应商版本的普遍能力。
精简输入的一次失败不是严格的指令长度因果实验，单代理的一次成功也不是成功率统计。

优先固定两个复现场景：管理者提前返回后 parent 的重复催办，以及根会话未接收下级结果就结束。
先明确运行中 / 未读工作在收尾时如何如实诊断，再决定是否需要更强约束；不要据此新增工具、工作流语言或靠 TODO 自动推进。
本次不修改“group 无运行成员即停止”的既定语义，也不将业务测试变绿偷偷变成 runtime 的完成判据。

### 本地证据

- 详细指令：`/tmp/ash-grok-complex-lle_xktl/`，包含主动停止原因、完整状态、聊天与每秒组织状态采样。
- 精简指令：`/tmp/ash-grok-complex-30_zbnpt/`。
- 单代理对照：`/tmp/ash-grok-complex-t3qbrlxz/`，含 `verification.txt` 与 `properties.json`。
- 原始夹具：`/tmp/ash-grok-complex-fixture/`。
- 驱动 / 分析 / 随机验收：`/tmp/ash-grok-complex-run.py`、`/tmp/ash-grok-complex-inspect.py`、`/tmp/ash-grok-complex-properties.py`。

上述原始日志和生成代码均留在临时目录，不加入仓库。首轮 491 个 Rust 测试的结果沿用首轮记录；
本次没有改 Rust 代码，不把此前测试写成重新执行，只对文档改动补 `git diff --check`。

## 再测：最小收尾与单组订单对照

继续使用相同 Grok 参数，重新构建当前版本，未修改角色提示词或 Rust 运行逻辑。
这次将问题拆开：对完全相同的最小 Workflow 任务做两次全新会话测试，再用单组而非多组处理完整订单业务。
各案例独立 HOME / 项目目录，业务夹具仍从最初的错误代码开始，不继承上一轮修复结果。

### 相同最小任务的两次结果

任务只需创建一名组员和一个组，让组员回复 `WAIT_RECEIPT_OK`，管理者 wait 整组，外部 parent wait 管理者。
不涉及代码修复、组内转交、多组并行或集成步骤，因此不能把失败简单归因于任务太复杂。

| 案例 | 耗时 | 结果 |
| --- | --- | --- |
| `wait-one` | 102.82 秒 | 通过：管理者实际调用 skill / workflow / message / wait / list 各一次，parent 实际 wait 一次；组已停止且已读 |
| `wait-two` | 20.60 秒 | 失败：parent 没有调用任何工具，仅声称将等待；CLI 正常退出取消了管理者，管理者只完成一次 skill 调用，尚未装配蓝图 |

两次使用相同任务文本和二进制，未做提示词干预；全新会话的实例 ID / 临时路径自然不同，实际模型响应也不同。
观察到的成功和失败都保留，不把这两次样本解释为稳定成功率。
成功样本表明此路径上的 wait 能够工作；失败样本中没有 wait 工具调用，也不能描述成 wait 被执行器卡住。

### 已核对的结束路径

`ash-agent/src/engine.rs:248` 在 EndTurn 且没有工具调用时提交正文并结束 turn；
`ash-cli/src/modes.rs:266` 在根 turn 完成后关闭协作树。结合失败样本的工具轨迹，
本轮现象符合当前代码路径：文字上的“将等待”不会执行 wait，根 turn 结束后仍在运行的下级会被取消。
这是对现有行为的定位，不代表已经确定应该自动续跑或禁止所有提前返回；本轮不改变既定完成语义。

相关自动化本次实际重跑：`cargo test -p ash-collab wait` 的 6 项、`cargo test -p ash-workflow` 的 3 项全部通过。
它们覆盖取消等待、连续工作线、接收提交门禁、组内串行、最后成员结果和蓝图装配等底层契约，
不证明真实模型一定会选择调用 wait。

### 完整订单的单组结果

`checkout-group` 明确覆盖夹具中的多组组织规则，但保留所有业务契约和测试：
只要求一个实施者与一个 reviewer 组成串行组，实施者修复三个模块后转交复核；parent 收取整组结果，
再复用 reviewer 验证跨订单 / 重启后的历史收据与库存，最后再次收取结果。

实际运行达到预先设置的 480 秒上限后由驱动终止，记录耗时 480.01 秒，退出码 -15。
本次不能与“根 agent 没有调用 wait 就提前退出”的失败混为一谈：parent 已真实提交 wait，并一直等待组停止。

已完成且有证据的部分：

- 实施者修改三个目标模块，未改测试、规范或规则；parent 只写了 group prompt，没有替子代理实现。
- 实施者实际 message 登记 reviewer 接续；reviewer 读取 history 和代码，并真实运行了分模块及完整测试，全套 30 项通过。
- 外部驱动终止后重新运行 30 项测试，以及 seed=20260910 的 100 组随机组合，全部通过；没有额外项目文件。
- Responses 结构日志记录了 parent 的 wait function_call，终端工具事件也有 wait；这不是只有“将等待”的文字。

未完成或不符合要求的部分：

- 截止时 reviewer 已完成测试工具调用，但其 turn 尚未结束，组未收束，parent 尚未收到 wait 结果。
- 因此没有进入 reviewer 的第二轮复用验证，也没有完成 parent 的最终验收。不能用外部测试通过替代这些步骤。
- parent 首批发出了两个 `agent({})`，均创建 default，随后才显式创建 review；结果多出一个未启动 default 实例。
  这两个创建调用有不同 call ID，完成事件也包含两个 function_call，不是同一个工具调用被执行器重复执行。
- 最终组织有三个子实例而非要求的两个；额外实例没有启动，不影响当前测试结果，但不符合最小组织要求。

工具结果只在返回后持久化，所以个人 JSONL 没有已完成的 parent wait 记录，不等于没有发起 wait。
本次必须结合调用结构日志区分“未调用”与“已调用、未返回”。强制终止后的磁盘 execution 标记是中断前快照，
不是说进程退出后还有 agent 在后台继续运行；三次测试进程均已退出，本轮未测试 resume。

结论分开记录：单组业务实现通过，但本次 480 秒内没有完成协作闭环，且创建数量超出要求。
目前只能说达到本轮时间上限，不能据此推断永久死锁；没有证据声称延长时间必然成功。
最小 Workflow 的重复样本则证明提前退出并不只出现在复杂任务上。两个问题需要分别追踪，不能统一归因成 wait 的 bug。

### 本轮证据与复核

目录：`/tmp/ash-grok-focused-ziq9i1h6/`，包含 `wait-one`、`wait-two`、`checkout-group` 三个独立案例。
每个案例保存 result、analysis 和逐条验收的 verdict；单组案例另有 verification / properties。
驱动：`/tmp/ash-grok-focused.py`；判定脚本：`/tmp/ash-grok-focused-judge.py`。
判定保留全部约束，所以单组 verdict 为 false，即使业务测试和随机验收都通过。
本轮共 3 次真实模型 CLI 运行；重新构建、9 项相关 Rust 自动化测试和文档 `git diff --check` 均通过。

## 能力收紧后的验证

本节对应实际代码变更，而不是仅调整测试任务：普通子代理默认移除创建能力；叶子无协作工具，组员按关系安装交流工具；
Workflow 管理者只保留蓝图创建入口，可用 profile 清单随 workflow 工具注入。所有实例的常规工具仍按原角色定义。
wait/list 提供 pending 诊断，CLI 不再把带未收取工作结束的根 turn 静默报为正常退出；没有自动续跑或业务完成状态机。

重新构建后，使用相同 `ash -m grok --protocol openai-responses --max-context-tokens=500000` 参数执行两次：

| 案例 | 耗时 | 结果 |
| --- | --- | --- |
| 独立 explore 委派 | 22.92 秒 | 根 agent 创建并启动子代理后仍提前结束，未调用 wait；任务未完成，但新收尾检查列出目标 / owner / running，并以退出码 1 结束 |
| 最小 Workflow 组 | 47.77 秒 | 管理者 skill / workflow / message / wait / list 各一次，外部 parent wait 一次；GROUP_SCOPE_OK 已接收，组与管理者均已读，pending 为空，退出码 0 |

第二例的实际个人记录确认：

- 管理者首请求工具包含 workflow 与协调工具，没有 agent/group 创建工具，仍获得 profile 选择清单。
- default 组员的协作工具只有 message/history/list，没有 wait 或创建工具。
- 管理者最后结果未被诊断改写，parent 实际 wait 返回该结果；没有额外管理者或重复装配。

第一例的组织定义确认 explore 只有 read/glob/grep/bash/webfetch。取消发生在子代理个人执行记录形成之前，
所以它验证了已安装定义和非零退出诊断，不声称子代理已真实完成工具自检；普通子代理首请求能力另有受控模型回归。
未收取工作的快照取自根 turn 结束时，随后关闭流程取消后代；不要把该快照中的 running 理解为 CLI 退出后仍有后台进程。

结论：工具与提示词范围已按组织身份生效，最小闭环有通过样本；模型提前结束的问题仍可出现，
但现在能得到明确失败信号与未收尾事实，而不是错误的成功退出。不能把“失败被正确报告”记作“任务已完成”。

证据目录：`/tmp/ash-grok-scope-1hjou3m_/`；驱动：`/tmp/ash-grok-scope-evaluate.py`。
本次全量 498 项 Rust 测试及 Clippy / rustfmt 通过。未重跑复杂订单，也未手工验证 TUI 或此失败样本的 resume。

## 再次验证：权限稳定，父级收尾仍有概率提前

2026-09-11 重新构建后，再次运行相同的 leaf / workflow scope 验收，仍使用完整参数：
`ash -m grok --protocol openai-responses --max-context-tokens=500000`。

- `leaf` 24.82 秒通过。外部 default 创建 explore 后实际 wait、list；被创建的 explore 没有产生工具调用，
  但其保存定义只含 `read/glob/grep/bash/webfetch`，没有任何 collab 工具。说明普通叶子能力收紧保持生效。
- `workflow` 24.16 秒失败，但失败原因比上一轮更具体：manager 实际调用 workflow、message、wait，
  group 最终 `running=false/unread=false`，wait 返回 `GROUP_SCOPE_OK`。外部 parent 没有再调用 wait，
  只输出“Manager is already running...”，根收尾检查发现 manager 仍在运行，于是关闭并取消 manager，退出码 1。

本次没有看到组本身遗留工作；遗留的是 manager 会话本身。也就是说，`wait(group_id)` 成功并不等于
manager 当前 turn 已经完成，manager 还需要在拿到工具结果后的下一轮生成最终答复，外部 parent 必须继续
`wait(agent_id=manager)`。新收尾逻辑正确识别并报告了这一层，不能把它算作 Workflow 完成，但也不能误报为 group wait 失效。
这进一步支持保持两层完成语义：组等待组，parent 等管理者；不把 runtime 自动续跑 manager，也不把 group 结果自动转发成 parent 的最终结果。

本轮自动回归保持通过：目标 crate 测试、全量 498 项、Clippy、格式和 diff 检查均通过。新证据目录为
`/tmp/ash-grok-scope-qml1ta7x/`；前一轮复杂订单仍按已有结果记录，没有冒充本轮重跑。
