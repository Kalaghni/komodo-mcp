//! komodo-mcp: an MCP server for the Komodo deployment platform.
//!
//! Runs on your machine as a stdio MCP server (Claude Desktop / Claude Code /
//! any MCP client spawns it) and talks to Komodo Core over its HTTPS API.

mod config;
mod format;
mod komodo;
mod server;

use anyhow::Context;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

const HELP: &str = "\
komodo-mcp — MCP server for Komodo (https://komo.do)

USAGE:
  komodo-mcp            Run as a stdio MCP server (what your MCP client does)
  komodo-mcp --check    Test the connection and list the tools that would be enabled
  komodo-mcp --version

ENVIRONMENT:
  KOMODO_ADDRESS                 Komodo Core URL, e.g. https://komodo.example.com   (required)
  KOMODO_API_KEY                 API key                                            (required)
  KOMODO_API_SECRET              API secret                                         (required)
  KOMODO_MCP_MODE                read (default) | execute | write
  KOMODO_MCP_ALLOW_DESTRUCTIVE   true to allow destroy / delete / remove / prune    (default off)
  KOMODO_CA_CERT                 Path to a PEM CA bundle for a private CA
  KOMODO_MCP_TIMEOUT_SECS        Per-request timeout                                (default 30)
  KOMODO_MCP_WAIT_TIMEOUT_SECS   Max wait for an execute to finish                  (default 600)
  KOMODO_MCP_MAX_OUTPUT_CHARS    Truncate tool output past this size                (default 60000)
  RUST_LOG                       Log level for stderr, e.g. komodo_mcp=debug

  Any of KOMODO_ADDRESS / KOMODO_API_KEY / KOMODO_API_SECRET can instead be given
  as a file path in the same name with _FILE appended.
";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("komodo-mcp: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("-h" | "--help") => {
            print!("{HELP}");
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("komodo-mcp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--check") | None => {}
        Some(other) => anyhow::bail!("unknown argument '{other}'. Try --help."),
    }

    // stdout carries the MCP protocol, so all logging goes to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("komodo_mcp=info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install TLS crypto provider"))?;

    let cfg = config::Config::from_env().context("configuration error (run with --help)")?;
    tracing::info!(?cfg, "starting");
    let komodo = komodo::Komodo::new(&cfg)?;
    let mode = cfg.mode;
    let server = server::KomodoMcp::new(komodo.clone(), cfg);

    if arg.as_deref() == Some("--check") {
        let v = komodo.read("GetVersion", serde_json::json!({})).await?;
        println!(
            "Connected to {} — Komodo Core {}",
            komodo.address(),
            v.get("version").and_then(|v| v.as_str()).unwrap_or("?")
        );
        // GetVersion doesn't need auth; this one does.
        let servers = komodo.read("ListServers", serde_json::json!({})).await?;
        println!(
            "API key works: can see {} server(s).",
            servers.as_array().map(Vec::len).unwrap_or(0)
        );
        println!("Mode: {mode}. Tools enabled: {}", server.tool_names().join(", "));
        return Ok(());
    }

    let service = server.serve(stdio()).await.inspect_err(|e| tracing::error!("failed to start: {e}"))?;
    service.waiting().await?;
    Ok(())
}
