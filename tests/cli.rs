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

#[cfg(unix)]
#[test]
fn a_tool_that_is_gone_says_so_before_any_login() {
    let home = tempfile::tempdir().expect("home");
    let out = dense()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PATH", home.path())
        .env_remove("CONDENSE_URL")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .args(["--url", "http://127.0.0.1:9", "claude", "-p", "hi"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("`claude` was not found on PATH"), "{err}");
}

#[test]
fn errors_are_for_people_not_for_rust_developers() {
    let out = dense()
        .env_remove("RUST_BACKTRACE")
        .args(["-e", "nosuch", "status"])
        .output()
        .expect("run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown profile `nosuch`"));
    assert!(!err.contains("Location:"), "{err}");
    assert!(!err.contains("RUST_BACKTRACE"), "{err}");
}

#[test]
fn env_flag_beats_condense_url() {
    let out = dense()
        .env("CONDENSE_URL", "http://127.0.0.1:9")
        .args(["-e", "nosuch", "status"])
        .output()
        .expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown profile `nosuch`"));
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

#[test]
fn help_lists_claude_desktop() {
    for args in [["--help"], ["help"]] {
        let out = dense().args(args).output().expect("run");
        assert!(out.status.success());
        let help = String::from_utf8_lossy(&out.stdout);
        assert!(
            help.contains("\n  claude-desktop "),
            "claude-desktop is missing, got: {help}"
        );
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
        .args(["status", "--details"])
        .output()
        .expect("run");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.lines()
            .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["Profile", "prod"]),
        "got: {s}"
    );
    assert!(s.contains("https://api.condense.chat"), "got: {s}");
}

#[cfg(unix)]
#[test]
fn status_defaults_to_tool_setup_without_internal_paths() {
    let home = tempfile::tempdir().expect("home");
    let out = dense()
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CONDENSE_URL")
        .arg("status")
        .output()
        .expect("run");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "Your tools",
        "Claude Code",
        "Codex",
        "OpenCode",
        "On demand: `dense claude`",
        "Not connected",
        "dense status --details",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    for hidden in [
        "Credentials",
        "https://api.",
        "autostart",
        "Device",
        "transparent",
    ] {
        assert!(!text.contains(hidden), "{text}");
    }
    assert!(!text.contains(&*home.path().to_string_lossy()));
}

#[cfg(unix)]
#[test]
fn status_distinguishes_missing_launchers_and_missing_login() {
    let home = tempfile::tempdir().expect("home");
    let data = home.path().join(".local/share/dense");
    let creds = home.path().join(".config/dense");
    std::fs::create_dir_all(data.join("bin")).expect("launcher folder");
    std::fs::create_dir_all(&creds).expect("credentials folder");
    std::fs::write(
        data.join("persist.toml"),
        "[tools]\nclaude = 'configured'\ncodex = 'configured'\n",
    )
    .expect("record");
    std::fs::write(data.join("bin/claude"), "launcher fixture").expect("launcher");
    for logged_in in [false, true] {
        if logged_in {
            std::fs::write(creds.join("token"), "test-token").expect("token");
        }
        let out = dense()
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("CONDENSE_URL")
            .args(["status"])
            .output()
            .expect("run");
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("Setup missing: run `dense persist codex`"),
            "{text}"
        );
        assert_eq!(
            text.contains("Automatic routing configured"),
            logged_in,
            "{text}"
        );
        assert_eq!(text.contains("log in to use it"), !logged_in, "{text}");
    }
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

#[cfg(unix)]
#[test]
fn piped_output_is_plain_and_repeated_logout_is_explicit() {
    let home = tempfile::tempdir().expect("home");
    let creds = home.path().join(".config/dense");
    std::fs::create_dir_all(&creds).expect("credentials folder");
    std::fs::write(creds.join("token"), "test-token").expect("token");
    for (args, expected) in [
        (vec!["status"], "Login saved on this computer"),
        (vec!["logout"], "Logged out of api.condense.chat"),
        (vec!["logout"], "Already logged out. Nothing to remove"),
        (vec!["status"], "You're not logged in yet"),
    ] {
        let out = dense()
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("CONDENSE_URL")
            .env("TERM", "xterm-256color")
            .env("CLICOLOR_FORCE", "1")
            .env("FORCE_COLOR", "1")
            .args(args)
            .output()
            .expect("run");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}");
        assert!(text.contains(expected), "{text}");
        assert!(text.starts_with("\n  dense / "), "{text}");
        assert!(!text.contains('\x1b'), "{text}");
        assert!(out.stderr.is_empty(), "{:?}", out.stderr);
    }
    assert!(!creds.join("token").exists());
}

#[cfg(unix)]
#[test]
fn narrow_errors_keep_commands_intact_and_stay_on_stderr() {
    let home = tempfile::tempdir().expect("home");
    let out = dense()
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("CONDENSE_URL")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("COLUMNS", "32")
        .args(["info", "--json"])
        .output()
        .expect("run");
    let error = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(error.starts_with("  ERROR   "), "{error}");
    assert!(error.contains("`dense login`"), "{error}");
    assert!(!error.contains('\x1b'));
    assert!(
        error
            .lines()
            .all(|line| console::measure_text_width(line) <= 32)
    );
}

#[cfg(unix)]
#[test]
fn info_formats_keep_json_clean_and_default_compact() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let home = tempfile::tempdir().expect("home");
    let listener = TcpListener::bind("127.0.0.1:0").expect("local API");
    let address = listener.local_addr().expect("address");
    let creds = home.path().join(".config/dense/test");
    std::fs::create_dir_all(&creds).expect("credentials folder");
    std::fs::write(creds.join("token"), "test-token").expect("token");
    std::fs::write(
        creds.join("profile.toml"),
        format!("name = \"test\"\napi_url = \"http://{address}\"\n"),
    )
    .expect("profile");
    let server = std::thread::spawn(move || {
        for _ in 0..8 {
            let (mut stream, _) = listener.accept().expect("request");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("timeout");
            let mut request = [0; 4096];
            let n = stream.read(&mut request).expect("read request");
            let body = if request[..n].starts_with(b"GET /v1/me/billing ") {
                r#"{"tier":"test","compressing":true}"#
            } else {
                r#"{"email":"test@example.com","usage":{"request_count":2,"pre_tokens":10000,"post_tokens":5000,"saved_usd":"1.25"}}"#
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
        }
    });
    for format in [None, Some("--json"), Some("--details"), Some("--matrix")] {
        let out = dense()
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("CONDENSE_URL")
            .env_remove("CONDENSE_SESSION_ID")
            .env("NO_PROXY", "127.0.0.1")
            .env("FORCE_COLOR", "1")
            .args(["-e", "test", "info"])
            .args(format)
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if format == Some("--json") {
            let data: serde_json::Value = serde_json::from_slice(&out.stdout).expect("only JSON");
            assert_eq!(data["account"]["email"], "test@example.com");
            assert_eq!(data["totals"]["request_count"], 2);
        } else {
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains("Account & savings"), "{text}");
            assert!(text.contains("$1.25"), "{text}");
            assert_eq!(text.contains('🌕'), format == Some("--matrix"), "{text}");
            assert_eq!(
                text.contains("Cache reads"),
                format != Some("--matrix"),
                "{text}"
            );
            assert!(!text.contains('\x1b'), "{text}");
        }
        assert!(out.stderr.is_empty());
    }
    server.join().expect("server");
}
