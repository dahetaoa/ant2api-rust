use crate::config::Config;
use crate::credential::oauth;
use crate::credential::types::Account;
use crate::gateway::manager::quota::group_quota_key;
use crate::quota_pool::QuotaPoolManager;
use crate::util::id;
use anyhow::{Context, anyhow};
use chrono::{Datelike, Utc};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

#[derive(Debug)]
pub struct Store {
    file_path: PathBuf,
    state: RwLock<State>,
    cfg: Config,
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
        let snapshot = {
            let state = self.state.read().await;
            state.accounts.clone()
        };
        self.save_snapshot(&snapshot).await
    }

    pub async fn get_token(&self) -> anyhow::Result<Account> {
        let len = { self.state.read().await.accounts.len() };
        if len == 0 {
            return Err(anyhow!("没有可用的账号"));
        }

        let now_ms = Utc::now().timestamp_millis();
        for _ in 0..len {
            let (idx, mut account) = {
                let mut state = self.state.write().await;
                let len = state.accounts.len();
                if len == 0 {
                    return Err(anyhow!("没有可用的账号"));
                }
                let idx = state.current_index;
                state.current_index = (state.current_index + 1) % len;
                (idx, state.accounts[idx].clone())
            };

            if !account.enable {
                continue;
            }

            if account.is_expired(now_ms) {
                if oauth::refresh_token(&self.cfg, &mut account).await.is_err() {
                    continue;
                }
                self.replace_account(idx, &account).await?;
                self.save().await?;
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

            let Some((idx, mut account)) = self.find_by_session_id(&session_id).await else {
                tracing::warn!(session_id = session_id, "配额池命中但 Store 未找到账号，已清理");
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
                match oauth::refresh_token(&self.cfg, &mut account).await {
                    Ok(()) => {
                        self.replace_account(idx, &account).await?;
                        self.save().await?;
                    }
                    Err(e) => {
                        tracing::warn!(session_id = session_id, error = ?e, "账号 token 刷新失败，回退轮询");
                        break;
                    }
                }
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
            let (idx, mut account) = {
                let mut state = self.state.write().await;
                let len = state.accounts.len();
                if len == 0 {
                    return Err(anyhow!("没有可用的账号"));
                }
                let idx = state.current_index;
                state.current_index = (state.current_index + 1) % len;
                (idx, state.accounts[idx].clone())
            };

            if exclude.contains(&account.session_id) {
                continue;
            }
            if !account.enable {
                continue;
            }

            if account.is_expired(now_ms) {
                if oauth::refresh_token(&self.cfg, &mut account).await.is_err() {
                    continue;
                }
                self.replace_account(idx, &account).await?;
                self.save().await?;
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

        let (idx, mut account) = {
            let state = self.state.read().await;
            let mut found: Option<(usize, Account)> = None;
            for (i, a) in state.accounts.iter().enumerate() {
                if a.enable && a.project_id == project_id {
                    found = Some((i, a.clone()));
                    break;
                }
            }
            found.ok_or_else(|| anyhow!("未找到指定的账号"))?
        };

        let now_ms = Utc::now().timestamp_millis();
        if account.is_expired(now_ms) {
            oauth::refresh_token(&self.cfg, &mut account).await?;
            self.replace_account(idx, &account).await?;
            self.save().await?;
        }
        Ok(account)
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

        let snapshot = {
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
            state.accounts.clone()
        };

        self.save_snapshot(&snapshot).await
    }

    pub async fn delete(&self, index: usize) -> anyhow::Result<()> {
        let snapshot = {
            let mut state = self.state.write().await;
            if index >= state.accounts.len() {
                return Err(anyhow!("索引超出范围"));
            }
            state.accounts.remove(index);
            if state.current_index >= state.accounts.len() {
                state.current_index = 0;
            }
            state.accounts.clone()
        };
        self.save_snapshot(&snapshot).await
    }

    pub async fn set_enable(&self, index: usize, enable: bool) -> anyhow::Result<()> {
        let snapshot = {
            let mut state = self.state.write().await;
            if index >= state.accounts.len() {
                return Err(anyhow!("索引超出范围"));
            }
            state.accounts[index].enable = enable;
            state.accounts.clone()
        };
        self.save_snapshot(&snapshot).await
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
        self.save().await
    }

    pub async fn refresh_all(&self) -> anyhow::Result<(usize, usize)> {
        let accounts = self.get_all().await;
        let mut updated = Vec::with_capacity(accounts.len());
        let mut success = 0usize;
        let mut failed = 0usize;

        for mut a in accounts {
            match oauth::refresh_token(&self.cfg, &mut a).await {
                Ok(()) => {
                    success += 1;
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

    async fn save_snapshot(&self, accounts: &[Account]) -> anyhow::Result<()> {
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
