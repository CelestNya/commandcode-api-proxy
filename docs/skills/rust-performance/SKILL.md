---
name: rust-performance
description: cc-proxy（Rust）性能优化方法论——先测量后优化、瓶颈定位优先级、分配/缓存/并行化手段、基准与剖析工具链（criterion/perf/flamegraph）。用于"优化代理性能、降低延迟、减少分配/内存、排查慢请求"等任务。
---

# cc-proxy Rust 性能优化（消纳自 actionbook/rust-skills `m10-performance`）

从 rust-skills 的 m10-performance 抽取**方法论 + 工具链 + 优先级**，丢弃其 meta-cognition 框架与领域扩展（本仓库不需要认知分层路由）。

## 适用边界

- 用：性能瓶颈定位、基准测试、热路径优化（分配/缓存/并行）、release 配置核对。
- 不用：架构级解耦（见 `cross-platform-architecture`）、并发模型选择（本项目是 tiny_http 多线程阻塞模型，`m07-concurrency` 不适用）。

## 铁律：先测量，再优化

1. **Have you measured?** 未剖析就优化 = 优化错误目标。工具：`cargo bench`（微基准）、`criterion`（统计基准）、`perf`/`flamegraph`（CPU 剖析）、`heaptrack`（分配追踪）。
2. **优化优先级**（收益递减）：算法（10x-1000x）→ 数据结构（2x-10x）→ 分配（2x-5x）→ 缓存（1.5x-3x）→ SIMD/并行（2x-8x）。本项目是 IO 密集代理，**先确认热点在 CPU（serde 序列化/SSE 解析/JSON 组装）还是 IO（网络/上游延迟）**，后者优化代码无意义。
3. **接受标准**：可接受的延迟/内存预算是什么？优化引入的复杂度值不值？

## 与 cc-proxy 现状对照

| 项 | 现状 | 行动 |
|---|---|---|
| release profile | 已有：`opt-level="s"` + `lto` + `codegen-units=1` + `panic="abort"` + `strip`（体积优先） | **保持**；若追求吞吐可评估 `opt-level=3` 的折中，需实测 |
| 基准测试 | **缺失**（无 `[dev-dependencies] criterion`，无 `benches/`） | 建议：为 SSE 解析、JSON 翻译、billing 聚合加基准，锁定回归 |
| 热路径检查 | 未系统做 | 排查 `generate.rs`/`stream_body.rs`/`translate/` 的隐藏 `.clone()`、`String` 循环拼接、`HashMap` 当小集合 |
| webui 聚合 | stats/attempts 每请求开 SQLite 连接 + JSON 组装 | 已在合理范围（读连接 WAL）；若变慢优先看查询索引而非优化代码 |

## 常见错误（本项目语境）

- **debug 模式 benchmark**——一律 `--release` 测。
- **优化掩盖真实问题**：上游慢/网络抖动时先查链路（见 `api-traffic-analysis`），别在代理代码里找不存在的热点。
- **热路径隐藏 clone**：`translate/` 的请求/响应重组是每请求执行——`.clone()`/`to_string()` 有真实成本。
- **LinkedList 等缓存不友好结构**——用 `Vec`/`VecDeque`。

## 验证

- `cargo bench --locked`（若已加 criterion）：数值变化可回溯。
- flamegraph/perf：热点确实在改动处。
- 改动后跑 `cargo test -p ccproxy --lib`（220+ 测试回归）与隔离实例实测。

## Self-Evolving

- 若加了 criterion 基准，把「缺失」改为「已有」并记录套件名。
- 若实测发现某处热点与本文判断不符，当场更新对照表。
