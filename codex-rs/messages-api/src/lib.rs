//! Anthropic Messages API served by Codex's own Responses client.
//!
//! Every request reaches the backend as a Codex turn would send it: same identity, headers,
//! turn metadata, WebSocket continuation, retries and remote compaction.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use codex_core::build_models_manager;
use codex_core::config::ConfigBuilder;
use codex_core::resolve_installation_id;
use codex_login::AuthManager;
use codex_login::default_client::USER_AGENT_SUFFIX;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_login::default_client::set_default_originator;
use codex_login::enforce_login_restrictions;
use codex_login::is_workload_identity_selected;
use codex_models_manager::manager::RefreshStrategy;
use codex_utils_cli::CliConfigOverrides;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod advisor;
mod agent_summary;
mod align;
mod anthropic;
mod backend;
mod cite;
mod client_tools;
mod client_toolsets;
mod codec;
mod compaction;
mod compactions;
mod context_edits;
mod error;
mod history;
mod images;
mod lifecycle;
mod mcp_connector;
mod persist;
mod request;
mod respond;
mod server;
mod server_tools;
mod settings;
mod stop;
mod stream;
mod tools;
mod watchdog;
mod web_run;

#[cfg(test)]
mod test_support;

/// The interactive client's identity; requests present themselves as its turns.
const ORIGINATOR: &str = "codex-tui";
const DEFAULT_ANALYTICS_ENABLED: bool = true;

#[derive(Debug, clap::Parser)]
pub struct Args {
    /// Loopback address to serve on.
    #[arg(long, default_value = "127.0.0.1:41817")]
    pub listen: SocketAddr,

    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,
}

pub async fn run_main(args: Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.listen.ip().is_loopback(),
        "--listen must be a loopback address"
    );
    set_default_originator(ORIGINATOR.to_string())
        .map_err(|err| anyhow::anyhow!("originator: {err:?}"))?;
    if let Ok(mut suffix) = USER_AGENT_SUFFIX.lock() {
        *suffix = Some(format!("{ORIGINATOR}; {}", env!("CARGO_PKG_VERSION")));
    }
    let overrides = args
        .config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    let config = Arc::new(
        ConfigBuilder::default()
            .cli_overrides(overrides)
            .build()
            .await?,
    );
    set_default_client_residency_requirement(config.enforce_residency.value());
    if !is_workload_identity_selected() {
        enforce_login_restrictions(&config.auth_config()).await?;
    }
    let otel = codex_core::otel_init::build_provider(
        &config,
        env!("CARGO_PKG_VERSION"),
        /*service_name_override*/ None,
        DEFAULT_ANALYTICS_ENABLED,
    )
    .map_err(|err| anyhow::anyhow!("otel exporter: {err}"))?;
    codex_core::otel_init::record_process_start(otel.as_ref(), ORIGINATOR);
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")));
    let _ = tracing_subscriber::registry()
        .with(stderr)
        .with(otel.as_ref().and_then(|otel| otel.tracing_layer()))
        .with(otel.as_ref().and_then(|otel| otel.logger_layer()))
        .try_init();
    let auth_manager =
        AuthManager::shared_from_config(config.as_ref(), /*enable_codex_api_key_env*/ false)
            .await?;
    let models = build_models_manager(&config, Arc::clone(&auth_manager));
    models
        .list_models(
            RefreshStrategy::OnlineIfUncached,
            config.http_client_factory(),
        )
        .await;
    let installation_id = resolve_installation_id(&config.codex_home).await?;
    let backend = Arc::new(backend::Backend::new(
        Arc::clone(&config),
        auth_manager,
        models,
        installation_id,
        ORIGINATOR.to_string(),
    ));
    backend.spawn_maintenance();
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    eprintln!("codex messages-api: serving http://{}", args.listen);
    axum::serve(listener, server::router(backend, args.listen)).await?;
    Ok(())
}
