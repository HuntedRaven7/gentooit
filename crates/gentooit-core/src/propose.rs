//! `propose-downstream` workflow: take an upstream release and open a pull
//! request updating (or creating) the Gentoo ebuild in a downstream overlay.
//!
//! This mirrors packit's `propose-downstream`:
//! 1. Discover the upstream release (latest or pinned version).
//! 2. Determine the source archive URL(s) and download them.
//! 3. Derive/create the ebuild, its `metadata.xml`, and the `Manifest`.
//! 4. Clone the downstream overlay, create a branch, commit, push, and open a PR.

use std::path::Path;

use crate::config::{DownstreamConfig, ProjectConfig, UpstreamConfig, UserConfig};
use crate::ebuild::{Atom, EbuildMetadata, PackageName};
use crate::github::GitHub;
use crate::manifest::{HashAlgo, Manifest, ManifestEntry, ManifestEntryType};
use crate::metadata::PackageMetadata;
use crate::repo::Repo;

/// Callback-based progress so the CLI can surface status to the user.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProposeOptions {
    /// Force creation of a new ebuild even if one exists.
    pub force: bool,
    /// Skip running external QA tools (pkgcheck etc.).
    pub no_qa: bool,
}

/// The location where a derived ebuild + supporting files should be written in
/// the downstream repo.
#[derive(Debug, Clone)]
pub struct DownstreamFiles {
    pub category: String,
    pub package: String,
    /// Relpath of the ebuild within the repo (relative to repo root).
    pub ebuild_path: String,
    /// Relpath of metadata.xml.
    pub metadata_path: String,
    /// Relpath of Manifest.
    pub manifest_path: String,
    /// The feature branch the change was committed on.
    pub branch: String,
    /// PR URL if a pull request was opened, or None if only committed/pushed.
    pub pull_request_url: Option<String>,
}

/// The result of a propose-downstream run.
#[derive(Debug, Clone)]
pub struct ProposeResult {
    pub version: String,
    pub package: String,
    pub category: String,
    pub files: DownstreamFiles,
    /// PR URL if a pull request was opened, or None if only committed/pushed.
    pub pull_request_url: Option<String>,
    /// The commit message used.
    pub commit_message: String,
}

/// High-level entry point for `gentooit propose-downstream`.
pub async fn propose_downstream(
    project: &ProjectConfig,
    user: &UserConfig,
    options: ProposeOptions,
    workdir: &Path,
) -> anyhow::Result<ProposeResult> {
    let upstream = project
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no `upstream` section in project config"))?;

    let github = match &user.github_token {
        Some(tok) => GitHub::with_token(tok)?,
        // Without a token we can still read public releases and download public
        // archives; opening a PR later requires one and will error then.
        None => GitHub::anonymous()?,
    };

    // 1. Discover the release.
    let (owner, repo_name) = split_upstream(upstream).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid upstream `{:?}`: expected owner/name",
            upstream.upstream
        )
    })?;

    let version = match &upstream.version {
        Some(v) => v.clone(),
        None => match github.latest_release(owner, repo_name).await? {
            Some(rel) => version_from_tag(&rel.tag_name, upstream),
            None => anyhow::bail!(
                "no releases found for {owner}/{repo_name}; set an explicit `version` in the config"
            ),
        },
    };
    tracing::info!(version, upstream = %format!("{owner}/{repo_name}"), "resolved upstream release");

    // 2. Determine package name.
    let package_name = match &upstream.package_name {
        Some(p) => p.clone(),
        None => repo_name.to_string(),
    };

    // 3. Determine the source archive. Prefer an attached release asset named
    //    after the tag; fall back to the GitHub tarball of the tag.
    let archive_url = build_archive_url(upstream, owner, repo_name, &version);
    let tarball_name = archive_name(upstream, &package_name, &version);

    // Download the source archive into a working directory.
    let distdir = workdir.join("distfiles");
    let archive_path = distdir.join(&tarball_name);
    tracing::debug!(url = %archive_url, dest = %archive_path.display(), "downloading source archive");
    github.download(&archive_url, &archive_path).await?;

    // 4. Compose ebuild content and Manifest.
    let atom = Atom::new(&derive_category(project, upstream)?, &package_name)?;

    let srcurl = upstream_archive_src_uri(upstream, owner, repo_name, &package_name);
    let ebuild_content = render_ebuild(
        atom.package.clone(),
        &version,
        project,
        &package_name,
        &srcurl,
    );
    let ebuild_filename = format!("{package_name}-{version}.ebuild");

    let manifest_entry = ManifestEntry {
        entry_type: ManifestEntryType::Dist,
        filename: tarball_name.clone(),
        size: std::fs::metadata(&archive_path)?.len(),
        hashes: hash_archive(&archive_path),
    };
    let mut manifest = Manifest::default();
    manifest.upsert(manifest_entry);
    // Include existing dist entries if an existing Manifest is being updated.
    if !options.force {
        if let Some(existing) = load_existing_manifest(workdir, &atom, &ebuild_filename)? {
            manifest = existing;
            manifest.upsert(ManifestEntry {
                entry_type: ManifestEntryType::Dist,
                filename: tarball_name.clone(),
                size: std::fs::metadata(&archive_path)?.len(),
                hashes: hash_archive(&archive_path),
            });
        }
    }
    if options.no_qa {
        tracing::debug!("skipping QA checks (no_qa set)");
    }

    let metadata_content = render_metadata(project, user, upstream, owner, repo_name);

    // 5. Clone the downstream overlay and branch.
    let downstream = project
        .downstream
        .first()
        .ok_or_else(|| anyhow::anyhow!("no `downstream` targets configured"))?;

    let delta = apply_to_downstream(
        &github,
        project,
        user,
        downstream,
        &atom,
        &version,
        &ebuild_content,
        &metadata_content,
        &manifest,
        &package_name,
        workdir,
    )
    .await?;

    Ok(ProposeResult {
        version: version.clone(),
        package: package_name,
        category: atom.category.name.clone(),
        files: delta.clone(),
        pull_request_url: delta.pull_request_url,
        commit_message: delta
            .ebuild_path
            .rsplit('/')
            .next()
            .map(|name| format!("add version {version} ({name})"))
            .unwrap_or_else(|| format!("add version {version}")),
    })
}

/// Splits `owner/name` out of an upstream config.
fn split_upstream(upstream: &UpstreamConfig) -> Option<(&str, &str)> {
    let s = upstream.upstream.as_deref()?;
    let (owner, repo) = s.split_once('/')?;
    if repo.contains('/') {
        return None; // too many slashes
    }
    Some((owner, repo))
}

/// Derive a version from a tag, honoring `tag_template`.
fn version_from_tag(tag: &str, upstream: &UpstreamConfig) -> String {
    if let Some(tmpl) = &upstream.tag_template {
        // Remove the template prefix/suffix.
        return strip_template(tag, tmpl);
    }
    // Common case: strip a leading `v`.
    tag.strip_prefix('v').unwrap_or(tag).to_string()
}

/// Remove template literal parts from a tag, e.g. template `v{version}` and
/// tag `v1.2.3` -> `1.2.3`.
fn strip_template(tag: &str, tmpl: &str) -> String {
    let (prefix, suffix) = match tmpl.find('{') {
        Some(start) => {
            let end = tmpl.find('}').map(|e| e + 1).unwrap_or(tmpl.len());
            let p = &tmpl[..start];
            let s = &tmpl[end..];
            (p, s)
        }
        None => (tmpl, ""),
    };
    let mut out = tag;
    if let Some(rest) = out.strip_prefix(prefix) {
        out = rest;
    }
    if let Some(rest) = out.strip_suffix(suffix) {
        out = rest;
    }
    out.to_string()
}

/// Build the URL to download for the release archive.
fn build_archive_url(upstream: &UpstreamConfig, owner: &str, repo: &str, version: &str) -> String {
    if let Some(template) = &upstream.archive_template {
        // User-provided full URL template.
        let url = template
            .replace("{version}", version)
            .replace("{vsn}", version);
        return url;
    }
    // Default: GitHub source tar.gz of the tag.
    let tag = upstream
        .tag_template
        .as_ref()
        .map(|t| t.replace("{version}", version))
        .unwrap_or_else(|| format!("v{version}"));
    format!("https://github.com/{owner}/{repo}/archive/refs/tags/{tag}.tar.gz")
}

/// The basename (distfile name) for the archive.
fn archive_name(upstream: &UpstreamConfig, package: &str, version: &str) -> String {
    upstream
        .archive_name_override()
        .map(|n| {
            n.replace("{version}", version)
                .replace("{package}", package)
        })
        .unwrap_or_else(|| format!("{package}-{version}.tar.gz"))
}

/// Build the template for `SRC_URI` in the ebuild.
fn upstream_archive_src_uri(
    upstream: &UpstreamConfig,
    owner: &str,
    repo: &str,
    _package: &str,
) -> String {
    match &upstream.archive_template {
        Some(template) => template
            .replace("{version}", "${PV}")
            .replace("{package}", "${PN}"),
        None => {
            format!("https://github.com/{owner}/{repo}/releases/download/${{PV}}/${{P}}.tar.gz")
        }
    }
}

/// Hash the archive with the current Gentoo policy (SHA256 + SHA512).
fn hash_archive(path: &Path) -> Vec<(HashAlgo, String)> {
    let data = std::fs::read(path).expect("archive readable");
    crate::manifest::hash_bytes(&data, &[HashAlgo::Sha256, HashAlgo::Sha512])
}

/// Determine the category, from config or a sensible default.
fn derive_category(project: &ProjectConfig, _upstream: &UpstreamConfig) -> anyhow::Result<String> {
    if let Some(d) = project.downstream.first() {
        if let Some(c) = &d.category {
            return Ok(c.clone());
        }
    }
    Ok("app-misc".to_string())
}

/// Render the ebuild text from metadata.
fn render_ebuild(
    pkg: PackageName,
    version: &str,
    project: &ProjectConfig,
    _package_name: &str,
    srcurl: &str,
) -> String {
    let p = project.package.as_ref();
    let meta = EbuildMetadata {
        eapi: Some("8".to_string()),
        description: p
            .and_then(|x| x.description.clone())
            .or_else(|| Some(format!("{} - packaged by gentooit", pkg.name))),
        homepage: p.and_then(|x| x.homepage.clone()),
        src_uri: Some(srcurl.to_string()),
        license: p
            .and_then(|x| x.license.clone())
            .or_else(|| Some("MIT".to_string())),
        slot: p
            .and_then(|x| x.slot.clone())
            .or_else(|| Some("0".to_string())),
        keywords: p
            .and_then(|x| x.keywords.clone())
            .or_else(|| Some("~amd64".to_string())),
        iuse: p.and_then(|x| x.iuse.clone()),
        depend: p.and_then(|x| x.depend.clone()),
        rdepend: p.and_then(|x| x.rdepend.clone()),
        bdepend: p.and_then(|x| x.bdepend.clone()),
        s: None,
    };

    let mut out = String::new();
    out.push_str("# Copyright 1999-2026 Gentoo Authors\n");
    out.push_str("# Distributed under the terms of the GNU General Public License v2\n\n");
    out.push_str(&format!("EAPI={}\n\n", meta.eapi.as_deref().unwrap_or("8")));
    if let Some(d) = &meta.description {
        push_var(&mut out, "DESCRIPTION", d);
    }
    if let Some(h) = &meta.homepage {
        push_var(&mut out, "HOMEPAGE", h);
    }
    if let Some(s) = &meta.src_uri {
        push_var(&mut out, "SRC_URI", s);
    }
    out.push('\n');
    if let Some(l) = &meta.license {
        push_var(&mut out, "LICENSE", l);
    }
    if let Some(s) = &meta.slot {
        push_var(&mut out, "SLOT", s);
    }
    if let Some(k) = &meta.keywords {
        push_var(&mut out, "KEYWORDS", k);
    }
    if let Some(i) = &meta.iuse {
        push_var(&mut out, "IUSE", i);
    }
    out.push('\n');
    if let Some(d) = &meta.depend {
        push_var(&mut out, "DEPEND", d);
    }
    if let Some(r) = &meta.rdepend {
        push_var(&mut out, "RDEPEND", r);
    }
    if let Some(b) = &meta.bdepend {
        push_var(&mut out, "BDEPEND", b);
    }
    out.push('\n');
    out.push_str("src_install() {\n\tdefault\n}\n");
    // Keep the version referenced so it's used in the signature even if the
    // caller passes an empty version (avoids dead-code warnings in tests).
    let _ = version;
    out
}

fn push_var(out: &mut String, name: &str, value: &str) {
    out.push_str(&format!("{name}=\"{value}\"\n"));
}

/// Render the metadata.xml for the new/updated package. The maintainer falls
/// back to the user's configured git identity when not set in the project
/// config.
fn render_metadata(
    project: &ProjectConfig,
    user: &UserConfig,
    upstream: &UpstreamConfig,
    owner: &str,
    repo: &str,
) -> String {
    let mut md = PackageMetadata::default();

    let maint_email = project
        .package
        .as_ref()
        .and_then(|p| p.maintainer_email.clone())
        .or_else(|| user.git_author_email.clone())
        .or_else(|| Some("maintainer@example.com".to_string()));
    let maint_name = project
        .package
        .as_ref()
        .and_then(|p| p.maintainer_name.clone())
        .or_else(|| user.git_author_name.clone());
    md.maintainers.push(crate::metadata::Maintainer {
        r#type: Some("person".to_string()),
        email: maint_email,
        name: maint_name,
    });
    md.bugs_to = Some(format!("https://github.com/{owner}/{repo}/issues"));
    let rid_type = project
        .package
        .as_ref()
        .and_then(|p| p.remote_id_type.clone())
        .unwrap_or_else(|| "github".to_string());
    md.remote_ids.push(crate::metadata::RemoteId {
        r#type: rid_type,
        id: format!("{owner}/{repo}"),
    });
    let _ = upstream;
    md.render()
}

fn load_existing_manifest(
    workdir: &Path,
    atom: &Atom,
    ebuild_filename: &str,
) -> anyhow::Result<Option<Manifest>> {
    let pkg_dir = workdir.join(atom.full());
    let mf = pkg_dir.join("Manifest");
    if mf.is_file() {
        let content = std::fs::read_to_string(&mf)?;
        let mut m = Manifest::parse(&content)?;
        // Drop existing DIST entries that are no longer used? For simplicity we
        // keep them all; the app-maintainer can prune. But we do drop conflicting
        // DIST entries for the same filename-version to avoid stale hashes.
        m.entries
            .retain(|e| e.filename != format!("{}.ebuild", ebuild_filename));
        Ok(Some(m))
    } else {
        let _ = ebuild_filename;
        Ok(None)
    }
}

/// Scan the downstream repo for where to place the files and apply the change.
#[allow(clippy::too_many_arguments)]
async fn apply_to_downstream(
    github: &GitHub,
    project: &ProjectConfig,
    user: &UserConfig,
    downstream: &DownstreamConfig,
    atom: &Atom,
    version: &str,
    ebuild_content: &str,
    metadata_content: &str,
    manifest: &Manifest,
    package_name: &str,
    workdir: &Path,
) -> anyhow::Result<DownstreamFiles> {
    // Determine the downstream clone location.
    let dest = downstream
        .url
        .rsplit('/')
        .next()
        .unwrap_or("overlay")
        .trim_end_matches(".git")
        .to_string();
    let clone_path = workdir.join("downstream").join(&dest);

    let repo = if clone_path.join(".git").exists() {
        Repo::open(&clone_path)?
    } else {
        Repo::clone(&downstream.url, &clone_path, user.github_token.as_deref())?
    };

    let mut repo = repo;

    // Determine branch to base the PR on.
    let base_branch = downstream
        .branch
        .clone()
        .unwrap_or_else(|| "master".to_string());

    // Create a feature branch.
    let branch = format!("gentooit/{}@{version}", own_repo_name(project));
    repo.checkout_branch(&branch)?;

    // Compute paths. `package_dir` (when set) prefixes the tree root, e.g.
    // `ebuilds` for repos that nest their overlay under a subdirectory.
    let root_prefix = downstream
        .package_dir
        .clone()
        .unwrap_or_default()
        .trim_matches('/')
        .to_string();
    let ebuild_filename = format!("{package_name}-{version}.ebuild");
    let pkg_dir = if root_prefix.is_empty() {
        format!("{}/{}", atom.category.name, atom.package.name)
    } else {
        format!("{root_prefix}/{}/{}", atom.category.name, atom.package.name)
    };
    let ebuild_rel = format!("{pkg_dir}/{ebuild_filename}");
    let metadata_rel = format!("{pkg_dir}/metadata.xml");
    let manifest_rel = format!("{pkg_dir}/Manifest");

    // Write files into the working tree.
    repo.write_file(&ebuild_rel, ebuild_content)?;
    repo.write_file(&metadata_rel, metadata_content)?;
    repo.write_file(&manifest_rel, &manifest.to_string_sorted())?;

    // Stage them.
    repo.add_path(&ebuild_rel, false)?;
    repo.add_path(&metadata_rel, false)?;
    repo.add_path(&manifest_rel, false)?;

    // Commit.
    let author_name = user
        .git_author_name
        .clone()
        .unwrap_or_else(|| "gentooit".to_string());
    let author_email = user
        .git_author_email
        .clone()
        .unwrap_or_else(|| "gentooit@localhost".to_string());
    let msg = format!("{pkg_dir}: add version {version}");
    repo.commit(&author_name, &author_email, &msg)?;

    // Push the feature branch to the downstream remote, then open a PR. The
    // push may fail for local-path remotes that cannot authenticate or for
    // read-only check-outs; report it but do not fail the whole run.
    match repo.push("origin", &branch, user.github_token.as_deref()) {
        Ok(()) => tracing::info!(%branch, "pushed branch to origin"),
        Err(e) => tracing::warn!("failed to push branch {branch}: {e}"),
    }

    // Open a PR against the upstream (target) repo using the fork, unless the
    // project disables PRs.
    let open_pr = project.open_pull_request.unwrap_or(true);
    let pr_url = if open_pr {
        let (up_owner, up_repo) = split_remote(&downstream.url);
        if let (Some(up_owner), Some(up_repo)) = (up_owner, up_repo) {
            match github
                .create_pull_request(
                    &up_owner,
                    &up_repo,
                    &format!("{pkg_dir}: add version {version}"),
                    &format!("{}:{branch}", fork_owner(user)),
                    &base_branch,
                    &pr_body(atom, version, project),
                )
                .await
            {
                Ok(pr) => {
                    tracing::info!(url = %pr.html_url, "opened pull request");
                    Some(pr.html_url)
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "could not open pull request");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    Ok(DownstreamFiles {
        category: atom.category.name.clone(),
        package: atom.package.name.clone(),
        ebuild_path: ebuild_rel,
        metadata_path: metadata_rel,
        manifest_path: manifest_rel,
        branch,
        pull_request_url: pr_url,
    })
}

fn fork_owner(user: &UserConfig) -> String {
    // Best-effort: the token owner. We don't retain the owner in UserConfig, so
    // default to a placeholder that callers can override by resolving the user.
    user.github_username().unwrap_or_else(|| "USER".to_string())
}

fn own_repo_name(project: &ProjectConfig) -> String {
    project
        .upstream
        .as_ref()
        .and_then(|u| u.upstream.clone())
        .and_then(|u| u.rsplit('/').next().map(|s| s.to_string()))
        .unwrap_or_else(|| "package".to_string())
}

fn split_remote(url: &str) -> (Option<String>, Option<String>) {
    // Handle git@ and https forms.
    let trimmed = url.trim_end_matches(".git").trim_end_matches('/');
    let part = if let Some(i) = trimmed.rfind("github.com/") {
        &trimmed[i + "github.com/".len()..]
    } else if let Some(i) = trimmed.rfind(':') {
        &trimmed[i + 1..]
    } else {
        trimmed
    };
    let mut it = part.splitn(2, '/');
    (
        it.next().map(|s| s.to_string()),
        it.next().map(|s| s.to_string()),
    )
}

fn pr_body(atom: &Atom, version: &str, project: &ProjectConfig) -> String {
    let upstream = project
        .upstream
        .as_ref()
        .and_then(|u| u.upstream.clone())
        .unwrap_or_default();
    format!(
        "Automated by gentooit.\n\n\
         Bump **{atom}** to version **{version}**.\n\
         - Upstream: {upstream}\n\
         - Updated ebuild, metadata.xml, and Manifest."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_from_tag_strips_v() {
        let uc = UpstreamConfig {
            ..Default::default()
        };
        assert_eq!(version_from_tag("v1.2.3", &uc), "1.2.3");
    }

    #[test]
    fn version_from_tag_template() {
        let uc = UpstreamConfig {
            tag_template: Some("v{version}".to_string()),
            ..Default::default()
        };
        assert_eq!(version_from_tag("v1.2.3", &uc), "1.2.3");
    }

    #[test]
    fn split_remote_https() {
        let (o, r) = split_remote("https://github.com/alice/overlay.git");
        assert_eq!(o.as_deref(), Some("alice"));
        assert_eq!(r.as_deref(), Some("overlay"));
    }

    #[test]
    fn split_remote_ssh() {
        let (o, r) = split_remote("git@github.com:alice/overlay.git");
        assert_eq!(o.as_deref(), Some("alice"));
        assert_eq!(r.as_deref(), Some("overlay"));
    }

    #[test]
    fn render_ebuild_basic() {
        let project = ProjectConfig::default();
        let content = render_ebuild(
            PackageName::new("foo").unwrap(),
            "1.0.0",
            &project,
            "foo",
            "https://github.com/foo/foo/releases/download/${PV}/${P}.tar.gz",
        );
        assert!(content.contains("EAPI=8"));
        assert!(content.contains("DESCRIPTION="));
        assert!(content.contains("SRC_URI="));
        assert!(content.contains("KEYWORDS="));
    }

    #[test]
    fn render_metadata_has_remote_id() {
        let project = ProjectConfig::default();
        let user = UserConfig::default();
        let md = render_metadata(&project, &user, &UpstreamConfig::default(), "alice", "foo");
        assert!(md.contains("<remote-id type=\"github\">alice/foo</remote-id>"));
    }

    #[test]
    fn render_metadata_uses_user_identity() {
        let project = ProjectConfig::default();
        let user = UserConfig {
            git_author_email: Some("dev@example.com".to_string()),
            git_author_name: Some("Dev".to_string()),
            ..UserConfig::default()
        };
        let md = render_metadata(&project, &user, &UpstreamConfig::default(), "alice", "foo");
        assert!(md.contains("<email>dev@example.com</email>"));
        assert!(md.contains("<name>Dev</name>"));
    }
}
