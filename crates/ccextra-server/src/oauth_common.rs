// 四个 OAuth 模块(antigravity / claude / codex / xai)共用的纯工具函数:
// 凭证原子写 + 锁、auth_dir 解析、文件名清洗、刷新单飞锁、登录辅助。
// 各模块保留自身的凭证结构、文件命名与扫描规则。
// (写回语义对齐 CPA sdk/auth/filestore.go:原子写、目录 0700、文件 0600)

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// 四个 OAuth 模块共用的控制面 HTTP client:30s 总超时(对齐 CPA fetch 路径,
/// 防端点不可达时启动/reload 挂死);代理按 `ccextra_core::proxy::parse_proxy` 解析,
/// 未配置即报错,不继承环境变量或系统代理。
pub fn oauth_http_client(proxy_url: Option<&str>, label: &str) -> Result<reqwest::Client> {
    let setting = ccextra_core::proxy::parse_proxy(proxy_url)
        .map_err(|e| anyhow!("{label} OAuth 代理配置错误: {e}"))?;
    let builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    let builder = match &setting {
        ccextra_core::proxy::ProxySetting::Direct => builder.no_proxy(),
        ccextra_core::proxy::ProxySetting::Url(url) => builder
            .proxy(reqwest::Proxy::all(url).with_context(|| format!("parse proxy URL {url}"))?),
    };
    builder
        .build()
        .with_context(|| format!("build {label} http client"))
}

static PATH_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut map = PATH_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn write_atomic(path: &Path, data: &[u8], tmp_prefix: &str) -> Result<()> {
    let dir = path.parent().context("credential path missing parent")?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).context("credential temp nonce")?;
    let tmp = dir.join(format!(
        ".{tmp_prefix}.{:016x}.tmp",
        u64::from_be_bytes(nonce)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("create temp credential {}", tmp.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path).with_context(|| format!("replace credential {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 凭证 JSON 写到指定路径:目录 0700、进程内 path 锁、跨进程文件锁(`{path}.lock`)、
/// 临时文件原子替换、文件 0600。
pub(crate) fn write_credential_file(path: &Path, data: &[u8], tmp_prefix: &str) -> Result<()> {
    let auth_dir = path.parent().context("credential path missing parent")?;
    fs::create_dir_all(auth_dir)
        .with_context(|| format!("创建凭证目录失败: {}", auth_dir.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(auth_dir, fs::Permissions::from_mode(0o700))?;
    }

    let inproc = path_lock(path);
    let _guard = inproc.lock().unwrap_or_else(|p| p.into_inner());

    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let mut file_lock = fslock::LockFile::open(&lock_path)
        .with_context(|| format!("open credential lock {}", lock_path.display()))?;
    file_lock
        .lock()
        .with_context(|| format!("acquire credential lock {}", lock_path.display()))?;

    write_atomic(path, data, tmp_prefix)?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        let _ = fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600));
    }
    let _ = file_lock.unlock();
    Ok(())
}

/// auth_dir 解析:空 → `default`;`~/` 前缀展开到 $HOME(或 USERPROFILE);其余原样
pub(crate) fn resolve_auth_dir(dir: Option<&str>, default: &str) -> PathBuf {
    match dir {
        Some(d) if !d.trim().is_empty() => {
            if let Some(rest) = d.strip_prefix("~/") {
                if let Some(home) = home_dir() {
                    return home.join(rest);
                }
            }
            PathBuf::from(d)
        }
        _ => PathBuf::from(default),
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// 凭证文件名片段清洗:仅保留 `[A-Za-z0-9@._-]`,其余替换为 `-`,去首尾 `-`
pub(crate) fn sanitize_file_segment(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '@' | '.' | '_' | '-' => out.push(c),
            _ => out.push('-'),
        }
    }
    out.trim_matches('-').to_string()
}

type RefreshLock = Arc<tokio::sync::Mutex<()>>;

static REFRESH_LOCKS: OnceLock<tokio::sync::Mutex<HashMap<String, RefreshLock>>> = OnceLock::new();

/// 按 refresh_token 单飞的刷新锁(对齐 CPA conductor_refresh refreshLocks 按凭证串行刷新)
pub(crate) async fn lock_for_refresh_token(refresh_token: &str) -> RefreshLock {
    let mut map = REFRESH_LOCKS
        .get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
        .lock()
        .await;
    acquire_refresh_lock(&mut map, refresh_token)
}

/// refresh_token 每次刷新都会轮换,旧 key 不再出现;取锁时顺带清掉
/// 只剩 map 自身持有(无人持有/等待)的条目,防止表无限增长。
fn acquire_refresh_lock(
    map: &mut HashMap<String, RefreshLock>,
    refresh_token: &str,
) -> RefreshLock {
    map.retain(|_, lock| Arc::strong_count(lock) > 1);
    map.entry(refresh_token.trim().to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// OAuth state:16 字节随机数 hex
pub(crate) fn random_state() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("generate oauth state")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// macOS `open` 打开授权页
pub(crate) fn open_browser(url: &str) -> Result<()> {
    let status = std::process::Command::new("open")
        .arg(url)
        .status()
        .context("open browser")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("open browser failed: {status}"))
    }
}

/// RFC 3986 unreserved 之外的字节一律 `%XX`
pub(crate) fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_lock_shared_while_held_and_pruned_when_idle() {
        let mut map = HashMap::new();
        let a1 = acquire_refresh_lock(&mut map, " rt-a ");
        let a2 = acquire_refresh_lock(&mut map, "rt-a");
        assert!(Arc::ptr_eq(&a1, &a2), "同一 refresh_token 共用一把锁");
        drop(a1);
        drop(a2);
        let _b = acquire_refresh_lock(&mut map, "rt-b");
        assert_eq!(map.len(), 1, "无人持有的旧 token 锁被清理");
        assert!(map.contains_key("rt-b"));
    }

    #[test]
    fn write_credential_file_creates_lock_file_and_restricts_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("antigravity-a.json");
        write_credential_file(&path, b"{}", "antigravity").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
        assert!(PathBuf::from(format!("{}.lock", path.display())).exists());
        #[cfg(unix)]
        {
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(&path), 0o600);
        }
    }
}
