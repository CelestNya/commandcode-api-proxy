# 架构审查——验证与改法账本（实时维护）

> 用途：防止上下文清空，边查边记。所有结论必须带代码证据（文件:行号）。
> 维护规则：新增事实往下追加；查出错误时把错误条目**就地更正**并标注 `[已更正]`。
> 审查对象：D:\Projects\cc-proxy\ccproxy-rust（HEAD 082ccf7 + 工作树未提交改动）
> 更新：2026-09-16（全量核验完成）

## 0. 审查范围与候选清单

- 范围：main...HEAD 27 提交 + 工作树 6 文件（crates/ccproxy-tray/src/{portguard,process,settings,win}.rs、crates/ccproxy/src/main.rs、tray/build-rust.cmd）
- 候选：A 双 handler 合并；B SseBody 8 处 dialect match；C 翻译层终局装配三文件重复；D 时间算法双份；E 死 interface；F 托盘重启策略不可测
- 验证原则：
  1. **deletion test**：删除候选目标后，复杂度若重新散落到 N 个 callers = 它在发挥价值（值得抽）；若直接消失 = 只是 pass-through（不值得）。
  2. **Two adapters = real seam**：只有出现两个真实实现时才引入 seam；A/B 都有 OpenAI/Anthropic 两个真实实现。
  3. **Golden 契约**：spec 要求逐字节行为复刻；所有改法必须行为保持，由 conformance golden（52 例）+ translate golden（52/52）+ stream golden（32/32，tests/stream_golden.rs 双 encoder 驱动）兜底。
  4. **Interface is the test surface**：测试已经穿过的接口形状 = 可安全固化的接口。

## 1. 逐项核验事实（全部已核，含逐字比对）

### A. 两个 dialect handler 并成一个生命周期

- `server.rs:397-490` handle_chat（94 行）与 `server.rs:493-596` handle_messages（104 行）同序九步，逐行平行：
  1. parse_body（413 信封不同：openai 直返 message；anthropic 分 413→api_error / 其他→invalid_request_error）
  2. validation（validate_openai_chat_request vs validate_anthropic_request）
  3. extract_api_key（401 信封不同：Unauthorized vs authentication_error + "Missing API key"）
  4. encoder_model（`server.rs:415` "default" vs `server.rs:521` ""）**[已漂移]**
  5. log_incoming / `[Anthropic] Model` 行（server.rs:523，仅 anthropic 有）**[已漂移]**
  6. thread_id + build_body（openai_to_cc vs anthropic_to_cc_with_env，两处各自闭包 + threadId insert）
  7. RequestLedger::new（Wire::Openai vs Wire::Anthropic）
  8. upstream_options + send_with_model_discovery
  9. stream → serve_sse（调用点 458 / 558）| non-stream → collect → settle_ok → record_usage → respond；失败 → end_attempt + respond_upstream_error
- id 前缀：openai 用裸 uuid，anthropic 用 `msg_{uuid}` **[已漂移]**
- 差异集合**封闭可枚举**：信封形状、validator、translator、encoder fallback、id 前缀、413 kind 映射。无顺序差异。
- `serve_sse`（server.rs:729）9 参数 + `#[allow(clippy::too_many_arguments)]`（728）
- **deletion test**：生命周期是承重逻辑（splice 接线、账本结算、model discovery 都挂在上面），它值得一个唯一归宿；两个 dialect 是真实 seam 的两个 adapter。
- **结论：成立。** 重复 ~90 行/处，且已出现 3 处分叉。

### B. SseBody 的 8 处 dialect match

- stream_body.rs 中 `match self.dialect` 出现 **8 处**：
  has_emitted_content (136-139)、can_splice_retry (150-153)、begin_continuation (157-160)、encode (164-175)、terminal (199-213)、last_usage (252-257)、publish_usage (316-319)、collect_non_streaming 的 built (375-378)
- 两个 encoder 已暴露**同形方法**：new / finished / has_emitted_content / can_splice_retry / begin_continuation / emit / terminal / last_usage
  - openai_stream.rs:42-66 字段、98-119（has_emitted_content/can_splice_retry/begin_continuation）、473-487（terminal→Vec<Value>）、395-430（usage_chunk）
  - anthropic_stream.rs:88 last_usage、111 message_id、115 finished、124 has_emitted_content、138 can_splice_retry、144 begin_continuation、150 emit、594 terminal→Vec<AnthropicRecord>
- **emit/terminal 返回类型不同**：openai→Vec\<Value\>（chunk），anthropic→Vec\<AnthropicRecord\>——归一化点在 adapter 内做 SSE 序列化（format_sse / to_sse），不改变 encoder 内部。
- **tests/stream_golden.rs:226-306 已经直接以 emit/terminal 驱动两个 encoder** → de facto interface 已被测试面穿过。
- **结论：成立。** 加第三种 dialect 须改 8 处；控制流重复分派是 082ccf7（encoders own terminal protocol）之后的残留。

### C. 翻译层终局装配三文件重复（逐字比对）

- `map_finish_reason`：nonstream.rs:11-20 ≡ openai_stream.rs:25-34 — **逐字相同**
- `map_stop_reason`：nonstream.rs:22-33 ≡ anthropic_stream.rs:27-38 — **逐字相同**
- `THINKING_SIGNATURE`：nonstream.rs:35 ≡ anthropic_stream.rs:43 — **同值常量**
- `openai_usage` 五字段装配：nonstream.rs:294-319 ≡ openai_stream.rs:395-418 — **逐字相同**（后者包 envelope）
- `error_message`：openai_stream.rs:513-521 ≡ anthropic_stream.rs:615-623 — **逐字相同**
- `canonical_arguments`（nonstream.rs:322-329）≡ `arguments_text`（anthropic_stream.rs:606-613）— **逐字相同**（String→clone；Null|None→""；其他→to_string）
- **但是** `arguments_text`（openai_stream.rs:503-510）带 `truthy` 门（`Some(v) if truthy(v)` → 序列化；否则 ""）——**同一概念三种实现、两种语义，漂移已经发生**。这是"重复无害论"的反证。
- **结论：成立，且是已发生 bug 的温床**（stream 与 non-stream 的 tool-arguments 序列化行为不同，golden 按模式采样不会抓到这个不一致）。

### D. 时间算法双份

- `now_epoch_secs`：lib.rs:57 ≡ billing.rs:869 — **逐字相同**
- Hinnant days-from-civil：lib.rs:92 `civil_from_unix`（返回全字段 tuple）与 billing.rs:895 `iso8601_from`（同算法，直接格式化无毫秒）；billing.rs:878-884 `iso8601_now` 另手动拼 `{millis:03}Z`
- **同一算法两个实现，格式已分叉**（lib 返回 tuple vs billing 拼字符串）。
- **结论：成立（小但真实）。**

### E. 死 interface

- `GenerationError`（generate.rs:139-157）：全 crate（含 tests/）无构造点、无匹配点 → 死
- `requires_key`（generate.rs:170-172）：无调用点，且对两个 variant 恒 true → 死 + 误导
- `body_object`（server.rs:619-621）：无调用点 → 死
- `anthropic_error_type`（server.rs:164-175）：**有调用点**（server.rs:804）→ 其上的 `#[allow(dead_code)]`（163）是 stale suppression，注释"used by the upstream error mapping in M4"已过时
- Accept-Encoding：build_headers（upstream.rs:96）与 attempt_send（upstream.rs:464）两处构造**同值**，attempt_send 循环里跳过 build_headers 的副本（upstream.rs:466-468）→ 三处作用同一 header
- **结论：成立。** 删除不改变任何行为。

### F. 托盘重启策略埋在闭包

- tray 六个模块有 `#[cfg(test)]`（win.rs:289、tray.rs:484、settings.rs:249、portguard.rs:158、owner.rs:94、health.rs:105），**唯独 main.rs 没有**。
- run_ui 闭包（main.rs:157-199）承载：crash→3s 延迟重启（tick 状态 Cell）、手动动作取消待重启、只允许一个 pending。
- 策略（延迟 3s / cancel 语义 / 单 pending）散落在闭包状态，**测试不可达**。
- **结论：成立。** "让崩溃代理活下去"的策略是托盘最重要的行为之一，却完全不可测。

## 2. 推荐改法（每项含收益证明）

### A 改法：抽 `handle_generation(state, req, ctx, surface: DialectSurface)`
- 形状：`DialectSurface` = 封闭差异集（wire、validator、to_cc、错误信封、encoder fallback、id 前缀）；两个真实 adapter（OpenAI/Anthropic）。
- `serve_sse` 的 9 参数收进 `RequestCtx<'a>`（Data Clumps 一并解决）。
- **收益证明**：
  - 生命周期修改点 2→1：splice 接线、账本结算、model discovery 变更目前须双改（已出现 3 处分叉 = 漏改的实害）。
  - 删除 ~90 行重复；每次需求变更的 diff 减半。
  - 测试面不变：stream_golden / translate_golden 双 dialect 双模式已覆盖；接口层测试穿过同一 seam。
  - 第三种协议 = 一个 adapter，不碰生命周期。
  - 反驳"负优化"：合并删除重复而非新增抽象；seam 有两个真实实现（two adapters = real seam）；差异集合封闭，不会变成 god function。

### B 改法：内部 `Encoder` interface，SseBody 持一个 encoder
- 形状：trait `Encoder { has_emitted_content / can_splice_retry / begin_continuation / emit→Vec<String>(SSE lines) / terminal→Vec<String> / last_usage }`；两个薄 impl 在返回值层包装 format_sse / to_sse。
- **收益证明**：
  - 8 处 match → 0；第三种 dialect 只加一个 adapter。
  - splice 门槛 / 终局 / usage 每 dialect 只测一次；tests/stream_golden.rs 现有 emit/terminal 驱动代码直接复用。
  - 控制流与编码器解耦，SseBody 的复杂逻辑（断流重发拼接）不再按 dialect 复制。
  - 不改变 golden 的 terminal 协议所有权（082ccf7 已把终局逻辑收进 encoder，这里只消掉分派残留）。

### C 改法：`translate/terminal` module + parity test
- 形状：六个 pub 函数 + `THINKING_SIGNATURE` 常量收进 translate/terminal.rs；三文件 import；新增一条 parity test 断言 stream 与 non-stream 走同一映射（堵死"只改一半"）。
- **收益证明**：
  - 改 finish-reason / tool-arguments / error-message 只动一处；**已发生的 truthy 门漂移被静态消除**（当前 openai_stream 与 anthropic/nonstream 对 `false`/`0` 的 arguments 序列化行为不同，golden 按模式采样不可见）。
  - 纯去重、零新抽象；encoder interface 变薄。
  - 反驳"重复是刻意的"：抽到共享 module 不影响函数纯度，反而让三条路径强制一致。

### D 改法：`time` module
- 形状：lib.rs 收拢 `now_epoch_secs / iso8601_now / today_utc / civil_from_unix`；billing.rs 删除本地副本（~25 行）改 import。
- **收益证明**：日期算法单一实现；时间格式单一来源（已出现 lib tuple vs billing 字符串的分叉）；可注入 fake clock 测试。

### E 改法：删除死项 + 单一头构造点
- 形状：删 GenerationError、requires_key、body_object；删 stale `#[allow(dead_code)]`（anthropic_error_type 实际在用，allow 撒谎）；Accept-Encoding 只留 build_headers:96（删 464 与 466-468）。
- **收益证明**：interface 缩小 = 读者不再为死项付理解税；无任何行为变化；"interface is the test surface"——没人调的接口不该存在。

### F 改法：`Supervision` module
- 形状：`struct Supervision { decide(tick_elapsed: Duration, manual_action: bool, crashed: bool) -> Action { RestartNow, CancelPending, Stay } }`；run_ui 闭包退化为调用其 interface。
- **收益证明**：三条规则（3s 延迟 / cancel 语义 / 单 pending）首次可 unit-test；与托盘其他模块对齐（其余六模块全有测试，main.rs 是唯一空白）；行为零变化。

## 3. 收益汇总

| 候选 | 强度 | 核心收益 | 风险 |
|---|---|---|---|
| A | Strong | 生命周期 2→1；~90 行重复消失；3 处分叉被消除 | 低（行为保持，golden 兜底） |
| B | Strong | 8→0 match；新 dialect 只加 adapter | 低（adapter 只做序列化包装） |
| C | Strong | 6×3→1；**已发生的 truthy 漂移被消除** | 低（纯去重 + parity test） |
| D | Worth exploring | 算法单一实现；格式分叉消除 | 极低（~25 行） |
| E | Speculative | interface 缩小；单一头构造点 | 零（纯删除） |
| F | Worth exploring | 重启策略首次可测 | 零（抽出不改行为） |

## 4. 交叉对照（与上一轮 code-review 报告）

- 已修复：resend（generate.rs 现直调 send_once）、账本结算规则（RequestLedger.settle_ok）、UpstreamOptions 单一构造点、splice gate 的 out_pos guard、encoders 终局协议归位（082ccf7）。
- **未触及**：C（翻译层六函数重复，全部仍在）、B（8 处 match 仍在）、D（时间双份仍在）、A（双 handler 仍在）、E（死项仍在）、F（未测策略仍在）。
- 上一轮 Standards 的 tray 硬违规（message_box bool、_uses_X 假函数、stale allow）：本轮工作树未提交改动中，win.rs/settings.rs/portguard.rs 正在重构——本报告不重复上一轮 Standards 内容，聚焦架构 deepening。

## 5. 已更正记录

- `[已更正]`（首轮核查时）：C 候选初判"canonical_arguments 与 arguments_text 是否同形待比" → 核验后结论：nonstream:322 ≡ anthropic_stream:606（逐字相同）；openai_stream:503 带 truthy 门（不同）。即**同一概念三种实现、两种语义**，属漂移实证而非简单重复——C 的严重性上调。
- `[已更正]`（首轮核查时）：E 候选初判"anthropic_error_type 死代码" → 核验后发现其有调用点（server.rs:804），死的只是它身上的 `#[allow(dead_code)]` 属性。结论改为：删属性不删函数。
- `[已更正]`（首轮核查时）：Accept-Encoding 两处构造值为同值（gzip, deflate, br），非"不同值冲突"，是冗余 + 循环跳过 → 改法仍是单一构造点，描述已修正。

## 6. TDD 落地进展（refactor/architecture-review 分支，基于 bbf124c）

- **[C] fa4672b** `refactor(translate): one terminal module shared by both paths, parity-tested`
  - 红：tests/terminal_parity.rs 漂移用例失败（stream="" vs non_stream="false"），3 条 parity 绿
  - 绿：translate/terminal.rs（6 函数 + 5 单测），三文件接线；4 条 parity + 5 单测全绿；全量测试通过
- **[D] 2ca37e2** `refactor(time): one wall-clock module, single Hinnant implementation`
  - 红：tests/time_contract.rs 编译失败（crate::time 不存在）
  - 绿：time.rs（now_epoch_secs/now_iso8601/today_utc/iso8601_secs + 7 单测），lib.rs 再导出，billing.rs 三处改 import；全量 135 通过
- **[E] 7cb5d55** `refactor(dead-code): drop never-used generation surface and header redundancy`
  - 删 generate.rs GenerationError（全 crate 无构造/匹配点）+ requires_key（恒 true、无调用）+ Dialect import
  - 删 server.rs body_object（无调用）+ anthropic_error_type 的 stale `#[allow(dead_code)]`（有调用点）
  - upstream.rs Accept-Encoding 双构造点收敛为 build_headers 单一构造点
  - 全量测试通过
- **[B] 5b8785d** `refactor(stream): one Encoder dispatch handle replaces eight dialect matches`
  - 红：tests 引用不存在的 Encoder → E0425
  - 绿：私有 Encoder<'a> 枚举（has_emitted_content/can_splice_retry/begin_continuation/emit_sse/terminal_sse/last_usage），SseBody 7 处 match 归零（仅剩 encoder() 单一分发点）；NonStreamingCollector::response 承接 collect_non_streaming 的分派
  - 新 2 测试钉分发生命周期 + reasoning-splice 契约（golden error-after-reasoning-recovered 形状）；stream_golden 19 项全绿
  - 注：测试中发现 OpenAI 在 start-only 失败后 splice 会重发 role chunk（golden 未钉此边界，两方言行为本就不一）；不属 B 修复范围，记录备查
- **[A] ed42f7a** `refactor(server): one generation pipeline via DialectSurface`
  - 红：divergence-inventory 测试引用不存在类型 → E0433
  - 绿：DialectSurface trait（dialect/wire/model_fallback/nonstream_id/validate/log_incoming/build_body/body_error_type/validation_error_type/respond_missing_key/respond_error）+ Openai/Anthropic 两 impl + handle_generation<D> 统一管线；serve_sse 9 参数 → SseRequest struct（too_many_arguments allow 移除）
  - 端到端：billing_ledger 经真实 HTTP 驱动两个端点，全绿；138 lib + 全部集成
- **[F] 0b304c4** `test(tray): extract the restart policy into a tested module`
  - 红：supervision.rs 剥离实现只留测试 → E0433 `cannot find type Supervision`（main.rs 已接 `mod supervision;`）
  - 绿：`Supervision`（cancel/tick(now,crashed)->bool 纯状态机，3s 延迟内聚）+ main.rs run_ui 闭包改为只观测崩溃并执行 I/O；3 单测：崩溃调度、到期一次性触发并消费、手动取消、待命中再崩溃重武装 deadline
  - 行为保持：同一 3s 延迟、同一单次语义、同一手动命令无条件取消；tray 34 测试全绿
- **[style] 41b5317** `style: rustfmt normalization across the review commits`
  - cargo fmt --check（DEVELOPMENT.md 门禁）标记 C/D/B/A 提交的换行与 E 删除遗留空行；纯格式无行为变更
- **最终门禁（全过）**：build --release --locked ✅ / test --locked ✅（138 lib + terminal_parity 4 + time_contract 3 + translate_env 4 + translate_golden 3 + stream_golden 19 + tray 34 全绿）/ clippy -D warnings ✅ / fmt --check ✅（conformance 为可选工具性步骤，Node 环境未验证）
- **已推送并开 PR**：`origin/refactor/architecture-review` → https://github.com/CelestNya/commandcode-api-proxy/pull/1 （base: rust-rewrite）
- **HTML 报告**：`%TEMP%\architecture-review-fixed-20260916-150304.html`（同款样式，六候选 TDD 落地 + 收益证明 + 更正记录 + PR）

## §7 PR 自审查（2026-09-16，双轴：rust-rewrite bbf124c → HEAD 41b5317，8 提交）

### Standards 轴（DEVELOPMENT.md §1–§8 + Fowler baseline）
- **无硬性违规**。新增抽象均为 two-adapters real seam（DialectSurface 两 impl、Encoder 两臂）；命名与既有 `Openai`/`Anthropic`/`Wire` 词汇一致；clippy -D warnings / fmt --check 全绿。
- LF 转换纯度已验证：settings.rs/config.rs/tests/billing_ledger.rs 的 `-w` diff 为 0 行（纯行尾，无内容篡改）；generate.rs 的 `-w` diff 仅含 E 的删除。

### Spec 轴（行为保持逐项核实）
- **A 等价**：handle_generation<D> 与两原 handler 九步逐一对照（parse/validate/key/fallback/encoder_model/log/threadId/ledger/send/stream-collapse/settle/respond），错误信封、413 kind、msg_ 前缀、SseRequest 解构全一致。
- **B 等价**：Encoder 枚举 6 行为 + NonStreamingCollector::response 与原 8 处 match 语义一致；`last_usage` 仅提位无副作用；新增 reasoning-splice 测试钉住 golden error-after-reasoning-recovered。
- **D 等价**：time::iso8601_secs/now_iso8601/now_epoch_secs 与原 billing/lib 实现格式与输出一致（含 SystemTime Err → "1970-01-01T00:00:00.000Z" 分支）；原 billing 测试迁移完整。
- **E 等价**：Accept-Encoding 收敛为 build_headers 单一构造点，同值单次设置；头顺序变化被 golden header-order 归一化覆盖。
- **F 等价**：deadline pending 期间 child 已为 None（crash 检测即消耗），tick 重排读 crash 无行为差异；3 单测钉住 3s/cancel/重武装。
- **⚠ C 唯一行为变化（有意）**：openai stream "tool-call" 事件假值工具参数（`false`/`0`/`[]`/`{}`）从 `""` 变为序列化原值（`"false"`/`"0"`/`"[]"`/`"{}"`）。方向正确（消除与 non-stream/anthropic 的 truthy 漂移，parity 测试钉住统一语义），但这是下游 wire 变化，**PR 描述"行为保持不变"措辞不准确，已修订并登记**；该边界 conformance golden 未采样（按模式采样），新契约由 tests/terminal_parity.rs 钉住。

### 缺口与处置
- conformance（Node 工具性步骤）未跑——DEVELOPMENT.md 标注为可选工具性验证，环境无 Node 时跳过。
- C 行为变化未在 ADEVIATIONS.md 登记——已在本账本 §7 登记，并修订 PR 描述。
- 账本 MD 未跟踪未进 PR（用户未拍板是否 git add），保持现状。
- **[A] 待做**：DialectSurface + handle_generation + RequestCtx
- **[F] 待做**：Supervision module（tray）
- 验证门槛：cargo test（全量）+ clippy -D warnings + fmt --check

## §8 清剿旧残留（2026-09-17，rust-rewrite → 纯 Rust 工程）

### 用户指令
"完全向新架构看齐，旧的无用的残留全部去除，golden 的使命也结束了"——清成纯 Rust 工程。

### 执行事实（全部验证过）
1. **build-rust.cmd 迁移**：`tray/build-rust.cmd` → 根 `build-rust.cmd`；ROOT=`%~dp0`；verify 段由 `node conformance/verify-build.mjs` 改为 `cargo test --locked`（golden 删除后发布门禁换 Rust 原生）；保留 CRLF。
2. **git rm 80 条**：src/（Node TS 14 文件）、tests/（vitest 22 文件）、conformance/（golden 夹具 + record/verify harness + client-probes + 探针记录 + RETRY-MATRIX/CONTINUATION-MEMO）、tray/（Tray.cs/build.cmd/CCProxyTray.exe）、Makefile、matrix-test.py、smoke-test.py、package.json、pnpm-lock.yaml、tsconfig.json、vitest.config.ts、RUST-REWRITE-PLAN.md、chaos/__pycache__。
3. **编译断链修复（两处硬依赖）**：
   - `src/models.json`（模型 catalog + reasoningEfforts）被 crates 三处 `include_str!` 引用（catalog.rs:19、models.rs:8、translate/models.rs:12）→ 迁入 `crates/ccproxy/src/models.json`（git show HEAD 恢复 9881 字节，内容逐字节一致），三处 include_str 路径更新。
   - golden JSON 三份被 Rust 测试 `include_str!`（tests/translate_golden.rs:13、stream_golden.rs:20-21）→ 迁入 `crates/ccproxy/tests/fixtures/`（translate.json 47343B、behaviour.json 285123B、upstream-scenarios.json 12209B），include 更新。**决策：golden Node harness（record/verify/probes）使命结束删除；行为快照数据作为 Rust 回归 fixture 保留**——stream/translate 测试是 SSE/翻译契约的回归保护，数据迁入后完全归属 Rust 测试体系。
4. **CI 重写**：ci.yml = ubuntu（-p ccproxy build/test/clippy/fmt，tray 为 Windows-only 不上 Linux runner）+ windows（全 workspace build/test）；release.yml = windows-latest + softprops/action-gh-release 附两 exe。
5. **文档清理**：README（16/204/262-271/301 行）、DEVELOPMENT（头部/Prereq/Commands/结构树/Tech stack/§1/§6/§8）改写为纯 Rust 现实；RUST-REWRITE-SPEC.md + ADEVIATIONS.md 保留为历史契约文档（行为偏差记录仍有效）。
6. **chaos 修复**：handover-test.py 默认 EXE `release\CCProxy\CCProxyTray.exe` → `release\CCProxyRust\CCProxyTray.exe`（旧路径随 release/CCProxy 删除失效）；soak.py 注释去 dist/proxy.js 引用。
7. **物理删除未跟踪**：node_modules/、dist/、release/CCProxy/、根 20 个 *.log。
8. **门禁全绿**：cargo test --locked = 224 全过（138 lib + 7 生成集成 + 2 + 10 + 19 stream_golden + 4 terminal_parity + 3 time_contract + 4 translate_golden + 3 + 34 tray）；clippy --all-targets --all-features -D warnings ✅；fmt --check ✅；build --release --locked ✅（ccproxy.exe 3.06MB + CCProxyTray.exe 1.38MB < 10MB 预算）。

### 保留对象（有意的）
- **RUST-REWRITE-SPEC.md / ADEVIATIONS.md**：行为契约 + 6 条有意偏差记录，维护者仍需；不再列为活动工作流。
- **chaos/**（soak.py、handover-test.py）：驱动 Rust 二进制的 Python 脚本。
- **.env.example、Cargo 工程、crates/ccproxy/tests/fixtures/**。
- **architecture-review-verification.md**：本账本，未跟踪未提交（用户此前要求留根防上下文清空）。

### 注释清理
lib.rs:2 顶层文档（引用已删 RUST-REWRITE-PLAN.md）改指向 spec+ADEVIATIONS；5 个文件的 conformance/client-probes 死路径注释改中性；测试函数名 golden()/GOLDEN 等标识符保留（自解释，不指向已删文件）。

### 提交与 PR
- 分支 `chore/remove-node-legacy`（自 rust-rewrite 3f2bdaf 切出），提交 `20a388d`（95 文件，+594/−20020），已 push。
- **PR #2**：https://github.com/CelestNya/commandcode-api-proxy/pull/2 （base: rust-rewrite）。创建时需 `-R CelestNya/commandcode-api-proxy --head "CelestNya:chore/remove-node-legacy"`（gh 缺省解析报 "No commits between" 假阴性）；gh pr edit 同理需显式 -R。
- 账本保持未跟踪（用户此前要求留根防上下文清空）。

## §9 PR #2 自审查 + 修复 + 合并（2026-09-17）

### 自审查发现（3 项，全部修复并验证）
1. **fix(build) verify 段 errorlevel 语义 bug**（build-rust.cmd）——`cargo test --locked` 失败后 `popd`（内部命令，成功即清零 errorlevel），`if errorlevel 1` 永远假，**测试失败的构建会被发布**。修复：popd 前 `set "TEST_EC=!ERRORLEVEL!"`，popd 后 `if not "!TEST_EC!"=="0" goto :verify_fail`。用隔离 cmd 模拟验证：旧写法 popd 后错误码丢失（bug 确认），新写法保留（修复生效）。
2. **fix(tests) upstream-scenarios.json 迁移被 PowerShell 管道篡改**——git blob hash 与 origin/rust-rewrite 源不匹配（其余 3 个文件逐字节一致）。原因：git show 输出经字符串管道重写。修复：`git cat-file blob` 字节安全重提取，hash 现匹配（1506436...）。教训：二进制/文本迁移一律用 cat-file 直写。
3. **ci.yml 监听过时 main 分支**——main 仍含 conformance//tray/ 残留树，push 到 main 会触发 Rust job 跑旧内容。收窄为仅 rust-rewrite（唯一维护分支）。

### 合并
- PR #2（chore/remove-node-legacy → rust-rewrite）已 MERGED：merge commit `98f70b45`（2026-09-17T13:24:55Z），远端分支已删。
- 提交链：20a388d（清剿主体）→ 8ecf41a?（fixtures 字节恢复）→ cadd2bb（verify errorlevel）→ b8e84ca（ci 分支收窄）。
- 合并后 rust-rewrite 全量 `cargo test --locked` 224 全绿；git 跟踪内无旧残留；build-rust.cmd / models.json / fixtures 均在位。
- 历史远端分支 main/fix/production-hardening/refactor/architecture-review 未动（保留；如需清理可另行指示）。

## §10 分支归一 + release 治理（2026-09-17）

### 用户四个问题
1. **conformance 是什么/为什么留**——golden 夹具 + Node record/verify harness（验证 Rust 复刻 Node 行为逐字节一致）。PR #2 已删除 harness；行为快照数据迁为 `crates/ccproxy/tests/fixtures/` 供 Rust 回归测试。**用户看到的 conformance 残留在 main 分支（过时分支）**，随 main 合并归一后消失。
2. **安全清理无用分支**——核查发现 main 不是无用分支（有 catalog 持久化 e904ab7、CLI 版本修复 83638e9、0.5.1/0.5.2 bump）。方案：**main 为收容方**（用户指示"为什么不合进main"）——rust-rewrite 吸收 main 12 个独有提交（merge fb4e0fb，冲突解决 + fix 7aa7219），再 `git push origin rust-rewrite:main` 快进归一。已删远端：rust-rewrite、fix/production-hardening、refactor/architecture-review、chore/remove-node-legacy；本地同步删（main 分支被 `D:/Projects/cc-proxy/oldproxy` worktree 占用无法 checkout，用 refspec 推送绕过）。**最终远端唯一分支 main = d72a759**。
3. **release CI**——release.yml：tag `v*` + workflow_dispatch 手动触发；windows-latest 上 test → build → softprops/action-gh-release（附 ccproxy.exe + CCProxyTray.exe）；`make_latest: true` 显式钉 latest。ci.yml 监听改为 [main]。
4. **v0.5.0 被识别为 latest**——根因：GitHub 按 publishedAt（发布时间）而非语义版本判 latest；v0.5.0 的发布动作（01:42:37Z）比 v0.5.1（01:42:36Z）、v0.5.2（01:42:10Z）各晚 1 秒/27 秒，故被标 latest。已 `gh release edit v0.5.2 --latest` 修正；release workflow 加 make_latest 防止复发。

### 合并冲突处理记录
- rename/rename：models.json（main: crates/ccproxy/models.json vs rust-rewrite: crates/ccproxy/src/models.json）→ 保留 src/ 位置，删 main 位置。
- rename/delete：RUST-REWRITE-PLAN.md（main 移 docs/，rust-rewrite 删）→ 保留删除。
- 内容冲突 4 处：catalog/lib/models/translate-models——初解冲突脚本只删 marker 行导致 ours+theirs 双保留（MODELS_JSON 重复定义 + 路径指向已删文件），编译失败后修正（fix 7aa7219）。
- docs/ 布局（main 把 4 文档移 docs/）胜出；git 自动把 PR #2 清理内容合并进 docs/DEVELOPMENT.md。
- Cargo.toml 版本 0.5.2（main）胜出。
- **验证**：合并后 227 测试全绿（catalog persist +3）、clippy -D warnings ✅、fmt ✅。

### 当前状态
- 远端：唯一 main（d72a759）；本地 worktree 分支名 rust-rewrite（内容=main，upstream 已解除；main 名被 oldproxy worktree 占用）。
- release latest：v0.5.2 ✓。CI：main 触发 + release 双触发。
- 账本继续留根未跟踪。

## §11 解压即用 zip 发布 + 补包 050-052（2026-09-17）

### 用户指令
发布的应是解压即用的压缩包（不要两个 exe 分开发），并把 v0.5.0/0.5.1/0.5.2 的包补上。

### 执行
1. **release.yml 改为单一 zip**：布局与 build-rust.cmd 一致——zip 根 `CCProxyTray.exe` + `service\ccproxy.exe` + `package.json`（版本从 Cargo.toml 提取，tag 与 workflow_dispatch 均正确）；softprops 只附 `CCProxy-v*.zip`，`make_latest: true`。提交 bb8dda0 已推 main。
2. **补包**（三个 release 原本是空壳，无任何资产）：
   - v0.5.2：当前 main 代码构建（ccproxy.exe 3.07MB + tray 1.38MB）。
   - v0.5.0 / v0.5.1：git worktree 检出 tag 构建（共享 CARGO_TARGET_DIR 增量；v0.5.1 的 Cargo.lock 过期 → `cargo build --release --offline` 重建 lock，仅临时 worktree 不提交）。
   - 三个 zip 均已 `gh release upload`（v0.5.0=2.40MB / v0.5.1=2.40MB / v0.5.2=2.40MB），worktree 已清理。
3. **zip 布局验证**（抽查 v0.5.2）：`service\ccproxy.exe` + `CCProxyTray.exe` + `package.json` ✓，解压后双击 tray 即用。

### 注意事项
- v0.5.1 tag 的 Cargo.lock 与 Cargo.toml 失配（--locked 拒绝），构建需 --offline；v0.5.1/v0.5.2 tag 的代码含 conformance/（历史状态），不影响构建。
- 三版本二进制由各自 tag 源码构建，忠实对应各 release。
---

## §12 WebUI 两屏控制面板（2026-09-17）

### 用户指令
做 ccproxy 的 Web 控制面板，只做两屏：首屏统计信息、二屏详细数据；两屏实时更新、无刷新操作。

### 探查结论
- 记忆库/两 worktree/spec/stash 均无 webui 实现记录；spec 仅在 SQLite WAL 章节把「WebUI 与托盘都是读者」作为设计上下文（代理唯一写入者、单写多读互不阻塞）。
- billing.db（%LOCALAPPDATA%\cc-proxy\billing.db）354 行真实数据；表字段 ts/reqId/model/wire/stream/attempt/status/errorTag/四 token 列/durationMs/ttfbMs；token 列 NULL=未知（不得示为 0）。

### 方案
- 同端口 8787 挂 4 路由（不另开端口）：GET /webui（单文件自包含 HTML，include_str! 内嵌）、/webui/api/stats、/webui/api/attempts?limit=N、/webui/events（SSE）。
- SSE 实现：轮询 max(id) 每秒一次，变化则推全量 snapshot（stats+attempts），空闲发 keepalive 注释；无 async runtime（tiny_http 阻塞模型），SseReader 实现 Read 阻塞在轮询循环。
- 新模块 webui.rs（深模块：4 路由接口，内部 SQLite 查询 + SSE + HTML）；前端单文件原生 JS + hash 路由两屏 + 内联 SVG 趋势图，零外部依赖（代理可离线）。

### 验证
- 单测 +5：stats 聚合（按 outcome/token）、NULL token 不示 0、attempts 最新优先+limit、trend 排序、limit 解析钳制。全量 232 测试绿（基线 227+5）；clippy -D warnings ✅；fmt ✅。
- 实机：代理在线时 4 端点全通；SSE 首帧立即推全量；插入测试行（保持 3s）→ 1s 内第二个 snapshot 命中该行 → 删除后再推（3 帧，实时性确认）；测试行已清理。
- html skill shot.py 自检：桌面/移动 consoleErrors=0、overflow=0、deadButtons=0、touchWarn=0（修过：hash 改 #/ 前缀避误报、移动端 touch target/字号）。

### 提交
b83f4fb feat(webui): two-screen live control panel over SSE（rust-rewrite；推送待定）。
---

## §13 WebUI 两屏样式改造（2026-09-18）

### 用户指令
列表做成图一（API 运维明细表）样式，概览做成图二（统计仪表盘）样式。

### 图二概览（已按参考图重排）
- 8 指标卡：今日请求（总计:355）、今日 Token、累计 Token（781.3K，输入/输出/缓存明细）、成功率 57.2%、平均响应 7.74s、平均首字 3.77s、缓存命中率 48.6%、总尝试（成功/失败）。
- 按平台拆分：openai/anthropic 各平台 Token 总量/请求/输入 Token。
- Token 使用趋势：多系列折线（Input/Output/Cache Read 左轴 + Cache Hit Rate 右轴 %），时间范围近7/14/30 天切换（前端按钮 → fetch stats?days=N）。
- 模型分布：环形图（donut SVG，中心累计 Token）+ 表格（模型/请求/Token）。
- 状态分布：ok/error/aborted/interrupted 四条比例条。
- 数据口径：本地 billing.db 无余额/费用/API密钥概念 → 8 卡为真实字段近似（成功率/平均首字/命中率替代），未编造缺失字段。

### 图一明细（已按参考图重排）
- 顶部筛选栏：模型（从数据去重）/类型（流式/非流式）/状态 + 刷新/重置。
- 表格列：# / API 密钥（reqId 截断，title 全量）/ 模型 / 端点（wire）/ 类型 / 状态徽章 / TOKEN（↓prompt ↑completion + 缓存率小字）/ 延迟（首字/总耗时两行）/ 时间（YYYY/MM/DD HH:MM:SS）/ 尝试。
- 省略无数据源列（推理强度/分组/IP/计费模式/费用）。

### 后端扩展（webui.rs）
- stats_json(conn, window)：trend 改多系列 {date,attempts,prompt,cached,completion,hitRate}，limit=window；新增 platforms[]（wire 聚合）、models[]（Top10 按尝试降序）；totals 增 avgTtfbMs、hitRate（cached/(prompt+cached)，分母 0 → null）；parse_days clamp 1..90 默认 14（DEFAULT_TREND_DAYS）。
- 参数改名 window 避免与集合 days 遮蔽（编译错误已修）。

### 验证
- 全量测试 234 绿（含 webui 7 新单测：trend 多系列/hitRate null 语义/平台聚合/模型聚合/parse_days 钳制）；clippy -D warnings ✅；fmt ✅。
- 实机：/webui 200；stats?days=30 返回 platforms=2/models=5/trend 多系列（355 尝试、命中率 48.6%）；days=999 钳制 90；attempts 正常。
- SSE 实时性：插入 webui-live-test 行 → 3 帧（首帧/插入后/删除后），插入行 1s 内命中；已清理无残留。
- shot.py：桌面/移动 consoleErrors=0、overflow=0、deadButtons=0；移动端仅卡片 caption 小字（12-12.5px，caption 豁免类）。

### 提交
未提交（amend b83f4fb 或新 commit 待定）。
---

## §14 WebUI 第三轮：布局/首帧/系统信息/M3 美化（2026-09-18）

### 用户反馈（本轮）
1. 状态分布排序不对 → 改为按次数降序。
2. "API 密钥"列是别称不是真密钥 → 查明：billing 表无密钥字段（只有 reqId=完整 UUID），代理刻意不落密钥（隐私）；列改名"请求 ID"，显示完整 UUID（截断+hover 全量）。
3. "m"模型是啥？ → 非 mock：billing 真实记录里 model='m' 共 42 行，来自请求体 model 参数（legacy JSONL 导入也有同值），是用户请求的真实模型名。
4. 明细页太空 → 列表上方加 4 张迷你统计卡（今日请求/成功率/平均耗时/缓存命中）。
5. 导航栏移到左侧 → sidebar（概览/明细，M3 pill 选中态）。
6. 顶栏：左仅软件名，右侧靠右 连接状态/消耗/内存/运行（留白 spacer）；金额无数据源（meta 表仅 legacy_jsonl_imported，无定价/费用字段）→ 用"消耗 Token 总量"诚实替代。
7. Astro 性能范式 → /webui 服务端内联首帧快照 window.__INIT__（静态优先，首屏零网络往返）；sysinfo 独立 5s 轮询（岛屿式，不阻塞数据流）。
8. 外观参考 shirone.mysqil.com（Material 3）→ M3 深色 tonal 色板、12-16px 圆角、pill 导航、chip 顶栏、柔和边框阴影。

### 后端
- 新端点 GET /webui/api/sysinfo：进程工作集 MB + 运行时长（sysinfo crate——lib.rs forbid(unsafe_code) 硬约束，改用纯第三方 crate，跨平台返回真实值）。
- handle_page 内联 __INIT__（stats(14)+attempts(200)，DB 不可开时页面仍渲染）。
- ccproxy 新增依赖 sysinfo 0.35（default-features=false + system）。

### 验证
- 全量 235 测试绿（+1 sysinfo 可移植性测试）；clippy -D warnings ✅；fmt ✅。
- 实机：/webui 200 且含 window.__INIT__（真实 356 尝试数据内联，页面 92KB）；sysinfo memMb≈19MB、uptime 秒级准确（启动 34s 显示 34）；health 200。
- shot.py：概览/明细 桌面+移动 consoleErrors/overflow/deadButtons 全 0；明细移动端 filterbar 触控目标修复后 touch/font 全 0（此前 touch=2）。

### 提交
636b3d5 feat(webui): sidebar shell, static-first init, sysinfo chips, M3 restyle（rust-rewrite，未推送）。
---

## §15 WebUI 第四轮：明细表分页（2026-09-18）

### 需求
去除明细表内部拖动条；长列表改为可变长度有限分页；表格底部加「上一页」「页码(1…x-1,x,x+1…N)」「下一页」「每页展示{50,100,200,500}条」。

### 实现（webui.html，纯前端）
- .table-wrap 去掉 overflow:auto + max-height，页面整体滚动。
- state 增加 page=1、pageSize=50；renderAttempts 改为 filterAttempts（从原函数抽出）→ slice 当前页 → 渲染 pageRows；filterAttempts 同时供 setPage 复算总页数。
- pageList(cur,total)：≤7 页全列；否则 1 … x-1,x,x+1 … N 省略号窗口。
- renderPager：共 N 条 + 上一页 + 页码（active 高亮）+ 下一页 + 每页展示 select(50/100/200/500)；prev/next 边界 disabled。
- 筛选 change / 重置 / pageSize 变更 → page 回 1；SSE 快照刷新 → 页超界自动 clamp。
- 移动端 @media 640px 内 pager 控件 min-height/min-width 44px。

### 验证
- 全量测试不受影响（纯 HTML 改动）；cargo build 需强制重编（cargo 增量未检测 include_str 的 html 变更，曾导致旧页面服务，用 cargo clean -p ccproxy 后正常）。
- shot.py：明细页 desktop/mobile err/overflow/dead/touch 全 0（font=15 为卡片 caption 豁免）；DOM buttons=8 = 刷新/重置/上一页/4 页码/下一页。
- 大视口截图（1440x3200）实证：表格 50 行 + 分页条「共 200 条 | 上一页 | 1 2 3 4 | 下一页 | 每页展示 [50]」。
- 此前疑惑澄清：shot.py chrome_cli 降级模式只截视口（1440x900 恒等），全页内容需调 --desktop 大视口；侧栏 footer 为固定元素，非页面底。

### 提交
3d2fa5d feat(webui): paginate the details table（rust-rewrite，未推送）。
---

## §16 WebUI 第五轮：字体/动画/滚动条/趋势图（2026-09-18）

### 用户反馈与落实
1. 字体大小不统一 → 收敛层级：正文 13.5px 单层、说明/标签统一 12px（side-foot、平台副标题、模型表头、图例、卡标签）、微标签 11.5px（badge、表格 small）、数值 18/23px；消除 11/11.5/12/12.5 混用。
2. 虚拟路由无过渡动画 → .view.active 加 viewIn（220ms fadeUp），prefers-reduced-motion 关闭。
3. 导航栏无过渡动画 → nav-item transition 扩展 background/color/transform .18s，hover 位移 3px + svg 微缩放。
4. 列表拖动条自定义（细）→ 全局 ::-webkit-scrollbar 6px 圆角 thumb + scrollbar-width:thin。
5. 折线图时段按钮样式不统一、按了不生效 →
   - 样式：统一 pill 段选（非 active muted 边框、hover 亮、active container 色加粗）。
   - 生效：根因是后端 trend SQL 只按 limit 取"最近 N 个有数据的天"，数据跨度 ≤7 天时 7/14/30 三档图形完全相同 → 改为严格 N 天日历窗口（where ts >= datetime('now','localtime','-'||?1||' days')），跨度大时三档真正不同形。
   - SSE 节流：days≠14 时每 30s 才重取一次，避免每秒打爆接口。
6. 折线图按时段加点并平滑 → Catmull-Rom→三次贝塞尔平滑（smoothPath），每条线按时段密度画数据点（dotStep=ceil(n/14)，短窗口全画、长窗口抽样），点带底色描边。

### 验证
- 全量 235 测试绿（含修 flaky：uptime 秒级截断可能为 0，断言改为 <30 天合理性上界）；fmt/clippy 净。
- 实机：/webui/api/stats?days=7/30 均正常（trend 严格窗口）；shot.py 概览大视口 1440x1250：时段按钮三档 pill 统一、近14天 active 高亮、平滑曲线+数据点可见；明细页正常；lint 全零。
- 附注：截图 OCR 曾把运行时长 00:00 误读为 06:00，sysinfo 实测 uptime=203s 正常；「Output 线贴底」系 Output 总量 10K vs 轴 198.7K 真实比例，非 bug。

### 提交
2e25bcb feat(webui): unify type scale, view/nav motion, slim scrollbar, smooth trend（rust-rewrite，未推送）。
---

## §17 WebUI 第六轮：模型表裁切/趋势窗口/移除协议拆分（2026-09-18）

### 用户反馈与根因
1. 模型表格列头半个字看不清 → 根因：.ov-row 给模型分布面板 340px，环形图占 200px 后表格仅剩 ~94px；且 .model-table 无 table-layout:fixed/显式列宽。修：ov-row 改 1fr 1fr 等宽 + 表格 fixed 布局 + 46/24/30 列宽。实测列头"模型/请求/Token"完整。
2. 7/14/30 天按钮"依然不生效" → 根因：trend SQL 只返回"有数据的天"（DB 仅 5 天），三档窗口图形完全相同。修：递归 CTE 物化窗口内每一天 + left join 聚合，空日补 0；X 轴显示完整窗口。实测 7d=7 点/14d=14 点/30d=30 点，切换明显改变跨度与密度。
3. 按协议拆分两行没价值 → 移除"按平台拆分"面板（HTML+JS+CSS），后端 platforms 数据保留不渲染。

### 验证
- 235 测试全绿（trend 测试断言更新为窗口补齐语义）；fmt/clippy 净。
- 实机：三档窗口点数 7/14/30；概览截图模型表格列头完整、布局均衡（趋势全宽 + 模型/状态双栏）；lint 全零。

### 提交
(本轮) fix(webui): model table clipping, real trend windows, drop platform split（rust-rewrite，未推送）。
---

## §18 WebUI 第七轮：趋势曲线保形插值（2026-09-18）

### 用户反馈
贝塞尔平滑不好，至少不该超出极值（overshoot）。

### 根因
Catmull-Rom→三次贝塞尔转换的控制点由相邻点斜率外推，数据骤变时曲线会冲过峰/谷（视觉上出现超过数据极值的假峰值）。

### 修复
改用 Fritsch-Carlson PCHIP（单调保形三次插值）：
- 内部点斜率：相邻割线异号→0（局部极值点切线水平，绝不冲顶/穿底）；同号→加权调和平均（保单调）。
- 段间三次 Hermite 用这些切线，控制点 y 偏移 = m·h/3，天然落在数据包络内。
- 端点单边差分，保持单调。

### 验证
- python 独立复现同算法 + 实盘 14 天数据采样 1000 点：prompt[0,198692]→curve[0,198692]、cached、completion、hitRate 四线 overshoot=False，全部严格落包络。
- 实机截图：曲线平滑、峰值 198.7K 到顶不冲出；lint 全零。
- 235 测试不受影响（纯前端 JS）。

### 提交
(本轮) fix(webui): shape-preserving trend curves, no overshoot（rust-rewrite，未推送）。
---

## §19 WebUI 第八轮：两屏字体统一（2026-09-18）

### 用户反馈
两个页面字体大小还是不一样。

### 残留差异与修复
- 明细 mini-card 数值 18px / 副说明 11px，概览 mcard 数值 23px / 副说明 12px → 统一 23px / 12px（含行距微调）。
- 模型表 12px（th 12px），主表 12.5px（th 11.5px）→ 模型表统一 12.5px / 11.5px。
- 至此两屏共用一套层级：数值 23、标签/说明 12、表体/控件 12.5、正文 13.5、表头 11.5、标题 15；图表内 SVG 刻度 10 为图内固有。

### 验证
- 两页卡片区放大对比截图：数值/标签/副说明像素级一致；lint 全零。
- 235 测试不受影响（纯 CSS）。

### 提交
(本轮) fix(webui): unify font sizes across both screens（rust-rewrite，未推送）。
## §20 模型-价格映射表（commandcode 官网爬取）— e60ca62

### 需求
维护一张模型-价格映射表，从 commandcode 官网文档自动爬取（官网价格确实总变：DEAL 徽章、免费模型、限时 boost、Geo 限制）。

### 实现
- 新模块 `crates/ccproxy/src/pricing.rs`：
  - `fetch_latest()`：GET https://commandcode.ai/docs/resources/pricing-limits（12s 超时，UA ccproxy-pricing/<ver>），解析 Tailwind row grid。
  - 解析器为手写字节级 HTML 扫描（无新依赖）：`role="row"` 切行 → 模型名 span → `px-2/px-3 py-3` 数据 cell → 价格提取（DEAL 行取最后一个 $ 值，Free 行取 0，"—"→None）。`#![deny]` 全库下对解析辅助函数加 `#[allow(indexing_slicing, arithmetic_side_effects)]`，边界由 find 保证。
  - `Pricing::load(dir)`：读 `billing_dir()/pricing.json` 缓存；>24h stale 时同步抓取并写回；**抓取失败沿用旧缓存（降级不崩）**。
  - `cost(model, prompt, cached, completion)`：uncached prompt 按 input 价、cached 按 cache-read 价、completion 按 output 价，USD/百万 token，saturating 防负。
  - **名称匹配三层**：精确 → 去 provider 前缀（`deepseek/` 后 id）→ normalize（小写、分隔符折叠、删 `(latest)/(exp)` 括号段）。页面名 "DeepSeek V4 Flash (latest)" ↔ 账本 API id "deepseek/deepseek-v4-flash" 由此对齐。
- webui.rs：models 分列取 prompt/cached/completion/reasoning，每模型输出 costUsd（查不到价格 → null，绝不编造）；totals 增 costUsd；顶层增 pricing 元数据 {source, fetchedAt, count}。
- webui.html：顶栏 chip 从 Token 数改为消耗金额；概览模型表加"费用"列；明细 mini-cards 5 卡一行加"消耗金额"。

### 调试过程（记录根因）
1. 前缀匹配漏 DEAL 行（价格列 class 为 `flex flex-col items-end px-2 py-3`，前缀不同）→ 改 class 值 contains 匹配。
2. `next_cell` 引号定位把 class 开引号当闭引号 → 值定位跳过开引号。
3. **关键 bug**：`next_cell` 内部 scan 推进后返回的偏移相对已推进的 scan，而调用方按传入 rest 索引 → 错位（row[125..] 落到 span class 中部）。修：累计 base 偏移，返回相对 rest 的绝对偏移。
4. normalize 括号段前分隔符残留尾 `-`（"deepseek-v4-flash-"）→ `(` 时回退 pop 前导分隔符。
5. fetched_at 类型 i64/u64 与 `now_epoch_secs()` 对齐。

### 验证
- pricing 单测 7 个（plain/DEAL/Free 行、缓存往返、stale 时效、成本公式、三层名称匹配、normalize 后缀删除）。
- 真实抓取：live fetch 解析出 **58 个模型**（Claude Fable 5 in=10/out=50/cr=1/cw=12.5 等价格正确）。
- 全量 156 lib 测试绿；clippy --all-targets --all-features --locked -- -D warnings 净；fmt 干净。
- 实机：stats JSON totals.costUsd=$0.0102；模型费用 deepseek/deepseek-v4-flash=$0.0030、deepseek-v4.1-flash=$0.0072、deepseek/deepseek-v4.1-flash=$0.0001；`m`/`deepseek-flash`（价格表无此模型）→ null（前端 "—"）。两屏截图确认费用列/顶栏金额/mini 卡齐全。

### 提交
(本轮) feat(webui): crawl commandcode price table and surface spend（e60ca62，rust-rewrite，未推送 main）。

### 已知限制
- 价格表页面结构若大改（非 Tailwind grid），解析器需适配；fetch 失败时沿用旧缓存，成本列仅显示缺失（—）。
- 计费口径：官方按底层模型原价、All deals apply；缓存写入价（cache_write）已解析但成本模型暂未计入（当前只按 read 价计 cached prompt）。

## §21 峰谷价：内嵌 RSC JSON 主路径（a95cddb）

### 需求
用户追加「价格还有峰谷价的」——价目清单需反映峰值/谷值时段价格。

### 调研结论（官网 https://commandcode.ai/docs/resources/pricing-limits）
- 页面是 Next.js：HTML 表格只渲染 58 个模型且**看不到峰谷价**；RSC payload 内嵌结构化 JSON `"models":[{id,name,inputCost,outputCost,cacheReadCost,category,provider,timeOfDay:{effective,peak,offPeak}}, …]`，共 **69 个模型**。
- **4 个 DeepSeek 模型带 timeOfDay**：deepseek-v4-pro、deepseek-v4-flash、deepseek-v4-flash-vision-exp、deepseek-v4.1-flash（effective=2026-08-16T16:00:00Z）。
- 峰谷窗口：**Peak = 01:00–04:00 与 06:00–10:00 UTC（每天 7 小时，全价）；Off-peak = 其余 17 小时（约半价）**。
- 示例（USD/百万 token，in/out/cache-read）：v4-flash peak 0.3/1.2/0.006 ↔ off 0.15/0.6/0.003；v4-pro peak 1.32/3.96/0.044 ↔ off 0.66/1.98/0.022。
- JSON **无 cacheWriteCost**——cache-write 仍须从 HTML 表格补（19 个模型有）。

### 实现
- pricing.rs：`RateSet{input,output,cache_read}`；`ModelPrice` 增 `id: Option<String>`、`peak/off_peak: Option<RateSet>`。
- `parse_models_json`（主路径）：找 `\"models\":[`（RSC 转义形式）→ 括号配对 → 反转义 `\"` → serde_json 解析；**数组从 `[` 处切片**（首版 bug：从反斜杠切片含 `"models":` 前缀导致非合法 JSON，返回空）。
- `fetch_latest()`：JSON 非空 → 用之 + `patch_cache_write` 从 HTML 补 cache-write；JSON 空 → HTML fallback（无峰谷无 id）。
- `rate_at(utc_minute_of_day)`：60..240 / 360..600 分钟窗口选 peak，否则 off-peak；无 timeOfDay 模型走 flat rates。
- `utc_minute_of_day()` = `(now_epoch_secs() % 86400) / 60`（now_epoch_secs 是 SystemTime UNIX_EPOCH，真 UTC）。
- `cost()` 加第 5 参 utc_minute；webui.rs stats 传 `crate::pricing::utc_minute_of_day()`。
- 缓存 schema 增 id/peak/offPeak；`stale()` 增「全部模型无 id → 旧 schema → 立即重抓」；rate_from_json 兼容网页字段名（inputCost…）与缓存字段名（input/cacheRead/cache_read，含 rename 前下划线版）。

### 调试记录（根因）
1. **首版 needle 反斜杠层级错**：Python 写入 Rust 的 find 参数变成 3 反斜杠，找不到 → JSON 空 → 静默 fallback HTML（无峰谷）。修：raw string `r#"\"models\":["#`（单反斜杠+引号）。
2. **raw 切片起点错**：从 needle 起点（反斜杠）切片，unescape 后顶层是 `"models":[...]` 非合法 JSON 表达式。修：从 `[` 处切（`start + NEEDLE.len() - 1`）。
3. **缓存字段名错**：ModelPrice 直接 serde 序列化，`cache_read` 字段写成了下划线；rate_from_json 只认 `cacheRead` → peak/offPeak 解析全 None → 费用恒走 flat。修：`#[serde(rename = "cacheRead")]` + rate_from_json 三候选字段名。
4. **旧缓存不重抓**：fetchedAt 未过期（<24h）→ stale()=false → 峰谷价要等一天。修：无 id 缓存视为旧 schema 立即重抓。

### 验证
- pricing 单测 10 个（+JSON 解析、rate_at 峰谷窗口、cost 按时段、live fetch ignored）；全量 **159 过**；clippy -D warnings 净；fmt 净。
- live fetch：69 模型、4 峰谷模型价格正确、cache-write 补丁 ≥10。
- 实机（UTC 03:0x 峰值窗口）：v4-flash $0.0030→**$0.0060**、v4.1-flash $0.0072→**$0.0144**、totals $0.0102→**$0.0204**（全部翻倍=peak 价生效）；WebUI 顶栏/模型表同步更新。

### 已提交
- a95cddb（rust-rewrite，未推送 main）。

### 已知限制
- 峰谷价**只影响 4 个 DeepSeek 模型**；其余 65 模型 flat。
- 历史账本行按**当前时段价**重估（账本不存请求时刻）；若要按请求真实时段计费需给 billing 加时间字段——未做，用户未要求。
- HTML fallback 路径无峰谷价（页面结构若再变，JSON 与表格都解析不出时沿用旧缓存）。

## §22 成本口径核查与展示层修复（2026-09-18）

**触发**：用户质疑「一个模型 4 个价目（输入/输出/缓存读/缓存创建），怎么可能总价这么少？怎么拿缓存读价算所有 token」。

**结论：成本公式正确，展示层重复计数。**

AI SDK totalUsage 口径（已核实）：promptTokens = 总输入（含缓存命中），cachedTokens 是其子集；completionTokens = 总输出（含 reasoning），reasoningTokens 是其子集。

成本公式（pricing.rs::cost，未改）：
`(prompt - cached) * input + cached * cache_read + completion * output`，单位 USD/百万。
缓存创建（cache write）：官网仅 19 模型有价，CC 实际几乎不产生（totalUsage 无 cacheWriteTokens），账本无该列，用户确认「通常没有」→ 价格表保留列，成本不计。

账本真实数据手算验证（deepseek-v4.1-flash，78 次，UTC 峰值价 0.3/1.2/0.006）：
- 未命中输入 (227331-211200)=16131 × 0.3 = 4839.3
- 缓存读 211200 × 0.006 = 1267.2
- 输出 6904 × 1.2 = 8284.8
- 合计 14391.3/M = **$0.01439**，上游 costUsd = $0.0143913，精确一致。
v4-flash 同法 $0.005952，上游 $0.005952144，一致。

钱少的真实原因：缓存命中率 94.5%（非旧显示的 48.6%），缓存读价是输入价 1/50，输出 token 极少（10.2K / 396K 输入）。

**展示 bug（已修，提交于 §22）**：
1. 模型表 tokens = prompt+cached+completion+reasoning，cached/reasoning 各被加两次（v4.1-flash 显示 445.8K，真实 234.2K）→ 改 prompt+completion。
2. 命中率分母 prompt+cached（48.6%）→ 改 prompt（94.5%），趋势/总计/明细行三处。
3. 前端累计 Token 781.3K（虚高）→ 406.4K；卡片文案改为「输入 X（含缓存 Y）/ 输出 Z」。
4. models JSON 增 promptTokens/cachedTokens/completionTokens/reasoningTokens 明细。

验证：159 lib 测试全过（2 个 webui 断言更新到新口径）、clippy -D warnings 净、fmt 净、实机 stats API 与两屏截图（webui-v13/v13b）核对一致；成本数字修复前后不变。
## §23 缓存创建价验证与计费修复（2026-09-18，commit d9e0d45）

**触发**：用户要求「试一次有缓存创建价的模型，连续请求两次看看价格和响应价格是否一致」。

**验证结论：不一致——缓存创建 token 此前完全没被提取。**

代码核查：extract_usage 只提取 prompt/completion/total/cached/reasoning 五字段，**丢弃 inputTokenDetails.cacheCreationTokens**；cost() 只有 input/cache_read/output 三价目，cacheWrite 列存在但从不参与。所以创建缓存的 token 全按输入价计费（gemini-3.7-flash 输入 $1.5/M vs 缓存创建 $0.08334/M，贵约 18 倍）。

**端到端验证**（chaos/cache-cycle-mock.cjs，隔离实例 8890 + CC_TRAY_NS）：
- 第一次请求（mock 返回 cacheCreation=100, cacheRead=0）→ 账本 prompt=100 cached=0 **creation=100** ✓
- 第二次请求（cacheRead=80, creation=0）→ prompt=100 cached=80 creation=0 ✓
- 聚合 4 次：prompt=400 cached=240 creation=100 compl=40；cost $0.0004343
- 手算：60×1.5 + 240×0.15 + 100×0.08334 + 40×7.5 = 434.334e-6 ✓ 精确一致

**修复（5 文件）**：
1. usage.rs UsageData + cache_creation_tokens
2. translate/util.rs extract_usage 提取（AI SDK inputTokenDetails/cacheCreationTokens、扁平 cacheCreationInputTokens、snake_case 别名）
3. billing.rs AttemptRecord/schema/insert/record_attempt 全链路 + **migrate_column 幂等迁移**（旧库 open 时自动加列，旧行 NULL=未知，不误计）
4. pricing.rs cost() 加第 5 参 cache_creation；uncached = prompt − cached − cache_creation；creation 按 cache_write 价，无写价模型回退输入价（不低估）
5. webui.rs 模型表加 cacheCreationTokens 列

**验证**：162 测试（+2：extract_usage 提取 cacheCreation、legacy db 迁移）、clippy -D warnings 净、fmt 净、mock 端到端、生产库迁移后 stats 正常（旧数据 creation=0，谷值时段 v4-flash $0.002976 / v4.1-flash $0.007196，恰为峰值价一半，符合峰谷设计）。

**遗留**：缓存创建价模型需真实 key 才能对上游做最终一致性确认（本地 mock 验证的是提取+计费逻辑）；真实 CC 上游是否返回 cacheCreationTokens 待用户实测一次确认。
## §24 显示逻辑优化（2026-09-19，未提交）

**触发**：用户指令「优化控制面板的显示逻辑」。全量读 webui.rs/webui.html 后定位 9 处，全部修复。

**后端（webui.rs，+4 测试中 3 留存）**：
1. `handle_page` 的 `window.__INIT__` 注入未转义：serde_json 不转义 `<`/`>`/`&`，模型名（来自请求体）含 `</script>` 可从数据直接破开 script 标签 → 新增 `inline_json()`（`\u003c/\u003e/\u0026`）+ 测试（转义后仍是合法 JSON、parse 还原原串）。
2. **today 统计漏计**：ts 列是 T 分隔 UTC ISO（`2026-09-19T01:30:00.000Z`），旧边界 `datetime('now','localtime','start of day')` 是空格分隔——`'T' > ' '` 使同日字符串恒大于边界，**每天本地 00:00–08:00 的请求被漏出"今日"**（UTC 日期还停昨天）→ 改 `strftime('%Y-%m-%dT%H:%M:%S','now','localtime','start of day','utc')`，与 ts 同格式精确比较；测试用边界表达式自身构造 ±1s 两行，确定性断言。
3. 趋势窗口边界同病同修（`strftime('now','-'||?2||' days')`，UTC 整日平移与时区无关）。注：曾写一个"UTC 午夜行应被排除"的图表断言测试，失败后想通——对 UTC+8 该 quirk 在图上不可见（被排除时段永远落在未物化日期上），测试删除，SQL 修正保留。
4. `attempts_json` 补 `cacheCreationTokens` 列（§23 后端有数据、明细端点没带）；`handle_attempts` 账本不可用时返回 `[]`（数组契约，此前返回 `{"error"}` 会让前端 `.filter` 抛 TypeError 被静默吞掉）。

**前端（webui.html）**：
5. `renderStats` 拆为 `renderSummary`（卡片/模型/状态/消耗 chip/迷你卡）+ `renderTrend`；SSE 对**任意**时间窗都更摘要（此前选 7/30 天时顶栏消耗、迷你卡最长冻结 30s），趋势仅 14 天直用、其余保持 30s 节流。
6. 「每页展示 500」与只拉 200 条矛盾：`attemptsLimit() = max(pageSize, 200)`；改每页条数即重拉；SSE 快照行数不足时自动补拉一次。
7. `#` 列跨页连续编号（`start+i+1`，此前第 2 页又从 1 开始）。
8. 防御：attempts 非数组（账本不可用/INIT 缺失）按 `[]` 处理；空表区分「暂无记录」与「没有符合条件的记录」。
9. 小项：`fmtDur` 加 m/h 档（此前 3661s 显示 "3661.00s"）；明细 tokens 小字展示「缓存创建 N」（>0 时）；mini 卡成功率双除 100 清理；pager 加范围 tooltip（最近 N 条）。

**验证**：cargo clean -p ccproxy 后全量 165 lib + 19 集成测试绿（+3）、clippy -D warnings 净、fmt 净；隔离实例 8890 + CC_TRAY_NS 实机：health/stats/attempts/页面全通，插入恶意模型名行后 `__INIT__` 中为 `\u003c` 转义、页面 0 处裸 `<script>`、数据仍正常聚合（cost=null 不编造）；headless Edge 概览/明细/移动三截图：行号连续、缓存创建小字、时区正确（02:00Z→10:00 显示）。SSE 服务端路径本轮未动、未重测（§12–§13 已验）；客户端 onmessage 新分发由截图间接覆盖。测试数据目录已清理。

## §25 托盘右键菜单新增「打开 WebUI」（2026-09-19，未提交）

**触发**：用户指令「右键菜单里新增打开 webui 选项」。

**实现**（ccproxy-tray）：
- `tray.rs`：`cmd::OPEN_WEBUI = 6`、`Action::OpenWebUi`；`action_from_id` / `record` 同步映射。
- **菜单条目抽成纯函数 `menu_entries(running, autostart, cache, version) -> Vec<(id, String, bool)>`**（此前内联在 `show_menu` 里，无法测试——正是账本候选 F 那类「策略埋在闭包」问题的轻量版）。`show_menu` 只负责把条目喂给 Win32。
- 条目位置：启动/停止分隔线之下，「打开日志」之上；启用条件 = `running`（WebUI 由代理自身在实例端口提供，停止时置灰而非隐藏，保持可发现）。
- `main.rs`：`dispatch` 增 `port` 参数；新增 `webui_url(port)` → `http://127.0.0.1:{port}/webui`（必须用实例端口，命名空间实例 8890 不能指向生产 8787）；`open_webui`；`ShellExecuteW` 调用抽成 `shell_open(target, log)` 供 open_log/open_webui 共用，并检查返回码 ≤32 时写日志（此前静默丢弃返回值）。

**验证**：37 托盘测试绿（+3：webui_url 端口正确性、WebUI 条目存在且仅停止时置灰、六个动作 id 在四种 running×autostart 组合下各出现恰好一次）；全工作区 clippy -D warnings 净、fmt 净；隔离实例 8891 确认 `webui_url` 目标可服务（/webui 200 且含 __INIT__、/health 200、attempts API 200）。菜单项本身无法在无桌面会话下点击，其组合逻辑由上述纯函数测试覆盖；打包脚本/README 无需同步（未逐项列菜单）。

## §26 实验：零字节超时重试可行性（2026-09-19，未改代码）

**触发**：用户要求「30s 未出字就丢弃旧请求、代理内立即重试，失败三次才停」，并要求先做实验。

**实验工具**：`chaos/mock-upstream.cjs` 新增两模式——`silent`（flushHeaders 发 200 头后 body 零字节）、`gap`（先发一块、沉默 3s、再发完）；`/tmp/ureqprobe`（ureq 2.12.1，与工程同大版本）。

**关键陷阱（已修正）**：Node 的 `res.writeHead()` 只入缓冲不 flush，首版 silent 实测到的是"连响应头都没发"（错误为 `Error encountered in the status line`），不是"流已开、正文沉默"。加 `res.flushHeaders()` 后才是目标场景。

**实验结论**：

1. **双阈值可行（决定性）**：ureq 的 `timeout_read` 触发 `TimedOut` 后，**同一个 reader 可以继续读**——探针在 2018ms 超时，继续读后在 3013ms 拿到后续字节并正常 EOF（total=269）。所以「零字节 30s / 已出字后静默 120s」两个阈值能在应用层自行累积计时实现，**无需改用非阻塞 socket 或自建 TLS 层**。这推翻了 ADEVIATIONS §3 里"无法在响应头到达后调整"所暗示的限制——不是调整超时，而是超时后继续读、自行计时。

2. **字节确实重置计时**：`trickle` 模式（每 300ms 一块，远小于 idle 4s）实测 15 个 text_delta 全数到达、全程无 idle 超时、无重试触发。

3. **丢弃+重发机制已存在**：`silent` 模式实测 idle 4s 精确触发（mock 侧 `silentCloseMs:[4003,4004]` 证明旧 socket 被关闭）、自动重发（`hits:2`）、客户端侧仅见一个连续流（`message_start` → error → `message_stop`）。当前走的是 Layer C splice 路径，且 **`retried` 是 bool，只重试 1 次**——这正是要改的点。

4. **待验证（阶段二前）**：同 threadId 重发是否被 CC 重复计费；被丢弃的请求是否已写入 prompt cache（决定"丢弃"真实成本）。需真 key + 当前网络，未做。

**据此定稿**：新增独立通道（不与现有 Layer A/B/C 混），应用层累积"零字节时长"，超 30s 丢弃重发、计数上限 3。

## §27 零字节快速重试通道（2026-09-19，未提交）

**需求**：网络差导致 CC 超时，需要"30s 未出字就丢弃旧请求、立即重发，失败三次才停"。

**设计要点（基于 §26 实验）**：
- 实验证明 ureq 的 `timeout_read` 触发后连接**仍可续读**、连续超时不会变硬错误 → 双阈值可在应用层累积实现，无需自建 socket 层。
- socket `timeout_read` 退化为**检查刻度**（`read_tick_ms` = min(idle/2, no-output/2)，上限 2s），真正的截止由 `UpstreamStream` 累积 `silent_ms` 判定（每次超时按 tick 加，字节到达清零）。用 tick 计数而非墙上时间，保证确定性、且无自旋风险。
- **新增独立通道，不与现有重试混**（用户明确要求"无字和网络抖动是两个逻辑"）：Layer A（5xx/429 退避）、Layer B（403 模型发现）、Layer C（已出内容后的 splice）**全部原样保留**；新通道只在"零字节"这唯一交叉点生效。

**实现（6 文件）**：
1. `sse.rs`：新增 `StreamFailure::NoOutput{ms}` + `[no-output]` 标签（与 `[idle-timeout]` 区分，日志/客户端可辨）。
2. `upstream.rs`：`UpstreamStream` 增 `no_output_timeout_ms`/`bytes_seen`/`silent_ms`/`tick_ms`；`with_tick` 构造器；`silence_deadline()` 分派——首字节前且 no-output 启用 → `NoOutput`，否则 `IdleTimeout`（**未配置时行为与改动前逐字一致**，兜底防自旋）；`read_tick_ms`。
3. `config.rs`：`CC_NO_OUTPUT_TIMEOUT_MS`（默认 30000）、`CC_NO_OUTPUT_RETRIES`（默认 3，上限 10）；`parse_non_negative_int`（0 有意义）。
4. `generate.rs`/`server.rs`：`UpstreamOptions` 与 `upstream_options` 透传两字段。
5. `stream_body.rs`：`retried: bool` 之外增 `no_output_retries_left: u32` 计数；`try_replacement` 按失败类分派——`NoOutput` **从头重发**（不调 `begin_continuation`，避免丢弃替换流的正常开头）且扣减计数，其他失败仍是单次 splice。新增 `with_no_output_retries` 构造器。
6. `chaos/mock-upstream.cjs`：新增 `silent`（flushHeaders 后 body 零字节）、`gap`（发一块→静默→再发完）两模式；前者记录 socket 关闭时刻。

**验证**：
- 单测 +8：`silent_before_the_first_byte_is_a_no_output_failure`、`silence_after_the_first_byte_is_an_idle_timeout_not_a_no_output`（安全属性：出字节后不得从头重发）、`bytes_reset_the_accumulated_silence`、`a_disabled_no_output_window_keeps_the_old_idle_behaviour`、`the_read_tick_is_at_most_half_the_smallest_deadline`、`a_silent_start_is_re_sent_from_scratch_more_than_once`（证明是预算不是一次性 bool）、`the_no_output_retry_budget_is_respected`、`a_zero_budget_never_re_sends`。
- 全量 **260 测试绿**（lib 170 + 集成 90）；clippy `-D warnings` 净；fmt 净。
- **端到端（隔离实例 8894 + mock，CC_NO_OUTPUT_TIMEOUT_MS=1500/上限3）**：
  - silent 模式：mock 收到 **4 次**请求（1+3 重发），每次 ~1.5s 后旧 socket 被关闭（`silentCloseMs:[1632,1633,1512,1511]`），客户端只见一个连续响应，末尾 `[no-output] ... for 1500ms` + 正常终止；总耗时 6.2s ≈ 4×1.5s。
  - **解耦验证**：`status_429` 仍走原快速退避（mock 3 次=1+2、总耗时 1.5s），4xx 原样透传，与新通道无关。
  - **误触发验证**：`trickle`（300ms/块 < 1.5s 窗口）15 块全达、零重试、无 no-output 错误 → 字节确实重置计时。

**已知限制/未验证**：
- 同 threadId 被丢弃的请求是否已被 CC 计费、是否已写入 prompt cache —— **需真 key + 稳定网络实测，本次未做**（§26 已列为待验证项）。
- 未启用 `CC_NO_OUTPUT_TIMEOUT_MS` 时行为与改动前完全一致（已验证）；生产未部署本次改动。

## §28 真实网络实测（2026-09-21，生产默认参数）

**背景**：上游当日被挤爆，晚间恢复后实测。

**真实上游连通性**（隔离实例 8895 → https://api.commandcode.ai，真 key 仅内存传递、未落盘）：
- 流式请求成功：TTFB 4.95s，收 thinking + 正文；
- 非流式请求成功：输入 7614（缓存命中 7424，97.5%）、输出 300；
- 账本 4 行：3 次 ok + 1 次 `http-403-model-unknown`（首次未知模型名触发 Layer B 模型发现重试，第二次成功）——**证明新无字通道在真实环境下零误触发**，且 Layer B 照常工作（解耦确认）。
- 实例日志无任何 `no output` / `retrying` 异常行。

**生产默认参数（30s / 3 次）静默场景**（实例 8896 → mock silent）：
- 总耗时 **120.35s = 4 × 30s**（1 + 3 次重发）；
- mock 侧 4 次连接，存活 [30139, 30095, 30072, 30075] ms —— **每次精确 30s 后被关闭**；
- 客户端收到 `message_start` → `[no-output] ... for 30000ms` error → `message_stop`，全程单一连续响应。

**误伤/边界矩阵**（新增 `chaos` slow_start 模式：flushHeaders 后延迟 N 秒才出字）：
| 上游首次出字延迟 | 窗口 | 结果 | 判定 |
| --- | --- | --- | --- |
| 29s | 30s | 正常完成，正文送达，mock 仅 1 次请求 | ✅ 不误伤 |
| 31s | 30s | 4 次全超时 → 120s 后 `[no-output]` 失败，无正文 | ⚠️ 见下 |

**关键权衡（必须知晓）**：30s 窗口是硬阈值，**"极慢但最终能出字"的请求（>30s）会被牺牲**——4 次尝试各 30s 后整体失败。这是用户明确要求"30s 未出字就丢弃重试"的直接后果，非缺陷。若实际环境中上游常态性慢启动（如重推理模型排队），应调大 `CC_NO_OUTPUT_TIMEOUT_MS`（或设 0 禁用）；调大只会让重试更晚介入，不会削弱"网络断了快速重发"的收益。

**计费影响仍未验证**：被丢弃请求的 CC 端计费/cache 写入，本轮真实 key 测试中**未出现任何丢弃重发**（真实请求全部正常），故此项仍无法回答——仅静默 mock 场景下 mock 侧记录到 4 次连接，与 CC 计费无关。如实标记为未验证。

## §29 计费准确性修复（2026-09-21，未提交）

**触发**：用户核对"计费是否准确"，实测发现两个缺陷。

**核对方法**：用生产 `pricing.json`（71 模型）+ 生产账本全量数据，Python 复现 Rust 的 `cost()` 公式逐模型对比。

### 缺陷 1：模型名匹配有盲区（已修）
- **现象**：账本 `qwen3.8-flash`（30 次、435 万 token）在页面上显示"— 无价格"。
- **根因**：`normalize()` 只折叠 ` / \ _` 与空格，**不折叠点号**，且页面名 "Qwen 3.8 Flash" 与客户端名 "qwen3.8-flash" 的**厂商后空格**也不一致。两者归一为 `qwen-3-8-flash` vs `qwen3-8-flash`。
- **修法**：① `normalize` 加入 `.`；② 新增 `match_key`（在 normalize 基础上删掉所有 `-`）作为兜底，使厂商空格不影响匹配；③ `price()` 查找链改为 exact → byId → normalize → 去前缀 normalize → 去前缀 match_key → match_key。仍是内容精确匹配，`gpt-5` 不会匹配 `gpt-5-pro`（有测试守着）。

### 缺陷 2：历史行按"查看时刻"计价（已修）
- **现象**：账本行不存计价时刻，`cost()` 每次用 `utc_minute_of_day()`（当前时刻）。生产库有 **331 行 peak 时段请求**，在 off-peak 打开页面时被按 off-peak 价（约半价）重估。
- **修法**：`minute_of_day_from_iso8601(ts)` 从账本 `ts`（本就存了 UTC 时刻）取该行自己的分钟数；webui 的 models 聚合改为**按 (model, 峰/谷) 分桶**（SQL 用 `strftime` 判峰谷），每桶用该时段价再合并——聚合值无法知道哪些行是 peak，故必须在 SQL 侧分桶。任一分桶无价格则整模型标 `null`（不伪装成免费）。
- **顺带**：修正 `CC_UPSTREAM_TIMEOUT_MS` 在 README 里的错误描述（它只管 connect+write，不是读期限）。

**验证**：
- 单测 +5（`normalize_folds_dots…`、`a_vendor_space_does_not_hide_the_price`、`an_unknown_model_still_does_not_match_a_prefix`、`minute_of_day_is_read_from_the_rows_own_timestamp`、`a_peak_row_is_priced_at_the_peak_rate_when_read_later`）；全量 **175 lib + 集成 全绿**；clippy `-D warnings` 净；fmt 净。
- **真实生产库实测**（只读，端口 8902）：
  - `qwen3.8-flash` 从"— 无价格" → **$0.105431**；
  - 总计 $0.9715 → **$1.5230**（差额 = Qwen 费用 + 331 行 peak 按 peak 价）；
  - 数字不再随查看时刻浮动。

**仍未验证/限制**：
- `cacheCreationTokens` 在生产库全为 NULL（CC 实际很少产生该 token），§23 的缓存创建计费在生产中未获真实检验。
- 峰谷仅覆盖 4 个 DeepSeek 模型；其余模型 flat。
- 匹配仍是启发式（名称相似度），页面若再改命名风格可能又出现新的匹配缺口。
