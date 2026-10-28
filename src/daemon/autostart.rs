//! Per-user autostart for `dense daemon run`: a systemd user unit (Linux), a
//! launchd agent (macOS), a hidden Startup-folder launcher (Windows). The
//! entry pins `--url` so the daemon serves the profile it was installed from,
//! and carries the installing shell's PATH/XDG dirs so it finds the same
//! tools and credentials. There is one entry per user: one daemon owns the
//! shared shim dir.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::Config;
use crate::error::{Context, Error};

const LABEL: &str = "chat.condense.dense.daemon";
#[cfg(all(unix, not(target_os = "macos")))]
const UNIT: &str = "dense-daemon.service";
/// Milliseconds the Windows launcher waits before restarting a crashed daemon.
const VBS_RESTART_MS: u32 = 10_000;

/// What install/uninstall did, for the caller to print.
pub struct Report {
    pub notes: Vec<String>,
    pub path: PathBuf,
}

/// `dense --url <api> daemon run` — the command every autostart entry runs.
pub fn command(cfg: &Config) -> Result<(PathBuf, Vec<String>)> {
    let exe = std::env::current_exe().ctx("locating the dense binary")?;
    Ok((
        exe,
        vec![
            "--url".into(),
            cfg.api_base_url.clone(),
            "daemon".into(),
            "run".into(),
        ],
    ))
}

/// Write the entry and start it now. Fails (leaving the entry, which still
/// applies at next login) when the service manager would not start it.
pub fn install(cfg: &Config) -> Result<Report> {
    let path = entry_path(cfg)?;
    let (exe, args) = command(cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    std::fs::create_dir_all(cfg.daemon_dir()).ctx("creating dense daemon dir")?;
    let mut notes = Vec::new();
    if let Ok(old) = std::fs::read_to_string(&path)
        && !old.contains(&cfg.api_base_url)
    {
        notes.push("replaced the autostart entry of another profile".to_string());
    }
    std::fs::write(&path, render(cfg, &exe, &args, &env_vars()))
        .ctx(format!("writing {}", path.display()))?;
    activate(&path).map_err(|e| {
        Error::msg(format!(
            "autostart entry written to {} but the daemon could not be started: {e}",
            path.display()
        ))
    })?;
    Ok(Report { notes, path })
}

/// The autostart entry, if installed.
pub fn installed(cfg: &Config) -> Option<PathBuf> {
    entry_path(cfg).ok().filter(|p| p.exists())
}

/// Stop and remove the entry; `None` when there was nothing installed.
/// `pid` is the running daemon's, for platforms that stop it by pid.
pub fn uninstall(cfg: &Config, pid: Option<u32>) -> Result<Option<Report>> {
    let Some(path) = installed(cfg) else {
        return Ok(None);
    };
    let notes = deactivate(&path, pid);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).ctx(format!("removing {}", path.display())),
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = run("systemctl", &["--user", "daemon-reload"]);
    Ok(Some(Report { notes, path }))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn activate(_path: &Path) -> Result<()> {
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", "--now", UNIT])
}

#[cfg(target_os = "macos")]
fn activate(path: &Path) -> Result<()> {
    let p = path.to_string_lossy();
    let _ = run("launchctl", &["unload", &p]);
    run("launchctl", &["load", "-w", &p])
}

#[cfg(windows)]
fn activate(path: &Path) -> Result<()> {
    std::process::Command::new("wscript.exe")
        .arg(path)
        .spawn()
        .map(drop)
        .map_err(|e| Error::msg(format!("running wscript.exe: {e}")))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn deactivate(_path: &Path, _pid: Option<u32>) -> Vec<String> {
    run("systemctl", &["--user", "disable", "--now", UNIT])
        .err()
        .map(|e| vec![format!("systemctl disable: {e}")])
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn deactivate(path: &Path, _pid: Option<u32>) -> Vec<String> {
    run("launchctl", &["unload", "-w", &path.to_string_lossy()])
        .err()
        .map(|e| vec![format!("launchctl unload: {e}")])
        .unwrap_or_default()
}

/// The launcher restarts the daemon only while its own file exists, so the
/// file goes first; then the daemon is stopped.
#[cfg(windows)]
fn deactivate(path: &Path, pid: Option<u32>) -> Vec<String> {
    let mut notes = Vec::new();
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        notes.push(format!("removing {}: {e}", path.display()));
    }
    let Some(pid) = pid else { return notes };
    let image = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "dense.exe".into());
    // The image filter keeps a stale, reused pid from killing something else.
    if let Err(e) = run(
        "taskkill",
        &[
            "/F",
            "/FI",
            &format!("PID eq {pid}"),
            "/FI",
            &format!("IMAGENAME eq {image}"),
        ],
    ) {
        notes.push(format!("could not stop daemon pid {pid}: {e}"));
    }
    notes
}

#[cfg(all(unix, not(target_os = "macos")))]
fn entry_path(cfg: &Config) -> Result<PathBuf> {
    let base = cfg
        .config_dir()
        .parent()
        .ok_or_else(|| Error::msg("config dir has no parent"))?;
    Ok(base.join("systemd").join("user").join(UNIT))
}

#[cfg(target_os = "macos")]
fn entry_path(cfg: &Config) -> Result<PathBuf> {
    Ok(cfg
        .home()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup` — the config
/// dir is `%APPDATA%\dense`.
#[cfg(windows)]
fn entry_path(cfg: &Config) -> Result<PathBuf> {
    let appdata = cfg
        .config_dir()
        .parent()
        .ok_or_else(|| Error::msg("config dir has no parent"))?;
    Ok(appdata
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
        .join("dense-daemon.vbs"))
}

/// Environment the daemon needs to see what the installing shell sees. A
/// Windows logon session already carries the user's environment.
fn env_vars() -> Vec<(String, String)> {
    if cfg!(windows) {
        return Vec::new();
    }
    ["PATH", "XDG_CONFIG_HOME", "XDG_DATA_HOME"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn render(_cfg: &Config, exe: &Path, args: &[String], env: &[(String, String)]) -> String {
    render_systemd(exe, args, env)
}

#[cfg(target_os = "macos")]
fn render(cfg: &Config, exe: &Path, args: &[String], env: &[(String, String)]) -> String {
    render_launchd(&cfg.daemon_dir().join("daemon.log"), exe, args, env)
}

#[cfg(windows)]
fn render(_cfg: &Config, exe: &Path, args: &[String], _env: &[(String, String)]) -> String {
    render_vbs(exe, args)
}

/// systemd expands `%` specifiers everywhere and `$VAR` in `ExecStart`,
/// quoted or not, so both are doubled.
#[cfg_attr(
    not(any(test, all(unix, not(target_os = "macos")))),
    expect(dead_code, reason = "the Linux renderer")
)]
fn render_systemd(exe: &Path, args: &[String], env: &[(String, String)]) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let exec: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| quote(&a.replace('%', "%%").replace('$', "$$")))
        .collect();
    let environment: String = env
        .iter()
        .map(|(k, v)| {
            format!(
                "Environment={}\n",
                quote(&format!("{k}={v}").replace('%', "%%"))
            )
        })
        .collect();
    format!(
        "[Unit]\nDescription=dense device daemon\nAfter=network-online.target\n\n\
         [Service]\n{environment}ExecStart={}\nRestart=always\nRestartSec=10\n\n\
         [Install]\nWantedBy=default.target\n",
        exec.join(" ")
    )
}

#[cfg_attr(
    not(any(test, target_os = "macos")),
    expect(dead_code, reason = "the macOS renderer")
)]
fn render_launchd(log: &Path, exe: &Path, args: &[String], env: &[(String, String)]) -> String {
    let program: String = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| format!("    <string>{}</string>\n", xml_escape(&a)))
        .collect();
    let vars: String = env
        .iter()
        .map(|(k, v)| {
            format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v)
            )
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key>\n  <string>{LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n{program}  </array>\n\
         \x20 <key>EnvironmentVariables</key>\n  <dict>\n{vars}  </dict>\n\
         \x20 <key>RunAtLoad</key>\n  <true/>\n\
         \x20 <key>KeepAlive</key>\n  <true/>\n\
         \x20 <key>StandardErrorPath</key>\n  <string>{}</string>\n\
         </dict>\n</plist>\n",
        xml_escape(&log.to_string_lossy())
    )
}

/// A VBScript launcher: `Run …, 0, True` runs the console binary with no
/// window and waits; a non-zero exit is restarted while this file exists.
#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "the Windows renderer")
)]
fn render_vbs(exe: &Path, args: &[String]) -> String {
    let line: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| format!("\"{a}\""))
        .collect();
    format!(
        "Set sh = CreateObject(\"WScript.Shell\")\r\n\
         Set fso = CreateObject(\"Scripting.FileSystemObject\")\r\n\
         Do\r\n\
         \x20 rc = sh.Run(\"{}\", 0, True)\r\n\
         \x20 If rc = 0 Or Not fso.FileExists(WScript.ScriptFullName) Then Exit Do\r\n\
         \x20 WScript.Sleep {VBS_RESTART_MS}\r\n\
         Loop\r\n",
        line.join(" ").replace('"', "\"\"")
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::msg(format!("running {program}: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::msg(format!(
        "{program} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(url: &str) -> Vec<String> {
        ["--url", url, "daemon", "run"].map(str::to_string).to_vec()
    }

    #[test]
    fn systemd_unit_runs_daemon_with_the_install_env() {
        let env = vec![
            (
                "PATH".to_string(),
                "/home/u/.local/bin:/usr/bin".to_string(),
            ),
            ("XDG_CONFIG_HOME".to_string(), "/home/u/cfg".to_string()),
        ];
        let unit = render_systemd(
            Path::new("/home/u/.local/bin/dense"),
            &args("http://127.0.0.1:9"),
            &env,
        );
        assert!(unit.contains(
            "ExecStart=\"/home/u/.local/bin/dense\" \"--url\" \"http://127.0.0.1:9\" \"daemon\" \"run\"\n"
        ));
        assert!(unit.contains("Environment=\"PATH=/home/u/.local/bin:/usr/bin\"\n"));
        assert!(unit.contains("Environment=\"XDG_CONFIG_HOME=/home/u/cfg\"\n"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn systemd_unit_escapes_specifiers_and_variables() {
        let env = vec![("PATH".to_string(), "/opt/100%/bin".to_string())];
        let unit = render_systemd(Path::new("/x/$HOME/dense"), &args("http://h/a%2Fb"), &env);
        assert!(unit.contains("\"/x/$$HOME/dense\""));
        assert!(unit.contains("\"http://h/a%%2Fb\""));
        assert!(unit.contains("Environment=\"PATH=/opt/100%%/bin\"\n"));
    }

    #[test]
    fn launchd_plist_carries_args_and_env() {
        let env = vec![("PATH".to_string(), "/usr/bin:/a&b".to_string())];
        let plist = render_launchd(
            Path::new("/tmp/daemon.log"),
            Path::new("/usr/local/bin/dense"),
            &args("https://api.condense.chat"),
            &env,
        );
        assert!(plist.contains("<string>chat.condense.dense.daemon</string>"));
        assert!(plist.contains("    <string>/usr/local/bin/dense</string>\n    <string>--url</string>\n    <string>https://api.condense.chat</string>\n"));
        assert!(plist.contains("<key>PATH</key>\n    <string>/usr/bin:/a&amp;b</string>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
    }

    #[test]
    fn vbs_launcher_doubles_quotes_and_restarts_while_present() {
        let vbs = render_vbs(
            Path::new("C:\\dense.exe"),
            &["daemon".to_string(), "run".to_string()],
        );
        assert!(vbs.contains(
            "rc = sh.Run(\"\"\"C:\\dense.exe\"\" \"\"daemon\"\" \"\"run\"\"\", 0, True)\r\n"
        ));
        assert!(
            vbs.contains("If rc = 0 Or Not fso.FileExists(WScript.ScriptFullName) Then Exit Do")
        );
        assert!(vbs.contains("WScript.Sleep 10000"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn systemd_entry_lives_next_to_the_config_dir() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        assert_eq!(
            entry_path(&cfg).unwrap(),
            root.path().join("systemd/user/dense-daemon.service")
        );
    }
}
