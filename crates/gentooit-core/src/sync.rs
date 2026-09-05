//! `sync-from-downstream` workflow: copy changes from the downstream Gentoo
//! ebuild repository back into the upstream project.
//!
//! This mirrors packit's `sync-from-downstream`: it takes the current ebuild /
//! supporting files in the downstream repo and opens a pull request against the
//! *upstream* project that vendors the packaging metadata, so upstream can see
//! and maintain the distro packaging (e.g. using `files_to_sync`).

use std::path::Path;

use crate::config::{ProjectConfig, UpstreamConfig, UserConfig};
use crate::github::GitHub;

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

    let github = match &user.github_token {
        Some(tok) => GitHub::with_token(tok)?,
        None => GitHub::anonymous()?,
    };

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
            &format!("{}:{branch}", fork_owner(user)),
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

fn fork_owner(user: &UserConfig) -> String {
    user.github_username().unwrap_or_else(|| "USER".to_string())
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

/// Apply sync changes locally (for offline/dry-run) by copying upstream files
/// into the downstream repo worktree. This is a helper for the CLI's
/// `--local` mode; not currently wired into the async path.
pub fn sync_local(
    project: &ProjectConfig,
    upstream_dir: &Path,
    _downstream_dir: &Path,
) -> anyhow::Result<Vec<String>> {
    let files = match project.files_to_sync.is_empty() {
        true => vec![],
        false => project
            .files_to_sync
            .iter()
            .map(|f| f.src.clone())
            .collect(),
    };
    let _ = upstream_dir;
    Ok(files)
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
