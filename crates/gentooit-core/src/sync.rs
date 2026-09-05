//! `sync-from-downstream` workflow: copy changes from the downstream Gentoo
//! ebuild repository back into the upstream project.
//!
//! This mirrors packit's `sync-from-downstream`: it takes the current ebuild /
//! supporting files in the downstream repo and opens a pull request against the
//! *upstream* project that vendors the packaging metadata, so upstream can see
//! and maintain the distro packaging (e.g. using `files_to_sync`).

use std::path::Path;
use std::process::Command;

use crate::config::{ProjectConfig, UpstreamConfig, UserConfig};
use crate::github::GitHub;
use crate::repo::Repo;

/// The result of a sync-from-downstream run.
#[derive(Debug, Clone)]
pub struct SyncResult {
    /// Files that were synced, relative to the upstream repo.
    pub files: Vec<String>,
    /// PR URL if opened.
    pub pull_request_url: Option<String>,
}

/// Entry point for `gentooit sync-from-downstream`.
pub async fn sync_from_downstream(
    project: &ProjectConfig,
    user: &UserConfig,
    workdir: &Path,
) -> anyhow::Result<SyncResult> {
    let upstream = project
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no `upstream` section in project config"))?;

    let (owner, repo_name) = split_upstream(upstream).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid upstream `{:?}`: expected owner/name",
            upstream.upstream
        )
    })?;

    let github = if let (Some(app_id), Some(key_path)) = (&user.github_app_id, &user.github_app_key)
    {
        let app = GitHub::with_app(*app_id, key_path)?;
        // For sync we don't have a downstream repo URL yet; use the upstream
        // repo to discover the installation (the App must be installed on the
        // upstream repo to open PRs there).
        let installation = app.get_repository_installation(owner, repo_name).await?;
        app.with_installation(*installation.id)?
    } else if let Some(tok) = &user.github_token {
        GitHub::with_token(tok)?
    } else {
        GitHub::anonymous()?
    };

    // Resolve the authenticated user's login for fork refs. Falls back to the
    // config hint / env var when the client is unauthenticated or the API call
    // fails.
    let username = match github.resolve_username().await {
        Ok(name) => name,
        Err(_) => user.github_username().unwrap_or_else(|| "USER".to_string()),
    };
    tracing::info!(%username, "resolved GitHub username");

    // The downstream package files live in `workdir` (a checkout of the
    // overlay). Determine which files to copy based on `files_to_sync`, or a
    // sensible default (the files gentooit manages).
    let files = resolve_files_to_sync(project, workdir)?;

    // Open a PR against the upstream repo with these files.
    let default_branch = github.default_branch(owner, repo_name).await?;
    let branch = format!("gentooit-sync/{}", chrono_ts());

    let pr_url = github
        .create_pull_request(
            owner,
            repo_name,
            "Sync distro packaging files from Gentoo",
            &format!("{}:{branch}", fork_owner(&username)),
            &default_branch,
            &sync_body(&files),
        )
        .await?
        .html_url;

    Ok(SyncResult {
        files,
        pull_request_url: Some(pr_url),
    })
}

fn split_upstream(upstream: &UpstreamConfig) -> Option<(&str, &str)> {
    let s = upstream.upstream.as_deref()?;
    let (owner, repo) = s.split_once('/')?;
    if repo.contains('/') {
        return None;
    }
    Some((owner, repo))
}

fn fork_owner(username: &str) -> String {
    username.to_string()
}

/// Determine which files to sync, mirroring packit's `files_to_sync`.
fn resolve_files_to_sync(project: &ProjectConfig, workdir: &Path) -> anyhow::Result<Vec<String>> {
    let mut paths = Vec::new();
    if project.files_to_sync.is_empty() {
        // Default: sync the ebuild files that gentooit manages, if present.
        for candidate in [
            "metadata.xml",
            "Manifest",
            "gentooit.yaml",
            ".gentooit.yaml",
        ] {
            let full = workdir.join(candidate);
            if full.is_file() {
                paths.push(candidate.to_string());
            }
        }
        // Also match any *.ebuild in the workdir subtree.
        paths.extend(find_ebuilds(workdir)?);
        if paths.is_empty() {
            anyhow::bail!(
                "no files to sync; configure `files_to_sync` or place ebuilds in {workdir:?}"
            );
        }
        return Ok(paths);
    }

    for fs in &project.files_to_sync {
        let src = if fs.src.contains('/') {
            let parts = Path::new(&fs.src);
            let mut found = None;
            let mut dir = workdir.to_path_buf();
            for comp in parts.components() {
                dir = dir.join(comp);
                if dir.is_file() {
                    found = Some(dir.clone());
                }
            }
            match found {
                Some(p) => p,
                None => workdir.join(&fs.src),
            }
        } else {
            workdir.join(&fs.src)
        };
        if src.is_file() {
            paths.push(fs.dest.clone());
        }
    }
    Ok(paths)
}

fn find_ebuilds(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut found = Vec::new();
    recurse_ebuilds(dir, &mut found)?;
    Ok(found)
}

fn recurse_ebuilds(dir: &Path, out: &mut Vec<String>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && !path.starts_with(".git") {
            recurse_ebuilds(&path, out)?;
        } else if path.extension().map(|e| e == "ebuild").unwrap_or(false) {
            out.push(path.file_name().unwrap().to_string_lossy().to_string());
        }
    }
    Ok(())
}

fn sync_body(files: &[String]) -> String {
    format!(
        "Automated by gentooit.\n\n\
         Synced the following Gentoo packaging files into the upstream repo:\n{}",
        files
            .iter()
            .map(|f| format!("- `{f}`"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn chrono_ts() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn git_user(upstream_dir: &Path) -> anyhow::Result<(String, String)> {
    let name = Command::new("git")
        .arg("-C")
        .arg(upstream_dir)
        .arg("config")
        .arg("user.name")
        .output()?;
    let email = Command::new("git")
        .arg("-C")
        .arg(upstream_dir)
        .arg("config")
        .arg("user.email")
        .output()?;
    let name = if name.status.success() {
        String::from_utf8_lossy(&name.stdout).trim().to_string()
    } else {
        String::new()
    };
    let email = if email.status.success() {
        String::from_utf8_lossy(&email.stdout).trim().to_string()
    } else {
        String::new()
    };
    if name.is_empty() && email.is_empty() {
        anyhow::bail!("no git user configured");
    }
    Ok((name, email))
}

/// Apply sync changes locally by copying files from the downstream worktree
/// into the upstream worktree and creating a single commit.
pub fn sync_local(
    project: &ProjectConfig,
    upstream_dir: &Path,
    downstream_dir: &Path,
) -> anyhow::Result<Vec<String>> {
    let repo = Repo::open(upstream_dir)?;
    let mut synced = Vec::new();

    if project.files_to_sync.is_empty() {
        // Default: sync the ebuild files that gentooit manages, if present.
        for candidate in [
            "metadata.xml",
            "Manifest",
            "gentooit.yaml",
            ".gentooit.yaml",
        ] {
            let src = downstream_dir.join(candidate);
            if src.is_file() {
                let dest = upstream_dir.join(candidate);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&src, &dest)?;
                repo.add_path(candidate, false)?;
                synced.push(candidate.to_string());
            }
        }
        // Also match any *.ebuild in the downstream subtree, preserving
        // relative paths.
        for entry in walkdir::WalkDir::new(downstream_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file()
                && path.extension().map(|e| e == "ebuild").unwrap_or(false)
                && !path.starts_with(downstream_dir.join(".git"))
            {
                let rel = path.strip_prefix(downstream_dir).map_err(|_| {
                    anyhow::anyhow!("path outside downstream dir: {}", path.display())
                })?;
                let dest = upstream_dir.join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(path, &dest)?;
                repo.add_path(&rel.to_string_lossy(), false)?;
                synced.push(rel.to_string_lossy().to_string());
            }
        }
    } else {
        for fs in &project.files_to_sync {
            let src_path = downstream_dir.join(&fs.src);
            if !src_path.is_file() {
                continue;
            }
            let dest_path = upstream_dir.join(&fs.dest);
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src_path, &dest_path)?;
            repo.add_path(&fs.dest, false)?;
            synced.push(fs.dest.clone());
        }
    }

    if synced.is_empty() {
        return Ok(synced);
    }

    let (author_name, author_email) = match git_user(upstream_dir) {
        Ok((name, email)) => (name, email),
        Err(_) => ("gentooit".to_string(), "gentooit@localhost".to_string()),
    };

    repo.commit(
        &author_name,
        &author_email,
        "gentooit: sync packaging files from downstream",
    )?;

    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_body_lists_files() {
        let body = sync_body(&["Manifest".to_string(), "metadata.xml".to_string()]);
        assert!(body.contains("`Manifest`"));
        assert!(body.contains("`metadata.xml`"));
    }
}
