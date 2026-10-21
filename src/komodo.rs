//! Minimal Komodo Core API client.
//!
//! Komodo's API is RPC-style: every call is `POST /{read|write|execute}` with
//! a body of `{"type": "<RequestName>", "params": {...}}`, authenticated with
//! the `X-Api-Key` / `X-Api-Secret` headers. Talking to it as untyped JSON
//! (instead of depending on the `komodo_client` crate) keeps this server
//! working across Komodo releases: new request types and fields pass straight
//! through without a rebuild.

use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Read,
    Write,
    Execute,
}

impl Api {
    pub fn path(self) -> &'static str {
        match self {
            Api::Read => "/read",
            Api::Write => "/write",
            Api::Execute => "/execute",
        }
    }
}

#[derive(Clone)]
pub struct Komodo {
    http: reqwest::Client,
    address: String,
    key: String,
    secret: String,
}

impl Komodo {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("komodo-mcp/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(cfg.request_timeout);
        if let Some(pem) = &cfg.ca_cert_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .context("KOMODO_CA_CERT is not a valid PEM certificate bundle")?;
            builder = builder.tls_certs_merge(certs);
        }
        Ok(Komodo {
            http: builder.build().context("failed to build HTTP client")?,
            address: cfg.address.clone(),
            key: cfg.api_key.clone(),
            secret: cfg.api_secret.clone(),
        })
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Make one API call. `request` is the Komodo request type, e.g. `ListStacks`.
    pub async fn call(&self, api: Api, request: &str, params: Value) -> anyhow::Result<Value> {
        validate_request_name(request)?;
        let params = if params.is_null() { json!({}) } else { params };
        let url = format!("{}{}", self.address, api.path());
        let res = self
            .http
            .post(&url)
            .header("x-api-key", &self.key)
            .header("x-api-secret", &self.secret)
            .json(&json!({ "type": request, "params": params }))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("could not reach Komodo at {}: {}", self.address, error_chain(&e)))?;

        let status = res.status();
        let body = res.text().await.context("failed to read Komodo response body")?;

        if status.is_success() {
            if body.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&body)
                .with_context(|| format!("Komodo returned non-JSON for {request}: {}", preview(&body)));
        }

        let mut msg = format!("{request} failed: HTTP {status}");
        match serde_json::from_str::<Value>(&body) {
            Ok(err) => {
                if let Some(e) = err.get("error").and_then(Value::as_str) {
                    msg.push_str(&format!(" — {e}"));
                }
                if let Some(trace) = err.get("trace").and_then(Value::as_array) {
                    for t in trace.iter().filter_map(Value::as_str) {
                        msg.push_str(&format!("\n  caused by: {t}"));
                    }
                }
            }
            Err(_) if !body.trim().is_empty() => msg.push_str(&format!(" — {}", preview(&body))),
            Err(_) => {}
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            msg.push_str("\n(hint: check KOMODO_API_KEY / KOMODO_API_SECRET)");
        }
        anyhow::bail!(msg)
    }

    pub async fn read(&self, request: &str, params: Value) -> anyhow::Result<Value> {
        self.call(Api::Read, request, params).await
    }
}

/// Request names are plain PascalCase identifiers. Rejecting anything else
/// keeps model-supplied strings from turning into something odd.
fn validate_request_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        anyhow::bail!("'{name}' is not a valid Komodo request name (expected PascalCase like ListStacks)")
    }
}

fn preview(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 300 {
        format!("{}…", s.chars().take(300).collect::<String>())
    } else {
        s.to_string()
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(&format!(": {s}"));
        src = s.source();
    }
    out
}

/// Pull a resource or update id out of a Komodo response. Full documents use
/// `{"_id": {"$oid": "..."}}`; list items use a plain `"id"`.
pub fn extract_id(v: &Value) -> Option<String> {
    if let Some(id) = v.get("_id") {
        if let Some(oid) = id.get("$oid").and_then(Value::as_str) {
            return Some(oid.to_string());
        }
        if let Some(s) = id.as_str() {
            return Some(s.to_string());
        }
    }
    v.get("id").and_then(Value::as_str).map(str::to_string)
}
