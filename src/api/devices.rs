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
/// The device id is bound to another user: regenerate it.
pub const DEVICE_OWNED_ELSEWHERE: &str = "device_owned_elsewhere";
/// The server has no row for this device yet; a heartbeat creates it.
pub const DEVICE_UNKNOWN: &str = "device_unknown";
/// The user is at their live-device cap.
pub const DEVICE_LIMIT: &str = "device_limit";

/// A device endpoint's answer: the parsed body, or a status the server will
/// keep returning (4xx other than 408/429), so retrying it is pointless.
/// Network errors, 5xx, 408 and 429 are `Err` — worth retrying.
#[derive(Debug)]
pub enum Reply<T> {
    Ok(T),
    Rejected(Rejection),
}

/// A permanent refusal: the status plus the server's `detail.code`, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: Option<String>,
    pub status: u16,
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

impl Rejection {
    #[cfg(test)]
    pub fn new(status: u16, code: Option<&str>) -> Self {
        Self {
            code: code.map(str::to_string),
            status,
        }
    }

    pub fn is(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }

    fn parse(status: u16, body: &str) -> Self {
        let code = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["detail"]["code"].as_str().map(str::to_string));
        Self { code, status }
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{} {code}", self.status),
            None => write!(f, "{}", self.status),
        }
    }
}

pub async fn heartbeat(api: &Api, body: &Heartbeat<'_>) -> Result<Reply<HeartbeatReply>> {
    post(api, "/v1/devices/heartbeat", body).await
}

/// `Ok(false)` when the server never knew the device (nothing to mark).
pub async fn uninstall(api: &Api, device_id: &str) -> Result<bool> {
    let path = "/v1/devices/uninstall";
    match post::<serde_json::Value>(api, path, &json!({ "device_id": device_id })).await? {
        Reply::Ok(_) => Ok(true),
        Reply::Rejected(r) if r.is(DEVICE_UNKNOWN) => Ok(false),
        Reply::Rejected(r) => Err(Error::msg(format!("POST {path} returned {r}"))),
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
        let body = resp.text().await.unwrap_or_default();
        return Ok(Reply::Rejected(Rejection::parse(status.as_u16(), &body)));
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

    #[test]
    fn rejection_reads_the_detail_code() {
        let r = Rejection::parse(403, r#"{"detail":{"code":"device_owned_elsewhere"}}"#);
        assert!(r.is(DEVICE_OWNED_ELSEWHERE));
        assert_eq!(r.to_string(), "403 device_owned_elsewhere");
        let plain = Rejection::parse(403, r#"{"detail":"forbidden"}"#);
        assert_eq!(plain.code, None);
        assert_eq!(Rejection::parse(422, "not json").to_string(), "422");
    }
}
