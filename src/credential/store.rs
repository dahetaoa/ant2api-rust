use crate::config::Config;
use crate::credential::oauth;
use crate::credential::types::Account;
use crate::util::id;
use anyhow::{Context, anyhow};
use chrono::{Datelike, Utc};
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
