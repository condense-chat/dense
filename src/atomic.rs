use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Replace `path` with `bytes` through a temp file no other writer shares,
/// renamed into place, so a reader never sees a partial file.
pub fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = temp_beside(path);
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A fresh `.tmp` path in `path`'s directory, unique to this call.
pub fn temp_beside(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_never_share_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        assert_ne!(temp_beside(&path), temp_beside(&path));
        std::thread::scope(|s| {
            for i in 0..16 {
                let path = &path;
                s.spawn(move || replace(path, format!("v{i}\n").as_bytes()).unwrap());
            }
        });
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with('v') && body.ends_with('\n') && body.len() <= 4);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1);
    }
}
