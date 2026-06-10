//! A live condense session: a heartbeat posted while a tool runs, and an end
//! signal when it exits, so usage and presence track the real process.

use std::time::Duration;

use serde_json::json;

use crate::api::Api;

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
                api.post_forget("/v1/sessions/heartbeat", &heartbeat_body(&id))
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

fn git_branch() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn heartbeat_body(session_id: &str) -> serde_json::Value {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    json!({
        "session_id": session_id,
        "cwd": cwd,
        "branch": git_branch(),
        "pid": std::process::id(),
        "host": gethostname::gethostname().to_string_lossy().into_owned(),
    })
}
