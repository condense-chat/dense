//! Resolved paths and URLs for a dense invocation.
//!
//! A run targets one [`Profile`]. `prod` credentials live directly under the
//! config dir; every other profile nests under `<config>/<profile>/`. The
//! active profile is `prod` unless a `<config>/target` pointer (written by
//! `dense profile <name>`) says otherwise. Config files are TOML. The config
//! and data directories are XDG-style on every unix (macOS included):
//! `~/.config/dense` + `~/.local/share/dense`, honoring `$XDG_CONFIG_HOME` /
//! `$XDG_DATA_HOME`; on Windows `%APPDATA%\dense` + `%LOCALAPPDATA%\dense`.

use std::path::{Path, PathBuf};

use directories::BaseDirs;

use crate::Result;
use crate::error::{Context, Error};
use crate::profile::Profile;

pub const DEFAULT_API_URL: &str = "https://api.condense.chat";
pub const DEFAULT_RELEASE_URL: &str = "https://github.com/condense-chat/dense";

pub struct Config {
    /// Condense api base, e.g. `https://api.condense.chat`.
    pub api_base_url: String,
    /// Host portion of `api_base_url`.
    pub api_host: String,
    /// Whether the api gates auth — `$CONDENSE_AUTH_REQUIRED` if set, else the
    /// profile's declared value (`None` = unknown).
    pub auth_required: Option<bool>,
    /// `$CONDENSE_CODEX_WEBSOCKET` — codex Responses transport; WS unless set to `0` (→ HTTP).
    pub codex_websocket: bool,
    config_dir: PathBuf,
    data_dir: PathBuf,
    home: PathBuf,
    /// `false` when `$CONDENSE_NO_OPEN` asks us not to open URLs in a browser.
    open_links: bool,
    profile_name: String,
    /// `$DENSE_PROFILE_URL` — overrides where `dense profile` fetches from.
    profile_url: Option<String>,
    /// Release-asset base — `$CONDENSE_RELEASE_URL` if set, else the
    /// profile's declared value (`None` = the baked GitHub default).
    release_url: Option<String>,
    /// `$CONDENSE_UPSTREAM_URL` — routes the proxy to a non-default upstream.
    upstream: Option<String>,
}

impl Config {
    /// The active profile as a descriptor (for caching / persisting).
    pub fn as_profile(&self) -> Profile {
        Profile {
            name: self.profile_name.clone(),
            api_url: self.api_base_url.clone(),
            auth_required: self.auth_required,
            release_url: self.release_url.clone(),
        }
    }

    /// Directory the `dense` binary itself is installed to (and put on PATH).
    #[cfg(not(windows))]
    pub fn bin_dir(&self) -> PathBuf {
        std::env::var_os("XDG_BIN_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join(".local").join("bin"))
    }

    /// `%LOCALAPPDATA%\dense\bin` — where `install.ps1` drops `dense.exe`.
    #[cfg(windows)]
    pub fn bin_dir(&self) -> PathBuf {
        self.data_dir.join("bin")
    }

    /// Drop the target pointer, reverting to the baked `prod` default.
    pub fn clear_target(&self) -> Result<()> {
        let target = self.target_file();
        match std::fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).ctx(format!("removing {}", target.display())),
        }
    }

    /// The dense config dir — all profiles' credentials live under it.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Credential directory for the active profile: the bare config dir for
    /// `prod`, a per-profile subdir otherwise.
    pub fn cred_dir(&self) -> PathBuf {
        cred_dir_for(&self.config_dir, &self.profile_name)
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    pub fn env_file(&self) -> PathBuf {
        self.data_dir.join("env")
    }

    /// Only unix callers need the raw home dir (shell-profile paths); on
    /// Windows it would be dead code.
    #[cfg(not(windows))]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Names of the registered (cached) profiles — every config subdir that
    /// holds a `profile.toml`.
    pub fn list_cached_profiles(&self) -> Vec<String> {
        list_cached(&self.config_dir)
    }

    /// Load a registered (cached) profile by name, if present.
    pub fn load_cached_profile(&self, name: &str) -> Option<Profile> {
        read_cached_profile(&self.config_dir, name)
    }

    /// Whether dense may open URLs in a browser (off via `$CONDENSE_NO_OPEN`).
    pub fn open_links(&self) -> bool {
        self.open_links
    }

    pub fn persist_file(&self) -> PathBuf {
        self.data_dir.join("persist.toml")
    }

    /// Active profile name (`prod`, `stage`, `dev`, a dev-cell prefix, …).
    pub fn profile(&self) -> &str {
        &self.profile_name
    }

    /// `$DENSE_PROFILE_URL` — overrides where `dense profile` fetches from.
    pub fn profile_url(&self) -> Option<&str> {
        self.profile_url.as_deref()
    }

    /// Base URL release assets (binaries + manifests) are fetched from —
    /// GitHub releases unless the profile serves its own builds (dev).
    pub fn release_base(&self) -> &str {
        self.release_url.as_deref().unwrap_or(DEFAULT_RELEASE_URL)
    }

    /// Persist the active profile so future bare runs target the same api:
    /// `prod` clears the target, anything else caches + records it.
    pub fn remember_profile(&self) -> Result<()> {
        if self.profile_name == "prod" {
            return self.clear_target();
        }
        self.save_cached_profile(&self.as_profile())?;
        self.write_target(&self.profile_name)
    }

    /// Resolve the active profile in precedence order: explicit `--url` (an
    /// ad-hoc profile named after its host), then `--env <name>` (a registered
    /// profile), then the persisted `target` pointer, then the baked `prod`
    /// default. An `--env` naming an unregistered profile is an error — the
    /// binary won't guess a non-prod host.
    pub fn resolve(url_override: Option<String>, env_override: Option<String>) -> Result<Self> {
        let dirs = BaseDirs::new().ok_or_else(|| Error::msg("cannot determine home directory"))?;
        let home = dirs.home_dir().to_path_buf();
        #[cfg(windows)]
        let (config_dir, data_dir) = (
            dirs.config_dir().join("dense"),
            dirs.data_local_dir().join("dense"),
        );
        #[cfg(not(windows))]
        let (config_dir, data_dir) = (
            xdg_dir("XDG_CONFIG_HOME", &home, ".config").join("dense"),
            xdg_dir("XDG_DATA_HOME", &home, ".local/share").join("dense"),
        );
        let profile = resolve_profile(&config_dir, url_override, env_override)?;
        let api_base_url = profile.api_url.trim_end_matches('/').to_string();
        let api_host = crate::hosts::host_of(&api_base_url);
        Ok(Self {
            api_base_url,
            api_host,
            auth_required: env_auth_required().or(profile.auth_required),
            codex_websocket: env_flag_or("CONDENSE_CODEX_WEBSOCKET", true),
            config_dir,
            data_dir,
            home,
            open_links: !env_flag("CONDENSE_NO_OPEN"),
            profile_name: profile.name,
            profile_url: env_value("DENSE_PROFILE_URL"),
            release_url: env_value("CONDENSE_RELEASE_URL").or(profile.release_url),
            upstream: env_value("CONDENSE_UPSTREAM_URL"),
        })
    }

    /// Cache a fetched profile descriptor under its credential directory.
    pub fn save_cached_profile(&self, p: &Profile) -> Result<()> {
        let dir = cred_dir_for(&self.config_dir, &p.name);
        std::fs::create_dir_all(&dir).ctx("creating profile dir")?;
        let body = toml::to_string_pretty(p).ctx("serializing profile")?;
        std::fs::write(dir.join("profile.toml"), body).ctx("writing profile.toml")
    }

    /// Render `path` for a POSIX shell, using `$HOME` when it lives under the
    /// home dir (keeps generated env/shim files portable, like cargo's env).
    #[cfg(not(windows))]
    pub fn sh_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rel) => format!("$HOME/{}", rel.to_string_lossy().replace('\\', "/")),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }

    /// Override directory holding the tool shims, prepended to PATH. Kept
    /// distinct from [`bin_dir`] on Windows (where the dense binary itself
    /// lives under `…\dense\bin`).
    pub fn shim_dir(&self) -> PathBuf {
        #[cfg(windows)]
        let leaf = "shims";
        #[cfg(not(windows))]
        let leaf = "bin";
        self.data_dir.join(leaf)
    }

    pub fn target_file(&self) -> PathBuf {
        self.config_dir.join("target")
    }

    pub fn token_file(&self) -> PathBuf {
        self.cred_dir().join("token")
    }

    /// `$CONDENSE_UPSTREAM_URL` — a non-default upstream for the proxy.
    pub fn upstream(&self) -> Option<&str> {
        self.upstream.as_deref()
    }

    pub fn user_file(&self) -> PathBuf {
        self.cred_dir().join("user")
    }

    /// Windows equivalent of [`sh_path`], using `%USERPROFILE%` for cmd shims.
    #[cfg(windows)]
    pub fn win_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rel) => format!("%USERPROFILE%\\{}", rel.display()),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }

    /// Record `name` as the persistent active profile.
    pub fn write_target(&self, name: &str) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        let target = self.target_file();
        std::fs::write(&target, format!("{name}\n")).ctx(format!("writing {}", target.display()))
    }
}

fn cred_dir_for(config_dir: &Path, name: &str) -> PathBuf {
    if name == "prod" {
        config_dir.to_path_buf()
    } else {
        config_dir.join(name)
    }
}

/// `$CONDENSE_AUTH_REQUIRED` as a bool override (`1`/`true`), if set.
fn env_auth_required() -> Option<bool> {
    std::env::var("CONDENSE_AUTH_REQUIRED")
        .ok()
        .map(|v| env_truthy(&v))
}

/// Whether `key` is set to a truthy value (`1`/`true`).
fn env_flag(key: &str) -> bool {
    std::env::var(key).ok().is_some_and(|v| env_truthy(&v))
}

/// Whether `key` is truthy, or `default` when unset.
fn env_flag_or(key: &str, default: bool) -> bool {
    std::env::var(key).ok().map_or(default, |v| env_truthy(&v))
}

fn env_truthy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    v == "1" || v == "true"
}

/// A non-empty env value, if `key` is set to one.
fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Profile name for an ad-hoc URL: its full host, path-safe. Distinct hosts
/// never share a name — and so never share a credential dir.
fn host_label(url: &str) -> String {
    let host = crate::hosts::host_of(url).replace(':', "-");
    if host.is_empty() || host == "prod" {
        format!("adhoc-{host}")
    } else {
        host
    }
}

fn list_cached(config_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(config_dir) {
        for entry in entries.flatten() {
            if entry.path().join("profile.toml").is_file()
                && let Some(name) = entry.file_name().to_str()
            {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// `prod` is baked; any other name must be a registered (cached) profile.
fn load_named(config_dir: &Path, name: &str) -> Option<Profile> {
    if name == "prod" {
        return Some(Profile::prod());
    }
    read_cached_profile(config_dir, name)
}

/// Resolve an explicit `--url` to a profile. It reuses a known profile —
/// and its stored creds — only when the URL is exactly that profile's api;
/// any other URL gets a profile named by its host, so creds issued for one
/// endpoint are never sent to another that merely shares a zone label.
fn profile_for_url(config_dir: &Path, url: &str) -> Profile {
    let url = url.trim_end_matches('/').to_string();
    let prod = Profile::prod();
    if url == prod.api_url {
        return prod;
    }
    for name in list_cached(config_dir) {
        if let Some(p) = read_cached_profile(config_dir, &name)
            && p.api_url.trim_end_matches('/') == url
        {
            return p;
        }
    }
    Profile {
        name: host_label(&url),
        api_url: url,
        auth_required: None,
        release_url: None,
    }
}

fn read_cached_profile(config_dir: &Path, name: &str) -> Option<Profile> {
    let raw = std::fs::read_to_string(cred_dir_for(config_dir, name).join("profile.toml")).ok()?;
    toml::from_str(&raw).ok()
}

fn read_target(config_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(config_dir.join("target")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn resolve_profile(
    config_dir: &Path,
    url_override: Option<String>,
    env_override: Option<String>,
) -> Result<Profile> {
    if let Some(url) = url_override {
        return Ok(profile_for_url(config_dir, &url));
    }
    if let Some(env) = env_override {
        return load_named(config_dir, &env).ok_or_else(|| {
            Error::Profile(format!(
                "unknown profile `{env}` — register it with `dense profile {env} --url <zone>`, \
                 or install dense from that environment"
            ))
        });
    }
    if let Some(target) = read_target(config_dir) {
        if let Some(p) = load_named(config_dir, &target) {
            return Ok(p);
        }
        eprintln!("warning: target profile `{target}` is not registered; using prod.");
    }
    Ok(Profile::prod())
}

/// `$<var>` if set to an absolute path, else `<home>/<default>` — the XDG
/// base-dir rule, applied on every unix so macOS doesn't drift into
/// `~/Library/Application Support`.
#[cfg(not(windows))]
fn xdg_dir(var: &str, home: &Path, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cred_dir_nests_non_prod() {
        let base = PathBuf::from("/home/u/.config/dense");
        // prod is the bare config dir; every other profile nests under it.
        assert_eq!(cred_dir_for(&base, "prod"), base);
        assert_eq!(cred_dir_for(&base, "stage"), base.join("stage"));
        assert_eq!(cred_dir_for(&base, "dev-foo"), base.join("dev-foo"));
    }

    #[test]
    fn url_matching_prod_is_prod() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = profile_for_url(dir.path(), "https://api.condense.chat/");
        assert_eq!(p.name, "prod");
        assert_eq!(p.auth_required, Some(true));
    }

    #[test]
    fn url_matching_registered_profile_reuses_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("stage")).expect("mkdir");
        std::fs::write(
            dir.path().join("stage/profile.toml"),
            "name = \"stage\"\napi_url = \"https://api.stage.condense.chat\"\nauth_required = true\n",
        )
        .expect("write profile");
        let p = profile_for_url(dir.path(), "https://api.stage.condense.chat");
        assert_eq!(p.name, "stage");
    }

    // The collision case: a foreign host sharing a zone label (or even the
    // `condense` label) must NOT inherit a known profile's credential dir.
    #[test]
    fn url_of_foreign_host_is_named_by_host() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = profile_for_url(dir.path(), "https://api.stage.elsewhere.io");
        assert_eq!(p.name, "api.stage.elsewhere.io");
        let p = profile_for_url(dir.path(), "https://api.condense.evil.com");
        assert_eq!(p.name, "api.condense.evil.com");
        let p = profile_for_url(dir.path(), "http://api.dev.condense.localhost:8080");
        assert_eq!(p.name, "api.dev.condense.localhost-8080");
    }

    // Old profile.toml files predate release_url; they must keep parsing,
    // and a declared release_url must survive the cache round-trip.
    #[test]
    fn release_url_is_optional_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("stage")).expect("mkdir");
        std::fs::write(
            dir.path().join("stage/profile.toml"),
            "name = \"stage\"\napi_url = \"https://api.stage.condense.chat\"\n",
        )
        .expect("write profile");
        let p = read_cached_profile(dir.path(), "stage").expect("parse");
        assert_eq!(p.release_url, None);

        std::fs::create_dir_all(dir.path().join("dev")).expect("mkdir");
        std::fs::write(
            dir.path().join("dev/profile.toml"),
            toml::to_string_pretty(&Profile {
                name: "dev".into(),
                api_url: "http://api.dev.condense.localhost".into(),
                auth_required: None,
                release_url: Some("http://cli.dev.condense.localhost".into()),
            })
            .expect("serialize"),
        )
        .expect("write profile");
        let p = read_cached_profile(dir.path(), "dev").expect("parse");
        assert_eq!(
            p.release_url.as_deref(),
            Some("http://cli.dev.condense.localhost")
        );
    }

    #[test]
    fn resolve_profile_precedence() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("stage")).expect("mkdir");
        std::fs::write(
            dir.path().join("stage/profile.toml"),
            "name = \"stage\"\napi_url = \"https://api.stage.condense.chat\"\n",
        )
        .expect("write profile");

        // --url beats --env; --env beats target; target beats prod.
        let p = resolve_profile(
            dir.path(),
            Some("https://x.example.com".into()),
            Some("stage".into()),
        )
        .expect("resolve");
        assert_eq!(p.name, "x.example.com");

        let p = resolve_profile(dir.path(), None, Some("stage".into())).expect("resolve");
        assert_eq!(p.name, "stage");

        std::fs::write(dir.path().join("target"), "stage\n").expect("write target");
        let p = resolve_profile(dir.path(), None, None).expect("resolve");
        assert_eq!(p.name, "stage");

        std::fs::write(dir.path().join("target"), "").expect("clear target");
        let p = resolve_profile(dir.path(), None, None).expect("resolve");
        assert_eq!(p.name, "prod");

        // --env naming an unregistered profile refuses to guess.
        assert!(resolve_profile(dir.path(), None, Some("nope".into())).is_err());
    }
}
