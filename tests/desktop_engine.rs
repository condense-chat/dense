#![cfg(unix)]
// `expect` in shared rig helpers, which clippy only exempts inside #[test] fns.
#![allow(clippy::expect_used)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

// A child forked by one test inherits another test's open write fd until it
// execs, and exec of a file open for writing fails with ETXTBSY. Holding this
// across writing an executable and across spawn keeps the two apart.
static EXEC: Mutex<()> = Mutex::new(());

const ENGINE: &[u8] = b"a published engine";

const FAKE_ENGINE: &str = r#"#!/bin/sh
{
  printf 'base=%s\n' "$ANTHROPIC_BASE_URL"
  printf 'token=%s\n' "$CLAUDE_CODE_OAUTH_TOKEN"
  printf 'kind=%s\n' "$(printf '%s' "$ANTHROPIC_CUSTOM_HEADERS" | grep -c x-condense-kind)"
  printf 'device=%s\n' "$(printf '%s' "$ANTHROPIC_CUSTOM_HEADERS" | grep -c x-condense-device-id)"
  printf 'session=%s\n' "$CONDENSE_SESSION_ID"
  printf 'args=%s\n' "$*"
} > "$ENGINE_REPORT"
printf '{"stream":"only this"}\n'
if [ -n "$ENGINE_WAIT" ]; then
  trap 'echo term > "${ENGINE_REPORT}.signal"; exit 7' TERM
  : > "${ENGINE_REPORT}.ready"
  while :; do sleep 0.05; done
fi
exit 3
"#;

#[cfg(feature = "managed-setup")]
const MANAGED_OFF: &str = r#"{"managed":true,"enabled":false,"heartbeat_interval_s":60}"#;
const MANAGED_ON: &str = r#"{"managed":true,"enabled":true,"heartbeat_interval_s":60}"#;
const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

const REGULAR: &str = r#"{"managed":false,"enabled":true,"heartbeat_interval_s":60}"#;
const REJECTED: &str = "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

/// A local condense api that says yes to everything until it's killed.
struct Api {
    addr: SocketAddr,
    heartbeat: Arc<Mutex<Option<&'static str>>>,
    server: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

struct Rig {
    api: RefCell<Option<Api>>,
    home: tempfile::TempDir,
    port: u16,
}

impl Api {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let heartbeat = Arc::new(Mutex::new(None));
        let beat = Arc::clone(&heartbeat);
        let server = std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let body = *beat.lock().expect("heartbeat");
                match body.filter(|_| buf[..n].starts_with(b"POST /v1/devices/heartbeat")) {
                    Some(body) => {
                        let _ = stream.write_all(json_reply(body).as_bytes());
                    }
                    None => {
                        let _ = stream.write_all(OK);
                    }
                }
            }
        });
        Self {
            addr,
            heartbeat,
            server,
            stop,
        }
    }

    fn kill(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        self.server.join().expect("join");
    }
}

impl Rig {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join("engine");
        fs::create_dir(&dir).expect("mkdir");
        let real = dir.join("claude.real");
        {
            let _exec = exec_lock();
            fs::copy(env!("CARGO_BIN_EXE_dense"), dir.join("claude")).expect("copy dense");
            fs::write(&real, FAKE_ENGINE).expect("write engine");
            fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        // A local api, so no test can reach a real one.
        let api = Api::start();
        let port = api.addr.port();
        let rig = Self {
            api: RefCell::new(Some(api)),
            home,
            port,
        };
        fs::create_dir_all(rig.creds()).expect("mkdir");
        fs::write(
            rig.creds().join("profile.toml"),
            format!("name = \"local\"\napi_url = \"http://127.0.0.1:{port}\"\n"),
        )
        .expect("write profile");
        fs::write(
            rig.creds().parent().expect("parent").join("target"),
            "local\n",
        )
        .expect("write target");
        rig
    }

    fn creds(&self) -> PathBuf {
        self.home.path().join(".config/dense/local")
    }

    fn data_dir(&self) -> PathBuf {
        self.home.path().join(".local/share/dense")
    }

    /// The CLI against this rig's home. PATH holds only `tools()`, so no test
    /// can reach this machine's real service manager.
    fn dense(&self) -> Command {
        self.dense_at(Path::new(env!("CARGO_BIN_EXE_dense")))
    }

    fn dense_at(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        cmd.env("HOME", self.home.path())
            .env("PATH", self.tools())
            .env_remove("CONDENSE_URL")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdin(Stdio::null());
        cmd
    }

    fn engines(&self) -> PathBuf {
        self.home.path().join("engines")
    }

    fn entry(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            let plist = "Library/LaunchAgents/chat.condense.dense.daemon.plist";
            return self.home.path().join(plist);
        }
        let unit = ".config/systemd/user/dense-daemon.service";
        self.home.path().join(unit)
    }

    fn fake_service_manager(&self) {
        self.tool("systemctl", "exit 0");
        self.tool("launchctl", "exit 0");
    }

    fn hold_lock(&self) -> File {
        fs::create_dir_all(self.data_dir()).expect("mkdir");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.data_dir().join("desktop.lock"))
            .expect("open lock");
        file.lock().expect("lock");
        file
    }

    fn install(&self) -> Output {
        run(self
            .dense()
            .args(["claude-desktop", "install", "--data-dir"])
            .arg(self.engines()))
    }

    fn install_no_watch(&self) -> Command {
        let mut cmd = self.dense();
        cmd.args(["claude-desktop", "install", "--no-watch", "--data-dir"])
            .arg(self.engines());
        cmd
    }

    /// A copy of dense for commands that act on their own binary.
    fn own_dense(&self) -> PathBuf {
        let exe = self.home.path().join("dense");
        let _exec = exec_lock();
        fs::copy(env!("CARGO_BIN_EXE_dense"), &exe).expect("copy dense");
        exe
    }

    fn once(&self) {
        let out = run(self.dense().args(["daemon", "run", "--once"]));
        assert!(out.status.success(), "{}", stderr(&out));
    }

    fn publish(&self, version: &str) -> PathBuf {
        self.publish_in(&self.engines(), version)
    }

    /// Where Desktop really keeps engines, the folder setup looks in.
    fn publish_desktop(&self, version: &str) -> PathBuf {
        let base = if cfg!(target_os = "macos") {
            "Library/Application Support"
        } else {
            ".config"
        };
        self.publish_in(
            &self.home.path().join(base).join("Claude/claude-code"),
            version,
        )
    }

    fn publish_in(&self, root: &Path, version: &str) -> PathBuf {
        let dir = root.join(version).join("abcdef012345");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("claude"), ENGINE).expect("write engine");
        fs::write(dir.join(".verified"), "manifest-checksum").expect("write marker");
        let sha = hex::encode(Sha256::digest(ENGINE));
        fs::write(dir.join(".payload"), format!(r#"{{"sha256":"{sha}"}}"#)).expect("write record");
        dir
    }

    /// An engine whose restore fails: a folder sits where `claude` goes back.
    fn publish_stuck(&self, version: &str) -> PathBuf {
        let dir = self.publish(version);
        fs::rename(dir.join("claude"), dir.join("claude.real")).expect("move engine");
        fs::create_dir(dir.join("claude")).expect("mkdir");
        fs::write(dir.join("claude").join("in-the-way"), "").expect("write");
        dir
    }

    fn publish_wrapped(&self, version: &str) -> PathBuf {
        let dir = self.publish(version);
        fs::rename(dir.join("claude"), dir.join("claude.real")).expect("move engine");
        fs::write(dir.join("claude"), "dense").expect("write shim");
        dir
    }

    fn reply_to_heartbeats(&self, body: &'static str) {
        let api = self.api.borrow();
        *api.as_ref()
            .expect("api")
            .heartbeat
            .lock()
            .expect("heartbeat") = Some(body);
    }

    fn record_routing(&self, added_daemon: bool) {
        fs::create_dir_all(self.data_dir()).expect("mkdir");
        let engines = self.engines();
        fs::write(
            self.routing_file(),
            format!("added_daemon = {added_daemon}\ndata_dir = {:?}\n", engines),
        )
        .expect("write routing");
    }

    fn register_autostart(&self) {
        let entry = self.entry();
        fs::create_dir_all(entry.parent().expect("parent")).expect("mkdir");
        fs::write(entry, "registered").expect("write entry");
    }

    fn routing_file(&self) -> PathBuf {
        self.data_dir().join("desktop.toml")
    }

    fn tool(&self, name: &str, body: &str) {
        fs::create_dir_all(self.tools()).expect("mkdir");
        let path = self.tools().join(name);
        let _exec = exec_lock();
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write tool");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn tools(&self) -> PathBuf {
        self.home.path().join("tools")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(self.home.path().join("engine").join("claude"));
        cmd.env("HOME", self.home.path())
            .env("ENGINE_REPORT", self.report())
            .env("CLAUDE_CODE_OAUTH_TOKEN", "desktop-oauth")
            .env("ANTHROPIC_BASE_URL", "https://api.anthropic.com")
            .env("ANTHROPIC_CUSTOM_HEADERS", "")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("CONDENSE_AUTH_REQUIRED")
            .env_remove("CONDENSE_URL")
            .stdin(Stdio::null());
        cmd
    }

    fn engine_log(&self) -> String {
        fs::read_to_string(
            self.home
                .path()
                .join(".local/share/dense/desktop-engine.log"),
        )
        .unwrap_or_default()
    }

    fn kill_api(&self) {
        if let Some(api) = self.api.take() {
            api.kill();
        }
    }

    fn log_in(&self) {
        fs::write(self.creds().join("user"), "test-user").expect("write user");
    }

    fn log_in_with_token(&self) {
        fs::write(self.creds().join("token"), "test-token").expect("write token");
    }

    fn report(&self) -> PathBuf {
        self.home.path().join("report")
    }

    fn reported(&self) -> String {
        fs::read_to_string(self.report()).expect("engine ran")
    }

    fn data(&self) -> PathBuf {
        self.home.path().join(".local/share/dense")
    }

    fn routed_base(&self) -> String {
        format!("base=http://127.0.0.1:{}/anthropic", self.port)
    }
}

fn exec_lock() -> MutexGuard<'static, ()> {
    EXEC.lock().unwrap_or_else(PoisonError::into_inner)
}

fn json_reply(body: &str) -> String {
    if body == REJECTED {
        return body.to_string();
    }
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn run(cmd: &mut Command) -> Output {
    spawn(cmd.stdout(Stdio::piped()).stderr(Stdio::piped()))
        .wait_with_output()
        .expect("run")
}

fn spawn(cmd: &mut Command) -> Child {
    let _exec = exec_lock();
    cmd.spawn().expect("spawn")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stream(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wait_or_kill(child: &mut Child, limit: Duration) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("poll") {
            return status;
        }
        let late = start.elapsed() > limit;
        if late {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(!late, "still running after {limit:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for(path: &Path, child: &mut Child) {
    let start = Instant::now();
    while !path.exists() {
        assert!(child.try_wait().expect("poll").is_none(), "engine exited");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "engine never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn logged_out_runs_the_engine_untouched() {
    let rig = Rig::new();
    let out = run(rig.command().args(["--output-format", "stream-json"]));

    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stream(&out), "{\"stream\":\"only this\"}\n");
    let report = rig.reported();
    assert!(
        report.contains("base=https://api.anthropic.com"),
        "{report}"
    );
    assert!(report.contains("kind=0"), "{report}");
}

#[test]
fn logged_in_routes_and_keeps_desktops_token() {
    let rig = Rig::new();
    rig.log_in();
    let out = run(rig.command().args(["--output-format", "stream-json"]));

    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stream(&out), "{\"stream\":\"only this\"}\n");
    let report = rig.reported();
    assert!(report.contains(&rig.routed_base()), "{report}");
    assert!(report.contains("token=desktop-oauth"), "{report}");
    assert!(report.contains("kind=1"), "{report}");
    assert!(
        report.contains("args=--plugin-dir ") && report.ends_with("--output-format stream-json\n"),
        "{report}"
    );
    assert!(
        rig.data_dir()
            .join("harness/claude/commands/info.md")
            .is_file(),
        "{report}"
    );
}

#[cfg(feature = "offline-usage")]
#[test]
fn routed_engine_carries_the_device_and_records_its_span() {
    let rig = Rig::new();
    rig.log_in();
    fs::create_dir_all(rig.data().join("daemon")).expect("daemon dir");
    fs::write(rig.data().join("daemon/state.toml"), "managed = true\n").expect("state");
    let out = run(rig.command().args(["--output-format", "stream-json"]));

    assert_eq!(out.status.code(), Some(3));
    let report = rig.reported();
    assert!(report.contains("device=1"), "{report}");
    let device = fs::read_to_string(rig.creds().join("device")).expect("device id");
    assert_eq!(device.trim().len(), 36);
    let spans: Vec<_> = fs::read_dir(rig.data().join("routed"))
        .expect("routed dir")
        .flatten()
        .collect();
    assert_eq!(spans.len(), 1);
    let span = fs::read_to_string(spans[0].path()).expect("span");
    assert!(span.contains("\"end_ms\""), "{span}");
}

#[cfg(feature = "desktop-switch")]
#[test]
fn org_off_switch_runs_direct_until_it_is_back_on() {
    let rig = Rig::new();
    rig.log_in();
    let state = rig.data().join("daemon/state.toml");
    fs::create_dir_all(state.parent().expect("parent")).expect("mkdir");

    let user = {
        use sha2::{Digest, Sha256};
        hex::encode(&Sha256::digest(b"local\ntest-user")[..8])
    };
    fs::write(
        &state,
        "managed = true\nenabled = false\nuser = \"someone-else\"\n",
    )
    .expect("write state");
    run(rig.command().args(["--output-format", "stream-json"]));
    assert!(rig.reported().contains(&rig.routed_base()));

    fs::write(
        &state,
        format!("managed = true\nenabled = false\nuser = \"{user}\"\n"),
    )
    .expect("write state");
    let out = run(rig.command().args(["--output-format", "stream-json"]));
    assert_eq!(out.status.code(), Some(3));
    let report = rig.reported();
    assert!(
        report.contains("base=https://api.anthropic.com"),
        "{report}"
    );
    assert!(report.contains("kind=0"), "{report}");
    assert!(
        rig.engine_log().contains("turned off"),
        "{}",
        rig.engine_log()
    );

    fs::write(
        &state,
        format!("managed = true\nenabled = true\nuser = \"{user}\"\n"),
    )
    .expect("write state");
    run(rig.command().args(["--output-format", "stream-json"]));
    assert!(rig.reported().contains(&rig.routed_base()));
}

#[cfg(feature = "remote-control")]
#[test]
fn dense_claude_runs_the_tool_directly_while_switched_off() {
    let rig = Rig::new();
    rig.log_in();
    let bin = rig.home.path().join("bin");
    fs::create_dir(&bin).expect("mkdir");
    fs::copy(
        rig.home.path().join("engine/claude.real"),
        bin.join("claude"),
    )
    .expect("copy");
    let user = {
        use sha2::{Digest, Sha256};
        hex::encode(&Sha256::digest(b"local\ntest-user")[..8])
    };
    let state = rig.data().join("daemon/state.toml");
    fs::create_dir_all(state.parent().expect("parent")).expect("mkdir");
    fs::write(
        &state,
        format!("managed = true\nenabled = false\nuser = \"{user}\"\n"),
    )
    .expect("write state");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let cmd = rig.command();
    let mut dense = Command::new(env!("CARGO_BIN_EXE_dense"));
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => dense.env(k, v),
            None => dense.env_remove(k),
        };
    }
    let out = run(dense
        .env("PATH", path)
        .args(["claude", "-p", "hi"])
        .stdin(Stdio::null()));
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("turned off"));
    let report = rig.reported();
    assert!(report.contains("kind=0"), "{report}");
    assert!(report.contains("args=-p hi"), "{report}");
    assert!(!rig.data().join("routed").exists());
}

#[cfg(not(feature = "offline-usage"))]
#[test]
fn a_build_without_offline_usage_records_no_span() {
    let rig = Rig::new();
    rig.log_in();
    let out = run(rig.command().args(["--output-format", "stream-json"]));

    assert_eq!(out.status.code(), Some(3));
    assert!(!rig.data().join("routed").exists());
}

/// Management can start mid-session, and a routed session with no span would
/// then be read from its session file and counted a second time.
#[cfg(feature = "offline-usage")]
#[test]
fn a_device_not_yet_managed_still_records_a_closed_span() {
    let rig = Rig::new();
    rig.log_in();
    let out = run(rig.command().args(["--output-format", "stream-json"]));

    assert_eq!(out.status.code(), Some(3));
    assert!(rig.reported().contains("device=1"));
    let spans: Vec<_> = std::fs::read_dir(rig.data().join("routed"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(spans.len(), 1);
    assert!(spans[0].contains("\"end_ms\":"), "{}", spans[0]);
    assert!(!spans[0].contains("\"end_ms\":null"), "{}", spans[0]);
}

#[test]
fn version_probe_is_not_routed() {
    let rig = Rig::new();
    rig.log_in();
    run(rig.command().arg("--version"));
    assert!(rig.reported().contains("base=https://api.anthropic.com"));
}

#[test]
fn third_party_gateway_is_not_rerouted() {
    let rig = Rig::new();
    rig.log_in();
    run(rig
        .command()
        .env("ANTHROPIC_BASE_URL", "https://gateway.example/anthropic")
        .arg("-p"));
    assert!(
        rig.reported()
            .contains("base=https://gateway.example/anthropic")
    );
}

#[test]
fn unreachable_condense_with_a_token_runs_direct() {
    let rig = Rig::new();
    rig.log_in_with_token();
    rig.kill_api();

    let out = run(rig.command().arg("-p"));
    assert_eq!(out.status.code(), Some(3));
    assert!(rig.reported().contains("base=https://api.anthropic.com"));
}

#[test]
fn a_fresh_probe_stamp_does_not_route_to_a_dead_host() {
    let rig = Rig::new();
    rig.log_in_with_token();
    run(rig.command().arg("-p"));
    assert!(rig.reported().contains(&rig.routed_base()));

    rig.kill_api();
    let out = run(rig.command().arg("-p"));
    assert_eq!(out.status.code(), Some(3));
    assert!(rig.reported().contains("base=https://api.anthropic.com"));
}

#[test]
fn each_agent_launch_logs_one_line_and_no_token() {
    let rig = Rig::new();
    rig.log_in_with_token();
    run(rig.command().arg("-p"));
    let session = rig.reported();
    let session = session
        .lines()
        .find_map(|l| l.strip_prefix("session="))
        .expect("session reported");
    run(rig.command().arg("--version"));
    rig.kill_api();
    run(rig.command().arg("-p"));

    let log = rig.engine_log();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(lines[0].contains(&format!("routed launch: session {session} via 127.0.0.1")));
    assert!(lines[1].contains("direct launch: 127.0.0.1"), "{log}");
    assert!(lines[1].contains("unreachable"), "{log}");
    assert!(!log.contains("test-token"), "{log}");
    assert!(!log.contains("desktop-oauth"), "{log}");
}

#[test]
fn sigterm_reaches_the_engine_and_its_status_comes_back() {
    let rig = Rig::new();
    rig.log_in();
    let mut child = spawn(
        rig.command()
            .env("ENGINE_WAIT", "1")
            .arg("-p")
            .stdout(Stdio::null()),
    );
    wait_for(&rig.report().with_extension("ready"), &mut child);

    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("signal");

    assert_eq!(child.wait().expect("wait").code(), Some(7));
    assert!(rig.report().with_extension("signal").exists());
}

#[test]
fn a_slow_git_does_not_hold_up_stop_signals() {
    let rig = Rig::new();
    rig.log_in();
    rig.tool("git", "sleep 5");
    let path = format!(
        "{}:{}",
        rig.tools().display(),
        std::env::var("PATH").expect("PATH")
    );
    let mut child = spawn(
        rig.command()
            .env("PATH", path)
            .env("ENGINE_WAIT", "1")
            .arg("-p")
            .stdout(Stdio::null()),
    );
    wait_for(&rig.report().with_extension("ready"), &mut child);

    let sent = Instant::now();
    let pid = rustix::process::Pid::from_child(&child);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("signal");

    assert_eq!(child.wait().expect("wait").code(), Some(7));
    let took = sent.elapsed();
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[test]
fn doctor_keeps_reporting_after_desktop_replaced_every_engine() {
    let rig = Rig::new();
    let doctor = || stream(&run(rig.dense().arg("doctor")));
    assert!(!doctor().contains("daemon not running"));

    rig.record_routing(false);
    rig.publish("2.1.309");
    let report = doctor();
    assert!(report.contains("daemon not running"), "{report}");
    assert!(report.contains("engine not routed"), "{report}");
}

#[test]
fn the_watcher_is_announced_only_once_the_daemon_started() {
    let rig = Rig::new();
    rig.publish("2.1.309");

    // No service manager on PATH, so the daemon can't start.
    let failed = rig.install();
    assert!(failed.status.success(), "{}", stderr(&failed));
    assert!(!stream(&failed).contains("new engines connect automatically"));
    assert!(stderr(&failed).contains("could not be started"));

    rig.fake_service_manager();
    assert!(stream(&rig.install()).contains("new engines connect automatically"));
}

#[test]
fn logged_out_says_wrapped_and_never_claims_routing() {
    let rig = Rig::new();
    rig.publish("2.1.309");
    let status = || {
        let mut cmd = rig.dense();
        cmd.args(["claude-desktop", "status", "--data-dir"])
            .arg(rig.engines());
        stream(&run(&mut cmd))
    };

    let out = stream(&run(&mut rig.install_no_watch()));
    assert!(!out.contains("route through"), "{out}");
    assert!(out.contains("Log in to connect"), "{out}");
    assert!(out.contains("`dense login`"), "{out}");
    assert!(
        status()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("login required (sessions run directly)")
    );

    rig.log_in();
    let out = stream(&run(&mut rig.install_no_watch()));
    assert!(out.contains("route through 127.0.0.1"), "{out}");
    assert!(status().contains(": Connected\n"));
}

#[test]
fn repeated_desktop_uninstall_explains_that_nothing_remains() {
    let rig = Rig::new();
    let engine = rig.publish("2.1.309");
    assert!(run(&mut rig.install_no_watch()).status.success());
    for expected in [
        "Condense disconnected from Claude Desktop",
        "Already disconnected. Nothing to remove",
    ] {
        let out = run(rig
            .dense()
            .args(["claude-desktop", "uninstall", "--data-dir"])
            .arg(rig.engines()));
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(stream(&out).contains(expected), "{}", stream(&out));
        assert!(stream(&out).contains("Reconnect: dense claude-desktop install"));
        assert!(!stream(&out).contains('\x1b'));
        assert_eq!(
            fs::read(engine.join("claude")).expect("restored engine"),
            ENGINE
        );
        assert!(!engine.join("claude.real").exists());
    }
}

#[test]
fn an_unstarted_watcher_never_promises_to_connect_future_engines() {
    let rig = Rig::new();
    let out = rig.install();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("could not be started"));
    assert!(!stream(&out).contains("will prepare"));
    assert!(!stream(&out).contains("new engines connect automatically"));
    assert!(
        stream(&out)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("run this again")
    );
}

// Runs the real seal check, so it lives here: a spawn inside a unit test holds
// that process's sockets and locks for an instant, which fails its neighbours.
#[test]
fn a_bundle_anthropic_did_not_sign_is_left_untouched() {
    let rig = Rig::new();
    let dir = rig.publish("2.1.309");
    let macos = dir.join("claude.app/Contents/MacOS");
    fs::create_dir_all(&macos).expect("mkdir");
    fs::rename(dir.join("claude"), macos.join("claude")).expect("move engine");

    let out = run(&mut rig.install_no_watch());
    assert!(
        stderr(&out)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("isn't signed by Anthropic"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fs::read(macos.join("claude")).expect("engine"), ENGINE);
    assert!(!dir.join("claude.real.app").exists());
}

#[test]
fn install_names_the_host_the_engine_will_really_use() {
    let rig = Rig::new();
    rig.log_in();
    rig.publish("2.1.309");
    let target = rig.creds().parent().expect("parent").join("target");

    let out = run(rig
        .install_no_watch()
        .args(["--url", "https://elsewhere.example"]));
    assert!(stream(&out).contains(&format!("route through 127.0.0.1:{}", rig.port)));
    assert!(
        stderr(&out).contains("elsewhere.example"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fs::read_to_string(target).expect("target"), "local\n");

    assert!(stderr(&run(&mut rig.install_no_watch())).is_empty());
}

#[test]
fn uninstall_removes_only_a_daemon_that_install_added() {
    let rig = Rig::new();
    rig.publish("2.1.309");
    rig.fake_service_manager();
    let added = || {
        fs::read_to_string(rig.routing_file())
            .expect("routing")
            .contains("added_daemon = true")
    };
    let uninstall = || {
        run(rig
            .dense()
            .args(["claude-desktop", "uninstall", "--data-dir"])
            .arg(rig.engines()))
    };

    rig.install();
    assert!(added());
    rig.install();
    assert!(added(), "a rerun must not forget it added the daemon");

    let explicit = run(rig.dense().args(["daemon", "install"]));
    assert!(explicit.status.success(), "{}", stderr(&explicit));
    assert!(!added());
    uninstall();
    assert!(rig.entry().exists());

    rig.install();
    assert!(!added(), "the daemon was already the user's");
    run(rig.dense().args(["daemon", "uninstall"]));
    rig.install();
    assert!(added(), "the flag must not outlive the daemon it described");
    uninstall();
    assert!(!rig.entry().exists());
}

#[test]
fn uninstall_finishes_everything_after_a_failed_restore() {
    let rig = Rig::new();
    rig.publish_stuck("2.1.1");
    let good = rig.publish_wrapped("2.1.2");
    rig.record_routing(true);
    rig.register_autostart();
    rig.fake_service_manager();

    let out = run(rig.dense().args(["claude-desktop", "uninstall"]));
    assert!(!out.status.success());
    assert_eq!(fs::read(good.join("claude")).expect("engine"), ENGINE);
    assert!(!rig.entry().exists());
}

#[test]
fn self_uninstall_stops_the_daemon_before_it_unwraps() {
    let rig = Rig::new();
    let dir = rig.publish_wrapped("2.1.309");
    rig.record_routing(true);
    rig.register_autostart();
    let seen = rig.home.path().join("wrapped-at-stop");
    let stop = format!(
        "[ -e {:?} ] && : > {seen:?}\nexit 0",
        dir.join("claude.real")
    );
    rig.tool("systemctl", &stop);
    rig.tool("launchctl", &stop);
    let exe = rig.own_dense();

    let out = run(rig.dense_at(&exe).args(["self", "uninstall"]));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(seen.exists());
    assert_eq!(fs::read(dir.join("claude")).expect("engine"), ENGINE);
}

#[test]
fn self_uninstall_warns_and_carries_on() {
    let rig = Rig::new();
    let exe = rig.own_dense();
    rig.publish_stuck("2.1.1");
    rig.record_routing(false);
    // No service manager on PATH, so stopping the daemon leaves a note.
    rig.register_autostart();

    let out = run(rig.dense_at(&exe).args(["self", "uninstall"]));
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("Could not restore"), "{err}");
    assert!(err.contains("Uninstall finished with issues"), "{err}");
    assert!(!stream(&out).contains("Dense removed"));
    assert!(!stream(&out).contains("Reconnect:"));
    assert!(err.contains("ctl"), "{err}");
    assert!(!rig.entry().exists());
    assert!(!rig.data_dir().exists());
    assert!(!exe.exists());
}

#[cfg(feature = "managed-setup")]
#[test]
fn a_managed_on_routes_desktop_and_refuses_to_take_it_back() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(MANAGED_ON);
    let dir = rig.publish_desktop("2.1.309");

    rig.once();
    rig.once();
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);
    assert!(!rig.routing_file().exists());
    let status = run(rig.dense().args(["claude-desktop", "status"]));
    assert!(!stream(&status).contains("Disabled; run"));

    let undo: [&[&str]; 3] = [
        &["claude-desktop", "uninstall"],
        &["unpersist"],
        &["daemon", "uninstall"],
    ];
    for args in undo {
        let out = run(rig.dense().args(args));
        assert!(!out.status.success(), "{args:?} ran on a managed device");
        assert!(stderr(&out).contains("managed remotely by your organization"));
    }
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);
}

#[cfg(feature = "managed-setup")]
#[test]
fn a_managed_off_still_refuses_manual_changes() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(MANAGED_OFF);
    rig.once();
    let out = run(rig.dense().args(["unpersist"]));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("`dense unpersist` is turned off"));
}

#[test]
fn a_device_the_org_does_not_manage_can_still_undo_routing() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(REGULAR);
    rig.once();
    let undo: [&[&str]; 3] = [
        &["claude-desktop", "uninstall"],
        &["unpersist"],
        &["daemon", "uninstall"],
    ];
    for args in undo {
        let out = run(rig.dense().args(args));
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    }
}

#[cfg(feature = "managed-setup")]
#[test]
fn a_managed_off_keeps_wrapped_engines_and_wraps_no_new_one() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(MANAGED_ON);
    let wrapped = rig.publish_desktop("2.1.309");
    rig.once();
    rig.once();

    rig.reply_to_heartbeats(MANAGED_OFF);
    rig.once();
    let new = rig.publish_desktop("2.1.310");
    rig.once();
    assert_eq!(
        fs::read(wrapped.join("claude.real")).expect("engine"),
        ENGINE
    );
    assert_eq!(fs::read(new.join("claude")).expect("engine"), ENGINE);
    assert!(!new.join("claude.real").exists());
}

#[cfg(feature = "managed-setup")]
#[test]
fn setup_on_a_managed_device_asks_nothing() {
    let rig = Rig::new();
    rig.fake_service_manager();
    rig.log_in();
    rig.reply_to_heartbeats(MANAGED_ON);
    rig.publish_desktop("2.1.309");

    let out = run(rig.dense().arg("setup"));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(said.contains("nothing else to set up"), "{said}");
    assert!(!said.contains("Which tools"), "{said}");
    assert!(!said.contains("Route Claude Desktop"), "{said}");
    assert!(rig.entry().exists());
}

#[test]
fn a_device_the_org_does_not_manage_is_only_ever_wrapped_by_hand() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(REGULAR);
    let dir = rig.publish_desktop("2.1.309");
    rig.once();
    rig.once();
    assert_eq!(fs::read(dir.join("claude")).expect("engine"), ENGINE);
    assert!(!dir.join("claude.real").exists());
}

#[cfg(not(feature = "managed-setup"))]
#[test]
fn a_build_without_managed_setup_leaves_desktop_to_the_user() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(MANAGED_ON);
    let dir = rig.publish_desktop("2.1.309");
    rig.once();
    rig.once();
    assert!(!dir.join("claude.real").exists());
}

#[test]
fn install_before_desktop_and_the_watcher_follows_it_through_an_uninstall() {
    let rig = Rig::new();
    rig.fake_service_manager();
    let out = rig.install();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hasn't run on this computer yet"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let status = run(rig
        .dense()
        .args(["claude-desktop", "status", "--data-dir"])
        .arg(rig.engines()));
    assert!(String::from_utf8_lossy(&status.stdout).contains("No engine yet"));
    rig.once();

    let dir = rig.publish("2.1.309");
    rig.once();
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);

    fs::remove_dir_all(rig.engines()).expect("Desktop is uninstalled");
    rig.once();
    let dir = rig.publish("2.1.310");
    rig.once();
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);
}

#[test]
fn install_with_desktop_but_no_engine_routes_the_first_download() {
    let rig = Rig::new();
    rig.fake_service_manager();
    fs::create_dir_all(rig.engines()).expect("mkdir");
    let out = rig.install();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("the next one Desktop downloads"));
}

#[test]
fn install_without_the_watcher_says_to_run_it_again() {
    let rig = Rig::new();
    let out = run(&mut rig.install_no_watch());
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("run this again")
    );
    assert!(!rig.routing_file().exists());
}

#[test]
fn a_daemon_on_a_new_build_replaces_older_wrappers() {
    let rig = Rig::new();
    let dir = rig.publish_wrapped("2.1.309");
    rig.record_routing(false);
    rig.once();
    let current = fs::read(env!("CARGO_BIN_EXE_dense")).expect("dense");
    assert!(fs::read(dir.join("claude")).expect("wrapper") == current);
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);
}

#[cfg(feature = "remote-control")]
#[test]
fn a_managed_on_appends_to_a_profile_it_cannot_decode() {
    let rig = Rig::new();
    rig.log_in();
    rig.tool("claude", "exit 0");
    rig.reply_to_heartbeats(MANAGED_ON);
    let profile = rig.home.path().join(".zshrc");
    fs::write(&profile, b"export NAME=Jos\xE9\n").expect("profile");

    rig.once();

    let wired = fs::read(&profile).expect("profile");
    assert!(wired.starts_with(b"export NAME=Jos\xE9\n"));
    assert!(String::from_utf8_lossy(&wired).contains("# >>> dense >>>"));
}

#[cfg(feature = "remote-control")]
#[test]
fn a_managed_on_respects_no_modify_path() {
    let rig = Rig::new();
    rig.log_in();
    rig.tool("claude", "exit 0");
    rig.tool("codex", "exit 0");
    rig.reply_to_heartbeats(MANAGED_ON);
    let profile = rig.home.path().join(".zshrc");
    fs::write(&profile, "export FOO=1\n").expect("profile");
    let persisted = run(rig.dense().args(["persist", "--no-modify-path", "claude"]));
    assert!(persisted.status.success(), "{}", stderr(&persisted));

    rig.once();

    assert!(rig.data_dir().join("bin/codex").exists());
    assert_eq!(fs::read(&profile).expect("profile"), b"export FOO=1\n");
}

#[cfg(feature = "managed-setup")]
#[test]
fn leaving_the_org_restores_what_the_org_wrapped() {
    let rig = Rig::new();
    rig.log_in();
    let dir = rig.publish_desktop("2.1.309");
    rig.reply_to_heartbeats(MANAGED_ON);
    rig.once();
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);

    rig.reply_to_heartbeats(REGULAR);
    rig.once();
    assert_eq!(fs::read(dir.join("claude")).expect("engine"), ENGINE);
    assert!(!dir.join("claude.real").exists());
    assert!(!rig.data_dir().join("desktop-forced").exists());
}

#[cfg(feature = "managed-setup")]
#[test]
fn a_rejected_heartbeat_is_no_reason_to_restore_anything() {
    let rig = Rig::new();
    rig.log_in();
    let dir = rig.publish_desktop("2.1.309");
    rig.reply_to_heartbeats(MANAGED_ON);
    rig.once();

    rig.reply_to_heartbeats(REJECTED);
    rig.once();
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);
    assert!(rig.data_dir().join("desktop-forced").exists());
}

#[cfg(feature = "managed-setup")]
#[test]
fn leaving_the_org_keeps_routing_the_user_chose() {
    let rig = Rig::new();
    rig.fake_service_manager();
    rig.log_in();
    let dir = rig.publish_desktop("2.1.309");
    rig.reply_to_heartbeats(MANAGED_ON);
    rig.once();
    let out = run(rig.dense().args(["claude-desktop", "install"]));
    assert!(out.status.success(), "{}", stderr(&out));

    rig.reply_to_heartbeats(REGULAR);
    rig.once();
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);
}

#[cfg(feature = "managed-setup")]
#[test]
fn a_laptop_back_after_leaving_the_org_wraps_nothing_new() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(MANAGED_ON);
    rig.once();

    let dir = rig.publish_desktop("2.1.309");
    rig.reply_to_heartbeats(REGULAR);
    rig.once();
    assert!(!dir.join("claude.real").exists());
}

#[cfg(feature = "login-autostart")]
#[test]
fn a_login_from_before_device_management_enrols_at_the_first_launch() {
    let rig = Rig::new();
    rig.fake_service_manager();
    rig.tool("claude", "exit 0");
    rig.log_in();
    rig.reply_to_heartbeats(MANAGED_ON);
    let launch = || run(rig.dense().args(["claude", "-p", "hi"]));
    assert!(launch().status.success());
    assert!(rig.entry().exists());
}

#[cfg(feature = "login-autostart")]
#[test]
fn a_regular_user_enrols_nothing_and_checks_only_once() {
    let rig = Rig::new();
    rig.fake_service_manager();
    rig.tool("claude", "exit 0");
    rig.log_in();
    rig.reply_to_heartbeats(REGULAR);
    assert!(
        run(rig.dense().args(["claude", "-p", "hi"]))
            .status
            .success()
    );
    assert!(!rig.entry().exists());
    let state = rig.data_dir().join("daemon/state.toml");
    let checked = fs::read(&state).expect("state");
    assert!(
        run(rig.dense().args(["claude", "-p", "hi"]))
            .status
            .success()
    );
    assert_eq!(fs::read(&state).expect("state"), checked);
}

#[test]
fn setup_offers_desktop_before_it_is_installed_and_defaults_to_no() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(REGULAR);
    let out = run(rig.dense().arg("setup"));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        said.contains("Route Claude Desktop through condense?"),
        "{said}"
    );
    assert!(said.contains("hasn't run on this computer yet"), "{said}");
    assert!(!rig.routing_file().exists());
}

#[test]
fn setup_offers_an_installed_desktop_and_leaves_it_alone_by_default() {
    let rig = Rig::new();
    rig.log_in();
    rig.reply_to_heartbeats(REGULAR);
    let dir = rig.publish_desktop("2.1.309");
    let out = run(rig.dense().arg("setup"));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        said.contains("Route Claude Desktop through condense?"),
        "{said}"
    );
    assert!(!said.contains("hasn't run on this computer yet"), "{said}");
    assert!(!dir.join("claude.real").exists());
    assert!(!rig.routing_file().exists());
}

#[test]
fn a_recorded_data_dir_reaches_install_and_uninstall() {
    let rig = Rig::new();
    let dir = rig.publish("2.1.309");
    rig.record_routing(false);

    let out = run(rig
        .dense()
        .args(["claude-desktop", "install", "--no-watch"]));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read(dir.join("claude.real")).expect("engine"), ENGINE);

    let out = run(rig.dense().args(["claude-desktop", "uninstall"]));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read(dir.join("claude")).expect("engine"), ENGINE);
    assert!(!dir.join("claude.real").exists());
}

#[test]
fn install_and_uninstall_wait_for_the_lock() {
    let rig = Rig::new();
    let dir = rig.publish("2.1.309");
    let queued = |cmd: &mut Command, moved: &dyn Fn() -> bool| {
        let held = rig.hold_lock();
        let mut child = spawn(cmd.stdout(Stdio::null()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(child.try_wait().expect("poll").is_none());
        assert!(!moved());
        drop(held);
        assert!(wait_or_kill(&mut child, Duration::from_secs(30)).success());
        assert!(moved());
    };

    queued(&mut rig.install_no_watch(), &|| {
        dir.join("claude.real").exists()
    });
    queued(
        rig.dense()
            .args(["claude-desktop", "uninstall", "--data-dir"])
            .arg(rig.engines()),
        &|| !dir.join("claude.real").exists(),
    );
}

#[test]
fn a_changed_binary_stops_the_daemon_with_a_failure() {
    let rig = Rig::new();
    let exe = rig.own_dense();
    // The first pass parks on the lock, which holds the daemon inside its loop.
    let held = rig.hold_lock();
    let mut daemon = spawn(
        rig.dense_at(&exe)
            .args(["daemon", "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    wait_for(&rig.data_dir().join("daemon/state.toml"), &mut daemon);

    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    File::open(&exe)
        .expect("open")
        .set_modified(an_hour_ago)
        .expect("touch");
    drop(held);

    assert_eq!(
        wait_or_kill(&mut daemon, Duration::from_secs(10)).code(),
        Some(1)
    );
}
