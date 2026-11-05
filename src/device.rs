use crate::Result;
use crate::config::Config;
use crate::error::Context;

pub const HEADER: &str = "x-condense-device-id";

pub fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The stored device id, generating and persisting one on first use.
pub fn id(cfg: &Config) -> Result<String> {
    if let Some(id) = load(cfg) {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let path = cfg.device_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    let tmp = crate::atomic::temp_beside(&path);
    write_private(&tmp, &format!("{id}\n"))?;
    // Hard-link, not rename: two first runs racing keep one id, never two.
    let mut linked = std::fs::hard_link(&tmp, &path);
    if linked
        .as_ref()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
        && load(cfg).is_none()
    {
        // Garbage, not a racing writer's id: replace it.
        linked = std::fs::rename(&tmp, &path);
    }
    let _ = std::fs::remove_file(&tmp);
    match (linked, load(cfg)) {
        (_, Some(stored)) => Ok(stored),
        (Err(e), None) => Err(e).ctx(format!("writing {}", path.display())),
        (Ok(()), None) => Err(crate::error::Error::msg(format!(
            "{} is unreadable after writing it",
            path.display()
        ))),
    }
}

/// Replace the device id with a fresh one (atomic rename, 0600).
pub fn regenerate(cfg: &Config) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let path = cfg.device_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    let tmp = crate::atomic::temp_beside(&path);
    let written = write_private(&tmp, &format!("{id}\n"))
        .and_then(|()| std::fs::rename(&tmp, &path).ctx(format!("replacing {}", path.display())));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    tracing::info!(device = %id, "generated a new device id");
    Ok(id)
}

/// Record `user_id` as the device's user; when a different user was recorded
/// before, the device id is regenerated first. Returns whether it was.
pub fn bind_user(cfg: &Config, user_id: &str) -> Result<bool> {
    let path = owner_file(cfg);
    let previous = std::fs::read_to_string(&path).ok();
    let previous = previous.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let changed = previous.is_some_and(|p| p != user_id) && load(cfg).is_some();
    if changed {
        regenerate(cfg)?;
    }
    if previous != Some(user_id) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
        }
        write_private(&path, &format!("{user_id}\n"))?;
    }
    Ok(changed)
}

/// The stored device id, if one exists and parses as a UUID.
pub fn load(cfg: &Config) -> Option<String> {
    let raw = std::fs::read_to_string(cfg.device_file()).ok()?;
    let id = raw.trim();
    uuid::Uuid::parse_str(id).ok().map(|_| id.to_string())
}

pub fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// The user the device id was last used with; outlives `dense logout`.
fn owner_file(cfg: &Config) -> std::path::PathBuf {
    cfg.cred_dir().join("device-user")
}

fn write_private(path: &std::path::Path, body: &str) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).ctx(format!("writing {}", path.display()))?;
    std::io::Write::write_all(&mut f, body.as_bytes()).ctx(format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_generated_once_and_reused() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(home.path(), "http://127.0.0.1:9");
        assert!(load(&cfg).is_none());
        let first = id(&cfg).unwrap();
        assert!(uuid::Uuid::parse_str(&first).is_ok());
        assert_eq!(id(&cfg).unwrap(), first);
        assert_eq!(
            std::fs::read_to_string(cfg.device_file()).unwrap().trim(),
            first
        );
    }

    #[test]
    fn racing_first_runs_agree_on_one_id() {
        for _ in 0..20 {
            let home = tempfile::tempdir().unwrap();
            let cfg = Config::for_test(home.path(), "http://127.0.0.1:9");
            let barrier = std::sync::Barrier::new(8);
            let ids: Vec<String> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        s.spawn(|| {
                            barrier.wait();
                            id(&cfg).unwrap()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");
            assert_eq!(load(&cfg).as_deref(), Some(ids[0].as_str()));
        }
    }

    #[test]
    fn garbage_device_file_is_replaced() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(home.path(), "http://127.0.0.1:9");
        std::fs::create_dir_all(cfg.cred_dir()).unwrap();
        std::fs::write(cfg.device_file(), "not-a-uuid\n").unwrap();
        let fresh = id(&cfg).unwrap();
        assert_ne!(fresh, "not-a-uuid");
    }

    #[cfg(unix)]
    #[test]
    fn device_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(home.path(), "http://127.0.0.1:9");
        id(&cfg).unwrap();
        let mode = std::fs::metadata(cfg.device_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn regenerate_replaces_the_id() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(home.path(), "http://127.0.0.1:9");
        let first = id(&cfg).unwrap();
        let second = regenerate(&cfg).unwrap();
        assert_ne!(first, second);
        assert_eq!(load(&cfg).as_deref(), Some(second.as_str()));
        assert_eq!(id(&cfg).unwrap(), second);
    }

    #[test]
    fn a_different_user_gets_a_fresh_id() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(home.path(), "http://127.0.0.1:9");
        let first = id(&cfg).unwrap();
        assert!(!bind_user(&cfg, "user-a").unwrap());
        assert!(!bind_user(&cfg, "user-a").unwrap());
        assert_eq!(id(&cfg).unwrap(), first);
        assert!(bind_user(&cfg, "user-b").unwrap());
        let second = id(&cfg).unwrap();
        assert_ne!(first, second);
        assert!(!bind_user(&cfg, "user-b").unwrap());
        assert_eq!(id(&cfg).unwrap(), second);
    }
}
