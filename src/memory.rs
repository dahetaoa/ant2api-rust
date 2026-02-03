use std::time::Duration;

/// 后台 RSS 守护（仅 Linux）。
///
/// 通过环境变量启用：
/// - `RSS_GUARD_MAX_MB`（例如 `50`）：当 RSS 超过阈值时触发 jemalloc purge
/// - `RSS_GUARD_INTERVAL_MS`（默认 `1000`）：检查间隔
/// - `RSS_GUARD_COOLDOWN_MS`（默认 `5000`）：两次 purge 的最小间隔（避免频繁抖动）
pub fn spawn_rss_guard_from_env() {
    let max_mb = env_u64("RSS_GUARD_MAX_MB").unwrap_or(0);
    if max_mb == 0 {
        return;
    }

    let interval_ms = env_u64("RSS_GUARD_INTERVAL_MS").unwrap_or(1000);
    let cooldown_ms = env_u64("RSS_GUARD_COOLDOWN_MS").unwrap_or(5000);

    let max_rss_bytes = max_mb.saturating_mul(1024).saturating_mul(1024);
    let interval = Duration::from_millis(interval_ms.max(100));
    let cooldown = Duration::from_millis(cooldown_ms.max(interval.as_millis() as u64));

    #[cfg(all(target_os = "linux", not(target_env = "msvc")))]
    linux::spawn_rss_guard_task(max_rss_bytes, interval, cooldown);

    #[cfg(not(all(target_os = "linux", not(target_env = "msvc"))))]
    {
        tracing::warn!("已启用 RSS 守护，但当前平台不支持（需要 Linux + jemalloc）");
    }
}

/// 后台页面缓存回收（仅 Linux）。
///
/// 目标：降低容器内 `file cache`（页面缓存）在 Portainer 等面板里的占用显示。
///
/// 说明：
/// - 容器环境下，Linux 会把文件读写的页面缓存计入 cgroup 内存使用（面板里看起来像“内存不回落”）。
/// - 容器里通常无法写 `/proc/sys/vm/drop_caches` 或 `/sys/fs/cgroup/memory.reclaim`（多为只读挂载）。
/// - 这里采用 `posix_fadvise(..., DONTNEED)` 的方式，对本进程产生的大文件做“丢弃缓存提示”（best-effort）。
pub fn spawn_page_cache_reclaimer(data_dir: String) {
    #[cfg(target_os = "linux")]
    linux::spawn_page_cache_reclaimer_task(data_dir);
}

fn env_u64(key: &str) -> Option<u64> {
    let v = std::env::var(key).ok()?;
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    v.parse::<u64>().ok()
}

#[cfg(all(target_os = "linux", not(target_env = "msvc")))]
mod linux {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{Instant, SystemTime};

    pub fn spawn_rss_guard_task(max_rss_bytes: u64, interval: Duration, cooldown: Duration) {
        tracing::info!(
            "RSS 守护已启用：max={}MB interval={}ms cooldown={}ms",
            max_rss_bytes / 1024 / 1024,
            interval.as_millis(),
            cooldown.as_millis()
        );

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            let mut last_purge_at = Instant::now()
                .checked_sub(cooldown)
                .unwrap_or_else(Instant::now);

            loop {
                tick.tick().await;

                if last_purge_at.elapsed() < cooldown {
                    continue;
                }

                let Some(rss_bytes) = read_rss_bytes() else {
                    continue;
                };
                if rss_bytes <= max_rss_bytes {
                    continue;
                }

                let now = SystemTime::now();
                last_purge_at = Instant::now();

                match super::jemalloc::purge_all() {
                    Ok(_) => {
                        let after = read_rss_bytes().unwrap_or(rss_bytes);
                        tracing::warn!(
                            "RSS guard purge: before={}MB after={}MB max={}MB",
                            rss_bytes / 1024 / 1024,
                            after / 1024 / 1024,
                            max_rss_bytes / 1024 / 1024
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "RSS guard purge failed: {e:#} (rss={}MB max={}MB time={:?})",
                            rss_bytes / 1024 / 1024,
                            max_rss_bytes / 1024 / 1024,
                            now
                        );
                    }
                }
            }
        });
    }

    fn read_rss_bytes() -> Option<u64> {
        // /proc/self/status 提供 VmRSS（单位 kB），与容器 RSS 统计口径比较接近。
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            let line = line.trim_start();
            if !line.starts_with("VmRSS:") {
                continue;
            }
            // 示例："VmRSS:\t  149176 kB"
            let mut parts = line.split_whitespace();
            let _ = parts.next()?; // VmRSS:
            let num = parts.next()?.parse::<u64>().ok()?;
            let unit = parts.next().unwrap_or("kB");
            let bytes = match unit {
                "kB" | "KB" => num.saturating_mul(1024),
                _ => num,
            };
            return Some(bytes);
        }
        None
    }

    pub fn spawn_page_cache_reclaimer_task(data_dir: String) {
        if !running_in_docker() {
            return;
        }

        // 固定每 5 分钟执行一次（满足“每五分钟回收一次”的诉求）。
        let interval = Duration::from_secs(5 * 60);
        tracing::info!("页面缓存回收已启用：interval={}s", interval.as_secs());

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;

                let dir = data_dir.clone();
                let res =
                    tokio::task::spawn_blocking(move || drop_signature_file_cache(&dir)).await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("页面缓存回收失败: {e:#}"),
                    Err(e) => tracing::warn!("页面缓存回收任务异常: {e:#}"),
                }
            }
        });
    }

    fn running_in_docker() -> bool {
        Path::new("/.dockerenv").exists()
    }

    fn drop_signature_file_cache(data_dir: &str) -> anyhow::Result<()> {
        let sig_dir = PathBuf::from(data_dir).join("signatures");
        if !sig_dir.is_dir() {
            return Ok(());
        }

        for de in std::fs::read_dir(&sig_dir)? {
            let de = match de {
                Ok(v) => v,
                Err(_) => continue,
            };
            let path = de.path();
            if !path.is_file() {
                continue;
            }

            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext != "jsonl" && ext != "idx" {
                continue;
            }

            // best-effort：不影响主流程，即使失败也继续处理其它文件。
            let _ = advise_drop_file_cache(&path);
        }

        Ok(())
    }

    fn advise_drop_file_cache(path: &Path) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;

        let file = std::fs::File::open(path)?;
        let fd = file.as_raw_fd();

        // offset=0,len=0 => 整个文件。返回值为 errno（0 表示成功）。
        let ret = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
        if ret != 0 {
            return Err(anyhow::anyhow!(
                "posix_fadvise(DONTNEED) failed: errno={ret}"
            ));
        }
        Ok(())
    }
}

#[cfg(not(target_env = "msvc"))]
mod jemalloc {
    use std::ffi::{CStr, CString};
    use std::mem;
    use std::ptr;

    pub fn purge_all() -> anyhow::Result<()> {
        // Best-effort：先 flush 当前线程 tcache（对大对象可能无效，但可减少小对象抖动）。
        unsafe {
            mallctl_noargs(CStr::from_bytes_with_nul(b"thread.tcache.flush\\0").unwrap()).ok();
        }

        let narenas: u32 = unsafe {
            mallctl_read(CStr::from_bytes_with_nul(b"arenas.narenas\\0").unwrap())
                .map_err(|e| anyhow::anyhow!("mallctl arenas.narenas failed: {e}"))?
        };

        for i in 0..(narenas as usize) {
            let key = CString::new(format!("arena.{i}.purge"))
                .map_err(|e| anyhow::anyhow!("build mallctl key arena.<i>.purge failed: {e}"))?;
            unsafe {
                // `arena.<i>.purge` is NEITHER_READ_NOR_WRITE (expects NULL oldp/newp).
                mallctl_noargs(&key).map_err(|e| {
                    anyhow::anyhow!("mallctl {} failed: {e}", key.to_string_lossy())
                })?;
            }
        }

        Ok(())
    }

    unsafe fn mallctl_noargs(name: &CStr) -> Result<(), i32> {
        let ret = unsafe {
            tikv_jemalloc_sys::mallctl(
                name.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
            )
        };
        if ret == 0 { Ok(()) } else { Err(ret) }
    }

    unsafe fn mallctl_read<T: Copy>(name: &CStr) -> Result<T, i32> {
        let mut value = mem::MaybeUninit::<T>::uninit();
        let mut len = mem::size_of::<T>();
        let ret = unsafe {
            tikv_jemalloc_sys::mallctl(
                name.as_ptr(),
                value.as_mut_ptr() as *mut _,
                &mut len,
                ptr::null_mut(),
                0,
            )
        };
        if ret == 0 && len == mem::size_of::<T>() {
            Ok(unsafe { value.assume_init() })
        } else if ret != 0 {
            Err(ret)
        } else {
            Err(-1)
        }
    }
}
