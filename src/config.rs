//! Configuration, read from environment variables.
//!
//! Every secret can also be supplied as a file by appending `_FILE` to the
//! variable name (e.g. `KOMODO_API_SECRET_FILE=/run/secrets/komodo`), so the
//! secret doesn't have to live in the MCP client's JSON config.

use std::{fmt, str::FromStr, time::Duration};

use anyhow::{Context, bail};

/// How much the server is allowed to do. Each tier includes the ones below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    /// Only `/read` requests. Nothing in Komodo can change.
    Read,
    /// Also `/execute`: deploy, restart, run builds and procedures, etc.
    Execute,
    /// Also `/write`: create, update and delete resource configuration.
    Write,
}

impl FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "read" | "readonly" | "read-only" | "ro" => Ok(Mode::Read),
            "execute" | "exec" => Ok(Mode::Execute),
            "write" | "rw" | "full" => Ok(Mode::Write),
            other => bail!("unknown KOMODO_MCP_MODE '{other}' (expected read, execute or write)"),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Read => "read",
            Mode::Execute => "execute",
            Mode::Write => "write",
        })
    }
}

#[derive(Clone)]
pub struct Config {
    pub address: String,
    pub api_key: String,
    pub api_secret: String,
    pub mode: Mode,
    pub allow_destructive: bool,
    pub request_timeout: Duration,
    pub wait_timeout: Duration,
    pub max_output_chars: usize,
    pub ca_cert_pem: Option<Vec<u8>>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("address", &self.address)
            .field("api_key", &mask(&self.api_key))
            .field("api_secret", &"***")
            .field("mode", &self.mode)
            .field("allow_destructive", &self.allow_destructive)
            .field("request_timeout", &self.request_timeout)
            .field("wait_timeout", &self.wait_timeout)
            .field("max_output_chars", &self.max_output_chars)
            .field("ca_cert", &self.ca_cert_pem.is_some())
            .finish()
    }
}

fn mask(s: &str) -> String {
    let visible: String = s.chars().take(4).collect();
    format!("{visible}…")
}

/// Read `NAME`, or the contents of the file at `NAME_FILE`.
fn var(name: &str) -> anyhow::Result<Option<String>> {
    if let Ok(v) = std::env::var(name)
        && !v.trim().is_empty() {
            return Ok(Some(v.trim().to_string()));
        }
    let file_var = format!("{name}_FILE");
    if let Ok(path) = std::env::var(&file_var)
        && !path.trim().is_empty() {
            let contents = std::fs::read_to_string(path.trim())
                .with_context(|| format!("failed to read {file_var} at '{}'", path.trim()))?;
            return Ok(Some(contents.trim().to_string()));
        }
    Ok(None)
}

fn required(name: &str) -> anyhow::Result<String> {
    var(name)?.with_context(|| format!("{name} (or {name}_FILE) must be set"))
}

fn flag(name: &str) -> anyhow::Result<bool> {
    Ok(match var(name)? {
        None => false,
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
    })
}

fn number<T: FromStr>(name: &str, default: T) -> anyhow::Result<T> {
    match var(name)? {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| anyhow::anyhow!("{name} must be a number, got '{v}'")),
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let address = required("KOMODO_ADDRESS")?.trim_end_matches('/').to_string();
        if !(address.starts_with("https://") || address.starts_with("http://")) {
            bail!("KOMODO_ADDRESS must start with https:// or http:// (got '{address}')");
        }
        let ca_cert_pem = match std::env::var("KOMODO_CA_CERT") {
            Ok(path) if !path.trim().is_empty() => Some(
                std::fs::read(path.trim())
                    .with_context(|| format!("failed to read KOMODO_CA_CERT at '{}'", path.trim()))?,
            ),
            _ => None,
        };
        Ok(Config {
            address,
            api_key: required("KOMODO_API_KEY")?,
            api_secret: required("KOMODO_API_SECRET")?,
            mode: var("KOMODO_MCP_MODE")?.unwrap_or_default().parse()?,
            allow_destructive: flag("KOMODO_MCP_ALLOW_DESTRUCTIVE")?,
            request_timeout: Duration::from_secs(number("KOMODO_MCP_TIMEOUT_SECS", 30u64)?),
            wait_timeout: Duration::from_secs(number("KOMODO_MCP_WAIT_TIMEOUT_SECS", 600u64)?),
            max_output_chars: number("KOMODO_MCP_MAX_OUTPUT_CHARS", 60_000usize)?,
            ca_cert_pem,
        })
    }
}
