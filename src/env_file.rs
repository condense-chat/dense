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
    for name in PROFILES {
        let path = cfg.home().join(name);
        if path.exists() {
            strip_block(&path)?;
        }
    }
    if let Some(fish) = fish_conf_path(cfg) {
        match std::fs::remove_file(&fish) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).ctx(format!("removing {}", fish.display())),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn unwire(cfg: &Config) -> Result<()> {
    let _ = set_user_path(&[cfg.bin_dir(), cfg.shim_dir()], false);
    Ok(())
}

#[cfg(not(windows))]
fn add_block(path: &std::path::Path, block: &str) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(BEGIN) {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(block);
    std::fs::write(path, next)
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
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if !existing.contains(BEGIN) {
        return Ok(());
    }
    let mut out = String::new();
    let mut skipping = false;
    for line in existing.lines() {
        if line.trim() == BEGIN {
            skipping = true;
            continue;
        }
        if line.trim() == END {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    std::fs::write(path, out).ctx(format!("updating {}", path.display()))
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
