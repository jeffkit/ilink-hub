# ilink-hub 架构深度评审报告

> 评审日期：2026-06-14  
> 评审范围：全量代码（Rust + Axum + SQLx + Tokio）  
> 项目定位：微信 iLink 协议多路复用网关（Hub）

---

## 问题汇总

| 级别 | ID | 问题简述 | 位置 |
|------|----|----------|------|
| Blocker | B-1 | 无鉴权模式下 vtoken 实质性暴露 | `src/server/routes.rs:619–655` |
| High | H-1 | 内联 DDL 迁移无版本管理、错误被静默吞掉 | `src/store/mod.rs:93–191` |
| High | H-2 | secondary HashMap 无界增长（内存泄漏） | `src/hub/queue.rs:47–165` |
| High | H-3 | 拼接 SQL 占位符，MySQL 下静默失效 | `src/store/mod.rs:453–538` |
| High | H-4 | async handler 中 std Mutex 的 Drop 含 unwrap panic 路径 | `src/relay/server.rs:169–225` |
| High | H-5 | async 上下文中使用阻塞 std RwLock，含 poison panic 路径 | `src/ilink/upstream.rs:36–115` |
| Medium | M-1 | sendmessage 对同一 key 执行两次相同 DB 查询 | `src/server/routes.rs:408–468` |
| Medium | M-2 | Relay Server 无 graceful shutdown | `src/relay/server.rs:354–365` |
| Medium | M-3 | SessionDispatcher 使用无界 channel | `src/bridge/mod.rs:177–229` |
| Medium | M-4 | RateLimiter 每次请求 O(N) 扫描 | `src/relay/ratelimit.rs:28–47` |
| Medium | M-5 | CORS 使用 permissive()，内网仍有跨域探测风险 | `src/server/mod.rs:22` |
| Medium | M-6 | Prometheus metrics 手动字符串拼接，缺乏类型安全 | `src/server/routes.rs:926–1077` |
| Nit | N-1 | seed_record 职责不单一，难测试 | `src/hub/queue.rs:96–165` |
| Nit | N-2 | handle_hub_command 300 行 match 无提取，单元测试盲区 | `src/hub/mod.rs:619–907` |
| Nit | N-3 | run_cli 两个超时叠加最坏 2× timeout，未文档化 | `src/bridge/mod.rs:509–533` |
| Nit | N-4 | ClientInfo.registered_at 用 Instant，重启后审计数据丢失 | `src/hub/registry.rs:8–31` |

---

## Blocker

### B-1: 无鉴权模式下 vtoken 实质性暴露

**位置**: `src/server/routes.rs:619–655`

**问题描述**

`admin_clients` 接口返回的 vtoken 仅截断为 8 字符，但 vtoken 格式是 `"vhub_" + UUID.simple()`（共 37 字符），8 字符仅剩约 13 bit 熵，并无实质保护。

更严重的是：

- `POST /hub/register` 会向任何可访问该端口的进程返回**完整 vtoken**
- `ILINK_ADMIN_INSECURE_NO_AUTH=true` 时无任何身份验证
- README 的 Docker Compose 示例**直接暴露 8765 端口**，未将 `ILINK_ADMIN_TOKEN` 设为必填
- 攻击者只需能访问该端口即可注册客户端并拿到有效 vtoken，进而长轮询拦截全部微信消息

**解决方案**

1. Docker Compose 示例中将 `ILINK_ADMIN_TOKEN` 设为非可选环境变量，并加注释说明
2. README 显著标注警告：无鉴权模式下，任何可访问端口的进程均可获取全部消息
3. 对 `no_auth` 模式的 register 接口增加来源 IP 白名单校验（可选）

---

## High

### H-1: 内联 DDL 迁移无版本管理、无回滚、错误被静默吞掉

**位置**: `src/store/mod.rs:93–191`

**问题描述**

`run_migrations` 函数直接执行裸 SQL 语句，所有 ALTER 的错误被 `let _ = self.ddl(...)` 丢弃：

- 没有记录当前已执行到哪个版本，新加 v5 迁移时无法判断线上是否已跑过 v4
- `ALTER TABLE ... ADD COLUMN` 在 SQLite 上静默 no-op，在 MySQL 上报错但被吞掉，导致 schema 不一致却无任何报警
- 现有代码注释引用了 `migrations/` 目录，但实际并未使用

**解决方案**

使用 `sqlx::migrate!` 替代手写 DDL：

```rust
// src/store/mod.rs
sqlx::migrate!("./migrations").run(&pool).await?;
```

迁移文件按序命名：

```
migrations/
  0001_init.sql
  0002_add_session_table.sql
  0003_add_context_token_map.sql
  0004_add_created_at.sql
```

sqlx 自动创建 `_sqlx_migrations` 表记录已执行版本，支持幂等重跑。

---

### H-2: ContextTokenMap 的两个 secondary HashMap 无界增长（内存泄漏）

**位置**: `src/hub/queue.rs:47–165`

**问题描述**

`ContextTokenMapInner` 包含三个结构：

```rust
v_to_record: LruCache<String, CtxRecord>,  // 有界，50,000 上限 ✅
real_to_v:   HashMap<String, String>,       // 无界 ❌
conv_to_v:   HashMap<String, String>,       // 无界 ❌
```

LRU 驱逐时 `remove_secondary` 负责清理 secondary map，但存在以下竞态导致清理不完整：

```
1. Entry A (real_token=r1) 被 LRU 驱逐，real_to_v[r1] 被删除
2. Entry B 以相同 real_token=r1 插入，real_to_v[r1] = B
3. 此后 A 对应的旧 vtoken 在 secondary 已指向 B，永远不会被清理
```

**运营影响**：经历数万名微信用户的账号，`conv_to_v` 和 `real_to_v` 会持续增长，而 Prometheus 的 `ilink_hub_ctx_map_size` 只监控 `v_to_record.len()`，**操作者看到指标正常，实则内存已泄漏**。

**解决方案**

将两个 secondary map 也换成 `LruCache`，容量与主 LRU 一致：

```rust
use lru::LruCache;

struct ContextTokenMapInner {
    v_to_record: LruCache<String, CtxRecord>,
    real_to_v:   LruCache<String, String>,   // cap = MAX_CTX_MAP_ENTRIES
    conv_to_v:   LruCache<String, String>,   // cap = MAX_CTX_MAP_ENTRIES
}
```

同时在 Prometheus 中分别暴露三个 map 的 `len()`，而非仅暴露主 LRU。

---

### H-3: get_hub_ext_batch 拼接 SQL 占位符，MySQL 下静默失效

**位置**: `src/store/mod.rs:453–538`

**问题描述**

代码通过 `format!` 循环拼接 `$1, $2, $3...` 形式的占位符：

- PostgreSQL 使用 `$N`，SQLite 的 AnyPool 兼容，但 **MySQL 只支持 `?`**，在 MySQL 上静默失败
- 绕过了 `sqlx::query!` 的编译期类型检查
- 参数数量大时可能超出数据库的参数数量上限

**解决方案**

使用 `QueryBuilder` 处理方言差异：

```rust
use sqlx::QueryBuilder;

let mut qb = QueryBuilder::new(
    "SELECT vctx, vtoken, ext FROM hub_ext WHERE (vctx, vtoken) IN ("
);
let mut sep = qb.separated(", ");
for (vctx, vtoken) in &pairs {
    sep.push("(");
    sep.push_bind(vctx.as_str());
    sep.push(", ");
    sep.push_bind(vtoken.as_str());
    sep.push(")");
}
qb.push(")");
let rows = qb.build().fetch_all(&self.pool).await?;
```

---

### H-4: async handler 中 std::sync::Mutex 的 Drop 含 unwrap()，有 panic 风险

**位置**: `src/relay/server.rs:169–225`

**问题描述**

`state.pending` 是 `Arc<std::sync::Mutex<HashMap<...>>>`，`PendingRequestGuard` 的 `Drop` 实现中调用 `lock().unwrap()`：

- 若某个 task panic 导致 mutex 被 poison，Drop 再次触发 `unwrap()` panic
- 双 panic 在 Rust 中会直接 abort 进程，跳过所有 cleanup

**解决方案**

替换为 `DashMap`（与仓库内 `InMemoryQueue` 的 `slots` 设计保持一致）：

```rust
use dashmap::DashMap;

pending: Arc<DashMap<String, oneshot::Sender<RelayResponse>>>,

// 插入
state.pending.insert(req_id.clone(), tx);

// 移除（Drop 中无需 lock）
state.pending.remove(&req_id);
```

---

### H-5: async 上下文中使用阻塞 std::sync::RwLock，含 poison panic 路径

**位置**: `src/ilink/upstream.rs:36–115`

**问题描述**

`UpstreamClient::token` 是标准库 `RwLock<String>`，在异步函数中直接 `.read().expect("poisoned")`：

- 任何持有写锁的线程 panic → lock 被 poison → 所有后续异步请求 panic
- 在 async context 中惯例应使用无 poison 语义的同步原语

**解决方案**

使用 `arc-swap` 实现读无锁、写原子替换：

```rust
use arc_swap::ArcSwap;

pub struct UpstreamClient {
    token: ArcSwap<String>,
    // ...
}

// 读取（无锁）
fn headers(&self) -> HeaderMap {
    let token = self.token.load();
    // 使用 token.as_ref()
}

// 更新
pub fn set_token(&self, new_token: String) {
    self.token.store(Arc::new(new_token));
}
```

---

## Medium

### M-1: sendmessage 对同一 key 执行两次相同 DB 查询

**位置**: `src/server/routes.rs:408–468`

**问题描述**

`get_active_session_name(&vctx, &vtoken)` 在同一请求中被调用两次，第一次结果未复用。在 SQLite 单连接池下，每条消息需两次串行 DB 读，广播风暴时加剧连接池竞争。

**解决方案**

```rust
// 修复：缓存第一次查询结果
let session_name = replied_session_name
    .or_else(|| store.get_active_session_name(&vctx, &vtoken).ok().flatten());
// 后续直接使用 session_name，不再重复查询
```

---

### M-2: Relay Server 无 graceful shutdown

**位置**: `src/relay/server.rs:354–365`

**问题描述**

`axum::serve(...).await?` 没有 `with_graceful_shutdown`，SIGTERM 时 in-flight 的 `(code, confirm)` 请求对静默丢失。Hub 的 `run_serve` 有完整 graceful shutdown，relay 应对齐。

**解决方案**

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(shutdown_signal())
    .await?;

async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
}
```

---

### M-3: SessionDispatcher 使用无界 channel，消息可无限积压

**位置**: `src/bridge/mod.rs:177–229`

**问题描述**

每个 session worker 从 `mpsc::unbounded_channel()` 接收消息。当 Claude Code 处理耗时任务时，用户持续发消息会导致 channel 缓冲区无限增长。Hub 自身有 `DEFAULT_MAX_QUEUE_SIZE=200` 的背压设计，bridge 层应对齐。

**解决方案**

```rust
// 替换为有界 channel
let (tx, rx) = mpsc::channel(200);

// 发送时处理背压
match tx.try_send(msg) {
    Ok(_) => {},
    Err(TrySendError::Full(_)) => {
        warn!("session queue full, dropping message");
        // 或返回错误给上游
    }
    Err(TrySendError::Closed(_)) => { /* session 已关闭 */ }
}
```

---

### M-4: RateLimiter 每次请求 O(N) 扫描

**位置**: `src/relay/ratelimit.rs:28–47`

**问题描述**

`bucket.retain(|t| ...)` 每次都扫描整个时间戳 Vec（最多 `WS_RATE_MAX=120` 条），应改为 O(1) 的滑动窗口计数器。

**解决方案**

```rust
struct Bucket {
    count: u32,
    window_start: Instant,
}

impl Bucket {
    fn is_allowed(&mut self, max: u32, window: Duration) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= window {
            self.count = 0;
            self.window_start = now;
        }
        if self.count < max {
            self.count += 1;
            true
        } else {
            false
        }
    }
}
```

---

### M-5: CORS 使用 permissive()，内网部署仍有跨域探测风险

**位置**: `src/server/mod.rs:22`

**问题描述**

`CorsLayer::permissive()` 允许 `*` 来源，内网部署时恶意页面可跨域探测 `/ilink/bot/getupdates`。

**解决方案**

```rust
// 替换为显式白名单
CorsLayer::new()
    .allow_origin(AllowOrigin::list([
        "http://localhost:3000".parse::<HeaderValue>().unwrap(),
        // 其他已知来源
    ]))
    .allow_methods([Method::GET, Method::POST])
    .allow_headers([CONTENT_TYPE, AUTHORIZATION])
```

或通过环境变量 `ILINK_CORS_ORIGINS` 配置允许的来源列表。

---

### M-6: Prometheus metrics 手动字符串拼接，缺乏类型安全

**位置**: `src/server/routes.rs:926–1077`

**问题描述**

手动拼接 `# HELP`、`# TYPE`、指标行。若 label value 含换行符或 `{` 会产生非法 Prometheus 文本格式，且随着指标增多维护成本线性增长。

**解决方案**

引入 `prometheus` crate：

```toml
# Cargo.toml
prometheus = "0.13"
```

```rust
use prometheus::{Counter, Gauge, Registry, TextEncoder};

// 注册指标
lazy_static! {
    static ref REGISTRY: Registry = Registry::new();
    static ref MSG_COUNTER: Counter = Counter::new("ilink_hub_messages_total", "...").unwrap();
}

// 输出
let encoder = TextEncoder::new();
let mut buffer = String::new();
encoder.encode_utf8(&REGISTRY.gather(), &mut buffer)?;
```

---

## Nit

### N-1: seed_record 职责不单一，难以测试

**位置**: `src/hub/queue.rs:96–165`

`seed_record` 同时处理四种 case（real_changed × conv_changed 的笛卡尔积），嵌套可变借用难以追踪。建议拆分为独立的 `update_real_index` 和 `update_conv_index` 私有方法，并对每个 case 添加单元测试。

---

### N-2: handle_hub_command 300 行 match 无提取，单元测试盲区

**位置**: `src/hub/mod.rs:619–907`

每个 `HubCommand` 分支是内联 async 块，无法独立测试。建议将每个分支提取为具名异步函数：

```rust
async fn handle_session_list(state: &HubState, vctx: &str, vtoken: &str) -> CommandResult { ... }
async fn handle_send_message(state: &HubState, req: SendMessageReq) -> CommandResult { ... }
```

`handle_hub_command` 本身退化为纯路由分发，圈复杂度从 ~15 降至 1。

---

### N-3: run_cli 的两个超时叠加，最坏情况 2× timeout，未文档化

**位置**: `src/bridge/mod.rs:509–533`

stdin write 和 `wait_with_output` 分别使用 `cfg.timeout_secs`，最坏总等待时间为 `2 × timeout_secs`。应在 `BridgeProfile` 字段注释中明确说明，或将两个超时分别配置为 `stdin_timeout_secs` 和 `exec_timeout_secs`。

---

### N-4: ClientInfo.registered_at 使用 Instant，重启后审计数据丢失

**位置**: `src/hub/registry.rs:8–31`

`Instant` 是进程本地、不可序列化的时间点。重启后 `registered_at` 信息永久丢失，admin 面板无法显示准确的注册时间。

**解决方案**：将 `registered_at` 改为 `chrono::DateTime<Utc>` 并持久化到 DB（`hub_clients` 表已有 `last_seen` 字段，增加 `registered_at` 列成本极低）。

---

## 修复优先级

| 优先级 | 问题 | 理由 |
|--------|------|------|
| **立即** | H-2 内存泄漏 | 长期运行必现，Prometheus 监控不可见 |
| **立即** | B-1 安全暴露 | 生产部署面临完整消息泄露风险 |
| **本迭代** | H-1 迁移版本管理 | 防止多环境 schema 漂移 |
| **本迭代** | H-4 / H-5 锁问题 | 消除进程级 panic 风险 |
| **下迭代** | H-3 SQL 方言兼容 | MySQL 支持完整性 |
| **技术债** | M-2 graceful shutdown | 防止重启时消息丢失 |
| **技术债** | M-3 有界 channel | 防止 bridge 层内存压力 |
| **技术债** | N-2 测试覆盖 | 提升 hub command 可维护性 |

---

> 如需对某个问题展开讨论或提供完整 PR diff，请指定问题 ID。
