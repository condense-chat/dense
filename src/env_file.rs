//! PATH wiring for the dense binary + shim dirs.
//!
//! On unix this is the cargo `~/.cargo/env` pattern: one dense-owned script
//! prepends the dirs to PATH, sourced from the shell profiles. On Windows the
//! dirs are added to the persistent User PATH via PowerShell's `[Environment]`
//! API; new terminals pick them up.

use crate::Result;
use crate::config::Config;
use crate::error::Context;

#[cfg(not(windows))]
const BEGIN: &str = "# >>> dense >>>";
#[cfg(not(windows))]
const END: &str = "# <<< dense <<<";
#[cfg(not(windows))]
const PROFILES: &[&str] = &[".profile", ".bashrc", ".zshrc", ".bash_profile"];

/// Outcome of wiring the dirs into PATH. No printing happens here — the
/// caller renders the notes in whatever UI it is running (cliclack frame
/// in setup, plain lines for `dense persist`).
pub enum PathWiring {
    /// Something was unwritable; the notes say what to finish by hand.
    Manual(Vec<String>),
    /// `--no-modify-path`: PATH was left untouched by request.
    Skipped,
    /// Wired with no failures.
    Wired,
}

/// How to make the dirs visible when no shell profile sources the env file
/// (wiring skipped or failed) — restarting a shell would not help there.
pub fn activate_hint(cfg: &Config) -> String {
    #[cfg(windows)]
    {
        let _ = cfg;
        "add the dense directories to your PATH, then open a new terminal".to_string()
    }
    #[cfg(not(windows))]
    {
        format!("run `. \"{}\"`", cfg.sh_path(&cfg.env_file()))
    }
}

#[cfg(not(windows))]
pub fn ensure_env(cfg: &Config, modify_path: bool) -> Result<PathWiring> {
    std::fs::create_dir_all(cfg.shim_dir()).ctx("creating dense shim dir")?;
    write_env_file(cfg)?;
    if !modify_path {
        return Ok(PathWiring::Skipped);
    }
    let notes = ensure_sourced(cfg)?;
    if notes.is_empty() {
        Ok(PathWiring::Wired)
    } else {
        Ok(PathWiring::Manual(notes))
    }
}

#[cfg(windows)]
pub fn ensure_env(cfg: &Config, modify_path: bool) -> Result<PathWiring> {
    std::fs::create_dir_all(cfg.bin_dir()).ctx("creating dense bin dir")?;
    std::fs::create_dir_all(cfg.shim_dir()).ctx("creating dense shim dir")?;
    if !modify_path {
        return Ok(PathWiring::Skipped);
    }
    let dirs = [cfg.bin_dir(), cfg.shim_dir()];
    match set_user_path(&dirs, true) {
        Ok(()) => Ok(PathWiring::Wired),
        Err(e) => Ok(PathWiring::Manual(vec![
            format!("could not update your PATH ({e})"),
            format!(
                "add these directories to your PATH yourself:\n  {}\n  {}",
                dirs[0].display(),
                dirs[1].display()
            ),
        ])),
    }
}

/// Whether any shell profile currently sources the dense env file (the
/// signal that the user accepted PATH wiring, vs `--no-modify-path`).
#[cfg(not(windows))]
pub fn is_wired(cfg: &Config) -> bool {
    PROFILES
        .iter()
        .any(|n| std::fs::read_to_string(cfg.home().join(n)).is_ok_and(|s| s.contains(BEGIN)))
        || fish_conf_path(cfg).is_some_and(|p| p.is_file())
}

/// Whether dense's bin dir is on PATH, which only its own wiring puts there.
#[cfg(windows)]
pub fn is_wired(cfg: &Config) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == cfg.bin_dir()))
}

/// One-line description of the PATH change `ensure_env` would make, for the
/// setup prompt.
pub fn path_change_summary(cfg: &Config) -> String {
    #[cfg(windows)]
    {
        format!(
            "adds {} and {} to your User PATH",
            cfg.bin_dir().display(),
            cfg.shim_dir().display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "adds {} and {} via {}, sourced from your shell profile",
            cfg.sh_path(&cfg.bin_dir()),
            cfg.sh_path(&cfg.shim_dir()),
            cfg.sh_path(&cfg.env_file())
        )
    }
}

/// A one-line, platform-appropriate hint for making a PATH change take effect.
pub fn reload_hint(cfg: &Config) -> String {
    #[cfg(windows)]
    {
        let _ = cfg;
        "open a new terminal to pick it up".to_string()
    }
    #[cfg(not(windows))]
    {
        format!(
            "restart your shell or run `. \"{}\"`",
            cfg.sh_path(&cfg.env_file())
        )
    }
}

/// Remove the source block from every profile (used by `self uninstall`).
#[cfg(not(windows))]
pub fn unwire(cfg: &Config) -> Result<()> {
    let mut failed = Vec::new();
    for name in PROFILES {
        let path = cfg.home().join(name);
        if path.exists()
            && let Err(e) = strip_block(&path)
        {
            let why =
                std::error::Error::source(&e).map_or_else(|| e.to_string(), ToString::to_string);
            failed.push(format!("{}: {why}", path.display()));
        }
    }
    if let Some(fish) = fish_conf_path(cfg) {
        match std::fs::remove_file(&fish) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => failed.push(format!("{}: {e}", fish.display())),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(crate::error::Error::msg(format!(
            "could not remove dense from {}",
            failed.join("; ")
        )))
    }
}

#[cfg(windows)]
pub fn unwire(cfg: &Config) -> Result<()> {
    let _ = set_user_path(&[cfg.bin_dir(), cfg.shim_dir()], false);
    Ok(())
}

#[cfg(not(windows))]
fn add_block(path: &std::path::Path, block: &str) -> std::io::Result<()> {
    use std::io::Write;

    let existing = read_profile(path)?;
    if has_block(&existing) {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with(b"\n") {
        ""
    } else {
        "\n"
    };
    // Only ever appended to: a shell profile can hold any bytes, and it isn't ours.
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?
        .write_all(format!("{separator}{block}").as_bytes())
}

/// Returns the manual-action notes for anything that couldn't be wired —
/// empty means the env file is sourced from every targeted profile.
#[cfg(not(windows))]
fn ensure_sourced(cfg: &Config) -> Result<Vec<String>> {
    let line = format!(". \"{}\"", cfg.sh_path(&cfg.env_file()));
    let block = format!("{BEGIN}\n{line}\n{END}\n");

    let mut targets: Vec<_> = PROFILES
        .iter()
        .map(|n| cfg.home().join(n))
        .filter(|p| p.exists())
        .collect();
    if targets.is_empty() {
        targets.push(cfg.home().join(".profile"));
    }

    // A read-only profile is the user's to fix — note it and move on rather
    // than aborting persist; the shims are already installed.
    let mut notes = Vec::new();
    let mut unwritable = false;
    for path in targets {
        if let Err(e) = add_block(&path, &block) {
            unwritable = true;
            notes.push(format!("{} is not writable ({e})", path.display()));
        }
    }
    if let Err(e) = write_fish_conf(cfg) {
        notes.push(format!("could not write the fish PATH config ({e})"));
    }
    if unwritable {
        notes.push(format!(
            "add this line to your shell profile yourself:\n  {line}"
        ));
    }
    Ok(notes)
}

/// `conf.d/dense.fish` under an existing fish config — `None` when the user
/// doesn't use fish (we never create the fish dir for them).
#[cfg(not(windows))]
fn fish_conf_path(cfg: &Config) -> Option<std::path::PathBuf> {
    let fish = cfg.home().join(".config").join("fish");
    fish.is_dir()
        .then(|| fish.join("conf.d").join("dense.fish"))
}

#[cfg(not(windows))]
fn has_block(profile: &[u8]) -> bool {
    profile.windows(BEGIN.len()).any(|w| w == BEGIN.as_bytes())
}

#[cfg(not(windows))]
fn read_profile(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        read => read,
    }
}

#[cfg(not(windows))]
fn render(shim: &str, bin: &str) -> String {
    let mut s = String::from("#!/bin/sh\n# dense shell environment — sourced from your profile.\n");
    for dir in [shim, bin] {
        s.push_str(&format!(
            "case \":${{PATH}}:\" in\n  *\":{dir}:\"*) ;;\n  *) export PATH=\"{dir}:$PATH\" ;;\nesac\n"
        ));
    }
    s
}

/// Add (`add = true`) or remove the directories from the persistent User PATH
/// via PowerShell's `[Environment]` API, idempotently. New shells see it.
#[cfg(windows)]
fn set_user_path(dirs: &[std::path::PathBuf], add: bool) -> Result<()> {
    use crate::error::Error;

    let list = dirs
        .iter()
        .map(|d| format!("'{}'", d.display().to_string().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let op = if add {
        "foreach ($d in $dirs) { if ($parts -notcontains $d) { $parts += $d } }"
    } else {
        "$parts = $parts | Where-Object { $dirs -notcontains $_ }"
    };
    let script = format!(
        "$dirs = @({list});\
         $p = [Environment]::GetEnvironmentVariable('Path','User');\
         $parts = @(if ($p) {{ $p -split ';' | Where-Object {{ $_ -ne '' }} }} else {{ @() }});\
         {op};\
         [Environment]::SetEnvironmentVariable('Path', ($parts -join ';'), 'User')"
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .ctx("running powershell to update PATH")?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::msg(format!(
            "powershell exited unsuccessfully ({status})"
        )))
    }
}

#[cfg(not(windows))]
fn strip_block(path: &std::path::Path) -> Result<()> {
    let existing = read_profile(path).ctx(format!("reading {}", path.display()))?;
    if !has_block(&existing) {
        return Ok(());
    }
    let mut out = Vec::with_capacity(existing.len());
    let mut skipping = false;
    for line in existing.split_inclusive(|b| *b == b'\n') {
        if line.trim_ascii() == BEGIN.as_bytes() {
            skipping = true;
        } else if line.trim_ascii() == END.as_bytes() {
            skipping = false;
        } else if !skipping {
            out.extend_from_slice(line);
        }
    }
    swap_profile(path, &out).ctx(format!("updating {}", path.display()))
}

/// Replace a profile in one step, on the file a dotfile manager's symlink
/// points at; in place when its folder can't take a temp file.
#[cfg(not(windows))]
fn swap_profile(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let real = std::fs::canonicalize(path)?;
    let tmp = crate::atomic::temp_beside(&real);
    let swapped = std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::metadata(&real))
        .and_then(|m| std::fs::set_permissions(&tmp, m.permissions()))
        .and_then(|()| std::fs::rename(&tmp, &real));
    if swapped.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return std::fs::write(&real, bytes);
    }
    swapped
}

#[cfg(not(windows))]
fn write_env_file(cfg: &Config) -> Result<()> {
    let env = cfg.env_file();
    let body = render(&cfg.sh_path(&cfg.shim_dir()), &cfg.sh_path(&cfg.bin_dir()));
    std::fs::write(&env, body).ctx(format!("writing {}", env.display()))
}

/// fish doesn't source POSIX env files; a conf.d drop-in is its native,
/// auto-loaded equivalent. Written only when a fish config already exists.
#[cfg(not(windows))]
fn write_fish_conf(cfg: &Config) -> Result<()> {
    let Some(path) = fish_conf_path(cfg) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    let body = format!(
        "# dense shell environment.\nfor dir in \"{}\" \"{}\"\n    if not contains -- $dir $PATH\n        set -gx PATH $dir $PATH\n    end\nend\n",
        cfg.sh_path(&cfg.shim_dir()),
        cfg.sh_path(&cfg.bin_dir())
    );
    std::fs::write(&path, body).ctx(format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    #[cfg(not(windows))]
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn add_then_strip_block_restores_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join(".bashrc");
        let original = "export FOO=1\nalias ll='ls -l'\n";
        std::fs::write(&profile, original).expect("seed profile");

        let block = format!("{BEGIN}\n. \"$HOME/.local/share/dense/env\"\n{END}\n");
        add_block(&profile, &block).expect("add");
        let with_block = std::fs::read_to_string(&profile).expect("read");
        assert!(with_block.contains(BEGIN) && with_block.contains(END));

        // Adding again is a no-op.
        add_block(&profile, &block).expect("re-add");
        assert_eq!(std::fs::read_to_string(&profile).expect("read"), with_block);

        strip_block(&profile).expect("strip");
        assert_eq!(std::fs::read_to_string(&profile).expect("read"), original);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_profile_that_is_not_utf8_keeps_every_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join(".zshrc");
        let original = b"export NAME=Jos\xE9\r\nalias ll='ls -l'";
        std::fs::write(&profile, original).expect("seed profile");

        let block = format!("{BEGIN}\nline\n{END}\n");
        add_block(&profile, &block).expect("add");
        let wired = std::fs::read(&profile).expect("read");
        assert!(wired.starts_with(original));
        assert!(has_block(&wired));

        strip_block(&profile).expect("strip");
        let mut expected = original.to_vec();
        expected.push(b'\n');
        assert_eq!(std::fs::read(&profile).expect("read"), expected);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_profile_that_cannot_be_read_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join(".zshrc");
        std::fs::write(&profile, "export FOO=1\n").expect("seed profile");
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(&profile, mode(0o200)).expect("write-only");
        if std::fs::read(&profile).is_ok() {
            return; // root reads anything
        }

        assert!(add_block(&profile, "block\n").is_err());
        std::fs::set_permissions(&profile, mode(0o600)).expect("restore");
        assert_eq!(
            std::fs::read_to_string(&profile).expect("read"),
            "export FOO=1\n"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn stripping_keeps_a_dotfile_symlink_and_its_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("dotfiles-zshrc");
        std::fs::write(&real, format!("a\n{BEGIN}\nline\n{END}\nb\n")).expect("seed");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).expect("mode");
        let link = dir.path().join(".zshrc");
        symlink(&real, &link).expect("link");

        strip_block(&link).expect("strip");
        assert!(link.is_symlink());
        assert_eq!(std::fs::read_to_string(&real).expect("read"), "a\nb\n");
        let mode = std::fs::metadata(&real).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[cfg(not(windows))]
    #[test]
    fn add_block_terminates_unterminated_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join(".profile");
        std::fs::write(&profile, "export FOO=1").expect("seed profile");

        let block = format!("{BEGIN}\nline\n{END}\n");
        add_block(&profile, &block).expect("add");
        let out = std::fs::read_to_string(&profile).expect("read");
        assert!(out.starts_with("export FOO=1\n"));
        assert!(out.contains(BEGIN));
    }
}
