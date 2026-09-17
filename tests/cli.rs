use std::path::PathBuf;
use std::process::Command;

fn dense() -> Command {
    Command::new(bin())
}

// Under wine (cross-compiled tests run from linux) the baked path is a unix
// path; map it through wine's Z: drive. On real Windows it's already native.
#[cfg(windows)]
fn bin() -> PathBuf {
    let baked = env!("CARGO_BIN_EXE_dense");
    match baked.strip_prefix('/') {
        Some(rest) => PathBuf::from(format!("Z:\\{}", rest.replace('/', "\\"))),
        None => PathBuf::from(baked),
    }
}

#[cfg(not(windows))]
fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dense"))
}

#[test]
fn version_smoke() {
    let out = dense().arg("--version").output().expect("run");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_smoke() {
    let out = dense().arg("--help").output().expect("run");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for cmd in ["claude", "login", "persist", "doctor", "info"] {
        assert!(help.contains(cmd), "help should mention `{cmd}`");
    }
}

// HOME-keyed config resolution is unix-shaped; Windows uses known folders.
#[cfg(unix)]
#[test]
fn status_defaults_to_prod() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = dense()
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CONDENSE_URL")
        .arg("status")
        .output()
        .expect("run");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("profile: prod"), "got: {s}");
    assert!(s.contains("https://api.condense.chat"), "got: {s}");
}

#[cfg(unix)]
#[test]
fn unknown_env_profile_refuses_to_guess() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = dense()
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("CONDENSE_URL")
        .args(["-e", "nope", "status"])
        .output()
        .expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown profile"));
}

#[cfg(unix)]
#[test]
fn info_without_creds_fails_before_needing_a_session() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = dense()
        .env("HOME", home.path())
        .env_remove("CONDENSE_SESSION_ID")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CONDENSE_URL")
        .arg("info")
        .output()
        .expect("run");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not logged in"), "got: {err}");
}

#[cfg(unix)]
#[test]
fn info_with_session_without_creds_fails() {
    const SID: &str = "6f1d2c3b-4a5e-4f60-8b7c-9d0e1f2a3b4c";
    let home = tempfile::tempdir().expect("tempdir");

    let out = dense()
        .env("HOME", home.path())
        .env_remove("CONDENSE_SESSION_ID")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CONDENSE_URL")
        .args(["info", SID])
        .output()
        .expect("run");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not logged in"), "got: {err}");

    let out = dense()
        .env("HOME", home.path())
        .env("CONDENSE_SESSION_ID", SID)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CONDENSE_URL")
        .args(["info", "--json"])
        .output()
        .expect("run");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not logged in"), "got: {err}");
}
