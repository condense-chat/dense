//! Per-user autostart for `dense daemon run`: a systemd user unit (Linux), a
//! launchd agent (macOS), a hidden Startup-folder launcher (Windows). The
//! entry pins `--url` so the daemon serves the profile it was installed from.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::Config;
use crate::error::{Context, Error};

#[cfg(target_os = "macos")]
const LABEL: &str = "chat.condense.dense.daemon";
#[cfg(all(unix, not(target_os = "macos")))]
const UNIT: &str = "dense-daemon.service";

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

pub fn install(cfg: &Config) -> Result<Report> {
    let path = entry_path(cfg)?;
    let (exe, args) = command(cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    std::fs::create_dir_all(cfg.daemon_dir()).ctx("creating dense daemon dir")?;
    std::fs::write(&path, render(cfg, &exe, &args)).ctx(format!("writing {}", path.display()))?;
    let notes = activate(&path);
    Ok(Report { notes, path })
}

/// The autostart entry, if installed.
pub fn installed(cfg: &Config) -> Option<PathBuf> {
    entry_path(cfg).ok().filter(|p| p.exists())
}

/// Stop and remove the entry; `None` when there was nothing installed.
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
fn activate(_path: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    for args in [
        &["--user", "daemon-reload"][..],
        &["--user", "enable", "--now", UNIT][..],
    ] {
        if let Err(e) = run("systemctl", args) {
            notes.push(format!("systemctl {}: {e}", args.join(" ")));
        }
    }
    notes
}

#[cfg(target_os = "macos")]
fn activate(path: &Path) -> Vec<String> {
    let p = path.to_string_lossy();
    let _ = run("launchctl", &["unload", &p]);
    run("launchctl", &["load", "-w", &p])
        .err()
        .map(|e| vec![format!("launchctl load: {e}")])
        .unwrap_or_default()
}

#[cfg(windows)]
fn activate(path: &Path) -> Vec<String> {
    match std::process::Command::new("wscript.exe").arg(path).spawn() {
        Ok(_) => Vec::new(),
        Err(e) => vec![format!(
            "could not start the daemon now ({e}); it starts at next login"
        )],
    }
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

#[cfg(windows)]
fn deactivate(_path: &Path, pid: Option<u32>) -> Vec<String> {
    let Some(pid) = pid else { return Vec::new() };
    let image = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "dense.exe".into());
    // The image filter keeps a stale, reused pid from killing something else.
    run(
        "taskkill",
        &[
            "/F",
            "/FI",
            &format!("PID eq {pid}"),
            "/FI",
            &format!("IMAGENAME eq {image}"),
        ],
    )
    .err()
    .map(|e| vec![format!("could not stop daemon pid {pid}: {e}")])
    .unwrap_or_default()
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

#[cfg(all(unix, not(target_os = "macos")))]
fn render(_cfg: &Config, exe: &Path, args: &[String]) -> String {
    let quoted: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!(
        "[Unit]\nDescription=dense device daemon\nAfter=network-online.target\n\n\
         [Service]\nExecStart={}\nRestart=always\nRestartSec=10\n\n\
         [Install]\nWantedBy=default.target\n",
        quoted.join(" ")
    )
}

#[cfg(target_os = "macos")]
fn render(cfg: &Config, exe: &Path, args: &[String]) -> String {
    let log = cfg.daemon_dir().join("daemon.log");
    let program: String = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| format!("    <string>{}</string>\n", xml_escape(&a)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key>\n  <string>{LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n{program}  </array>\n\
         \x20 <key>RunAtLoad</key>\n  <true/>\n\
         \x20 <key>KeepAlive</key>\n  <true/>\n\
         \x20 <key>StandardErrorPath</key>\n  <string>{}</string>\n\
         </dict>\n</plist>\n",
        xml_escape(&log.to_string_lossy())
    )
}

/// A VBScript launcher: `Run …, 0` starts the console binary with no window.
#[cfg(windows)]
fn render(_cfg: &Config, exe: &Path, args: &[String]) -> String {
    let line: Vec<String> = std::iter::once(exe.to_string_lossy().into_owned())
        .chain(args.iter().cloned())
        .map(|a| format!("\"{a}\""))
        .collect();
    format!(
        "CreateObject(\"WScript.Shell\").Run \"{}\", 0, False\r\n",
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
    Err(Error::msg(
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn systemd_unit_runs_daemon_for_the_profile() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        let unit = render(
            &cfg,
            Path::new("/home/u/.local/bin/dense"),
            &command(&cfg).unwrap().1,
        );
        assert!(unit.contains(
            "ExecStart=\"/home/u/.local/bin/dense\" \"--url\" \"http://127.0.0.1:9\" \"daemon\" \"run\"\n"
        ));
        assert!(unit.contains("WantedBy=default.target"));
        assert_eq!(
            entry_path(&cfg).unwrap(),
            root.path().join("systemd/user/dense-daemon.service")
        );
    }

    #[cfg(windows)]
    #[test]
    fn vbs_launcher_doubles_quotes() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), "http://127.0.0.1:9");
        let vbs = render(
            &cfg,
            Path::new("C:\\dense.exe"),
            &["daemon".to_string(), "run".to_string()],
        );
        assert_eq!(
            vbs,
            "CreateObject(\"WScript.Shell\").Run \"\"\"C:\\dense.exe\"\" \"\"daemon\"\" \"\"run\"\"\", 0, False\r\n"
        );
    }
}
