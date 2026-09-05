//! gentooit-service: a GitHub App webhook service that automates gentooit
//! workflows on GitHub events.
//!
//! The service is the "server" companion to the `gentooit` CLI (mirroring
//! packit's CLI + service split). It listens for GitHub webhooks and triggers
//! the appropriate workflow:
//!
//! * On a new upstream **release**, run `propose-downstream` to open an ebuild
//!   PR against the downstream overlay.
//! * On a **pull request** to the downstream overlay, run build/QA checks and
//!   report status.
//!
//! This is a foundation: event verification (HMAC), GitHub App authentication,
//! and wiring the workflows are designed in, with the actual long-running work
//! intended to run out-of-band (e.g. a queue) in production.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use clap::Parser;

/// The set of webhook events this service handles.
#[derive(Clone)]
struct AppState {
    /// GitHub client authenticated as the GitHub App, used once the relevant
    /// webhook events require calling back into GitHub.
    github: gentooit_core::github::GitHub,
    /// The webhook secret (HMAC key).
    webhook_secret: String,
}

#[derive(Parser)]
#[command(
    name = "gentooit-service",
    version,
    about = "gentooit GitHub App webhook service"
)]
struct Args {
    /// Port to bind.
    #[arg(long, default_value_t = 3000)]
    port: u16,

    /// GitHub App id.
    #[arg(long, env = "GENTOOIT_APP_ID")]
    app_id: i64,

    /// Path to the GitHub App private key PEM.
    #[arg(long, env = "GENTOOIT_APP_KEY")]
    key_path: PathBuf,

    /// Webhook secret used to verify signatures.
    #[arg(long, env = "GENTOOIT_WEBHOOK_SECRET")]
    webhook_secret: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();

    // Validate the app identity configuration early so a misconfigured service
    // fails fast rather than at the first webhook.
    if args.app_id <= 0 {
        anyhow::bail!("GENTOOIT_APP_ID must be a positive GitHub App id");
    }
    if !args.key_path.is_file() {
        anyhow::bail!(
            "GitHub App private key not found at {}",
            args.key_path.display()
        );
    }
    if args.webhook_secret.is_empty() {
        anyhow::bail!("GENTOOIT_WEBHOOK_SECRET must not be empty");
    }

    let github = gentooit_core::github::GitHub::with_app(args.app_id, &args.key_path)?;

    let state = AppState {
        github,
        webhook_secret: args.webhook_secret,
    };

    let app = Router::new()
        .route("/", post(handle_webhook))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Verify the X-Hub-Signature-256 HMAC-SHA256 header against the body.
fn verify_signature(state: &AppState, signature: &str, body: &[u8]) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    let Some(hex_sig) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let mut mac = match HmacSha256::new_from_slice(state.webhook_secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    // Constant-time compare.
    let expected_bytes = expected.as_bytes();
    let given_bytes = hex_sig.as_bytes();
    if expected_bytes.len() != given_bytes.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected_bytes.iter().zip(given_bytes.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

async fn handle_webhook(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let event = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !verify_signature(&state, signature, &body) {
        tracing::warn!("invalid webhook signature for event {event}");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let parsed: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        tracing::error!("invalid JSON webhook payload");
        StatusCode::BAD_REQUEST
    })?;

    let action = parsed
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();

    tracing::info!(%event, %action, "received verified webhook");

    // Dispatch to the appropriate workflow. In production the heavy lifting
    // would be queued/run in the background; here we log and acknowledge so the
    // webhook returns promptly. The app-authenticated client is available on
    // `state.github` for the handlers.
    match (event.as_str(), action.as_str()) {
        ("release", "published") => {
            tracing::info!("release published -> would run propose-downstream");
            let _ = &state.github;
        }
        ("pull_request", "opened" | "synchronize") => {
            tracing::info!("pull request -> would run build/QA");
            let _ = &state.github;
        }
        _ => {
            tracing::debug!("event {event}/{action} not handled");
        }
    }

    Ok(Json(
        serde_json::json!({ "ok": true, "event": event, "action": action }),
    ))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
