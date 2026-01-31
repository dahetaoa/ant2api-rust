//! 选择算法：Power of Two Choices（两次随机抽样取更优者）。
//!
//! 该策略在高并发下能在"接近最优"与"低开销"之间取得很好的平衡。

use crate::quota_pool::types::PoolEntry;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

thread_local! {
    /// 轻量 PRNG：每线程一个 state，避免锁与频繁分配。
    static RNG_STATE: Cell<u64> = Cell::new(seed());
}

fn seed() -> u64 {
    // 以 uuid v4 作为随机种子（仅在首次初始化线程本地 state 时调用一次）。
    let u = uuid::Uuid::new_v4().as_u128();
    let mut s = (u as u64) ^ ((u >> 64) as u64);
    if s == 0 {
        // 避免 xorshift 的零种子退化。
        s = 0x9E37_79B9_7F4A_7C15;
    }
    s
}

fn next_u64() -> u64 {
    RNG_STATE.with(|state| {
        // xorshift64*
        let mut x = state.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        state.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

fn random_usize(upper: usize) -> usize {
    if upper <= 1 {
        return 0;
    }
    (next_u64() as usize) % upper
}

fn random_pair_distinct(n: usize) -> (usize, usize) {
    // n >= 2
    let i1 = random_usize(n);
    let j = random_usize(n - 1);
    let i2 = if j >= i1 { j + 1 } else { j };
    (i1, i2)
}

/// 提取 PoolEntry 的 remaining_fraction，非有限值视为 0。
#[inline]
fn fraction_or_zero(e: &PoolEntry) -> f64 {
    if e.remaining_fraction.is_finite() {
        e.remaining_fraction
    } else {
        0.0
    }
}

/// 比较两个候选账号，返回配额更高者的 session_id。
fn pick_higher_fraction<'a>(
    sid_a: &'a String,
    e_a: &PoolEntry,
    sid_b: &'a String,
    e_b: &PoolEntry,
) -> &'a String {
    if fraction_or_zero(e_a) >= fraction_or_zero(e_b) {
        sid_a
    } else {
        sid_b
    }
}

/// 从 active 中选择一个 sessionId，但会跳过 exclude 内的 sessionId。
pub fn select_weighted_excluding(
    active: &HashMap<String, PoolEntry>,
    exclude: &HashSet<String>,
) -> Option<String> {
    // 快速路径：无需排除时直接在整个 active 上操作
    if exclude.is_empty() {
        return select_from_map(active, |_| true);
    }

    select_from_map(active, |sid| !exclude.contains(sid))
}

/// 通用的两选一选择逻辑，通过 filter 决定哪些 session_id 有效。
fn select_from_map<F>(active: &HashMap<String, PoolEntry>, filter: F) -> Option<String>
where
    F: Fn(&String) -> bool,
{
    // 计算有效候选数量
    let candidate_len = active.keys().filter(|sid| filter(sid)).count();

    if candidate_len == 0 {
        return None;
    }
    if candidate_len == 1 {
        return active.keys().find(|sid| filter(sid)).cloned();
    }

    let (i1, i2) = random_pair_distinct(candidate_len);
    let mut a: Option<(&String, &PoolEntry)> = None;
    let mut b: Option<(&String, &PoolEntry)> = None;

    let mut idx = 0usize;
    for (sid, entry) in active.iter() {
        if !filter(sid) {
            continue;
        }
        if idx == i1 {
            a = Some((sid, entry));
        } else if idx == i2 {
            b = Some((sid, entry));
        }
        if a.is_some() && b.is_some() {
            break;
        }
        idx += 1;
    }

    match (a, b) {
        (Some((sid_a, e_a)), Some((sid_b, e_b))) => {
            Some(pick_higher_fraction(sid_a, e_a, sid_b, e_b).clone())
        }
        (Some((sid, _)), None) | (None, Some((sid, _))) => Some(sid.clone()),
        _ => active.keys().find(|sid| filter(sid)).cloned(),
    }
}
