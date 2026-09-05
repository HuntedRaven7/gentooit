//! `propose-downstream` workflow: take an upstream release and open a pull
//! request updating (or creating) the Gentoo ebuild in a downstream overlay.
//!
//! This mirrors packit's `propose-downstream`:
//! 1. Discover the upstream release (latest or pinned version).
//! 2. Determine the source archive URL(s) and download them.
//! 3. Derive/create the ebuild, its `metadata.xml`, and the `Manifest`.
//! 4. Clone the downstream overlay, create a branch, commit, push, and open a PR.

use std::cmp::Ordering;
use std::path::Path;
use std::process::Command;

use crate::build::{pkgcheck_scan, pkgdev_manifest, BuildError};
use crate::config::{DownstreamConfig, ProjectConfig, UpstreamConfig, UserConfig};
use crate::ebuild::{Atom, EbuildMetadata, PackageName};
use crate::github::{GitHub, Release, ReleaseAsset};
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
    /// Optional summary of QA tool results (pkgcheck/pkgdev) from the run.
    pub qa_summary: Option<String>,
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

/// A resolved source archive for a release: where to download it, the
/// parameterized `SRC_URI` for the ebuild, and an optional `S` override for
/// archives whose extracted directory doesn't match `${P}`.
#[derive(Debug, Clone)]
pub struct SourceArchive {
    /// Literal URL to download the archive right now.
    pub download_url: String,
    /// `SRC_URI` value with `${PV}`/`${P}` substituted where safe.
    pub src_uri: String,
    /// The distfile basename (used for `Manifest` and local download).
    pub filename: String,
    /// Extracted source directory, as an ebuild `S` value
    /// (`${WORKDIR}/...`), when it differs from ${P}.
    pub extract_dir: Option<String>,
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

    let downstream = project
        .downstream
        .first()
        .ok_or_else(|| anyhow::anyhow!("no `downstream` targets configured"))?;

    // Resolve the downstream owner/repo early so App auth can discover the
    // installation for the correct repository.
    let (downstream_owner, downstream_repo) = split_remote(&downstream.url);
    let (downstream_owner, downstream_repo) = match (downstream_owner, downstream_repo) {
        (Some(o), Some(r)) => (o, r),
        _ => anyhow::bail!("invalid downstream URL: {}", downstream.url),
    };

    let github = if let (Some(app_id), Some(key_path)) = (&user.github_app_id, &user.github_app_key)
    {
        let app = GitHub::with_app(*app_id, key_path)?;
        let installation = app
            .get_repository_installation(&downstream_owner, &downstream_repo)
            .await?;
        tracing::info!(
            installation_id = %installation.id,
            account = %installation.account.login,
            "resolved GitHub App installation"
        );
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

    // 1. Discover the release. Keep the release object (with its assets) so we
    //    can pick a source archive from attached release assets.
    let (owner, repo_name) = split_upstream(upstream).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid upstream `{:?}`: expected owner/name",
            upstream.upstream
        )
    })?;

    let (version, release) = match &upstream.version {
        Some(v) => (
            v.clone(),
            find_release(&github, upstream, owner, repo_name, v).await?,
        ),
        None => match github.latest_release(owner, repo_name).await? {
            Some(rel) => (version_from_tag(&rel.tag_name, upstream), Some(rel)),
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

    // 3. Determine the source archive. Prefer an attached release asset (e.g.
    //    a tarball that doesn't match `${P}`), falling back to the GitHub
    //    source tarball of the tag.
    let archive = resolve_source_archive(
        upstream,
        release.as_ref(),
        owner,
        repo_name,
        &package_name,
        &version,
    );

    // Download the source archive into a working directory.
    let distdir = workdir.join("distfiles");
    let archive_path = distdir.join(&archive.filename);
    tracing::debug!(
        url = %archive.download_url,
        dest = %archive_path.display(),
        "downloading source archive"
    );
    github
        .download(&archive.download_url, &archive_path)
        .await?;

    // 4. Compose ebuild content and Manifest.
    let atom = Atom::new(&derive_category(project, upstream)?, &package_name)?;

    let is_cargo = detect_cargo(&archive_path)?;
    tracing::debug!(is_cargo, "detected project type");

    let ebuild_content = render_ebuild(
        atom.package.clone(),
        &version,
        project,
        &package_name,
        &archive.src_uri,
        archive.extract_dir.as_deref(),
        is_cargo,
    );
    let ebuild_filename = format!("{package_name}-{version}.ebuild");

    let manifest_entry = ManifestEntry {
        entry_type: ManifestEntryType::Dist,
        filename: archive.filename.clone(),
        size: std::fs::metadata(&archive_path)?.len(),
        hashes: hash_archive(&archive_path),
    };
    let mut manifest = Manifest::default();
    manifest.upsert(manifest_entry.clone());
    // Include existing dist entries if an existing Manifest is being updated.
    if !options.force {
        if let Some(existing) = load_existing_manifest(workdir, &atom, &ebuild_filename)? {
            manifest = existing;
            manifest.upsert(manifest_entry);
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
        options.no_qa,
        &username,
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

/// The git tag that corresponds to `version`.
fn tag_for_version(upstream: &UpstreamConfig, version: &str) -> String {
    upstream
        .tag_template
        .as_ref()
        .map(|t| t.replace("{version}", version).replace("{vsn}", version))
        .unwrap_or_else(|| version.to_string())
}

/// Candidate tag names to look up a release for `version` (tries the tag
/// template first, then the bare version and common prefixes).
fn candidate_tags(upstream: &UpstreamConfig, version: &str) -> Vec<String> {
    let mut tags = vec![tag_for_version(upstream, version), version.to_string()];
    if !version.starts_with('v') {
        tags.push(format!("v{version}"));
    }
    if !version.starts_with("release-") {
        tags.push(format!("release-{version}"));
    }
    tags.dedup();
    tags
}

/// Fetch the release for `version` (by trying candidate tags), if one exists.
async fn find_release(
    github: &GitHub,
    upstream: &UpstreamConfig,
    owner: &str,
    repo_name: &str,
    version: &str,
) -> anyhow::Result<Option<Release>> {
    for tag in candidate_tags(upstream, version) {
        match github.release_by_tag(owner, repo_name, &tag).await {
            Ok(rel) => return Ok(Some(rel)),
            Err(_) => continue,
        }
    }
    Ok(None)
}

/// Resolve which archive to use for the release: the user's explicit URL
/// template, a best-matching release asset, or the GitHub source tarball of
/// the tag.
fn resolve_source_archive(
    upstream: &UpstreamConfig,
    release: Option<&Release>,
    owner: &str,
    repo_name: &str,
    package: &str,
    version: &str,
) -> SourceArchive {
    let tag = tag_for_version(upstream, version);

    // 1. An explicit full URL template wins over everything.
    if let Some(template) = &upstream.archive_template {
        let url = template
            .replace("{version}", version)
            .replace("{vsn}", version)
            .replace("{tag}", &tag);
        let filename = upstream
            .archive_name_override()
            .map(|n| substitute_name(&n, package, version))
            .unwrap_or_else(|| url_basename(&url));
        return SourceArchive {
            download_url: url.clone(),
            src_uri: parameterize_url(&url, package, version, &tag),
            filename: filename.clone(),
            extract_dir: derive_extract_dir(
                upstream,
                package,
                version,
                &filename,
                &url,
                Some(repo_name),
                &tag,
            ),
        };
    }

    // 2. Pick the best release asset (an `archive-name` override narrows the
    //    search to that exact name).
    if let Some(rel) = release {
        if let Some(asset) = pick_asset(
            &rel.assets,
            package,
            version,
            upstream.archive_name_override().as_deref(),
        ) {
            tracing::debug!(asset = %asset.name, "using release asset as source archive");
            return SourceArchive {
                download_url: asset.browser_download_url.clone(),
                src_uri: parameterize_url(&asset.browser_download_url, package, version, &tag),
                filename: asset.name.clone(),
                extract_dir: derive_extract_dir(
                    upstream,
                    package,
                    version,
                    &asset.name,
                    &asset.browser_download_url,
                    Some(repo_name),
                    &tag,
                ),
            };
        }
    }

    // 3. Fall back to the GitHub source tarball of the tag.
    let fallback_url =
        format!("https://github.com/{owner}/{repo_name}/archive/refs/tags/{tag}.tar.gz");
    let filename = upstream
        .archive_name_override()
        .map(|n| substitute_name(&n, package, version))
        .unwrap_or_else(|| format!("{package}-{version}.tar.gz"));
    SourceArchive {
        download_url: fallback_url.clone(),
        src_uri: parameterize_url(&fallback_url, package, version, &tag),
        filename: filename.clone(),
        extract_dir: derive_extract_dir(
            upstream,
            package,
            version,
            &filename,
            &fallback_url,
            Some(repo_name),
            &tag,
        ),
    }
}

/// Choose the best source archive asset from a release's assets. Returns None
/// when no asset looks like a source tarball.
fn pick_asset<'a>(
    assets: &'a [ReleaseAsset],
    package: &str,
    version: &str,
    name_override: Option<&str>,
) -> Option<&'a ReleaseAsset> {
    let candidates: Vec<&ReleaseAsset> = assets
        .iter()
        .filter(|a| !is_checksum_or_meta(&a.name))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    if let Some(over) = name_override {
        let target = substitute_name(over, package, version);
        if let Some(a) = candidates.iter().find(|a| a.name == target) {
            return Some(a);
        }
    }
    candidates
        .iter()
        .max_by_key(|a| score_asset(&a.name, package, version))
        .copied()
        .filter(|a| score_asset(&a.name, package, version) > 0)
}

/// Score how likely an asset is the source tarball. Highest wins.
fn score_asset(name: &str, package: &str, version: &str) -> i32 {
    let n = name.to_ascii_lowercase();
    let exact = format!("{package}-{version}.tar.gz");
    if n == exact {
        return 100;
    }
    if !looks_like_source(name) || has_platform_marker(&n) {
        return 0;
    }
    if n.starts_with("source") || n.starts_with("src-") || n.starts_with("src_") {
        return 70;
    }
    let mut score = 25;
    let has_version = n.contains(&version.to_ascii_lowercase());
    let has_package = n.contains(&package.to_ascii_lowercase());
    if has_version {
        score += 20;
    }
    if has_package {
        score += 15;
    }
    score
}

/// Compiled binaries are nearly always named with a platform/target triplet;
/// such names can't be a source archive.
fn has_platform_marker(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "windows", "win32", "win64", "msvc", "mingw", "cygwin", "macos", "darwin", "apple",
        "linux", "musl", "gnu-", "gnu_", "x86_64", "x86-64", "amd64", "i686", "i386", "arm64",
        "aarch64", "armv7", "armhf", "ppc64", "powerpc", "s390x", "riscv", "mips", "android",
        "ios", "freebsd", "openbsd", "netbsd", ".exe", ".msi", ".dmg", ".apk", ".deb", ".rpm",
    ];
    MARKERS.iter().any(|m| name.contains(m))
}

/// Whether a filename looks like a source archive (as opposed to a binary
/// artifact, installer, or checksum).
fn looks_like_source(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n.ends_with(".tar.gz")
        || n.ends_with(".tgz")
        || n.ends_with(".tar.xz")
        || n.ends_with(".tar.bz2")
        || n.ends_with(".tar.zst")
        || n.ends_with(".tar.lz")
        || n.ends_with(".tar")
        || n.ends_with(".zip"))
        && !(n.contains("windows")
            || n.contains(".exe")
            || n.contains(".msi")
            || n.contains(".dmg")
            || n.contains("macos")
            || n.contains(".deb")
            || n.contains(".rpm")
            || n.contains(".apk"))
}

/// Checksums, signatures, and metadata files aren't source archives.
fn is_checksum_or_meta(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".sha256")
        || n.ends_with(".sha512")
        || n.ends_with(".sha1")
        || n.ends_with(".asc")
        || n.ends_with(".sig")
        || n.ends_with(".md")
        || n.ends_with(".txt")
        || n.ends_with(".json")
        || n.ends_with(".sums")
}

/// The the directory an archive extracts to (best effort): the basename minus
/// its archive extension(s).
fn archive_stem(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for ext in [
        ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz", ".tar", ".tgz", ".zip",
    ] {
        if lower.ends_with(ext) {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    name.to_string()
}

/// GitHub source tarballs (auto-generated) extract to `{repo}-{tag}`.
fn github_archive_extract_dir(repo_name: &str, tag: &str, url: &str) -> Option<String> {
    if url.contains(&format!("/archive/refs/tags/{tag}"))
        || url.contains(&format!("/archive/{tag}"))
    {
        Some(format!("{repo_name}-{tag}"))
    } else {
        None
    }
}

/// Determine the ebuild `S` override value, or None when the extracted
/// directory matches `${P}`. An explicit `s-dir` config wins.
fn derive_extract_dir(
    upstream: &UpstreamConfig,
    package: &str,
    version: &str,
    filename: &str,
    url: &str,
    repo_name: Option<&str>,
    tag: &str,
) -> Option<String> {
    if let Some(s) = &upstream.s_dir {
        return s_override_value(package, version, &substitute_name(s, package, version));
    }
    let dir = if let Some(repo) = repo_name {
        github_archive_extract_dir(repo, tag, url).unwrap_or_else(|| archive_stem(filename))
    } else {
        archive_stem(filename)
    };
    s_override_value(package, version, &dir)
}

/// Build the `S` value (`${WORKDIR}/...`), or None if `dir` equals `${P}`.
fn s_override_value(package: &str, version: &str, dir: &str) -> Option<String> {
    if dir == format!("{package}-{version}") {
        None
    } else {
        Some(format!(
            "${{WORKDIR}}/{}",
            parameterized_name_component(dir, package, version)
        ))
    }
}

/// Substitute `${PV}` (and `${P}` for a literal `{package}-{version}` run) into
/// a URL or directory name so the ebuild stays correct across bumps. Replacing
/// the version itself (rather than the tag string) keeps tag prefixes like `v`
/// intact: `refs/tags/v1.2.3.tar.gz` -> `refs/tags/v${PV}.tar.gz`.
fn parameterize_url(url: &str, package: &str, version: &str, _tag: &str) -> String {
    let mut s = url.replace(&format!("{package}-{version}"), "${P}");
    s = s.replace(version, "${PV}");
    s
}

/// Version/package parameterization for a bare name/path component.
fn parameterized_name_component(value: &str, package: &str, version: &str) -> String {
    value
        .replace(&format!("{package}-{version}"), "${P}")
        .replace(version, "${PV}")
}

/// Replace `{version}`/`{package}` templates with concrete values.
fn substitute_name(name: &str, package: &str, version: &str) -> String {
    name.replace("{version}", version)
        .replace("{package}", package)
}

/// The last path segment of a URL.
fn url_basename(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_string()
}

/// Detect whether the source archive contains a Rust `Cargo.toml`.
fn detect_cargo(archive_path: &Path) -> anyhow::Result<bool> {
    let output = Command::new("tar").arg("-tzf").arg(archive_path).output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    Ok(listing
        .lines()
        .any(|line| line.ends_with("/Cargo.toml") || line == "Cargo.toml"))
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
    s_override: Option<&str>,
    is_cargo: bool,
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
        s: s_override.map(|s| s.to_string()),
    };

    let mut out = String::new();
    out.push_str("# Copyright 1999-2026 Gentoo Authors\n");
    out.push_str("# Distributed under the terms of the GNU General Public License v2\n\n");
    out.push_str(&format!("EAPI={}\n\n", meta.eapi.as_deref().unwrap_or("8")));
    if is_cargo {
        out.push_str("inherit cargo\n\n");
    }
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
    if let Some(s) = &meta.s {
        push_var(&mut out, "S", s);
    }
    if let Some(i) = &meta.iuse {
        push_var(&mut out, "IUSE", i);
    }
    out.push('\n');
    if is_cargo && meta.depend.is_none() {
        out.push_str("DEPEND=\"dev-lang/rust:=\"\n\n");
    } else {
        if let Some(d) = &meta.depend {
            push_var(&mut out, "DEPEND", d);
        }
    }
    if let Some(r) = &meta.rdepend {
        push_var(&mut out, "RDEPEND", r);
    }
    if let Some(b) = &meta.bdepend {
        push_var(&mut out, "BDEPEND", b);
    }
    out.push('\n');
    if !is_cargo {
        out.push_str("src_install() {\n\tdefault\n}\n");
    }
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

/// Ebuild variables that `gentooit` (re)generates and rewrites on a bump.
/// Everything else in an existing ebuild is treated as user content and
/// preserved verbatim.
const MANAGED_VARS: [&str; 12] = [
    "EAPI",
    "DESCRIPTION",
    "HOMEPAGE",
    "SRC_URI",
    "LICENSE",
    "SLOT",
    "KEYWORDS",
    "S",
    "IUSE",
    "DEPEND",
    "RDEPEND",
    "BDEPEND",
];

/// If `line` assigns one of the managed ebuild variables, return its name.
fn managed_var_name(line: &str) -> Option<&'static str> {
    MANAGED_VARS
        .iter()
        .find(|v| {
            line.strip_prefix(*v)
                .is_some_and(|rest| rest.starts_with('='))
        })
        .copied()
}

/// The stock Gentoo header lines, re-emitted from the fresh render.
fn is_ebuild_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("# Copyright") || t.starts_with("# Distributed under the terms of")
}

/// Merge a freshly rendered (generated) ebuild with an existing one, keeping
/// the managed variables from the fresh render while preserving any custom
/// content from the old file (hand-written `src_*` functions, `RESTRICT`,
/// `QA_*`, comments, etc.).
fn render_diff_bump(old_content: &str, fresh_content: &str) -> String {
    let fresh_lines: Vec<&str> = fresh_content.lines().collect();
    let last_managed = fresh_lines
        .iter()
        .rposition(|l| managed_var_name(l).is_some())
        .unwrap_or_else(|| fresh_lines.len().saturating_sub(1));

    // Head: everything in the fresh render up to its last managed variable
    // (header comments, the variable block, and its formatting).
    let head = &fresh_lines[..=last_managed];

    // Tail: the old ebuild's lines that aren't a regenerated variable or the
    // stock header.
    let mut tail: Vec<&str> = Vec::new();
    for line in old_content.lines() {
        if managed_var_name(line).is_some() || is_ebuild_header(line) {
            continue;
        }
        tail.push(line);
    }
    while tail.first().is_some_and(|l| l.trim().is_empty()) {
        tail.remove(0);
    }
    while tail.last().is_some_and(|l| l.trim().is_empty()) {
        tail.pop();
    }

    let mut out = head.join("\n");
    if !tail.is_empty() {
        out.push_str("\n\n");
        out.push_str(&tail.join("\n"));
    }
    out.push('\n');
    out
}

/// Ebuilds already present for a package, as (version, content) pairs.
fn existing_ebuilds(pkg_dir: &Path) -> Vec<(String, String)> {
    let entries = match std::fs::read_dir(pkg_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".ebuild") else {
            continue;
        };
        // Ebuild names are `{package}-{version}.ebuild`; package names can't
        // contain `-`, so the version is everything after the first dash.
        let Some((_, version)) = stem.split_once('-') else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            out.push((version.to_string(), content));
        }
    }
    out
}

/// Pick the existing ebuild to diff-bump: the newest version at or below the
/// proposed version (or the newest overall if only later versions exist).
fn pick_base_ebuild(existing: &[(String, String)], new_version: &str) -> Option<String> {
    existing
        .iter()
        .filter(|(ver, _)| compare_versions(ver, new_version) != Ordering::Greater)
        .max_by(|a, b| compare_versions(&a.0, &b.0))
        .or_else(|| existing.iter().max_by(|a, b| compare_versions(&a.0, &b.0)))
        .map(|(_, c)| c.clone())
}

/// Approximate Gentoo version ordering: split on `.`, `-`, `_`, `+` and compare
/// pieces numerically when possible, lexically otherwise.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let ta = version_tokens(a);
    let tb = version_tokens(b);
    for (x, y) in ta.iter().zip(tb.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(xn), Ok(yn)) => xn.cmp(&yn),
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Ok(_)) => Ordering::Less,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ta.len().cmp(&tb.len())
}

fn version_tokens(v: &str) -> Vec<String> {
    v.split(['.', '-', '_', '+'])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
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
    no_qa: bool,
    username: &str,
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

    // Diff-bump: when an older ebuild for this package already exists in the
    // working tree, preserve its custom content (hand-written `src_*`
    // functions, `RESTRICT`/`QA_*` variables, comments) instead of starting
    // from a blank slate. Only the managed variables track the fresh render.
    let existing = existing_ebuilds(&repo.path.join(&pkg_dir));
    let base_ebuild = pick_base_ebuild(&existing, version);
    let (is_bump, ebuild_content) = match base_ebuild {
        Some(old) => {
            tracing::info!(pkg = %pkg_dir, "diff-bumping existing ebuild, preserving custom bits");
            (true, render_diff_bump(&old, ebuild_content))
        }
        None => (false, ebuild_content.to_string()),
    };

    // Write files into the working tree.
    repo.write_file(&ebuild_rel, &ebuild_content)?;
    repo.write_file(&metadata_rel, metadata_content)?;
    repo.write_file(&manifest_rel, &manifest.to_string_sorted())?;

    let qa_summary = if !no_qa {
        let pkg_dir_path = repo.path.join(&pkg_dir);
        let mut lines = Vec::new();

        match pkgdev_manifest(&pkg_dir_path) {
            Ok(report) => {
                if report.success {
                    lines.push("pkgdev manifest: passed".to_string());
                } else {
                    tracing::warn!("pkgdev manifest failed:\n{}", report.output);
                    lines.push(format!(
                        "pkgdev manifest: failed (exit {})",
                        report.exit_code
                    ));
                }
            }
            Err(BuildError::ToolNotFound { .. }) => {}
            Err(e) => {
                tracing::warn!("pkgdev manifest error: {e}");
                lines.push(format!("pkgdev manifest: {e}"));
            }
        }

        match pkgcheck_scan(&pkg_dir_path) {
            Ok(report) => {
                if report.success {
                    lines.push("pkgcheck scan: passed".to_string());
                } else {
                    tracing::warn!("pkgcheck scan failed:\n{}", report.output);
                    lines.push(format!("pkgcheck scan: failed (exit {})", report.exit_code));
                }
            }
            Err(BuildError::ToolNotFound { .. }) => {}
            Err(e) => {
                tracing::warn!("pkgcheck scan error: {e}");
                lines.push(format!("pkgcheck scan: {e}"));
            }
        }

        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    } else {
        None
    };

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
    let msg = if is_bump {
        format!("{pkg_dir}: bump to version {version}")
    } else {
        format!("{pkg_dir}: add version {version}")
    };
    repo.commit(&author_name, &author_email, &msg)?;

    // Push the feature branch to the downstream remote, then open a PR. The
    // push may fail for local-path remotes that cannot authenticate or for
    // read-only check-outs; report it but do not fail the whole run.
    let push_token = if let Some(tok) = &user.github_token {
        Some(tok.clone())
    } else if user.github_app_id.is_some() && user.github_app_key.is_some() {
        match github.installation_token().await {
            Ok(Some(tok)) => Some(tok),
            _ => None,
        }
    } else {
        None
    };
    match repo.push("origin", &branch, push_token.as_deref()) {
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
                    &msg,
                    &format!("{}:{branch}", fork_owner(username)),
                    &base_branch,
                    &pr_body(atom, version, project, qa_summary.as_deref()),
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
        qa_summary,
    })
}

fn fork_owner(username: &str) -> String {
    username.to_string()
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

fn pr_body(
    atom: &Atom,
    version: &str,
    project: &ProjectConfig,
    qa_summary: Option<&str>,
) -> String {
    let upstream = project
        .upstream
        .as_ref()
        .and_then(|u| u.upstream.clone())
        .unwrap_or_default();
    let mut body = format!(
        "Automated by gentooit.\n\n\
         Bump **{atom}** to version **{version}**.\n\
         - Upstream: {upstream}\n\
         - Updated ebuild, metadata.xml, and Manifest."
    );
    if let Some(qa) = qa_summary {
        body.push('\n');
        body.push_str("- QA:\n");
        for line in qa.lines() {
            body.push_str("  - ");
            body.push_str(line);
            body.push('\n');
        }
    }
    body
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
            None,
            false,
        );
        assert!(content.contains("EAPI=8"));
        assert!(content.contains("DESCRIPTION="));
        assert!(content.contains("SRC_URI="));
        assert!(content.contains("KEYWORDS="));
        assert!(
            !content.lines().any(|l| l.starts_with("S=")),
            "no S override expected"
        );
        assert!(
            content.contains("src_install() {"),
            "non-cargo ebuild should have src_install"
        );
    }

    #[test]
    fn render_ebuild_s_override() {
        let project = ProjectConfig::default();
        let content = render_ebuild(
            PackageName::new("foo").unwrap(),
            "1.0.0",
            &project,
            "foo",
            "https://example.com/download/deps-${PV}.tar.gz",
            Some("${WORKDIR}/deps-${PV}"),
            false,
        );
        assert!(content.contains("S=\"${WORKDIR}/deps-${PV}\""));
    }

    #[test]
    fn render_ebuild_cargo_eclass() {
        let project = ProjectConfig::default();
        let content = render_ebuild(
            PackageName::new("ripgrep").unwrap(),
            "15.2.0",
            &project,
            "ripgrep",
            "https://github.com/BurntSushi/ripgrep/releases/download/${PV}/${P}.tar.gz",
            None,
            true,
        );
        assert!(
            content.contains("inherit cargo"),
            "cargo ebuild should inherit cargo"
        );
        assert!(
            content.contains("DEPEND=\"dev-lang/rust:=\""),
            "cargo ebuild should depend on rust"
        );
        assert!(
            !content.lines().any(|l| l.starts_with("src_install")),
            "cargo ebuild should not have custom src_install"
        );
    }

    #[test]
    fn archive_matching_p_has_no_s_override() {
        assert_eq!(
            s_override_value("foo", "1.2.3", "foo-1.2.3").as_deref(),
            None
        );
    }

    #[test]
    fn archive_not_matching_p_needs_s_override() {
        assert_eq!(
            s_override_value("foo", "1.2.3", "foo-v1.2.3-extra").as_deref(),
            Some("${WORKDIR}/foo-v${PV}-extra")
        );
    }

    #[test]
    fn github_tarball_extract_dir() {
        assert_eq!(
            github_archive_extract_dir(
                "ripgrep",
                "v15.2.0",
                "https://github.com/BurntSushi/ripgrep/archive/refs/tags/v15.2.0.tar.gz"
            )
            .as_deref(),
            Some("ripgrep-v15.2.0")
        );
    }

    #[test]
    fn pick_asset_prefers_exact_match() {
        let assets = [
            ReleaseAsset {
                name: "grep".to_string(),
                browser_download_url: "https://example.com/grep".to_string(),
            },
            ReleaseAsset {
                name: "foo-1.2.3.tar.gz".to_string(),
                browser_download_url: "https://example.com/foo-1.2.3.tar.gz".to_string(),
            },
            ReleaseAsset {
                name: "foo-1.2.3-x86_64-linux.tar.gz".to_string(),
                browser_download_url: "https://example.com/alt.tar.gz".to_string(),
            },
        ];
        let chosen = pick_asset(&assets, "foo", "1.2.3", None).unwrap();
        assert_eq!(chosen.name, "foo-1.2.3.tar.gz");
    }

    #[test]
    fn pick_asset_falls_back_to_best_non_exact() {
        let assets = [
            ReleaseAsset {
                name: "foo-1.2.3.zip".to_string(),
                browser_download_url: "https://example.com/a.zip".to_string(),
            },
            ReleaseAsset {
                name: "checksums.txt".to_string(),
                browser_download_url: "https://example.com/c.txt".to_string(),
            },
            ReleaseAsset {
                name: "foo-1.2.3-x86_64-unknown-linux-gnu.tar.gz".to_string(),
                browser_download_url: "https://example.com/bin.tar.gz".to_string(),
            },
            ReleaseAsset {
                name: "source.tar.gz".to_string(),
                browser_download_url: "https://example.com/source.tar.gz".to_string(),
            },
        ];
        let chosen = pick_asset(&assets, "foo", "1.2.3", None).unwrap();
        assert_eq!(chosen.name, "source.tar.gz");
    }

    #[test]
    fn pick_asset_never_selects_binary() {
        let assets = vec![ReleaseAsset {
            name: "foo-1.2.3-x86_64-unknown-linux-gnu.tar.gz".to_string(),
            browser_download_url: "https://example.com/bin.tar.gz".to_string(),
        }];
        assert!(pick_asset(&assets, "foo", "1.2.3", None).is_none());
    }

    #[test]
    fn resolve_prefers_asset_over_tarball() {
        let upstream = UpstreamConfig {
            tag_template: Some("{version}".to_string()),
            ..Default::default()
        };
        let release = Release {
            tag_name: "1.2.3".to_string(),
            name: Some("1.2.3".to_string()),
            body: None,
            assets: vec![ReleaseAsset {
                name: "foo-1.2.3-src.tar.gz".to_string(),
                browser_download_url:
                    "https://github.com/o/r/releases/download/1.2.3/foo-1.2.3-src.tar.gz"
                        .to_string(),
            }],
            tarball_url: Some("https://github.com/o/r/archive/refs/tags/1.2.3.tar.gz".to_string()),
        };
        let arch = resolve_source_archive(&upstream, Some(&release), "o", "r", "foo", "1.2.3");
        assert_eq!(arch.filename, "foo-1.2.3-src.tar.gz");
        assert_eq!(
            arch.src_uri,
            "https://github.com/o/r/releases/download/${PV}/${P}-src.tar.gz"
        );
        assert_eq!(arch.extract_dir.as_deref(), Some("${WORKDIR}/${P}-src"));
    }

    #[test]
    fn resolve_falls_back_to_github_tarball() {
        let upstream = UpstreamConfig {
            tag_template: Some("v{version}".to_string()),
            ..Default::default()
        };
        let arch = resolve_source_archive(&upstream, None, "o", "foo", "foo", "1.2.3");
        assert_eq!(
            arch.download_url,
            "https://github.com/o/foo/archive/refs/tags/v1.2.3.tar.gz"
        );
        assert_eq!(arch.filename, "foo-1.2.3.tar.gz");
        assert_eq!(
            arch.src_uri,
            "https://github.com/o/foo/archive/refs/tags/v${PV}.tar.gz"
        );
        assert_eq!(arch.extract_dir.as_deref(), Some("${WORKDIR}/foo-v${PV}"));
    }

    #[test]
    fn resolve_p_style_asset_needs_no_s() {
        let upstream = UpstreamConfig::default();
        let release = Release {
            tag_name: "1.2.3".to_string(),
            name: None,
            body: None,
            assets: vec![ReleaseAsset {
                name: "foo-1.2.3.tar.gz".to_string(),
                browser_download_url:
                    "https://github.com/o/r/releases/download/1.2.3/foo-1.2.3.tar.gz".to_string(),
            }],
            tarball_url: None,
        };
        let arch = resolve_source_archive(&upstream, Some(&release), "o", "r", "foo", "1.2.3");
        assert_eq!(arch.filename, "foo-1.2.3.tar.gz");
        assert_eq!(
            arch.src_uri,
            "https://github.com/o/r/releases/download/${PV}/${P}.tar.gz"
        );
        assert_eq!(arch.extract_dir, None);
    }

    #[test]
    fn metadata_uses_remote_id_type_override() {
        let project = ProjectConfig {
            package: Some(crate::config::PackageConfig {
                remote_id_type: Some("crates-io".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let user = UserConfig::default();
        let md = render_metadata(&project, &user, &UpstreamConfig::default(), "alice", "foo");
        assert!(md.contains("<remote-id type=\"crates-io\">alice/foo</remote-id>"));
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

    #[test]
    fn render_diff_bump_preserves_custom_functions() {
        let old = "\
# Copyright 1999-2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

DESCRIPTION=\"old\"
HOMEPAGE=\"https://old\"
SRC_URI=\"https://example.com/download/${PV}/old-${PV}.tar.gz\"

LICENSE=\"old\"
SLOT=\"0\"
KEYWORDS=\"~amd64\"
S=\"${WORKDIR}/old-${PV}\"

RESTRICT=\"test\"

src_prepare() {
\tsed -i 's/hello/world/' main.c || die
}

src_install() {
\tdefault
}
";
        let fresh = render_ebuild(
            PackageName::new("foo").unwrap(),
            "2.0.0",
            &ProjectConfig::default(),
            "foo",
            "https://example.com/download/${PV}/foo-${PV}.tar.gz",
            None,
            false,
        );
        let merged = render_diff_bump(old, &fresh);

        // Managed variables track the fresh render.
        assert!(merged.contains("DESCRIPTION=\"foo - packaged by gentooit\""));
        assert!(merged.contains("SRC_URI=\"https://example.com/download/${PV}/foo-${PV}.tar.gz\""));
        assert!(merged.contains("KEYWORDS=\"~amd64\""));
        // Stale managed variable from the old version is dropped (fresh has no S).
        assert!(!merged.contains("old-${PV}"));
        assert!(
            !merged.lines().any(|l| l.starts_with("S=")),
            "stale S should be dropped"
        );
        // Custom content survives.
        assert!(merged.contains("RESTRICT=\"test\""));
        assert!(merged.contains("src_prepare() {"));
        assert!(merged.contains("sed -i 's/hello/world/' main.c || die"));
        assert!(merged.trim_end().ends_with("src_install() {\n\tdefault\n}"));
    }

    #[test]
    fn render_diff_bump_adds_s_from_fresh() {
        let old = "\
# Copyright 1999-2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8
DESCRIPTION=\"d\"
SRC_URI=\"https://example.com/${PV}/deps-${PV}.tar.gz\"

LICENSE=\"MIT\"
SLOT=\"0\"

# Keep this comment
src_compile() {
\tcargo build
}
";
        let fresh = render_ebuild(
            PackageName::new("foo").unwrap(),
            "1.0.0",
            &ProjectConfig::default(),
            "foo",
            "https://example.com/${PV}/source.tar.gz",
            Some("${WORKDIR}/source"),
            false,
        );
        let merged = render_diff_bump(old, &fresh);

        // The fresh render's S override is inserted.
        assert!(merged.contains("S=\"${WORKDIR}/source\""));
        // Custom comment and function are kept.
        assert!(merged.contains("# Keep this comment"));
        assert!(merged.contains("cargo build"));
        // The fresh render's own src_install is not duplicated (old has none).
        assert_eq!(merged.matches("src_install").count(), 0);
    }

    #[test]
    fn compare_versions_orders_correctly() {
        assert_eq!(compare_versions("1.0.0", "1.1.0"), Ordering::Less);
        assert_eq!(compare_versions("1.1.0", "1.1.0-r1"), Ordering::Less);
        assert_eq!(compare_versions("1.1.0-r1", "1.1.0-r2"), Ordering::Less);
        assert_eq!(compare_versions("0.16.5", "0.16.6"), Ordering::Less);
        assert_eq!(compare_versions("9.0.0", "10.0.0"), Ordering::Less);
        assert_eq!(compare_versions("2.0.0", "2.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.5.0", "1.0.0"), Ordering::Greater);
    }

    #[test]
    fn pick_base_ebuild_selects_newest_below_or_equal() {
        let existing = vec![
            ("1.0.0".to_string(), "c1".to_string()),
            ("1.5.0".to_string(), "c2".to_string()),
            ("2.0.0".to_string(), "c3".to_string()),
        ];
        assert_eq!(pick_base_ebuild(&existing, "1.9.9").as_deref(), Some("c2"));
        // Equal version bumps in place.
        assert_eq!(pick_base_ebuild(&existing, "2.0.0").as_deref(), Some("c3"));
        // Only newer versions exist: fall back to the newest overall.
        assert_eq!(pick_base_ebuild(&existing, "0.9.0").as_deref(), Some("c3"));
    }

    #[test]
    fn existing_ebuilds_scans_only_ebuild_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("foo-1.0.0.ebuild"), "EAPI=8").unwrap();
        std::fs::write(dir.path().join("foo-1.1.0.ebuild"), "EAPI=8").unwrap();
        std::fs::write(dir.path().join("Manifest"), "DIST x 1").unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        let found = existing_ebuilds(dir.path());
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|(v, _)| v == "1.0.0"));
        assert!(found.iter().any(|(v, _)| v == "1.1.0"));
    }
}
