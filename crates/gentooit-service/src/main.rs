//! gentooit-service: a GitHub App webhook service that automates gentooit
//! workflows on GitHub events.
//!
//! The service is the "server" companion to the `gentooit` CLI (mirroring
//! packit's CLI + service split). It listens for GitHub webhooks and triggers
//! the appropriate workflow in the background:
//!
//! * On a new upstream **release**, run `propose-downstream` to open an ebuild
//!   PR against the downstream overlay.
//! * On a **pull request** to the downstream overlay, run build/QA checks and
//!   post the results as a PR comment.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use clap::Parser;
use gentooit_core::build::{build, BuildMode};
use gentooit_core::config::{ProjectConfig, UserConfig};
use gentooit_core::propose::{propose_downstream, ProposeOptions};
use gentooit_core::repo::Repo;

/// The set of webhook events this service handles.
#[derive(Clone)]
struct AppState {
    /// GitHub client authenticated as the GitHub App.
    github: gentooit_core::github::GitHub,
    /// The webhook secret (HMAC key).
    webhook_secret: String,
    /// GitHub App id (used to build UserConfig for workflow runs).
    app_id: i64,
    /// Path to the GitHub App private key PEM.
    key_path: PathBuf,
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
        app_id: args.app_id,
        key_path: args.key_path,
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

    match (event.as_str(), action.as_str()) {
        ("release", "published") => {
            let owner = parsed["repository"]["owner"]["login"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let repo = parsed["repository"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let state = state.clone();
            tokio::spawn(async move {
                let _ = run_propose_downstream(state, owner, repo).await;
            });
        }
        ("pull_request", "opened" | "synchronize") => {
            let owner = parsed["repository"]["owner"]["login"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let repo = parsed["repository"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let pr_number = parsed["pull_request"]["number"].as_u64().unwrap_or(0);
            let state = state.clone();
            tokio::spawn(async move {
                let _ = run_build(state, owner, repo, pr_number).await;
            });
        }
        _ => {
            tracing::debug!("event {event}/{action} not handled");
        }
    }

    Ok(Json(
        serde_json::json!({ "ok": true, "event": event, "action": action }),
    ))
}

/// Background runner: on a published release, fetch `.gentooit.yaml` from the
/// upstream repo and run `propose-downstream`.
async fn run_propose_downstream(
    state: AppState,
    owner: String,
    repo: String,
) -> anyhow::Result<()> {
    let yaml = match state
        .github
        .fetch_file(&owner, &repo, ".gentooit.yaml")
        .await?
    {
        Some(yaml) => yaml,
        None => {
            tracing::warn!(
                "no .gentooit.yaml found in {owner}/{repo}, skipping propose-downstream"
            );
            return Ok(());
        }
    };

    let project = ProjectConfig::from_yaml(&yaml)?;
    let user = UserConfig::for_app(state.app_id, &state.key_path);

    let workdir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to create temp workdir: {e}");
            return Ok(());
        }
    };

    let result =
        propose_downstream(&project, &user, ProposeOptions::default(), workdir.path()).await?;

    tracing::info!(
        version = %result.version,
        package = %result.package,
        pr = ?result.pull_request_url,
        "propose-downstream completed"
    );
    Ok(())
}

/// Background runner: on a downstream PR, clone the repo and run build/QA
/// checks, then post the results as a PR comment.
async fn run_build(
    state: AppState,
    owner: String,
    repo: String,
    pr_number: u64,
) -> anyhow::Result<()> {
    let workdir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to create temp workdir: {e}");
            return Ok(());
        }
    };

    let token = state
        .github
        .installation_token_for_repo(&owner, &repo)
        .await?;

    let url = format!("https://github.com/{owner}/{repo}.git");
    if let Err(e) = Repo::clone(&url, workdir.path(), token.as_deref()) {
        tracing::error!("failed to clone {owner}/{repo}: {e}");
        return Ok(());
    }

    let project = match state
        .github
        .fetch_file(&owner, &repo, ".gentooit.yaml")
        .await?
    {
        Some(yaml) => ProjectConfig::from_yaml(&yaml)?,
        None => ProjectConfig::default(),
    };

    let report = build(&project, BuildMode::Check, workdir.path(), None);

    let report = match report {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("build/QA failed: {e}");
            let body = format!("❌ gentooit build/QA errored: {e}");
            let _ = post_comment(&state, &owner, &repo, pr_number, &body).await;
            return Ok(());
        }
    };

    tracing::info!(success = %report.success, exit = %report.exit_code, "build/QA completed");

    let body = format_build_report(&report);
    let _ = post_comment(&state, &owner, &repo, pr_number, &body).await;

    Ok(())
}

/// Post a comment on a pull request.
async fn post_comment(
    state: &AppState,
    owner: &str,
    repo: &str,
    number: u64,
    body: &str,
) -> anyhow::Result<()> {
    state.github.post_comment(owner, repo, number, body).await?;
    Ok(())
}

/// Format a build report for a GitHub PR comment.
fn format_build_report(report: &gentooit_core::build::BuildReport) -> String {
    let status = if report.success {
        "✅ passed"
    } else {
        "❌ failed"
    };
    let output = if report.output.len() > 5000 {
        &report.output[..5000]
    } else {
        &report.output
    };
    format!(
        "gentooit build/QA: {status} (exit {})\n<details><summary>output</summary>\n```\n{}\n```\n</details>",
        report.exit_code,
        output
    )
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
