//! A live condense session: a heartbeat posted while a tool runs, and an end
//! signal when it exits, so usage and presence track the real process.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;

use crate::api::Api;

/// Bounds every git call so a wedged repo never holds up a heartbeat.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

const HEARTBEAT_SECS: u64 = 30;

/// The remote for the last (cwd, branch) seen; looked up again only when either changes.
#[derive(Default)]
struct RemoteCache {
    key: Option<(PathBuf, String)>,
    remote: Option<String>,
}

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
    /// track a long session. `repo` adds the git remote and HEAD.
    pub fn start_heartbeat(&self, api: &Api, repo: bool) -> tokio::task::JoinHandle<()> {
        let api = api.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            let mut remotes = RemoteCache::default();
            loop {
                let cwd = std::env::current_dir().unwrap_or_default();
                let body = body_in(&cwd, &id, &mut remotes, repo).await;
                api.post_forget("/v1/sessions/heartbeat", &body).await;
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

async fn body_in(
    cwd: &Path,
    session_id: &str,
    remotes: &mut RemoteCache,
    repo: bool,
) -> serde_json::Value {
    let head = git(cwd, &["rev-parse", "HEAD", "--abbrev-ref", "HEAD"]).await;
    let (branch, head_sha) = match head.as_deref().and_then(|h| h.split_once('\n')) {
        Some((sha, branch)) => (branch.to_string(), Some(sha.to_string())),
        None => (String::new(), None),
    };
    let head_sha = head_sha.filter(|_| repo);
    let key = (cwd.to_path_buf(), branch.clone());
    if remotes.key.as_ref() != Some(&key) {
        let tracked = Some(branch.as_str()).filter(|b| !b.is_empty() && *b != "HEAD");
        remotes.remote = match head_sha {
            Some(_) => git_remote(cwd, tracked).await,
            None => None,
        };
        remotes.key = Some(key);
    }
    let mut body = json!({
        "session_id": session_id,
        "cwd": cwd.display().to_string(),
        "branch": branch,
        "pid": std::process::id(),
        "host": gethostname::gethostname().to_string_lossy().into_owned(),
        "dense_version": env!("CARGO_PKG_VERSION"),
    });
    if let Some(remote) = &remotes.remote {
        body["git_remote"] = remote.as_str().into();
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
    #[cfg(test)]
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// The branch's upstream remote, else `origin`, else the first remote.
async fn git_remote(cwd: &Path, branch: Option<&str>) -> Option<String> {
    if let Some(b) = branch {
        let key = format!("branch.{b}.remote");
        if let Some(name) = git(cwd, &["config", "--get", &key]).await
            && let Some(url) = remote_url(cwd, &name).await
        {
            return Some(url);
        }
    }
    if let Some(url) = remote_url(cwd, "origin").await {
        return Some(url);
    }
    let names = git(cwd, &["remote"]).await?;
    remote_url(cwd, names.lines().next()?).await
}

async fn remote_url(cwd: &Path, name: &str) -> Option<String> {
    public_remote(&git(cwd, &["remote", "get-url", name]).await?)
}

/// A network remote with credentials, query and fragment removed. Local paths
/// and `file://` remotes are dropped: they name directories, not repos.
fn public_remote(url: &str) -> Option<String> {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let host_end = rest.find('/').unwrap_or(rest.len());
        let host = rest[..host_end].rsplit('@').next().unwrap_or_default();
        return (!host.is_empty()).then(|| format!("{scheme}://{host}{}", &rest[host_end..]));
    }
    let (host, _) = url.split_once(':')?;
    let scp = !host.is_empty() && !host.contains(['/', '\\']) && host.len() > 1;
    scp.then(|| url.to_string())
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
    fn only_network_remotes_go_out_and_without_secrets() {
        let p = |u| public_remote(u);
        assert_eq!(
            p("https://u:tok@git.example/o/r.git").as_deref(),
            Some("https://git.example/o/r.git")
        );
        assert_eq!(
            p("https://git.example/o/r.git?private_token=x#frag").as_deref(),
            Some("https://git.example/o/r.git")
        );
        assert_eq!(
            p("git@github.com:o/r.git").as_deref(),
            Some("git@github.com:o/r.git")
        );
        assert_eq!(
            p("ssh://git@host/o/r@x").as_deref(),
            Some("ssh://host/o/r@x")
        );
        assert_eq!(p("/home/u/clients/acme/repo.git"), None);
        assert_eq!(p("../sibling"), None);
        assert_eq!(p("file:///home/u/repo.git"), None);
        assert_eq!(p("C:\\src\\repo"), None);
        assert_eq!(p("./a:b"), None);
    }

    #[tokio::test]
    async fn only_a_managed_session_names_its_repo() {
        let dir = repo();
        let p = dir.path();
        sh(p, &["remote", "add", "origin", "https://o.example/o.git"]);
        let body = body_in(p, "s", &mut RemoteCache::default(), false).await;
        assert_eq!(body["branch"], "main");
        assert!(body.get("git_remote").is_none() && body.get("head_sha").is_none());
    }

    #[cfg(feature = "session-repo")]
    #[tokio::test]
    async fn body_carries_repo_sha_and_version() {
        let dir = repo();
        let p = dir.path();
        sh(p, &["remote", "add", "zeta", "https://u:t@z.example/z.git"]);
        let body = body_in(p, "s", &mut RemoteCache::default(), true).await;
        assert_eq!(body["git_remote"], "https://z.example/z.git");
        assert_eq!(body["head_sha"].as_str().unwrap().len(), 40);
        assert_eq!(body["branch"], "main");
        assert_eq!(body["dense_version"], env!("CARGO_PKG_VERSION"));

        sh(p, &["remote", "add", "origin", "https://o.example/o.git"]);
        assert_eq!(
            body_in(p, "s", &mut RemoteCache::default(), true).await["git_remote"],
            "https://o.example/o.git"
        );

        sh(p, &["config", "branch.main.remote", "zeta"]);
        assert_eq!(
            body_in(p, "s", &mut RemoteCache::default(), true).await["git_remote"],
            "https://z.example/z.git"
        );
    }

    #[cfg(feature = "session-repo")]
    #[tokio::test]
    async fn remote_is_looked_up_again_only_when_cwd_or_branch_changes() {
        let dir = repo();
        let p = dir.path();
        sh(p, &["remote", "add", "origin", "https://o.example/o.git"]);
        let mut cache = RemoteCache::default();
        body_in(p, "s", &mut cache, true).await;
        sh(
            p,
            &["remote", "set-url", "origin", "https://n.example/n.git"],
        );
        assert_eq!(
            body_in(p, "s", &mut cache, true).await["git_remote"],
            "https://o.example/o.git"
        );
        sh(p, &["checkout", "-q", "-b", "other"]);
        assert_eq!(
            body_in(p, "s", &mut cache, true).await["git_remote"],
            "https://n.example/n.git"
        );
    }

    #[tokio::test]
    async fn outside_a_repo_the_git_fields_are_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let body = body_in(dir.path(), "s", &mut RemoteCache::default(), true).await;
        assert!(body.get("git_remote").is_none() && body.get("head_sha").is_none());
        assert_eq!(body["session_id"], "s");
        assert_eq!(body["branch"], "");
    }
}
