use crate::config::Config;
use crate::credential::oauth;
use crate::credential::types::Account;
use crate::gateway::manager::quota::group_quota_key;
use crate::quota_pool::QuotaPoolManager;
use crate::util::id;
use anyhow::{Context, anyhow};
use chrono::{Datelike, Utc};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// 连续刷新失败上限：达到后自动禁用账号（内存计数，重启清零）。
const MAX_REFRESH_FAILURES: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshSessionOutcome {
    /// 刷新成功并已落盘。
    Refreshed,
    /// 已有并发刷新在进行，本次跳过。
    SkippedAlreadyRefreshing,
    /// 账号已禁用，本次跳过。
    SkippedDisabled,
    /// 达到失败阈值并已自动禁用账号。
    DisabledAfterFailures,
}

#[derive(Debug)]
pub struct Store {
    file_path: PathBuf,
    state: RwLock<State>,
    cfg: Config,
    refreshing_sessions: Arc<Mutex<HashSet<String>>>,
    save_lock: Arc<Mutex<()>>,
    // 刷新失败计数（仅内存，服务重启后清零）
    refresh_failures: Arc<Mutex<HashMap<String, u32>>>,
}

#[derive(Debug, Default, Clone)]
struct State {
    accounts: Vec<Account>,
    current_index: usize,
}

impl Store {
    pub fn new(cfg: Config) -> Self {
        let file_path = PathBuf::from(&cfg.data_dir).join("accounts.json");
        Self {
            file_path,
            state: RwLock::new(State::default()),
            cfg,
            refreshing_sessions: Arc::new(Mutex::new(HashSet::new())),
            save_lock: Arc::new(Mutex::new(())),
            refresh_failures: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn load(&self) -> anyhow::Result<()> {
        ensure_parent_dir(&self.file_path).await?;

        let data = match tokio::fs::read(&self.file_path).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut state = self.state.write().await;
                state.accounts.clear();
                state.current_index = 0;
                return Ok(());
            }
            Err(e) => return Err(e).context("读取 accounts.json 失败"),
        };

        let mut accounts: Vec<Account> = match sonic_rs::from_slice(&data) {
            Ok(v) => v,
            Err(e) => {
                let mut state = self.state.write().await;
                state.accounts.clear();
                state.current_index = 0;
                return Err(anyhow!(e)).context("解析 accounts.json 失败");
            }
        };

        for a in &mut accounts {
            a.session_id = id::session_id();
        }

        let mut state = self.state.write().await;
        state.accounts = accounts;
        state.current_index = 0;

        Ok(())
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        // 串行化 accounts.json 的写入，避免并发写导致“后写覆盖先写”而丢失更新。
        let _guard = self.save_lock.lock().await;
        let snapshot = { self.state.read().await.accounts.clone() };
        self.write_accounts(&snapshot).await
    }

    pub async fn get_token(&self) -> anyhow::Result<Account> {
        let len = { self.state.read().await.accounts.len() };
        if len == 0 {
            return Err(anyhow!("没有可用的账号"));
        }

        let now_ms = Utc::now().timestamp_millis();
        for _ in 0..len {
            let account = {
                let mut state = self.state.write().await;
                let len = state.accounts.len();
                if len == 0 {
                    return Err(anyhow!("没有可用的账号"));
                }
                let idx = state.current_index;
                state.current_index = (state.current_index + 1) % len;
                state.accounts[idx].clone()
            };

            if !account.enable {
                continue;
            }

            if account.is_expired(now_ms) {
                tracing::debug!(
                    session_id = %account.session_id,
                    "账号 token 已接近过期（或已过期），将由后台任务刷新"
                );
            }

            return Ok(account);
        }

        Err(anyhow!("没有可用的 token"))
    }

    /// 按模型选择一个账号：
    /// 1) 优先从 QuotaPoolManager 对应的分组池中挑选（更倾向剩余配额更高的账号）
    /// 2) 若该分组没有配额数据/挑选失败，则退化为原有轮询策略（向后兼容）
    ///
    /// 参数 `exclude` 指定应跳过的 sessionId（用于网关重试时换 token）。
    pub async fn get_token_for_model_excluding(
        &self,
        model: &str,
        pool_mgr: &QuotaPoolManager,
        exclude: &HashSet<String>,
    ) -> anyhow::Result<Account> {
        let model = model.trim();
        if model.is_empty() {
            return self.get_token_excluding(exclude).await;
        }

        let pool_name = group_quota_key(model);

        // 防御：避免因池中存在陈旧 sessionId 导致反复命中同一个无效账号。
        for _ in 0..3 {
            let Some(session_id) = pool_mgr
                .get_account_for_pool_excluding(pool_name, exclude)
                .await
            else {
                break;
            };

            let Some((_idx, account)) = self.find_by_session_id(&session_id).await else {
                tracing::warn!(
                    session_id = session_id,
                    "配额池命中但 Store 未找到账号，已清理"
                );
                pool_mgr.remove_session(&session_id).await;
                continue;
            };

            if exclude.contains(&account.session_id) {
                // 理论上不会出现（上面已排除），但保持健壮性。
                continue;
            }
            if !account.enable {
                tracing::info!(session_id = session_id, "配额池命中但账号已禁用，已清理");
                pool_mgr.remove_session(&session_id).await;
                continue;
            }

            let now_ms = Utc::now().timestamp_millis();
            if account.is_expired(now_ms) {
                tracing::debug!(
                    session_id = session_id,
                    "账号 token 已接近过期（或已过期），将由后台任务刷新"
                );
            }

            return Ok(account);
        }

        // 没有配额池数据或无法使用：退化为原有轮询策略（同时尊重 exclude）。
        self.get_token_excluding(exclude).await
    }

    /// 轮询挑选账号，但会跳过 `exclude` 中的 sessionId。
    pub async fn get_token_excluding(&self, exclude: &HashSet<String>) -> anyhow::Result<Account> {
        let len = { self.state.read().await.accounts.len() };
        if len == 0 {
            return Err(anyhow!("没有可用的账号"));
        }

        let now_ms = Utc::now().timestamp_millis();
        for _ in 0..len {
            let account = {
                let mut state = self.state.write().await;
                let len = state.accounts.len();
                if len == 0 {
                    return Err(anyhow!("没有可用的账号"));
                }
                let idx = state.current_index;
                state.current_index = (state.current_index + 1) % len;
                state.accounts[idx].clone()
            };

            if exclude.contains(&account.session_id) {
                continue;
            }
            if !account.enable {
                continue;
            }

            if account.is_expired(now_ms) {
                tracing::debug!(
                    session_id = %account.session_id,
                    "账号 token 已接近过期（或已过期），将由后台任务刷新"
                );
            }

            return Ok(account);
        }

        Err(anyhow!("没有可用的 token"))
    }

    pub async fn get_token_by_project_id(&self, project_id: &str) -> anyhow::Result<Account> {
        let project_id = project_id.trim();
        if project_id.is_empty() {
            return Err(anyhow!("projectId 为空"));
        }

        let account = {
            let state = self.state.read().await;
            let mut found: Option<Account> = None;
            for a in &state.accounts {
                if a.enable && a.project_id == project_id {
                    found = Some(a.clone());
                    break;
                }
            }
            found.ok_or_else(|| anyhow!("未找到指定的账号"))?
        };

        let now_ms = Utc::now().timestamp_millis();
        if account.is_expired(now_ms) {
            tracing::debug!(
                session_id = %account.session_id,
                "指定 projectId 的账号 token 已接近过期（或已过期），将由后台任务刷新"
            );
        }
        Ok(account)
    }

    /// 触发后台刷新（非阻塞，fire-and-forget）
    /// 用于在收到 401 错误时异步刷新失效凭证。
    pub fn trigger_background_refresh(self: &Arc<Self>, session_id: String, cfg: Config) {
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            tracing::debug!("session_id 为空，跳过后台刷新");
            return;
        }

        let store = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = store.refresh_session(session_id.clone(), cfg).await {
                tracing::warn!(session_id = session_id, error = ?e, "后台刷新账号 token 失败");
            }
        });
    }

    /// 记录刷新失败次数（内存计数，服务重启后清零）。
    pub async fn record_refresh_failure(&self, session_id: &str) -> u32 {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return 0;
        }

        let mut failures = self.refresh_failures.lock().await;
        let count = failures.entry(session_id.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }

    /// 刷新成功后清除失败计数。
    pub async fn clear_refresh_failure(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }

        let mut failures = self.refresh_failures.lock().await;
        failures.remove(session_id);
    }

    /// 通过 session_id 禁用账号（用于连续失败后熔断）。
    pub async fn disable_by_session_id(&self, session_id: &str) -> anyhow::Result<()> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Ok(());
        }

        let mut changed = false;
        {
            let mut state = self.state.write().await;
            for acc in &mut state.accounts {
                if acc.session_id == session_id {
                    if acc.enable {
                        acc.enable = false;
                        changed = true;
                    }
                    break;
                }
            }
        }

        if changed {
            self.save().await?;
        }
        Ok(())
    }

    /// 刷新指定账号 token（内部会做去重，避免同一账号并发刷新）。
    ///
    /// 注意：该方法可能执行网络请求与磁盘写入，请仅用于后台任务。
    pub(crate) async fn refresh_session(
        self: &Arc<Self>,
        session_id: String,
        cfg: Config,
    ) -> anyhow::Result<RefreshSessionOutcome> {
        let session_id = session_id.trim().to_string();
        if session_id.is_empty() {
            return Err(anyhow!("session_id 为空"));
        }

        {
            let mut refreshing = self.refreshing_sessions.lock().await;
            if refreshing.contains(&session_id) {
                tracing::debug!(session_id = session_id, "账号正在刷新，跳过重复刷新");
                return Ok(RefreshSessionOutcome::SkippedAlreadyRefreshing);
            }
            refreshing.insert(session_id.clone());
        }

        let result = async {
            let Some((idx, mut account)) = self.find_by_session_id(&session_id).await else {
                return Err(anyhow!("未找到指定 session_id 对应的账号"));
            };

            if !account.enable {
                tracing::info!(session_id = %session_id, "账号已禁用，跳过 token 刷新");
                return Ok(RefreshSessionOutcome::SkippedDisabled);
            }

            match oauth::refresh_token(&cfg, &mut account).await {
                Ok(()) => {
                    self.replace_account(idx, &account).await?;
                    self.save().await?;
                    self.clear_refresh_failure(&session_id).await;
                    Ok(RefreshSessionOutcome::Refreshed)
                }
                Err(e) => {
                    let failures = self.record_refresh_failure(&session_id).await;
                    tracing::warn!(
                        session_id = %session_id,
                        failures = failures,
                        error = ?e,
                        "账号 token 刷新失败"
                    );

                    if failures >= MAX_REFRESH_FAILURES {
                        tracing::error!(
                            session_id = %session_id,
                            failures = failures,
                            "连续刷新失败次数达到阈值，自动禁用账号"
                        );
                        if let Err(disable_err) = self.disable_by_session_id(&session_id).await {
                            tracing::error!(
                                session_id = %session_id,
                                error = ?disable_err,
                                "自动禁用账号失败"
                            );
                        }
                        // 账号已被禁用：不再向上层返回错误，避免无意义重试。
                        return Ok(RefreshSessionOutcome::DisabledAfterFailures);
                    }

                    Err(e)
                }
            }
        }
        .await;

        {
            let mut refreshing = self.refreshing_sessions.lock().await;
            refreshing.remove(&session_id);
        }

        result
    }

    pub async fn get_all(&self) -> Vec<Account> {
        let state = self.state.read().await;
        state.accounts.clone()
    }

    pub async fn count(&self) -> usize {
        let state = self.state.read().await;
        state.accounts.len()
    }

    pub async fn enabled_count(&self) -> usize {
        let state = self.state.read().await;
        state.accounts.iter().filter(|a| a.enable).count()
    }

    pub async fn clear(&self) -> anyhow::Result<()> {
        {
            let mut state = self.state.write().await;
            state.accounts.clear();
            state.current_index = 0;
        }
        self.save().await
    }

    pub async fn add(&self, mut account: Account) -> anyhow::Result<()> {
        account.session_id = id::session_id();
        if account.created_at.year() == 1 {
            account.created_at = Utc::now();
        }

        {
            let mut state = self.state.write().await;
            let mut replaced = false;
            for existing in &mut state.accounts {
                if (!account.email.is_empty() && existing.email == account.email)
                    || (!account.refresh_token.is_empty()
                        && existing.refresh_token == account.refresh_token)
                {
                    // 保留原始 created_at
                    account.created_at = existing.created_at;
                    *existing = account.clone();
                    replaced = true;
                    break;
                }
            }

            if !replaced {
                state.accounts.push(account);
            }
        }

        self.save().await
    }

    pub async fn delete(&self, index: usize) -> anyhow::Result<()> {
        {
            let mut state = self.state.write().await;
            if index >= state.accounts.len() {
                return Err(anyhow!("索引超出范围"));
            }
            state.accounts.remove(index);
            if state.current_index >= state.accounts.len() {
                state.current_index = 0;
            }
        }
        self.save().await
    }

    pub async fn set_enable(&self, index: usize, enable: bool) -> anyhow::Result<()> {
        {
            let mut state = self.state.write().await;
            if index >= state.accounts.len() {
                return Err(anyhow!("索引超出范围"));
            }
            state.accounts[index].enable = enable;
        }
        self.save().await
    }

    pub async fn refresh_account(&self, index: usize) -> anyhow::Result<()> {
        let mut account = {
            let state = self.state.read().await;
            if index >= state.accounts.len() {
                return Err(anyhow!("索引超出范围"));
            }
            state.accounts[index].clone()
        };

        oauth::refresh_token(&self.cfg, &mut account).await?;
        self.replace_account(index, &account).await?;
        self.save().await?;
        self.clear_refresh_failure(&account.session_id).await;
        Ok(())
    }

    pub async fn refresh_all(&self) -> anyhow::Result<(usize, usize)> {
        let accounts = self.get_all().await;
        let mut updated = Vec::with_capacity(accounts.len());
        let mut success = 0usize;
        let mut failed = 0usize;

        for mut a in accounts {
            let sid = a.session_id.clone();
            match oauth::refresh_token(&self.cfg, &mut a).await {
                Ok(()) => {
                    success += 1;
                    self.clear_refresh_failure(&sid).await;
                    updated.push(a);
                }
                Err(_) => {
                    failed += 1;
                    updated.push(a);
                }
            }
        }

        {
            let mut state = self.state.write().await;
            state.accounts = updated;
            state.current_index = 0;
        }
        self.save().await?;
        Ok((success, failed))
    }

    async fn replace_account(&self, index: usize, account: &Account) -> anyhow::Result<()> {
        let mut state = self.state.write().await;
        if index >= state.accounts.len() {
            return Err(anyhow!("索引超出范围"));
        }
        state.accounts[index] = account.clone();
        Ok(())
    }

    async fn find_by_session_id(&self, session_id: &str) -> Option<(usize, Account)> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return None;
        }
        let state = self.state.read().await;
        state
            .accounts
            .iter()
            .enumerate()
            .find(|(_, a)| a.session_id == session_id)
            .map(|(i, a)| (i, a.clone()))
    }

    async fn write_accounts(&self, accounts: &[Account]) -> anyhow::Result<()> {
        ensure_parent_dir(&self.file_path).await?;
        let data = sonic_rs::to_vec_pretty(accounts).context("序列化 accounts.json 失败")?;
        tokio::fs::write(&self.file_path, data)
            .await
            .context("写入 accounts.json 失败")
    }
}

async fn ensure_parent_dir(path: &Path) -> anyhow::Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    tokio::fs::create_dir_all(dir)
        .await
        .context("创建数据目录失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::types::Account;
    use crate::quota_pool::QuotaPoolManager;

    fn test_cfg(data_dir: String) -> Config {
        Config {
            host: "127.0.0.1".to_string(),
            port: 0,
            api_user_agent: "ant2api-test".to_string(),
            timeout_ms: 1_000,
            proxy: String::new(),
            api_key: String::new(),
            retry_status_codes: vec![429, 500],
            retry_max_attempts: 3,
            debug: "off".to_string(),
            endpoint_mode: "production".to_string(),
            google_client_id: String::new(),
            google_client_secret: String::new(),
            data_dir,
            webui_password: String::new(),
            gemini3_media_resolution: String::new(),
        }
    }

    fn temp_data_dir() -> String {
        let mut dir = std::env::temp_dir();
        dir.push(format!("ant2api-test-{}", uuid::Uuid::new_v4()));
        dir.to_string_lossy().to_string()
    }

    fn expired_account(project_id: &str) -> Account {
        Account {
            access_token: "expired".to_string(),
            refresh_token: String::new(),
            expires_in: 0,
            timestamp: 0,
            project_id: project_id.to_string(),
            email: "test@example.com".to_string(),
            enable: true,
            created_at: Utc::now(),
            session_id: String::new(),
        }
    }

    #[tokio::test]
    async fn get_token_returns_even_if_expired() {
        let data_dir = temp_data_dir();
        let store = Store::new(test_cfg(data_dir.clone()));
        store.add(expired_account("")).await.unwrap();

        let got = store.get_token().await.unwrap();
        assert_eq!(got.access_token, "expired");
        assert!(!got.session_id.trim().is_empty());

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }

    #[tokio::test]
    async fn get_token_excluding_returns_even_if_expired() {
        let data_dir = temp_data_dir();
        let store = Store::new(test_cfg(data_dir.clone()));
        store.add(expired_account("")).await.unwrap();

        let got = store.get_token_excluding(&HashSet::new()).await.unwrap();
        assert_eq!(got.access_token, "expired");

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }

    #[tokio::test]
    async fn get_token_by_project_id_returns_even_if_expired() {
        let data_dir = temp_data_dir();
        let store = Store::new(test_cfg(data_dir.clone()));
        store.add(expired_account("p1")).await.unwrap();

        let got = store.get_token_by_project_id("p1").await.unwrap();
        assert_eq!(got.project_id, "p1");
        assert_eq!(got.access_token, "expired");

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }

    #[tokio::test]
    async fn get_token_for_model_excluding_returns_even_if_expired() {
        let data_dir = temp_data_dir();
        let store = Store::new(test_cfg(data_dir.clone()));
        store.add(expired_account("")).await.unwrap();

        let pool = QuotaPoolManager::new();
        let got = store
            .get_token_for_model_excluding("test-model", &pool, &HashSet::new())
            .await
            .unwrap();
        assert_eq!(got.access_token, "expired");

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }

    #[tokio::test]
    async fn refresh_session_disables_after_five_failures() {
        let data_dir = temp_data_dir();
        let cfg = test_cfg(data_dir.clone());
        let store = Arc::new(Store::new(cfg.clone()));
        store.add(expired_account("")).await.unwrap();

        let acc = store.get_token().await.unwrap();
        let sid = acc.session_id.clone();

        // refresh_token 为空会导致刷新直接失败（不触发网络请求），用于稳定测试失败计数逻辑。
        assert!(acc.refresh_token.trim().is_empty());

        for _ in 0..(MAX_REFRESH_FAILURES - 1) {
            assert!(
                store
                    .refresh_session(sid.clone(), cfg.clone())
                    .await
                    .is_err()
            );
        }

        let out = store
            .refresh_session(sid.clone(), cfg.clone())
            .await
            .unwrap();
        assert_eq!(out, RefreshSessionOutcome::DisabledAfterFailures);

        let all = store.get_all().await;
        let got = all.iter().find(|a| a.session_id == sid).unwrap();
        assert!(!got.enable);

        let out = store
            .refresh_session(sid.clone(), cfg.clone())
            .await
            .unwrap();
        assert_eq!(out, RefreshSessionOutcome::SkippedDisabled);

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }
}
