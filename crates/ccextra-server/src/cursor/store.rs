use super::constants::DEFAULT_AUTH_DIR;
use super::credential::CursorCredential;
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn resolve_auth_dir(raw: Option<&str>) -> PathBuf {
    PathBuf::from(raw.unwrap_or(DEFAULT_AUTH_DIR))
}

pub fn credential_path(dir: &Path) -> PathBuf {
    dir.join("cursor.json")
}

pub fn load(dir: &Path) -> Result<CursorCredential> {
    let path = credential_path(dir);
    let data =
        fs::read(&path).with_context(|| format!("读取 Cursor 凭证失败: {}", path.display()))?;
    serde_json::from_slice(&data)
        .with_context(|| format!("解析 Cursor 凭证失败: {}", path.display()))
}

pub fn save(dir: &Path, credential: &CursorCredential) -> Result<()> {
    let new_dir = !dir.exists();
    fs::create_dir_all(dir)
        .with_context(|| format!("创建 Cursor 凭证目录失败: {}", dir.display()))?;
    #[cfg(unix)]
    if new_dir {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = credential_path(dir);
    let data = serde_json::to_vec_pretty(credential)?;
    let mut nonce = [0u8; 8];
    getrandom::getrandom(&mut nonce)?;
    let tmp = dir.join(format!(".cursor.{:016x}.tmp", u64::from_be_bytes(nonce)));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&tmp)?;
    let result = (|| -> Result<()> {
        file.write_all(&data)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("替换 Cursor 凭证失败: {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_roundtrip_replaces_file_without_losing_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("cursor");
        let mut credential = CursorCredential {
            access_token: "first".into(),
            refresh_token: "secret".into(),
            sub: "account".into(),
            expires_at: Some(1234),
        };
        save(&dir, &credential).unwrap();
        assert_eq!(load(&dir).unwrap(), credential);
        credential.access_token = "second".into();
        save(&dir, &credential).unwrap();
        assert_eq!(load(&dir).unwrap(), credential);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        #[cfg(unix)]
        {
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(credential_path(&dir))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
