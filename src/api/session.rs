//! A live condense session: a heartbeat posted while a tool runs, and an end
//! signal when it exits, so usage and presence track the real process.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;

use crate::api::Api;

/// Bounds every git call so a wedged repo never holds up a heartbeat.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

const HEARTBEAT_SECS: u64 = 30;

pub struct Session {
    pub id: String,
}

impl Session {
    pub async fn end(&self, api: &Api) {
        api.post_forget("/v1/sessions/end", &json!({ "session_id": self.id }))
            .await;
    }

    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Spawn a background task heartbeating every [`HEARTBEAT_SECS`] until the
    /// returned handle is aborted. The body is rebuilt per beat so cwd/branch
    /// track a long session.
    pub fn start_heartbeat(&self, api: &Api) -> tokio::task::JoinHandle<()> {
        let api = api.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            loop {
                api.post_forget("/v1/sessions/heartbeat", &heartbeat_body(&id).await)
                    .await;
                tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECS)).await;
            }
        })
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

async fn body_in(cwd: &Path, session_id: &str) -> serde_json::Value {
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let head_sha = git(cwd, &["rev-parse", "HEAD"]).await;
    let remote = git_remote(cwd, branch.as_deref().filter(|b| *b != "HEAD")).await;
    let mut body = json!({
        "session_id": session_id,
        "cwd": cwd.display().to_string(),
        "branch": branch.unwrap_or_default(),
        "pid": std::process::id(),
        "host": gethostname::gethostname().to_string_lossy().into_owned(),
        "dense_version": env!("CARGO_PKG_VERSION"),
    });
    if let Some(remote) = remote {
        body["git_remote"] = remote.into();
    }
    if let Some(sha) = head_sha {
        body["head_sha"] = sha.into();
    }
    body
}

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// The branch's upstream remote, else `origin`, else the first remote.
async fn git_remote(cwd: &Path, branch: Option<&str>) -> Option<String> {
    let upstream = match branch {
        Some(b) => git(cwd, &["config", "--get", &format!("branch.{b}.remote")]).await,
        None => None,
    };
    let first = || async {
        git(cwd, &["remote"])
            .await
            .and_then(|r| r.lines().next().map(str::to_string))
    };
    for name in [upstream, Some("origin".into()), first().await]
        .into_iter()
        .flatten()
    {
        if let Some(url) = git(cwd, &["remote", "get-url", &name]).await {
            return Some(strip_userinfo(&url));
        }
    }
    None
}

async fn heartbeat_body(session_id: &str) -> serde_json::Value {
    body_in(&std::env::current_dir().unwrap_or_default(), session_id).await
}

/// Drop `user:token@` from an http(s) remote; scp-style `git@host:` stays.
fn strip_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let host_end = rest.find('/').unwrap_or(rest.len());
    match rest[..host_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        sh(p, &["init", "-q", "-b", "main"]);
        sh(
            p,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        dir
    }

    #[test]
    fn userinfo_is_stripped_from_http_remotes_only() {
        assert_eq!(
            strip_userinfo("https://u:tok@git.example/o/r.git"),
            "https://git.example/o/r.git"
        );
        assert_eq!(
            strip_userinfo("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
        assert_eq!(strip_userinfo("ssh://git@host/o/r@x"), "ssh://host/o/r@x");
    }

    #[tokio::test]
    async fn body_carries_repo_sha_and_version() {
        let dir = repo();
        let p = dir.path();
        sh(p, &["remote", "add", "zeta", "https://u:t@z.example/z.git"]);
        let body = body_in(p, "s").await;
        assert_eq!(body["git_remote"], "https://z.example/z.git");
        assert_eq!(body["head_sha"].as_str().unwrap().len(), 40);
        assert_eq!(body["branch"], "main");
        assert_eq!(body["dense_version"], env!("CARGO_PKG_VERSION"));

        sh(p, &["remote", "add", "origin", "https://o.example/o.git"]);
        assert_eq!(
            body_in(p, "s").await["git_remote"],
            "https://o.example/o.git"
        );

        sh(p, &["config", "branch.main.remote", "zeta"]);
        assert_eq!(
            body_in(p, "s").await["git_remote"],
            "https://z.example/z.git"
        );
    }

    #[tokio::test]
    async fn outside_a_repo_the_git_fields_are_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let body = body_in(dir.path(), "s").await;
        assert!(body.get("git_remote").is_none() && body.get("head_sha").is_none());
        assert_eq!(body["session_id"], "s");
        assert_eq!(body["branch"], "");
    }
}
