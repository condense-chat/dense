//! The condense HTTP API client. [`Api`] is the one place requests are built:
//! a shared connection pool, the base URL, default `x-condense-*` headers, and
//! the error-context ladder — call sites just name a path. Submodules:
//! authentication ([`auth`]), the proxy [`dialect`]s, environment profile
//! descriptors ([`profile`]), and live sessions ([`session`]).

pub mod auth;
pub mod devices;
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

    /// Client for the configured api with `creds` and the device id attached
    /// to every request.
    pub fn authed(cfg: &Config, creds: &Creds) -> Result<Self> {
        let mut headers = creds_headers(creds)?;
        if let Some(id) = crate::device::id(cfg).ok()
            && let Ok(value) = HeaderValue::from_str(&id)
        {
            headers.insert(crate::device::HEADER, value);
        }
        Self::build(&cfg.api_base_url, headers)
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

    /// GET returning the raw response — for callers that read the body of a
    /// non-2xx reply (the server's `detail`).
    pub async fn get_response(&self, path: &str) -> Result<reqwest::Response> {
        self.client
            .get(self.url(path)?)
            .send()
            .await
            .ctx(format!("GET {path}"))
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
        let home = reqwest::Url::parse(base).ctx("invalid api url")?;
        // reqwest only strips its own auth headers on a redirect, never ours, so
        // credentials follow one only within the api's origin (or its https).
        let redirects = if headers.is_empty() {
            reqwest::redirect::Policy::default()
        } else {
            reqwest::redirect::Policy::custom(move |attempt| {
                let to = attempt.url();
                let upgrade = home.scheme() == "http"
                    && to.scheme() == "https"
                    && to.host_str() == home.host_str();
                // 10 is reqwest's own limit.
                if attempt.previous().len() < 10 && (to.origin() == home.origin() || upgrade) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            })
        };
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(redirects)
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
    use std::io::{Read, Write};

    use super::*;

    /// One reply of 302 to `to`, for a client to follow or not.
    fn redirecting_to(to: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reply = format!(
            "HTTP/1.1 302 Found\r\nlocation: {to}/stolen\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.read(&mut [0; 4096]).unwrap();
            let _ = stream.write_all(reply.as_bytes());
        });
        url
    }

    /// One 302 to its own `/next`, then 200 there.
    fn redirecting_to_itself() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let replies = [
            format!(
                "HTTP/1.1 302 Found\r\nlocation: {url}/next\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            ),
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string(),
        ];
        std::thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = stream.read(&mut [0; 4096]).unwrap();
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        url
    }

    #[tokio::test]
    async fn a_client_with_credentials_follows_a_redirect_within_its_origin() {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), &redirecting_to_itself());
        let creds = Creds {
            token: Some("secret".into()),
            user_id: None,
        };
        let api = Api::authed(&cfg, &creds).unwrap();
        assert_eq!(api.status_of("/v1/me", Duration::from_secs(5)).await, 200);
    }

    #[tokio::test]
    async fn a_client_with_credentials_stops_at_a_redirect() {
        let (other, rx) = crate::daemon::managed::tests::serve(200, "{}");
        let root = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(root.path(), &redirecting_to(&other));
        let creds = Creds {
            token: Some("secret".into()),
            user_id: None,
        };
        let api = Api::authed(&cfg, &creds).unwrap();
        assert_eq!(api.status_of("/v1/me", Duration::from_secs(5)).await, 302);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[tokio::test]
    async fn an_anonymous_client_still_follows_one() {
        let (other, rx) = crate::daemon::managed::tests::serve(200, "{}");
        let api = Api::anonymous(&redirecting_to(&other)).unwrap();
        assert_eq!(api.status_of("/release", Duration::from_secs(5)).await, 200);
        assert!(
            rx.recv_timeout(Duration::from_secs(5))
                .unwrap()
                .contains("/stolen")
        );
    }

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
