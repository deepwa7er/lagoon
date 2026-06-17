//! buoy-server — a thin axum service that exposes the buoy core's `ThoughtStore`
//! as a JSON API and serves the web frontend. It holds the canonical store for
//! the web app and runs on the tailnet (see `buoy.toml`). The native clients are
//! unaffected; this is a separate, server-side store.

mod api;
mod config;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post, put};
use clap::Parser;
use tower_http::services::{ServeDir, ServeFile};

use buoy_core::{MiniLmEmbedder, ThoughtStore};

use crate::api::Shared;
use crate::config::Config;

const DEFAULT_CONFIG_PATH: &str = "/etc/buoy/config.toml";

/// How many thoughts to embed per backfill batch, and how long to idle between
/// sweeps once caught up (newly captured thoughts get embedded on the next sweep).
const EMBED_BATCH: usize = 32;
const EMBED_IDLE_SECS: u64 = 30;

/// buoy-server — serves the buoy notes web app over a server-side store.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the configuration file. Created from the baseline if absent.
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "buoy_server=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load_or_create(&cli.config)?;

    // Ensure the database's parent directory exists before opening it.
    if let Some(parent) = config.db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating data directory {}", parent.display()))?;
    }
    let store = ThoughtStore::open(&config.db_path)
        .with_context(|| format!("opening store at {}", config.db_path.display()))?;
    let shared: Shared = Arc::new(Mutex::new(store));

    // Attach the embedder (and run the backfill loop) in the background so the
    // server starts serving immediately. Keyword search works throughout;
    // semantic search and suggestions light up once the model finishes loading.
    if let Some(model_dir) = config.model_dir.clone() {
        if model_dir.is_dir() {
            tokio::spawn(embedder_task(Arc::clone(&shared), model_dir));
        } else {
            tracing::warn!(dir = %model_dir.display(), "model dir absent; keyword-only search");
        }
    }

    // The frontend does client-side routing, so a direct navigation to an app
    // path must return the SPA shell rather than 404; only real asset paths and
    // the API are served otherwise.
    let serve_dir = ServeDir::new(&config.static_dir)
        .fallback(ServeFile::new(config.static_dir.join("index.html")));

    let app = Router::new()
        .route(
            "/api/thoughts",
            get(api::list_thoughts).post(api::create_thought),
        )
        .route(
            "/api/thoughts/{id}",
            put(api::update_thought).delete(api::delete_thought),
        )
        .route("/api/thoughts/{id}/related", get(api::related_to_thought))
        .route("/api/thoughts/{id}/history", get(api::history))
        .route("/api/search", get(api::search))
        .route("/api/related", post(api::related_to_draft))
        .route("/api/sync", post(api::sync))
        .route("/healthz", get(api::healthz))
        .fallback_service(serve_dir)
        .with_state(shared);

    let addr = SocketAddr::new(config.bind, config.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "buoy-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

/// Load the embedding model off the runtime, attach it to the store, then
/// continuously backfill embeddings for any unembedded thoughts.
async fn embedder_task(store: Shared, model_dir: PathBuf) {
    let load = {
        let store = Arc::clone(&store);
        tokio::task::spawn_blocking(move || {
            let embedder = MiniLmEmbedder::load(&model_dir)?;
            store
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .set_embedder(Box::new(embedder));
            buoy_core::Result::Ok(())
        })
        .await
    };
    match load {
        Ok(Ok(())) => tracing::info!("embedder attached; semantic search enabled"),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "loading embedder failed; keyword-only search");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "embedder load task failed; keyword-only search");
            return;
        }
    }

    loop {
        let store = Arc::clone(&store);
        let embedded = tokio::task::spawn_blocking(move || {
            store
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .embed_missing(EMBED_BATCH)
        })
        .await;
        match embedded {
            Ok(Ok(0) | Err(_)) | Err(_) => {
                tokio::time::sleep(Duration::from_secs(EMBED_IDLE_SECS)).await;
            }
            Ok(Ok(_n)) => {} // more remain; loop immediately
        }
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
