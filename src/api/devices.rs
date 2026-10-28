//! Device management: the daemon's heartbeat (which carries the admin's
//! on/off switch back), offline usage upload, and uninstall.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Result;
use crate::api::Api;
use crate::daemon::queue::UsageRecord;
use crate::daemon::switch::Mode;
use crate::error::{Context, Error};

const DEFAULT_INTERVAL_S: u64 = 60;

/// A device endpoint's answer: the parsed body, or a status the server will
/// keep returning (4xx other than 408/429), so retrying it is pointless.
/// Network errors, 5xx, 408 and 429 are `Err` — worth retrying.
#[derive(Debug)]
pub enum Reply<T> {
    Ok(T),
    Rejected(u16),
}

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
    /// The org has device management; when false the switch is not ours to apply.
    #[serde(default)]
    pub managed: bool,
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

pub async fn heartbeat(api: &Api, body: &Heartbeat<'_>) -> Result<Reply<HeartbeatReply>> {
    post(api, "/v1/devices/heartbeat", body).await
}

pub async fn uninstall(api: &Api, device_id: &str) -> Result<()> {
    let path = "/v1/devices/uninstall";
    match post::<serde_json::Value>(api, path, &json!({ "device_id": device_id })).await? {
        Reply::Ok(_) => Ok(()),
        Reply::Rejected(status) => Err(Error::msg(format!("POST {path} returned {status}"))),
    }
}

pub async fn upload_usage(
    api: &Api,
    device_id: &str,
    records: &[UsageRecord],
) -> Result<Reply<UsageReply>> {
    post(api, "/v1/devices/usage", &UsageBatch { device_id, records })
        .await
        .ctx(format!("uploading {} usage records", records.len()))
}

fn is_permanent(status: reqwest::StatusCode) -> bool {
    status.is_client_error()
        && status != reqwest::StatusCode::REQUEST_TIMEOUT
        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
}

async fn post<T: DeserializeOwned>(
    api: &Api,
    path: &str,
    body: &impl Serialize,
) -> Result<Reply<T>> {
    let resp = api.post_response(path, body).await?;
    let status = resp.status();
    if is_permanent(status) {
        return Ok(Reply::Rejected(status.as_u16()));
    }
    if !status.is_success() {
        return Err(Error::msg(format!("POST {path} returned {status}")));
    }
    resp.json()
        .await
        .map(Reply::Ok)
        .ctx(format!("{path} returned malformed JSON"))
}

fn default_interval() -> u64 {
    DEFAULT_INTERVAL_S
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_non_retryable_client_errors_are_permanent() {
        let permanent = |c: u16| is_permanent(reqwest::StatusCode::from_u16(c).unwrap());
        assert!(permanent(400) && permanent(401) && permanent(403) && permanent(404));
        assert!(permanent(413) && permanent(422));
        assert!(!permanent(408) && !permanent(429) && !permanent(500) && !permanent(503));
        assert!(!permanent(200));
    }
}
