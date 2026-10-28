//! This machine's condense device identity: a UUIDv4 generated once and kept
//! at `<cred_dir>/device`, sent as `x-condense-device-id` on every request.

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
    std::fs::write(&path, format!("{id}\n")).ctx(format!("writing {}", path.display()))?;
    Ok(id)
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
    fn garbage_device_file_is_replaced() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(home.path(), "http://127.0.0.1:9");
        std::fs::create_dir_all(cfg.cred_dir()).unwrap();
        std::fs::write(cfg.device_file(), "not-a-uuid\n").unwrap();
        let fresh = id(&cfg).unwrap();
        assert_ne!(fresh, "not-a-uuid");
    }
}
