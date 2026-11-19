//! The one HTTP request the diagnostics upload makes: `POST <endpoint>` with
//! the JSON payload, `Authorization: Bearer <token>` when the embedder set a
//! token, a 30 s timeout and no redirects (the token never follows one).
//! HTTPS only: plain HTTP and certificates that do not verify are refused
//! unless the explicit developer flag (`OPENTUNE_DIAG_INSECURE=1` on an
//! alpha / bench channel) is on.
//!
//! The client is `reqwest` with rustls, already in the app for the updater
//! (`tauri-plugin-updater`), so no new crate is pulled in.

use super::SecretString;
use reqwest::Url;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Request timeout.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// What became of one POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 2xx.
    Sent(u16),
    /// 4xx: the server refused this payload; retrying would not help.
    Rejected(u16),
    /// Network error, timeout or 5xx: worth retrying later (auto mode).
    Failed(String),
}

/// Something that delivers a payload (the HTTPS client, or a test double).
pub trait Sender: Send + Sync {
    fn post<'a>(&'a self, body: &'a [u8]) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;
}

/// `endpoint` is usable: HTTPS (or HTTP with `insecure`), a host, no
/// user name or password in it.
pub fn check_endpoint(endpoint: &Url, insecure: bool) -> Result<(), String> {
    match endpoint.scheme() {
        "https" => {}
        "http" if insecure => {}
        "http" => return Err("the diagnostics endpoint must use HTTPS".into()),
        other => return Err(format!("unsupported diagnostics endpoint scheme '{other}'")),
    }
    if endpoint.host_str().is_none_or(str::is_empty) {
        return Err("the diagnostics endpoint has no host".into());
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err("the diagnostics endpoint must not carry a user name or password".into());
    }
    Ok(())
}

/// The HTTPS client.
pub struct HttpsSender {
    client: reqwest::Client,
    endpoint: Url,
    token: Option<SecretString>,
}

impl HttpsSender {
    /// A client for `endpoint`. `insecure` (developer flag only) allows plain
    /// HTTP and certificates that do not verify (a self-signed test server).
    pub fn new(
        endpoint: Url,
        token: Option<SecretString>,
        insecure: bool,
        version: &str,
    ) -> Result<Self, String> {
        check_endpoint(&endpoint, insecure)?;
        // reqwest is built without a bundled crypto provider (as the updater
        // uses it): install rustls' ring provider once, like the updater does.
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let mut b = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("OpenTune/{version} diagnostics"))
            .https_only(!insecure);
        if insecure {
            b = b.tls_danger_accept_invalid_certs(true);
        }
        let client = b
            .build()
            .map_err(|e| format!("cannot set up the diagnostics upload: {}", e.without_url()))?;
        Ok(HttpsSender {
            client,
            endpoint,
            token,
        })
    }

    async fn post_inner(&self, body: &[u8]) -> Outcome {
        if body.len() > super::payload::MAX_PAYLOAD {
            return Outcome::Rejected(413);
        }
        let mut req = self
            .client
            .post(self.endpoint.clone())
            .header("Content-Type", "application/json")
            .body(body.to_vec());
        if let Some(t) = &self.token {
            req = req.bearer_auth(t.expose());
        }
        match req.send().await {
            Ok(resp) => {
                let code = resp.status().as_u16();
                match code {
                    200..=299 => Outcome::Sent(code),
                    400..=499 => Outcome::Rejected(code),
                    _ => Outcome::Failed(format!("server error {code}")),
                }
            }
            Err(e) => {
                let what = if e.is_timeout() {
                    "timed out".to_string()
                } else if e.is_connect() {
                    format!("cannot connect: {}", source_chain(&e))
                } else {
                    source_chain(&e)
                };
                Outcome::Failed(what)
            }
        }
    }
}

/// The error and its causes, without the request URL.
fn source_chain(e: &reqwest::Error) -> String {
    let mut s = String::new();
    let mut cur: Option<&dyn std::error::Error> = std::error::Error::source(e);
    while let Some(c) = cur {
        if !s.is_empty() {
            s.push_str(": ");
        }
        s.push_str(&c.to_string());
        cur = c.source();
    }
    if s.is_empty() {
        s = "request failed".into();
    }
    s
}

impl Sender for HttpsSender {
    fn post<'a>(&'a self, body: &'a [u8]) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
        Box::pin(self.post_inner(body))
    }
}
