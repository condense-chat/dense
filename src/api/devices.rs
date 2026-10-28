//! Device management: the daemon's heartbeat (which carries the admin's
//! on/off switch back), offline usage upload, and uninstall.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Result;
use crate::api::Api;
use crate::daemon::queue::UsageRecord;
use crate::daemon::switch::Mode;
use crate::error::{Context, Error};

const DEFAULT_INTERVAL_S: u64 = 60;

#[derive(Serialize)]
pub struct Heartbeat<'a> {
    pub app_version: &'a str,
    pub device_id: &'a str,
    pub hostname: &'a str,
    pub mode: Option<Mode>,
    pub platform: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct HeartbeatReply {
    pub device_enabled: Option<bool>,
    pub enabled: bool,
    #[serde(default = "default_interval")]
    pub heartbeat_interval_s: u64,
    pub member_enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UsageReply {
    #[serde(default)]
    pub accepted: u64,
    #[serde(default)]
    pub duplicates: u64,
}

#[derive(Serialize)]
struct UsageBatch<'a> {
    device_id: &'a str,
    records: &'a [UsageRecord],
}

pub async fn heartbeat(api: &Api, body: &Heartbeat<'_>) -> Result<HeartbeatReply> {
    api.post_json("/v1/devices/heartbeat", body).await
}

pub async fn uninstall(api: &Api, device_id: &str) -> Result<()> {
    let resp = api
        .post_response("/v1/devices/uninstall", &json!({ "device_id": device_id }))
        .await?;
    if resp.status().is_success() {
        return Ok(());
    }
    Err(Error::msg(format!(
        "POST /v1/devices/uninstall returned {}",
        resp.status()
    )))
}

pub async fn upload_usage(
    api: &Api,
    device_id: &str,
    records: &[UsageRecord],
) -> Result<UsageReply> {
    api.post_json("/v1/devices/usage", &UsageBatch { device_id, records })
        .await
        .ctx(format!("uploading {} usage records", records.len()))
}

fn default_interval() -> u64 {
    DEFAULT_INTERVAL_S
}
