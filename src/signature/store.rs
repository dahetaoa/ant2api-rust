use crate::signature::cache::SignatureCache;
use crate::signature::types::{Entry, EntryIndex};
use anyhow::{Context, anyhow};
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, interval};

const QUEUE_CAPACITY: usize = 1024;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const BATCH_SIZE: usize = 256;

#[derive(Debug)]
pub struct Store {
    cache_dir: PathBuf,
    cache: SignatureCache,

    hot: RwLock<HotStore>,
    queue_tx: mpsc::Sender<Arc<Entry>>,

    writer: Mutex<WriterState>,
    readers: Mutex<HashMap<String, DayIndex>>,
}

#[derive(Debug, Default)]
struct HotStore {
    by_key: HashMap<String, Arc<Entry>>,
    by_tool_call: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct WriterState {
    date: String,
    data: Option<tokio::fs::File>,
    idx: Option<tokio::fs::File>,
    offset: u64,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct IndexRecord {
    #[serde(rename = "k", alias = "recordId")]
    k: String,
    #[serde(rename = "o", alias = "offset")]
    o: u64,
    #[serde(rename = "l", alias = "length")]
    l: u32,
}

#[derive(Debug, Default)]
struct DayIndex {
    by_record_id: HashMap<String, (u64, u32)>,
}

impl Store {
    pub fn new(data_dir: &str, cache: SignatureCache) -> anyhow::Result<Arc<Self>> {
        let cache_dir = PathBuf::from(data_dir).join("signatures");
        let (queue_tx, queue_rx) = mpsc::channel::<Arc<Entry>>(QUEUE_CAPACITY);

        let store = Arc::new(Self {
            cache_dir,
            cache,
            hot: RwLock::new(HotStore::default()),
            queue_tx,
            writer: Mutex::new(WriterState::default()),
            readers: Mutex::new(HashMap::new()),
        });

        // 启动后台刷盘任务
        Store::start_worker(store.clone(), queue_rx);
        Ok(store)
    }

    pub async fn enqueue(&self, e: Arc<Entry>) {
        // 与 Go 的行为一致：队列满时背压等待。
        let _ = self.queue_tx.send(e).await;
    }

    pub async fn put_hot(&self, e: Arc<Entry>) {
        let Some(key) = e.key() else {
            return;
        };
        if e.tool_call_id.is_empty() || e.signature.is_empty() {
            return;
        }

        let mut hot = self.hot.write().await;
        hot.by_tool_call.insert(e.tool_call_id.clone(), key.clone());
        hot.by_key.insert(key, e);
    }

    pub async fn load_recent(&self, days: usize) -> anyhow::Result<()> {
        let days = days.max(1);
        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .context("创建 signatures 目录失败")?;

        let mut entries = tokio::fs::read_dir(&self.cache_dir).await?;
        let mut idx_files: Vec<String> = Vec::new();
        while let Some(de) = entries.next_entry().await? {
            if !de.file_type().await?.is_file() {
                continue;
            }
            let name = de.file_name().to_string_lossy().to_string();
            if !name.ends_with(".idx") {
                continue;
            }
            idx_files.push(name);
        }
        idx_files.sort();
        if idx_files.len() > days {
            idx_files = idx_files[idx_files.len() - days..].to_vec();
        }

        for name in idx_files {
            let date = name.trim_end_matches(".idx").to_string();
            let idx_path = self.cache_dir.join(&name);
            let content = tokio::fs::read(&idx_path).await?;
            for line in content.split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let rec: IndexRecord = match sonic_rs::from_slice(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some((request_id, tool_call_id)) = split_record_id(&rec.k) else {
                    continue;
                };
                self.cache
                    .put(EntryIndex {
                        request_id,
                        tool_call_id,
                        date: date.clone(),
                        ..EntryIndex::default()
                    })
                    .await;
            }
        }

        Ok(())
    }

    pub async fn load_by_index(self: &Arc<Self>, idx: &EntryIndex) -> Option<Entry> {
        let key = idx.key()?;
        if idx.tool_call_id.is_empty() {
            return None;
        }

        if idx.date.is_empty() {
            let hot = self.hot.read().await;
            let cur = hot.by_key.get(&key)?;
            let mut e = cur.as_ref().clone();
            if let Some(last_access) = idx.last_access {
                e.last_access = last_access;
            }
            return Some(e);
        }

        let payload = self.load_record(&idx.date, &key).await.ok()?;
        let mut e: Entry = sonic_rs::from_slice(&payload).ok()?;
        if e.signature.is_empty() || e.request_id.is_empty() || e.tool_call_id.is_empty() {
            return None;
        }
        if let Some(last_access) = idx.last_access {
            e.last_access = last_access;
        }

        let today = Utc::now().format("%Y-%m-%d").to_string();
        if idx.date != today {
            let store = self.clone();
            let migrated = e.clone();
            tokio::spawn(async move {
                store.migrate_entry_to_today(&migrated).await;
            });
        }
        Some(e)
    }

    pub async fn migrate_entry_to_today(&self, entry: &Entry) {
        if entry.request_id.is_empty()
            || entry.tool_call_id.is_empty()
            || entry.signature.is_empty()
        {
            return;
        }

        let e = Arc::new(entry.clone());
        self.put_hot(e.clone()).await;
        self.cache
            .put(EntryIndex {
                request_id: e.request_id.clone(),
                tool_call_id: e.tool_call_id.clone(),
                model: e.model.clone(),
                created_at: Some(e.created_at),
                last_access: Some(e.last_access),
                date: String::new(),
            })
            .await;
        self.enqueue(e).await;
    }

    async fn load_record(&self, date: &str, record_id: &str) -> anyhow::Result<Vec<u8>> {
        let (offset, length) = self.get_index_entry(date, record_id).await?;
        let data_path = self.cache_dir.join(format!("{date}.jsonl"));
        let mut file = tokio::fs::File::open(&data_path)
            .await
            .with_context(|| format!("打开数据文件失败: {data_path:?}"))?;

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .context("seek 失败")?;
        let mut buf = vec![0u8; length as usize];
        file.read_exact(&mut buf).await.context("读取记录失败")?;
        Ok(buf)
    }

    async fn get_index_entry(&self, date: &str, record_id: &str) -> anyhow::Result<(u64, u32)> {
        {
            let readers = self.readers.lock().await;
            if let Some(day) = readers.get(date)
                && let Some(v) = day.by_record_id.get(record_id)
            {
                return Ok(*v);
            }
        }

        // 未命中则加载该日期的 idx 文件
        let idx_path = self.cache_dir.join(format!("{date}.idx"));
        let content = tokio::fs::read(&idx_path)
            .await
            .with_context(|| format!("读取索引文件失败: {idx_path:?}"))?;

        let mut map: HashMap<String, (u64, u32)> = HashMap::new();
        for line in content.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let rec: IndexRecord = match sonic_rs::from_slice(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            map.insert(rec.k, (rec.o, rec.l));
        }

        let mut readers = self.readers.lock().await;
        readers.insert(date.to_string(), DayIndex { by_record_id: map });
        readers
            .get(date)
            .and_then(|d| d.by_record_id.get(record_id).copied())
            .ok_or_else(|| anyhow!("cache record not found"))
    }

    fn start_worker(store: Arc<Self>, mut rx: mpsc::Receiver<Arc<Entry>>) {
        tokio::spawn(async move {
            let mut tick = interval(FLUSH_INTERVAL);
            let mut batch: Vec<Arc<Entry>> = Vec::new();
            let mut flush_blocked = false;

            async fn flush(
                store: &Arc<Store>,
                batch: &mut Vec<Arc<Entry>>,
                flush_blocked: &mut bool,
            ) {
                if batch.is_empty() {
                    *flush_blocked = false;
                    return;
                }

                match store.persist(batch).await {
                    Ok(persisted) => {
                        if persisted > 0 {
                            batch.drain(..persisted);
                        }
                        *flush_blocked = false;
                    }
                    Err(_) => {
                        // 磁盘写入失败：暂停读取，让下次 tick 再试。
                        *flush_blocked = true;
                    }
                }
            }

            loop {
                if flush_blocked {
                    tokio::select! {
                        _ = tick.tick() => { flush(&store, &mut batch, &mut flush_blocked).await; }
                        else => {}
                    }
                    continue;
                }

                tokio::select! {
                    Some(e) = rx.recv() => {
                        batch.push(e);
                        if batch.len() >= BATCH_SIZE {
                            flush(&store, &mut batch, &mut flush_blocked).await;
                        }
                    }
                    _ = tick.tick() => {
                        flush(&store, &mut batch, &mut flush_blocked).await;
                    }
                    else => break,
                }
            }
        });
    }

    async fn persist(&self, batch: &mut [Arc<Entry>]) -> anyhow::Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }

        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .context("创建 signatures 目录失败")?;

        let date = Utc::now().format("%Y-%m-%d").to_string();
        // 先写盘（持有 writer 锁），再做 cache/hot 更新（避免跨 await 持有 writer 锁过久）。
        let mut post_updates: Vec<(EntryIndex, String, DateTime<Utc>)> = Vec::new();

        let persisted = {
            let mut writer = self.writer.lock().await;
            self.ensure_writer(&mut writer, &date).await?;

            let mut persisted = 0usize;
            for e in batch.iter() {
                let Some(record_id) = e.key() else {
                    continue;
                };

                let json = sonic_rs::to_vec(e.as_ref()).context("序列化 signature entry 失败")?;
                let offset = writer.offset;
                let length: u32 = json.len().try_into().map_err(|_| anyhow!("记录过大"))?;

                {
                    let data = writer
                        .data
                        .as_mut()
                        .ok_or_else(|| anyhow!("writer 未初始化"))?;
                    data.write_all(&json).await?;
                    data.write_all(b"\n").await?;
                }

                let rec = IndexRecord {
                    k: record_id.clone(),
                    o: offset,
                    l: length,
                };
                {
                    let idx = writer
                        .idx
                        .as_mut()
                        .ok_or_else(|| anyhow!("writer 未初始化"))?;
                    let idx_line = sonic_rs::to_vec(&rec)?;
                    idx.write_all(&idx_line).await?;
                    idx.write_all(b"\n").await?;
                }

                writer.offset = writer
                    .offset
                    .checked_add(length as u64 + 1)
                    .ok_or_else(|| anyhow!("offset 溢出"))?;

                post_updates.push((
                    EntryIndex {
                        request_id: e.request_id.clone(),
                        tool_call_id: e.tool_call_id.clone(),
                        model: e.model.clone(),
                        created_at: Some(e.created_at),
                        last_access: Some(e.last_access),
                        date: date.clone(),
                    },
                    record_id,
                    e.created_at,
                ));

                persisted += 1;
            }
            persisted
        };

        for (idx, record_id, created_at) in post_updates {
            self.cache.put(idx).await;
            self.evict_hot(&record_id, created_at).await;
        }

        Ok(persisted)
    }

    async fn ensure_writer(&self, state: &mut WriterState, date: &str) -> anyhow::Result<()> {
        if state.date == date && state.data.is_some() && state.idx.is_some() {
            return Ok(());
        }

        // 关闭旧文件
        state.data = None;
        state.idx = None;
        state.offset = 0;
        state.date.clear();

        let data_path = self.cache_dir.join(format!("{date}.jsonl"));
        let idx_path = self.cache_dir.join(format!("{date}.idx"));

        let data = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&data_path)
            .await
            .with_context(|| format!("打开数据文件失败: {data_path:?}"))?;
        let idx = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&idx_path)
            .await
            .with_context(|| format!("打开索引文件失败: {idx_path:?}"))?;

        let offset = tokio::fs::metadata(&data_path).await?.len();

        state.date = date.to_string();
        state.data = Some(data);
        state.idx = Some(idx);
        state.offset = offset;
        Ok(())
    }

    async fn evict_hot(&self, record_id: &str, created_at: DateTime<Utc>) {
        let mut hot = self.hot.write().await;
        let Some(cur) = hot.by_key.get(record_id) else {
            return;
        };
        if cur.created_at != created_at {
            return;
        }

        let tool_call_id = cur.tool_call_id.clone();
        hot.by_key.remove(record_id);
        if tool_call_id.is_empty() {
            return;
        }
        let should_remove = hot
            .by_tool_call
            .get(&tool_call_id)
            .is_some_and(|mapped| mapped == record_id);
        if should_remove {
            hot.by_tool_call.remove(&tool_call_id);
        }
    }
}

fn split_record_id(record_id: &str) -> Option<(String, String)> {
    let (request_id, tool_call_id) = record_id.split_once(':')?;
    if request_id.is_empty() || tool_call_id.is_empty() {
        return None;
    }
    Some((request_id.to_string(), tool_call_id.to_string()))
}

pub async fn cleanup_signature_cache_files(
    data_dir: &str,
    cache_retention_days: u32,
) -> anyhow::Result<usize> {
    let cache_retention_days = cache_retention_days.max(1);
    let min_age_to_delete_days: i64 = cache_retention_days.max(2).into();

    let cache_dir = PathBuf::from(data_dir).join("signatures");
    let mut dir = match tokio::fs::read_dir(&cache_dir).await {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    let today = Utc::now().date_naive();
    let mut deleted = 0usize;

    while let Some(de) = dir.next_entry().await? {
        if !de.file_type().await?.is_file() {
            continue;
        }

        let name = de.file_name().to_string_lossy().to_string();
        let (date_str, is_target) = if name.ends_with(".idx") {
            (name.trim_end_matches(".idx"), true)
        } else if name.ends_with(".jsonl") {
            (name.trim_end_matches(".jsonl"), true)
        } else {
            ("", false)
        };
        if !is_target || date_str.len() != 10 {
            continue;
        }

        let Ok(file_date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") else {
            continue;
        };
        let age_days = (today - file_date).num_days();
        if age_days < min_age_to_delete_days {
            continue;
        }

        let path = de.path();
        match tokio::fs::remove_file(&path).await {
            Ok(_) => deleted += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    Ok(deleted)
}
