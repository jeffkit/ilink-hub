//! Sprint 1 — 并发正确性测试落地（concurrency correctness integration tests）。
//!
//! Contract：1 个高频 producer 向同一 [`InMemoryQueue`] 实例（同一 vtoken）投递 200 条
//! 全局唯一 `message_id` 消息；4 个 consumer 经 tokio 任务并发 `drain` 拉取。producer
//! 与 4 个 consumer 共享同一并发起跑门闩，投递与拉取在同一时间窗口内竞争。验证
//! 「不重、不丢、消费总数 = 投递总数（队列完全排空）」三个并发不变量。
//!
//! 仅新增本测试文件，不改动任何 `src/` 代码。测试只通过公开 API 使用：
//! [`InMemoryQueue`]、[`MessageQueue`] trait 与 [`WeixinMessage`]。并发计数统计只使用
//! 测试内部的线程安全原语（`Arc<AtomicBool>` 投递完成标记、`tokio::sync::Barrier`
//! 起跑门闩），各 consumer 的结果由独立 `Vec` 收集，join 之后才合并断言。

use ilink_hub::{hub::queue::InMemoryQueue, ilink::types::WeixinMessage, MessageQueue};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// 单一共享 vtoken：producer 与全部 consumer 都读写同一个队列 slot。
const VTOKEN: &str = "sprint1-concurrency";
/// 单个 producer 的投递条数。
const PRODUCED: i64 = 200;
/// 并发 consumer 数量。
const CONSUMERS: usize = 4;
/// 并发段（含起跑门闩、投递、拉取、join）的墙钟兜底上限。200 条消息 + 4 consumer
/// 在毫秒级完成，60s 是极端宽松的取值，仅用于把「挂起/死锁」变成显式失败而非卡死 CI。
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);
/// 队列容量上限。默认 200 恰好等于投递条数：若 producer 瞬时领先 consumer，队列满
/// 会触发「丢最旧」策略，让「不丢」不变量被容量策略污染。放大容量后，并发不变量
/// 只反映并发行为本身，与容量策略解耦；producer 侧仍断言没有任何 push 被丢弃。
const QUEUE_LIMIT: usize = 4096;

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
/// * `per_consumer`：各 consumer 独立汇总的 message_id 集合（尚未合并）。
/// * `produced`：producer 实际投递的全部 message_id。
/// * `remaining`：consumer 结束后收尾 drain 归零得到的剩余消息。
fn assert_invariants(per_consumer: &[Vec<i64>], produced: &[i64], remaining: &[i64]) {
    // ── C2 投递侧：message_id 两两互不相同 ──
    let produced_set: BTreeSet<i64> = produced.iter().copied().collect();
    assert_eq!(
        produced_set.len(),
        produced.len(),
        "producer must push pairwise-distinct message_ids; duplicates: {:?}",
        find_dups(produced)
    );
    assert_eq!(produced.len(), PRODUCED as usize);

    // ── C4 不重：无重复消费 ──
    let merged: Vec<i64> = per_consumer.iter().flatten().copied().collect();
    let sum_of_counts: usize = per_consumer.iter().map(Vec::len).sum();
    assert_eq!(
        merged.len(),
        sum_of_counts,
        "merged list length must equal the sum of per-consumer counts"
    );
    let distinct: BTreeSet<i64> = merged.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        sum_of_counts,
        "no message may be consumed twice; duplicate ids: {:?}",
        find_dups(&merged)
    );
    // 各 consumer 结果两两交集为空。
    for i in 0..per_consumer.len() {
        for j in (i + 1)..per_consumer.len() {
            let a: BTreeSet<i64> = per_consumer[i].iter().copied().collect();
            let b: BTreeSet<i64> = per_consumer[j].iter().copied().collect();
            let overlap: Vec<i64> = a.intersection(&b).copied().collect();
            assert!(
                overlap.is_empty(),
                "consumers {i} and {j} both consumed: {overlap:?}"
            );
        }
    }

    // ── C5 不丢：已消费集合 ∪ 收尾剩余集合 == 全部投递 ──
    let remaining_set: BTreeSet<i64> = remaining.iter().copied().collect();
    let union: BTreeSet<i64> = distinct.union(&remaining_set).copied().collect();
    assert_eq!(
        union.len(),
        produced_set.len(),
        "consumed ∪ remaining must cover every produced id"
    );
    for id in &produced_set {
        assert!(
            union.contains(id),
            "produced id {id} missing from consumed ∪ remaining"
        );
    }

    // ── C6 计数闭合：消费总数 == 投递总数，收尾后剩余为 0 ──
    assert_eq!(
        merged.len(),
        PRODUCED as usize,
        "total consumed must equal total produced (200)"
    );
    assert!(
        remaining.is_empty(),
        "after the final drain the queue must be empty, got {} remaining",
        remaining.len()
    );
}

/// 单个 producer：先通过起跑门闩，再向共享队列连续投递恰好 `PRODUCED` 条全局唯一
/// `message_id`，投递完成后置位「投递完成」标记。任一 push 因容量满被丢弃则直接失败。
async fn producer_loop(
    q: Arc<dyn MessageQueue>,
    start: Arc<Barrier>,
    done: Arc<AtomicBool>,
    base: i64,
) -> Vec<i64> {
    start.wait().await;
    let mut pushed = Vec::with_capacity(PRODUCED as usize);
    for i in 0..PRODUCED {
        let dropped = q
            .push(VTOKEN, msg(base + i))
            .await
            .expect("push must not error");
        assert!(
            !dropped,
            "queue overflowed: push for id {} was dropped (capacity policy interfered)",
            base + i
        );
        pushed.push(base + i);
    }
    done.store(true, Ordering::SeqCst);
    pushed
}

/// 单个 consumer：先通过起跑门闩，再循环 `drain`，直到「队列为空 且 producer 已完成
/// 全部投递」才退出；退出前不得仅凭队列空态判断（避免 producer 尚未投完时误判为空）。
async fn consumer_loop(
    q: Arc<dyn MessageQueue>,
    start: Arc<Barrier>,
    done: Arc<AtomicBool>,
) -> Vec<i64> {
    start.wait().await;
    let mut seen = Vec::new();
    loop {
        let batch = q.drain(VTOKEN).await.expect("drain must not error");
        if batch.is_empty() {
            if done.load(Ordering::SeqCst) {
                return seen;
            }
            // producer 仍在投递：让出调度点，避免无意义的忙等。
            tokio::task::yield_now().await;
            continue;
        }
        seen.extend(batch.iter().map(mid));
    }
}

/// 跑一轮完整场景：起跑门闩同步 → producer 投递 200 条 + 4 consumer 并发 drain →
/// join 全部任务 → 收尾 drain 归零 → 守恒断言。`base` 用于给每轮分配互不重叠的
/// message_id 区间（复用同一队列跨轮压测时避免碰撞）。
async fn run_one_round(q: Arc<dyn MessageQueue>, base: i64) {
    let done = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(1 + CONSUMERS));

    // ── C2：单一 producer 任务，与 consumer 共享同一起跑门闩 ──
    let producer = {
        let q = q.clone();
        let start = start.clone();
        let done = done.clone();
        tokio::spawn(producer_loop(q, start, done, base))
    };

    // ── C3：4 个 consumer 任务共享同一队列 Arc / 同一 vtoken，各自独立汇总 ──
    let mut consumers: Vec<JoinHandle<Vec<i64>>> = Vec::with_capacity(CONSUMERS);
    for _ in 0..CONSUMERS {
        let q = q.clone();
        let start = start.clone();
        let done = done.clone();
        consumers.push(tokio::spawn(consumer_loop(q, start, done)));
    }

    // 先 join producer（保证「投递完成」前置成立），再 join 全部 consumer。
    let produced_ids = producer.await.expect("producer task must not panic");
    let mut per_consumer = Vec::with_capacity(CONSUMERS);
    for h in consumers {
        per_consumer.push(h.await.expect("consumer task must not panic"));
    }

    // ── C5/C6：全部 consumer 退出后收尾 drain，确认队列被完全排空 ──
    let remaining: Vec<i64> = q
        .drain(VTOKEN)
        .await
        .expect("final drain must not error")
        .iter()
        .map(mid)
        .collect();

    assert_invariants(&per_consumer, &produced_ids, &remaining);
}

/// C2–C6 主场景：1 个 producer 与 4 个 consumer 共享同一队列实例、同一 vtoken 与
/// 同一起跑门闩，producer 高频投递 200 条全局唯一 message_id，4 个 consumer 并发
/// drain。全程有墙钟超时兜底，超时即显式失败而非挂起。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_producer_four_consumers_no_dup_no_loss() {
    let q: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::with_limit(QUEUE_LIMIT));
    timeout(SCENARIO_TIMEOUT, run_one_round(q, 0))
        .await
        .unwrap_or_else(|_| panic!("并发场景未在 {SCENARIO_TIMEOUT:?} 内完成（疑似挂起/死锁）"));
}

/// C6 补强：复用同一队列实例与同一 vtoken 连跑 20 轮，逐轮完整验证不变量，
/// 保证结果不因并发调度时序产生 flake，且队列每轮都能被完全排空。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_reused_queue_stable_across_rounds() {
    const ROUNDS: usize = 20;
    let q: Arc<dyn MessageQueue> = Arc::new(InMemoryQueue::with_limit(QUEUE_LIMIT));
    for round in 0..ROUNDS {
        // 每轮使用全局唯一的 id 区间，避免跨轮碰撞。
        let base = round as i64 * PRODUCED;
        timeout(SCENARIO_TIMEOUT, run_one_round(q.clone(), base))
            .await
            .unwrap_or_else(|_| {
                panic!("第 {round} 轮未在 {SCENARIO_TIMEOUT:?} 内完成（疑似挂起/死锁）")
            });
    }
}
