//! The condense HTTP API client. [`Api`] is the one place requests are built:
//! a shared connection pool, the base URL, default `x-condense-*` headers, and
//! the error-context ladder — call sites just name a path. Submodules:
//! authentication ([`auth`]), the proxy [`dialect`]s, environment profile
//! descriptors ([`profile`]), and live sessions ([`session`]).

pub mod auth;
pub mod dialect;
pub mod profile;
pub mod session;

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Result;
use crate::api::auth::Creds;
use crate::config::Config;
use crate::error::{Context, Error};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// A client bound to one condense host. Cheap to clone (the inner pool is
/// shared); paths append to the base, absolute URLs pass through. The base
/// may carry a path of its own (a GitHub `releases` root), so paths are
/// concatenated, never `Url::join`ed — join would resolve a leading `/`
/// against the host and drop the base path.
#[derive(Clone)]
pub struct Api {
    base: String,
    client: reqwest::Client,
}

impl Api {
    /// Client for `base` with no condense identity attached.
    pub fn anonymous(base: &str) -> Result<Self> {
        Self::build(base, HeaderMap::new())
    }

    /// Client for the configured api with `creds` attached to every request.
    pub fn authed(cfg: &Config, creds: &Creds) -> Result<Self> {
        Self::build(&cfg.api_base_url, creds_headers(creds)?)
    }

    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(self.url(path)?)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .ctx(format!("GET {path}"))?
            .error_for_status()
            .ctx(format!("GET {path}"))?;
        Ok(resp.bytes().await.ctx(format!("reading {path}"))?.to_vec())
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.client
            .get(self.url(path)?)
            .send()
            .await
            .ctx(format!("GET {path}"))?
            .error_for_status()
            .ctx(format!("GET {path}"))?
            .json()
            .await
            .ctx(format!("{path} returned malformed JSON"))
    }

    /// POST and ignore the outcome entirely — for fire-and-forget signals
    /// (heartbeats) that must never disturb the caller.
    pub async fn post_forget(&self, path: &str, body: &impl Serialize) {
        if let Ok(url) = self.url(path) {
            let _ = self
                .client
                .post(url)
                .timeout(Duration::from_secs(5))
                .json(body)
                .send()
                .await;
        }
    }

    pub async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T> {
        self.client
            .post(self.url(path)?)
            .json(body)
            .send()
            .await
            .ctx(format!("POST {path}"))?
            .error_for_status()
            .ctx(format!("POST {path}"))?
            .json()
            .await
            .ctx(format!("{path} returned malformed JSON"))
    }

    /// POST returning the raw response — for protocol loops that branch on
    /// the status code themselves (device polling).
    pub async fn post_response(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response> {
        self.client
            .post(self.url(path)?)
            .json(body)
            .send()
            .await
            .ctx(format!("POST {path}"))
    }

    pub async fn post_text(&self, path: &str) -> Result<String> {
        self.client
            .post(self.url(path)?)
            .send()
            .await
            .ctx(format!("POST {path}"))?
            .error_for_status()
            .ctx(format!("POST {path}"))?
            .text()
            .await
            .ctx(format!("{path} returned no body"))
    }

    /// GET `path` and report only the HTTP status (0 on any error).
    pub async fn status_of(&self, path: &str, timeout: Duration) -> u16 {
        let Ok(url) = self.url(path) else { return 0 };
        self.client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0)
    }

    fn build(base: &str, headers: HeaderMap) -> Result<Self> {
        let base = base.trim_end_matches('/');
        reqwest::Url::parse(base).ctx("invalid api url")?;
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(DEFAULT_TIMEOUT)
            .user_agent(concat!("dense/", env!("CARGO_PKG_VERSION")))
            .build()
            .ctx("building http client")?;
        Ok(Self {
            base: base.to_string(),
            client,
        })
    }

    fn url(&self, path: &str) -> Result<reqwest::Url> {
        if path.contains("://") {
            return reqwest::Url::parse(path).ctx(format!("invalid url {path}"));
        }
        let path = path.strip_prefix('/').unwrap_or(path);
        reqwest::Url::parse(&format!("{}/{path}", self.base)).ctx(format!("invalid path {path}"))
    }
}

fn creds_headers(creds: &Creds) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(token) = &creds.token {
        headers.insert(
            "x-condense-auth-token",
            HeaderValue::from_str(token).map_err(|_| Error::Auth("malformed token".into()))?,
        );
    }
    if let Some(user) = &creds.user_id {
        headers.insert(
            "x-condense-user-id",
            HeaderValue::from_str(user).map_err(|_| Error::Auth("malformed user id".into()))?,
        );
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_keeps_base_path() {
        let api = Api::anonymous("https://github.com/condense-chat/dense").expect("client");
        assert_eq!(
            api.url("/releases/latest/download/manifest-linux-x86_64.json")
                .expect("url")
                .as_str(),
            "https://github.com/condense-chat/dense/releases/latest/download/manifest-linux-x86_64.json"
        );
    }

    #[test]
    fn url_passes_absolute_through_and_trims_base_slash() {
        let api = Api::anonymous("https://api.condense.chat/").expect("client");
        assert_eq!(
            api.url("/v1/me").expect("url").as_str(),
            "https://api.condense.chat/v1/me"
        );
        assert_eq!(
            api.url("https://elsewhere.io/asset").expect("url").as_str(),
            "https://elsewhere.io/asset"
        );
    }
}
