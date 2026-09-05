//! gentooit CLI: packit-like automation for Gentoo Linux.
//!
//! Subcommands:
//! * `propose-downstream` — take an upstream release and open a PR updating
//!   (or creating) the Gentoo ebuild in a downstream overlay.
//! * `build` — build/test an ebuild (QA checks or full emerge build).
//! * `sync-from-downstream` — copy downstream ebuild files back into the
//!   upstream project via a PR.
//! * `init` — scaffold a `.gentooit.yaml` project config.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use gentooit_core::build::{self, BuildMode};
use gentooit_core::config::{config_dir, ProjectConfig, UserConfig};
use gentooit_core::propose;
use gentooit_core::sync;

#[derive(Parser)]
#[command(
    name = "gentooit",
    version,
    about = "packit-like automation for Gentoo Linux: upstream project <-> ebuild repository"
)]
struct Cli {
    /// Path to the project config file (`.gentooit.yaml`).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Working directory for clones/downloads (defaults to a temp dir).
    #[arg(long, global = true)]
    workdir: Option<PathBuf>,

    /// Be verbose (repeat for more).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a `.gentooit.yaml` in the current directory.
    Init {
        /// The upstream repository as owner/name.
        #[arg(long)]
        upstream: String,
        /// The downstream overlay git URL.
        #[arg(long)]
        downstream: Option<String>,
    },

    /// Take an upstream release and open a PR updating/creating the ebuild.
    ProposeDownstream {
        /// Pin a specific upstream version/tag (default: latest release).
        #[arg(long)]
        version: Option<String>,
        /// Create the ebuild even if one already exists.
        #[arg(long)]
        force: bool,
        /// Skip running QA checks.
        #[arg(long)]
        no_qa: bool,
    },

    /// Build and/or run QA checks on the ebuilds.
    Build {
        /// Only run QA checks (pkgcheck), don't do a full build.
        #[arg(long)]
        check: bool,
    },

    /// Copy downstream ebuild files back into the upstream repo via a PR.
    SyncFromDownstream {
        /// Apply changes locally instead of opening a PR.
        #[arg(long)]
        local: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    run(cli).await
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let user = UserConfig::load_default()?;

    match cli.command {
        Command::Init {
            upstream,
            downstream,
        } => init_project(&upstream, downstream.as_deref()),
        Command::ProposeDownstream {
            version,
            force,
            no_qa,
        } => {
            let project = load_project(cli.config.as_deref())?;
            if let Some(v) = &version {
                if let Some(u) = &project.upstream {
                    let mut u = u.clone();
                    u.version = Some(v.clone());
                    let mut p = project.clone();
                    p.upstream = Some(u);
                    run_propose(&p, &user, cli.workdir.as_deref(), force, no_qa).await
                } else {
                    run_propose(&project, &user, cli.workdir.as_deref(), force, no_qa).await
                }
            } else {
                run_propose(&project, &user, cli.workdir.as_deref(), force, no_qa).await
            }
        }
        Command::Build { check } => {
            let project = load_project(cli.config.as_deref())?;
            let mode = if check {
                BuildMode::Check
            } else {
                BuildMode::Build
            };
            let workdir = resolve_workdir(cli.workdir.as_deref());
            match build::build(&project, mode, &workdir) {
                Ok(report) => {
                    print_report(&report);
                    if report.success {
                        Ok(())
                    } else {
                        anyhow::bail!("build/QA failed with exit code {}", report.exit_code)
                    }
                }
                Err(e) => Err(e.into()),
            }
        }
        Command::SyncFromDownstream { local } => {
            let project = load_project(cli.config.as_deref())?;
            if local {
                let workdir = resolve_workdir(cli.workdir.as_deref());
                let files = sync::sync_local(&project, &workdir, &workdir)?;
                println!(
                    "would sync {} files (local mode, no changes applied)",
                    files.len()
                );
                Ok(())
            } else {
                let workdir = resolve_workdir(cli.workdir.as_deref());
                sync::sync_from_downstream(&project, &user, &workdir)
                    .await
                    .map(|r| {
                        println!("Synced files:");
                        for f in &r.files {
                            println!("  - {f}");
                        }
                        if let Some(url) = &r.pull_request_url {
                            println!("PR: {url}");
                        }
                    })
            }
        }
    }
}

async fn run_propose(
    project: &ProjectConfig,
    user: &UserConfig,
    workdir: Option<&Path>,
    force: bool,
    no_qa: bool,
) -> anyhow::Result<()> {
    let opts = propose::ProposeOptions { force, no_qa };
    let workdir = resolve_workdir(workdir);
    let result = propose::propose_downstream(project, user, opts, &workdir).await?;
    println!(
        "Proposed version {} ({}) to {}/{}",
        result.version, result.package, result.category, result.package
    );
    println!("  ebuild: {}", result.files.ebuild_path);
    println!("  metadata: {}", result.files.metadata_path);
    println!("  manifest: {}", result.files.manifest_path);
    if let Some(url) = &result.pull_request_url {
        println!("  PR: {url}");
    }
    Ok(())
}

fn init_project(upstream: &str, downstream: Option<&str>) -> anyhow::Result<()> {
    let config = ProjectConfig {
        upstream: Some(gentooit_core::config::UpstreamConfig {
            vcs: Some("github".to_string()),
            upstream: Some(upstream.to_string()),
            ..Default::default()
        }),
        downstream: vec![gentooit_core::config::DownstreamConfig {
            url: downstream.unwrap_or("").to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let path = PathBuf::from(".gentooit.yaml");
    if path.exists() {
        anyhow::bail!(".gentooit.yaml already exists");
    }
    let yaml = serde_yaml::to_string(&config)?;
    std::fs::write(&path, yaml)?;
    if downstream.is_none() {
        println!("Created .gentooit.yaml. Fill in the `downstream` url.");
    } else {
        println!("Created .gentooit.yaml");
    }
    println!(
        "Also consider setting credentials in {}",
        config_dir().join("config.yaml").display()
    );
    Ok(())
}

fn load_project(explicit: Option<&Path>) -> anyhow::Result<ProjectConfig> {
    match explicit {
        Some(p) => ProjectConfig::load(p),
        None => {
            let cwd = std::env::current_dir()?;
            match ProjectConfig::discover(&cwd)? {
                Some((path, cfg)) => {
                    tracing::info!(path = %path.display(), "using project config");
                    Ok(cfg)
                }
                None => {
                    anyhow::bail!(
                        "no .gentooit.yaml found from {}; run `gentooit init` or pass --config",
                        cwd.display()
                    )
                }
            }
        }
    }
}

fn resolve_workdir(explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(p) => p.to_path_buf(),
        None => std::env::temp_dir().join(format!("gentooit-{}", std::process::id())),
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::EnvFilter;
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn print_report(report: &build::BuildReport) {
    if report.success {
        println!("OK");
    } else {
        println!("FAILED (exit {})", report.exit_code);
    }
    if !report.output.trim().is_empty() {
        println!("{}", report.output.trim());
    }
}
