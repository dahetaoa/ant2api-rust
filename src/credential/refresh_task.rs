use crate::config::Config;
use crate::credential::store::{RefreshSessionOutcome, Store};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// 刷新失败后的重试间隔（递增）。
const RETRY_DELAYS: [u64; 5] = [10, 30, 60, 120, 300]; // 10秒, 30秒, 1分钟, 2分钟, 5分钟
/// 单轮最多尝试次数（达到后本轮结束；失败计数在 Store 内累计，成功会清零）。
const MAX_ATTEMPTS_PER_CYCLE: u32 = 5;
/// 提前刷新的时间（毫秒）。
const REFRESH_BEFORE_EXPIRY_MS: i64 = 5 * 60 * 1000; // 5分钟
/// 最大并发刷新数量（避免对 OAuth 端点造成突发压力）。
const MAX_CONCURRENT_REFRESHES: usize = 3;

/// 启动后台 Token 刷新任务（主动刷新层）。
///
/// - 动态调度：根据 token 的实际过期时间计算下一次检查时间
/// - 失败重试：刷新失败时按递增间隔重试
/// - 自动禁用：连续失败达到阈值后由 Store 自动禁用账号
pub fn spawn_token_refresh_task(store: Arc<Store>, cfg: Config) {
    tokio::spawn(async move {
        loop {
            let sleep_duration = match schedule_refresh_cycle(store.clone(), cfg.clone()).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = ?e, "后台 token 刷新任务执行失败");
                    Duration::from_secs(60)
                }
            };

            // 至少等 1 秒，最多等 30 分钟（兜底：避免极端数据导致过长 sleep）。
            let sleep_duration = sleep_duration
                .max(Duration::from_secs(1))
                .min(Duration::from_secs(30 * 60));

            tracing::debug!("下次 token 刷新检查将在 {:?} 后", sleep_duration);
            tokio::time::sleep(sleep_duration).await;
        }
    });
}

/// 执行一轮刷新，并返回下次应检查的等待时间。
async fn schedule_refresh_cycle(store: Arc<Store>, cfg: Config) -> anyhow::Result<Duration> {
    let accounts = store.get_all().await;
    if accounts.is_empty() {
        return Ok(Duration::from_secs(5 * 60));
    }

    let now_ms = Utc::now().timestamp_millis();
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_REFRESHES));
    let mut handles = Vec::new();

    for acc in accounts {
        if !acc.enable {
            continue;
        }

        let session_id = acc.session_id.trim().to_string();
        if session_id.is_empty() {
            continue;
        }

        // 计算此账号应该刷新的时间点（过期前 5 分钟）。
        let expires_at_ms = acc.timestamp + (acc.expires_in as i64) * 1000;
        let should_refresh_at_ms = expires_at_ms - REFRESH_BEFORE_EXPIRY_MS;

        if now_ms >= should_refresh_at_ms {
            let store = store.clone();
            let cfg = cfg.clone();
            let semaphore = semaphore.clone();
            handles.push(tokio::spawn(async move {
                refresh_with_retry(store, session_id, cfg, semaphore).await;
            }));
        }
    }

    // 等待本轮触发的刷新任务完成（包含其内部重试）。
    for h in handles {
        let _ = h.await;
    }

    // 刷新结束后，基于最新账号信息计算“最早需要刷新”的时间点。
    let accounts = store.get_all().await;
    let now_ms = Utc::now().timestamp_millis();
    let mut next_refresh_at_ms: Option<i64> = None;

    for acc in accounts {
        if !acc.enable {
            continue;
        }
        if acc.session_id.trim().is_empty() {
            continue;
        }

        let expires_at_ms = acc.timestamp + (acc.expires_in as i64) * 1000;
        let should_refresh_at_ms = expires_at_ms - REFRESH_BEFORE_EXPIRY_MS;

        // 仍然满足“应立即刷新”的条件：尽快再检查一次（可能是并发刷新跳过/数据未更新）。
        if now_ms >= should_refresh_at_ms {
            next_refresh_at_ms = Some(now_ms + 1000);
            break;
        }

        next_refresh_at_ms = Some(match next_refresh_at_ms {
            None => should_refresh_at_ms,
            Some(t) => t.min(should_refresh_at_ms),
        });
    }

    let wait_ms = match next_refresh_at_ms {
        Some(t) => (t - now_ms).max(1000),
        None => 5 * 60 * 1000,
    };

    Ok(Duration::from_millis(wait_ms as u64))
}

/// 带重试的刷新逻辑（重试间隔递增）。
async fn refresh_with_retry(
    store: Arc<Store>,
    session_id: String,
    cfg: Config,
    semaphore: Arc<Semaphore>,
) {
    for attempt in 0..MAX_ATTEMPTS_PER_CYCLE {
        // 仅在实际刷新调用期间占用并发配额，避免把“sleep 等待”也算进并发限制里。
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let res = store.refresh_session(session_id.clone(), cfg.clone()).await;
        drop(permit);

        match res {
            Ok(RefreshSessionOutcome::Refreshed) => {
                tracing::info!(session_id = %session_id, "定时刷新 access_token 成功");
                return;
            }
            Ok(RefreshSessionOutcome::SkippedAlreadyRefreshing) => {
                tracing::debug!(session_id = %session_id, "账号正在刷新，定时刷新跳过");
                return;
            }
            Ok(RefreshSessionOutcome::SkippedDisabled) => {
                tracing::info!(session_id = %session_id, "账号已禁用，定时刷新跳过");
                return;
            }
            Ok(RefreshSessionOutcome::DisabledAfterFailures) => {
                tracing::error!(session_id = %session_id, "连续刷新失败已达到阈值，账号已被禁用");
                return;
            }
            Err(_) => {
                if attempt + 1 >= MAX_ATTEMPTS_PER_CYCLE {
                    tracing::warn!(
                        session_id = %session_id,
                        attempt = attempt + 1,
                        "定时刷新失败，已达到本轮最大重试次数"
                    );
                    return;
                }
                let delay_secs = RETRY_DELAYS.get(attempt as usize).copied().unwrap_or(300);
                tracing::info!(
                    session_id = %session_id,
                    attempt = attempt + 1,
                    delay_secs = delay_secs,
                    "定时刷新失败，将在 {delay_secs} 秒后重试"
                );
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            }
        }
    }
}
