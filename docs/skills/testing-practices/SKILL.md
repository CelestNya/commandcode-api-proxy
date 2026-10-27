---
name: testing-practices
description: cc-proxy 的测试纪律与回归策略——修复先写复现测试、测试分层（unit/integration/golden）、行为命名、失败输出质量、纯核心/副作用分离、异步流边界、unsafe 经安全 API 验证、CI 高信号。用于"写测试、修 bug 补回归、审查测试质量、评估覆盖是否足够"等任务。不用于覆盖率数字攀比（测试数是动态数字，不写进文档）与全量 TDD 铁律（渐进改造型代码库不适用）。
---

# cc-proxy 测试纪律（消纳自 joshka.net/testing 规则集 + affaan-m/ECC rust-testing + obra/superpowers TDD）

从三个来源抽取**与 cc-proxy 现状可对接**的测试模式，丢弃：mockall/DI mock 教学（本项目是纯函数 + env 注入风格）、80% 覆盖率目标（数字会随改动漂移，写进文档即死）、TDD 铁律全流程（RED-GREEN 循环只保留"修复先写复现测试"一条纪律）。

## 适用边界

- 用：新增功能/修复缺陷时的测试设计、测试命名与断言质量、回归保护、异步流边界覆盖、测试与 CI 的衔接。
- 不用：覆盖率百分比指标、mock 框架选型、性能基准（本项目无 benchmark 需求）。

## 回归纪律（最高优先级，从 superpowers + joshka 抽取）

- **修 bug 前先写复现测试**：能复现缺陷的测试先落地并确认它会失败（watched it fail），修复后同一测试转绿。无法复现时记录最接近的可信验证。项目实证：`an_empty_read_means_nothing_new_and_never_resets`（webui.rs）就是先写失败测试再修的回归案例。
- **绝不删除失败的测试来"修好"CI**：测试失败说明契约被破坏，修代码或修断言，不是删测试。
- **修复的交付物必须带测试**：无测试的修复视为未完成（等价于 superpowers 的"没有失败过的测试 = 没证明测试有效"）。

## 测试分层（对照项目现有结构，从 rust-testing 抽取）

| 层 | 位置 | 覆盖对象 | 项目实证 |
|---|---|---|---|
| 单元测试 | `#[cfg(test)] mod tests` | 本地小逻辑：解析器、策略分支、边界 | webui.rs parse_days/parse_limit、log_increment、actions.rs packaged_webui_dir |
| 集成测试 | `tests/*.rs`（每文件独立二进制） | 公共模块/crate 边界组合 | tests/stream_golden.rs（SSE 黄金流）、billing_ledger.rs、translate_golden.rs |
| 共享测试工具 | `tests/common/` 或 `#[cfg(test)]` helper | 跨测试复用 | webui.rs `ledger()` 内嵌内存账本构造器 |

- 新端点/新模块先问：这逻辑属于哪一层？本地小逻辑 → 单元；跨 crate 组合 → 集成。
- 新增端点在 `server.rs` 路由表注册时，检查测试是否同步（端点完整性三处联动：路由 + count_tokens + usage 透传）。

## 命名与断言质量（从 joshka 抽取）

- **按行为/边界/回归命名**，让失败输出在打开测试体之前就有信息量：`rejects_invalid_email` ✓ / `test1` ✗。
- **避免不透明布尔断言**：`assert!(x.is_err())` 够用但要优先带值比较；第一个失败应展示有用的实际值。
- **拆开无关断言**：一个断言诊断一个行为，一个失败不能掩盖另一个。
- **失败输出要可修复**：期望值、实际值、输入、契约上下文都要出现在失败里。
- **测试命名不要用"and"**：一个测试一件事（superpowers 的 Good/Bad 表）。

## 纯核心 / 副作用分离（从 joshka 抽取，项目已是此风格）

- 领域计算与 I/O/渲染分开，测试才有稳定表面：`translate/*` 是纯转换（黄金流测试直接断言字节输出），`server.rs` 只做路由与装配。
- **环境输入显式化**：影响行为的 time/randomness/env/端口都走可见输入——本项目 `PORT`、`CC_*_DIR` env 注入即此模式；测试里用 `PORT=18787` 隔离实例，绝不隐式依赖生产环境。
- 需要控制 host 交互（文件/网络/时间）时在边界注入，核心保持确定性。

## 异步流边界（从 joshka 抽取，本项目 SSE 是重点）

- SSE/流式端点要覆盖：迟到响应、无关输入、超时、未匹配的异步响应（late replies / unrelated input / timeouts / unmatched）。
- 项目实证：`stream_golden.rs` 断言 SSE 事件序列与 usage 字段；`an_increment_read_is_capped_so_a_huge_burst_cannot_stall_the_poll` 覆盖突发流量边界。
- 流式轮询（webui 日志 increment）要测：空读不 reset、尾部残片滞留、旋转截断后全量回退（webui.rs 三个 increment 测试已覆盖）。

## unsafe 经安全 API 验证（从 joshka 抽取，对应 win.rs）

- unsafe 内部通过调用方依赖的安全包装验证，而非只写 unsafe 内部测试。
- 项目模式：`win.rs`/`tray.rs`/`owner.rs` 的 `#[expect(unsafe_code)]` 携带命名不变量；测试走安全 API 证明契约（如 owner.rs 的 `waiting_stops_when_the_predicate_holds`）。
- 新增 unsafe 时：小块、安全 API 包裹、注释不变量、测试经安全面到达。

## CI 高信号（从 joshka 抽取，对照项目现状）

- CI 是 ground-truth 执行环境：本机过了不算数，CI 全绿才算（实证：packaged_webui_dir 测试本机 Windows 过、Linux CI 抓出 canonicalize 平台差异）。
- 质量门保持严格、快、确定：`clippy -D warnings` + `fmt` + `doc -D warnings` 已在 ci.yml；新改动必须过同一套门。
- 慢/抖动检查不进 PR CI：重型验证放 release job（release.yml 的 deb/AppImage/AUR docker makepkg）。

## 反模式表（本地化）

| ID | 严重度 | 反模式 | 对策 |
|---|---|---|---|
| TP-01 | HIGH | 修复无回归测试 | 先写复现测试并确认失败，再修 |
| TP-02 | HIGH | 用删测试/ignore 修 CI | 失败 = 契约被破坏，修代码或修断言 |
| TP-03 | MEDIUM | 断言不透明（只报 predicate false） | 带值比较，失败输出可诊断 |
| TP-04 | MEDIUM | 测试只测本文件不跑全量 | 交付前跑 `cargo test --workspace`，任何失败点名报告 |
| TP-05 | MEDIUM | 平台测试只在本机验证 | 平台差异交给 CI 矩阵（cfg(unix) 模块本机 Windows 编不到） |
| TP-06 | LOW | 覆盖率数字写进文档 | 测试数是动态数字，写文档即过期；用"覆盖了哪些路径"描述 |
| TP-07 | LOW | 测试依赖隐式环境 | env/端口/时间显式注入（PORT=18787 隔离实例） |

## 验证（改动后）

- 本机：`cargo test --workspace`（全量，不只目标文件）+ `cargo clippy --workspace --all-targets -- -D warnings` 零告警。
- 平台：推送后 CI 矩阵全绿（windows + linux），publish tag-only 自动 skip。
- 隔离实例冒烟：`PORT=18787` + 独立数据目录起服务，/health + /webui + 翻译主流程可通，生产目录只读不碰。

## Self-Evolving

- 新增测试层（如 property test）或新端点时，当场更新本 skill 的分层表与三处联动清单。
- 若 CI 质量门调整（新 lint/新检查），更新「CI 高信号」一节。
