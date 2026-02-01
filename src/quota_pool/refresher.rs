//! 后台刷新任务：周期性拉取各账号配额并更新 QuotaPoolManager。

use crate::config::Config;
use crate::credential::store::Store;
use crate::gateway::common::auth_retry::is_auth_failure;
use crate::gateway::manager::quota::group_quota_groups;
use crate::quota_pool::QuotaPoolManager;
use crate::runtime_config;
use crate::vertex::client::VertexClient;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// 启动后台刷新任务。
///
/// - 间隔：10 分钟
/// - 单账号拉取：默认 0.2 秒间隔（避免对后端造成压力）
pub fn spawn_refresh_task(
    store: Arc<Store>,
    cfg: Config,
    vertex: VertexClient,
    pool_mgr: Arc<QuotaPoolManager>,
) {
    tokio::spawn(async move {
        // 启动后立即执行一次，尽快填充配额池；随后按周期刷新。
        loop {
            if let Err(e) = refresh_once(store.clone(), &cfg, &vertex, pool_mgr.clone()).await {
                tracing::warn!("配额池后台刷新失败：{e:#}");
            }
            tokio::time::sleep(Duration::from_secs(10 * 60)).await;
        }
    });
}

async fn refresh_once(
    store: Arc<Store>,
    cfg: &Config,
    vertex: &VertexClient,
    pool_mgr: Arc<QuotaPoolManager>,
) -> anyhow::Result<()> {
    let endpoint = runtime_config::current_endpoint();
    let accounts = store.get_all().await;
    if accounts.is_empty() {
        pool_mgr.sync_valid_sessions(&HashSet::new()).await;
        return Ok(());
    }

    // 仅保留启用账号；禁用账号立刻从池中移除，避免被选中。
    let mut enabled_sessions: HashSet<String> = HashSet::new();
    for a in &accounts {
        if a.enable && !a.session_id.trim().is_empty() {
            enabled_sessions.insert(a.session_id.clone());
        }
    }
    pool_mgr.sync_valid_sessions(&enabled_sessions).await;

    let due = pool_mgr.due_cooldown_sessions().await;
    if !due.is_empty() {
        tracing::info!("配额池：发现 {} 个冷却到期账号，准备刷新", due.len());
    }

    let mut ok = 0usize;
    let mut failed = 0usize;

    for acc in accounts {
        if !acc.enable {
            continue;
        }
        let sid = acc.session_id.trim();
        if sid.is_empty() {
            continue;
        }

        let project_id = if acc.project_id.trim().is_empty() {
            crate::util::id::project_id()
        } else {
            acc.project_id.clone()
        };

        match vertex
            .fetch_available_models(&endpoint, &project_id, &acc.access_token, &acc.email)
            .await
        {
            Ok(resp) => {
                let groups = group_quota_groups(&resp.models);
                pool_mgr.update_from_quota(sid, &groups).await;
                ok += 1;
            }
            Err(e) => {
                // 认证失败：触发后台刷新，但不阻塞配额刷新流程。
                if is_auth_failure(&e) {
                    store.trigger_background_refresh(acc.session_id.clone(), cfg.clone());
                }
                failed += 1;
                tracing::warn!(session_id = sid, error = ?e, "刷新账号配额失败");
            }
        }

        // 限速：每秒最多 5 个账号请求
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tracing::info!("配额池后台刷新完成：成功 {ok}，失败 {failed}");
    Ok(())
}
