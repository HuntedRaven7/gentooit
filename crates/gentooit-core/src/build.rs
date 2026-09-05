//! `build` workflow: build and test an ebuild, either in the local/overlay
//! check-out or in CI.
//!
//! This mirrors packit's `build`/testing capability. Gentoo ebuilds are built
//! with `emerge` (Portage). gentooit can:
//!
//! * Run `pkgcheck scan` and `pkgdev` QA checks on a package directory.
//! * Build a package via `emerge` in the local system when Portage is
//!   available.
//! * Emit a GitHub Actions workflow definition (the CI story) that builds the
//!   ebuild in a container.
//!
//! The local build path shells out to `emerge`/`pkgdev`/`pkgcheck`; gentooit is
//! intentionally a thin orchestrator around the standard Gentoo tooling rather
//! than re-implementing Portage.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::ProjectConfig;

/// An error in the build/QA workflow.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(
        "Portage tool `{tool}` not found on PATH. Install it (e.g. `emerge app-portage/pkgcheck`)."
    )]
    ToolNotFound { tool: String },
    #[error("command `{cmd}` failed with exit status {status}:\n{output}")]
    CommandFailed {
        cmd: String,
        status: i32,
        output: String,
    },
}

/// How to build/test the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BuildMode {
    /// Run QA checks only (pkgcheck scan), no full build.
    #[default]
    Check,
    /// Build the package with the system Portage.
    Build,
}

/// The result of a build/QA invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildReport {
    /// Whether the QA/build passed.
    pub success: bool,
    /// Complete stdout+stderr of the run.
    pub output: String,
    /// Exit code.
    pub exit_code: i32,
}

/// Run `pkgcheck scan` on a package directory. Returns the report.
pub fn pkgcheck_scan(package_dir: &Path) -> Result<BuildReport, BuildError> {
    run_tool("pkgcheck", &["scan", "--no-config", "."], package_dir)
}

/// Run `pkgcheck scan --commits` on a repository (checks the working tree diff
/// against HEAD). Useful right before proposing a commit.
pub fn pkgcheck_commits(repo_dir: &Path) -> Result<BuildReport, BuildError> {
    run_tool("pkgcheck", &["scan", "--commits", "HEAD"], repo_dir)
}

/// Run `pkgdev manifest` to regenerate the Manifest (and validate it).
pub fn pkgdev_manifest(package_dir: &Path) -> Result<BuildReport, BuildError> {
    run_tool("pkgdev", &["manifest"], package_dir)
}

/// Build a package via `emerge` for the atom (e.g. `sys-apps/foo`).
pub fn emerge_build(atom: &str, package_dir: &Path) -> Result<BuildReport, BuildError> {
    // Set PORTAGE_CONFIGROOT/PORTDIR to the overlay dir so `emerge` finds the
    // overlay's ebuilds. `--nodeps` isn't generally valid for emerge, so we use
    // a plain emerge of the atom; users on overlays typically add the overlay.
    let mut cmd = Command::new("emerge");
    cmd.arg("--buildpkg=n");
    cmd.arg("--jobs=1");
    cmd.arg(atom);
    cmd.env("EGIT_DIR", package_dir);
    run_command(cmd, package_dir)
}

/// The main entry point for `gentooit build`.
pub fn build(
    project: &ProjectConfig,
    mode: BuildMode,
    workdir: &Path,
) -> Result<BuildReport, BuildError> {
    let _ = project;
    // Resolve the package directory. For v1 we assume the ebuilds live at
    // `<workdir>/<category>/<package>`.
    let pkg_dir = find_package_dir(workdir).ok_or_else(|| BuildError::CommandFailed {
        cmd: "locate package".to_string(),
        status: 1,
        output: "could not locate the package directory to build".to_string(),
    })?;

    match mode {
        BuildMode::Check => pkgcheck_scan(&pkg_dir),
        BuildMode::Build => {
            // Run QA first, then the actual build.
            let qa = pkgcheck_scan(&pkg_dir)?;
            if !qa.success {
                return Ok(qa);
            }
            let atom = find_ebuild_atom(&pkg_dir)?;
            emerge_build(&atom, &pkg_dir)
        }
    }
}

/// Locate a package directory under `workdir` matching `<category>/<package>`.
fn find_package_dir(workdir: &Path) -> Option<PathBuf> {
    let mut found = None;
    if let Ok(entries) = std::fs::read_dir(workdir) {
        for cat in entries.flatten() {
            if !cat.path().is_dir() {
                continue;
            }
            if let Ok(pkgs) = std::fs::read_dir(cat.path()) {
                for pkg in pkgs.flatten() {
                    if pkg.path().is_dir() {
                        found = Some(pkg.path());
                        // Take the first found; refine if multiple.
                        break;
                    }
                }
            }
            if found.is_some() {
                break;
            }
        }
    }
    found
}

/// Find the first ebuild in a package directory and derive its atom.
fn find_ebuild_atom(pkg_dir: &Path) -> Result<String, BuildError> {
    let entries = std::fs::read_dir(pkg_dir).map_err(|e| BuildError::CommandFailed {
        cmd: "readdir".to_string(),
        status: 1,
        output: e.to_string(),
    })?;
    let mut ebuild = None;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.ends_with(".ebuild") {
            ebuild = Some(name);
            break;
        }
    }
    let ebuild = ebuild.ok_or_else(|| BuildError::CommandFailed {
        cmd: "find ebuild".to_string(),
        status: 1,
        output: format!("no ebuild in {}", pkg_dir.display()),
    })?;
    let category = pkg_dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        .unwrap_or("app-misc");
    let package = pkg_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("package");
    Ok(format!("{category}/{package}/{ebuild}"))
}

fn run_tool(tool: &str, args: &[&str], cwd: &Path) -> Result<BuildReport, BuildError> {
    let mut cmd = Command::new(tool);
    cmd.args(args);
    run_command(cmd, cwd)
}

fn run_command(mut cmd: Command, cwd: &Path) -> Result<BuildReport, BuildError> {
    if !cwd.is_dir() {
        return Err(BuildError::CommandFailed {
            cmd: format!("{cmd:?}"),
            status: 1,
            output: format!("cwd {} is not a directory", cwd.display()),
        });
    }
    cmd.current_dir(cwd);

    // Detect missing tools cleanly.
    let which = which_available(cmd.get_program().to_str().unwrap_or(""));
    if !which {
        return Err(BuildError::ToolNotFound {
            tool: cmd.get_program().to_str().unwrap_or("?").to_string(),
        });
    }

    let output = cmd.output();
    match output {
        Ok(out) => {
            let mut all = String::new();
            all.push_str(&String::from_utf8_lossy(&out.stdout));
            all.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(BuildReport {
                success: out.status.success(),
                output: all,
                exit_code: out.status.code().unwrap_or(-1),
            })
        }
        Err(e) => Err(BuildError::CommandFailed {
            cmd: format!("{cmd:?}"),
            status: 1,
            output: e.to_string(),
        }),
    }
}

fn which_available(tool: &str) -> bool {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

/// Emit a GitHub Actions workflow that builds the package in a container,
/// suitable for writing to `.github/workflows/gentooit.yml`.
pub fn ci_workflow_yaml(atom: &str) -> String {
    format!(
        r#"name: gentooit build
on:
  push:
    paths: ["**/*.ebuild", "**/Manifest", "**/metadata.xml"]
  pull_request:

jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: gentoo/stage3:latest
    steps:
      - uses: actions/checkout@v4
      - name: Add overlay
        run: |
          echo "[gentooit]" > /etc/portage/repos.conf/gentooit.conf
          echo "location = /__w/github/workspace" >> /etc/portage/repos.conf/gentooit.conf
          echo "masters = gentoo" >> /etc/portage/repos.conf/gentooit.conf
      - name: Build {atom}
        run: emerge --ask=n --jobs=2 {atom}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_ci_workflow() {
        let yaml = ci_workflow_yaml("sys-apps/foo");
        assert!(yaml.contains("sys-apps/foo"));
        assert!(yaml.contains("emerge"));
    }
}
