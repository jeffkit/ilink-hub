//! Sprint 1 — 并发正确性测试（投递语义改为 at-least-once 后的重写版）。
//!
//! **本轮重写说明**：原版约束「1 个 producer → 同一 vtoken，4 个 consumer 共享
//! 同一 slot 并发 `drain`，无重复消费」。该不变量正是旧破坏性 `drain` 的前提，
//! 与 at-least-once 直接冲突：非破坏性 `poll` 下，同一个 slot 被并发读必然会把
//! 同一批消息投给多个 consumer（重复投递是 ack 语义的固有代价，见 issue #27）。
//! 因此该不变量被**删除**，改为按投递语义真正成立的不变量：
//!
//! Contract：1 个高频 producer 向 `CONSUMERS` 个 vtoken（**per-client 扇出**）各投
//! `PRODUCED` 条全局唯一 `message_id` 的消息，且每个 client 的 id 区间互不重叠；
//! 每个 consumer 只 `poll` 自己的 vtoken 并回带游标（下一轮 ack）。producer 与全部
//! consumer 共享同一起跑门闩，投递与拉取在同一时间窗口内竞争。验证：
//!
//! - **client 内不重不丢**：每个 consumer 恰好看到自己的 `PRODUCED` 条；
//! - **跨 client 无串扰**：没有任何 consumer 收到别人的 id；
//! - **全量守恒**：全部投递 id 出现且仅出现一次；
//! - **收尾归零**：全部批次被 ack 后 `queue_sizes()` 全为 0。
//!
//! 仅新增/改写本测试文件，不改动任何 `src/` 代码。测试只通过公开 API 使用：
//! [`InMemoryQueue`]、[`MessageQueue`] trait 与 [`WeixinMessage`]。

use ilink_hub::{hub::queue::InMemoryQueue, ilink::types::WeixinMessage, MessageQueue};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// 单个 client 的投递条数。
const PRODUCED: i64 = 200;
/// 并发 consumer（= vtoken）数量。
const CONSUMERS: usize = 4;
/// 并发段（含起跑门闩、投递、拉取、join）的墙钟兜底上限。消息量在毫秒级完成，
/// 60s 是极端宽松的取值，仅用于把「挂起/死锁」变成显式失败而非卡死 CI。
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);
/// 队列容量上限。默认 200 恰好等于单 client 投递条数：若 producer 瞬时领先
/// consumer，队列满会触发背压拒绝，让「不丢」不变量被容量策略污染。放大容量后，
/// 并发不变量只反映并发行为本身，与容量策略解耦；producer 侧仍断言没有任何 push
/// 被背压拒绝。
const QUEUE_LIMIT: usize = 4096;

/// 第 `i` 个 client 独占的 vtoken。
fn vtoken(i: usize) -> String {
    format!("sprint1-concurrency-{i}")
}

/// 第 `i` 个 client 独占的 id 区间起点（区间互不重叠）。
fn id_base(i: usize, round_base: i64) -> i64 {
    round_base + i as i64 * PRODUCED
}

/// 构造一条携带全局唯一 `message_id` 的消息。
fn msg(id: i64) -> WeixinMessage {
    WeixinMessage {
        message_id: Some(id),
        from_user_id: Some("sprint1-producer".to_string()),
        ..Default::default()
    }
}

/// 从消息中取出统计键 `message_id`。
fn mid(m: &WeixinMessage) -> i64 {
    m.message_id
        .expect("every produced message must carry a message_id")
}

/// 找出 `all` 中出现超过一次的 id（失败时的可读诊断输出）。
fn find_dups(all: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::new();
    let mut dups = HashSet::new();
    for &id in all {
        if !seen.insert(id) {
            dups.insert(id);
        }
    }
    let mut v: Vec<i64> = dups.into_iter().collect();
    v.sort_unstable();
    v
}

/// 校验一轮消费结果满足全部并发不变量。
///
/// * `per_consumer`：各 consumer 独立汇总的 message_id（尚未合并）。
/// * `produced`：producer 实际投递的全部 message_id（按 client 分组）。
fn assert_invariants(per_consumer: &[Vec<i64>], produced: &[Vec<i64>]) {
    // ── C2 投递侧：全局 id 两两互不相同 ──
    let all_produced: Vec<i64> = produced.iter().flatten().copied().collect();
    assert_eq!(
        all_produced.len(),
        CONSUMERS * PRODUCED as usize,
        "producer must push PRODUCED messages per client"
    );
    let produced_set: BTreeSet<i64> = all_produced.iter().copied().collect();
    assert_eq!(
        produced_set.len(),
        all_produced.len(),
        "producer must push globally distinct message_ids; duplicates: {:?}",
        find_dups(&all_produced)
    );

    // ── C4 client 内不重、恰好覆盖自己的区间 ──
    for (i, (got, expected)) in per_consumer.iter().zip(produced.iter()).enumerate() {
        let got_set: BTreeSet<i64> = got.iter().copied().collect();
        assert_eq!(
            got_set.len(),
            got.len(),
            "client {i} consumed a message twice; duplicate ids: {:?}",
            find_dups(got)
        );
        assert_eq!(
            got.len(),
            PRODUCED as usize,
            "client {i} must consume exactly {PRODUCED} messages, got {}",
            got.len()
        );
        let expected_set: BTreeSet<i64> = expected.iter().copied().collect();
        for id in got {
            assert!(
                expected_set.contains(id),
                "client {i} received id {id}, which belongs to another client (cross-talk)"
            );
        }
        assert_eq!(
            got_set, expected_set,
            "client {i} must see exactly its own id range"
        );
    }

    // ── C5/C6 全量守恒：每个投递 id 出现且仅出现一次 ──
    let merged: Vec<i64> = per_consumer.iter().flatten().copied().collect();
    let distinct: BTreeSet<i64> = merged.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        merged.len(),
        "no message may be consumed twice; duplicate ids: {:?}",
        find_dups(&merged)
    );
    assert_eq!(
        merged.len(),
        CONSUMERS * PRODUCED as usize,
        "total consumed must equal total produced"
    );
    assert_eq!(
        distinct, produced_set,
        "consumed set must cover every produced id exactly once"
    );
}

/// 单个 producer：先通过起跑门闩，再向每个 client 自己的 vtoken 连续投递恰好
/// `PRODUCED` 条**全局唯一** `message_id`（每个 client 的区间互不重叠），投递完成
/// 后置位「投递完成」标记。任一 push 因容量满被背压拒绝则直接失败。
async fn producer_loop(
    q: Arc<dyn MessageQueue>,
    start: Arc<Barrier>,
    done: Arc<AtomicBool>,
    round_base: i64,
) -> Vec<Vec<i64>> {
    start.wait().await;
    let mut pushed = Vec::with_capacity(CONSUMERS);
    for i in 0..CONSUMERS {
        let base = id_base(i, round_base);
        let vt = vtoken(i);
        let mut per_client = Vec::with_capacity(PRODUCED as usize);
        for k in 0..PRODUCED {
            let rejected = q
                .push(&vt, msg(base + k))
                .await
                .expect("push must not error");
            assert!(
                !rejected,
                "queue overflowed: push for id {} was rejected (capacity policy interfered)",
                base + k
            );
            per_client.push(base + k);
        }
        pushed.push(per_client);
    }
    done.store(true, Ordering::SeqCst);
    pushed
}

/// 单个 consumer：先通过起跑门闩，再循环 `poll(自己的 vtoken, 上一轮游标)` ——
/// 回带游标即确认上一批，因此同一 client 内每条消息只会被汇总一次，直到
/// 「返回空批 且 producer 已完成全部投递」才退出（必须保留 done 判据，否则
/// producer 未投完就误判为空）。
///
/// 读到 `done == true` 后不能立即返回：poll 与读 done 之间虽无 await 点，仍可被
/// OS 抢占——抢占期间 producer 可能投完剩余消息并置位 done，consumer 恢复后直接
/// 退出就会把这些从未 poll 到的消息留在队列里。因为 done 只在全部 push 之后置位，
/// 读到 `done == true` 即意味着所有 push 已可见，再做一次确认性 poll 若仍为空才
/// 可判定队列已排空；非空则把该批计入 seen 继续。
async fn consumer_loop(
    q: Arc<dyn MessageQueue>,
    start: Arc<Barrier>,
    done: Arc<AtomicBool>,
    vt: String,
) -> Vec<i64> {
    start.wait().await;
    let mut seen = Vec::new();
    let mut cursor: Option<u64> = None;
    loop {
        let batch = q.poll(&vt, cursor).await.expect("poll must not error");
        cursor = Some(batch.cursor);
        if batch.is_empty() {
            if !done.load(Ordering::SeqCst) {
                // producer 仍在投递：让出调度点，避免无意义的忙等。
                tokio::task::yield_now().await;
                continue;
            }
            let confirm = q.poll(&vt, cursor).await.expect("poll must not error");
            cursor = Some(confirm.cursor);
            if confirm.is_empty() {
                return seen;
            }
            seen.extend(confirm.msgs.iter().map(mid));
            continue;
        }
        seen.extend(batch.msgs.iter().map(mid));
    }
}

/// 跑一轮完整场景：起跑门闩同步 → producer 向 4 个 vtoken 各投 200 条 +
/// 4 个 consumer 并发 poll 自己的 slot 并回带游标 → join 全部任务 →
/// 收尾断言队列全部归零 → 守恒断言。`round_base` 用于给每轮分配互不重叠的
/// message_id 区间（复用同一队列跨轮压测时避免碰撞）。
async fn run_one_round(q: Arc<dyn MessageQueue>, round_base: i64) {
    let done = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(1 + CONSUMERS));

    // ── C2：单一 producer 任务，与 consumer 共享同一起跑门闩 ──
    let producer = {
        let q = q.clone();
        let start = start.clone();
        let done = done.clone();
        tokio::spawn(producer_loop(q, start, done, round_base))
    };

    // ── C3：CONSUMERS 个 consumer 各自独占一个 vtoken ──
    let mut consumers: Vec<JoinHandle<Vec<i64>>> = Vec::with_capacity(CONSUMERS);
    for i in 0..CONSUMERS {
        let q = q.clone();
        let start = start.clone();
        let done = done.clone();
        consumers.push(tokio::spawn(consumer_loop(q, start, done, vtoken(i))));
    }

    // 先 join producer（保证「投递完成」前置成立），再 join 全部 consumer。
    let produced_ids = producer.await.expect("producer task must not panic");
    let mut per_consumer = Vec::with_capacity(CONSUMERS);
    for h in consumers {
        per_consumer.push(h.await.expect("consumer task must not panic"));
    }

    // ── C6：所有 consumer 退出后队列必须完全归零（全部批次已 ack）──
    let sizes = q.queue_sizes().await.expect("queue_sizes must not error");
    let total: usize = sizes.values().sum();
    assert_eq!(
        total, 0,
        "every delivered batch must be acknowledged; queue_sizes={sizes:?}"
    );

    assert_invariants(&per_consumer, &produced_ids);
}

/// C2–C6 主场景：1 个 producer + 4 个 consumer（各自独占 vtoken），producer 高频
/// 向每个 client 投递 200 条全局唯一 message_id，consumer 并发 poll 自己的 slot
/// 并回带游标确认。全程有墙钟超时兜底，超时即显式失败而非挂起。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_client_fanout_four_consumers_no_dup_no_loss() {
    let q: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::with_limit(QUEUE_LIMIT));
    timeout(SCENARIO_TIMEOUT, run_one_round(q, 0))
        .await
        .unwrap_or_else(|_| panic!("并发场景未在 {SCENARIO_TIMEOUT:?} 内完成（疑似挂起/死锁）"));
}

/// C6 补强：复用同一队列实例连跑 20 轮，逐轮完整验证不变量，保证结果不因并发
/// 调度时序产生 flake，且每轮都能把全部批次确认干净（队列归零）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_reused_queue_stable_across_rounds() {
    const ROUNDS: usize = 20;
    let q: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::with_limit(QUEUE_LIMIT));
    for round in 0..ROUNDS {
        // 每轮使用全局唯一的 id 区间，避免跨轮碰撞。
        let round_base = round as i64 * PRODUCED * CONSUMERS as i64;
        timeout(SCENARIO_TIMEOUT, run_one_round(q.clone(), round_base))
            .await
            .unwrap_or_else(|_| {
                panic!("第 {round} 轮未在 {SCENARIO_TIMEOUT:?} 内完成（疑似挂起/死锁）")
            });
    }
}
