//! `adopt` workflow: import an existing Gentoo package (ebuild, Manifest,
//! metadata.xml, and `files/` patches) into the local overlay.
//!
//! This covers packages that the naive generator cannot render because they
//! need a complex eclass (e.g. `zig`), bundled dependencies, or patch sets —
//! the kind of ebuild gentooit should vendor as-is rather than re-synthesize.
//! `gentooit adopt --atom x11-terms/ghostty` copies the whole package tree
//! from a Gentoo checkout (default `[DATADIR]/gentoo` or `/var/db/repos/gentoo`)
//! into `<package-dir>/<category>/<package>`, and writes a `.gentooit/<pkg>.yaml`
//! config so the package is pinned, verified, and diff-bumped like any other.

use std::path::{Path, PathBuf};

use crate::config::{DownstreamConfig, PackageConfig, ProjectConfig, UpstreamConfig};
use crate::ebuild::Atom;
use walkdir::WalkDir;

/// A parsed, adoptable package tree from the Gentoo tree.
struct SourcePackage {
    atom: Atom,
    ebuilds: Vec<(String, String)>, // (pkg-version filename, content)
    manifest: Option<String>,
    metadata_xml: Option<String>,
    patches: Vec<(PathBuf, Vec<u8>)>, // paths relative to the package dir
}

/// What `gentooit adopt` did.
#[derive(Debug, Clone)]
pub struct AdoptReport {
    pub atom: String,
    pub version: Option<String>,
    /// Absolute destination of the copied package directory.
    pub destination: PathBuf,
    /// Copied ebuild filenames.
    pub ebuilds: Vec<String>,
    /// Non-ebuild files copied (`Manifest`, `metadata.xml`, `files/...`).
    pub extra_files: Vec<String>,
    /// Path of the generated `.gentooit/<pkg>.yaml`, if written.
    pub config_path: Option<PathBuf>,
    /// The upstream archive template derived from the ebuild's `SRC_URI`.
    pub archive_template: Option<String>,
}

/// Adopt a package from `tree` into `dest_root`. `version` filters to a single
/// version (and pins the generated config to it); without it, all ebuild
/// versions are copied and the newest is pinned in the config. `project`
/// supplies the downstream `package-dir` prefix and package metadata defaults
/// when present.
pub fn adopt_package(
    atom_str: &str,
    version: Option<&str>,
    tree: &Path,
    dest_root: &Path,
    project: Option<&ProjectConfig>,
) -> anyhow::Result<AdoptReport> {
    let atom =
        Atom::parse(atom_str).map_err(|e| anyhow::anyhow!("invalid atom `{atom_str}`: {e}"))?;

    let src = tree.join(&atom.category.name).join(&atom.package.name);
    if !src.is_dir() {
        anyhow::bail!("no package `{atom}` in Gentoo tree `{}`", tree.display());
    }

    let source = read_source_package(&src, &atom, version)?;
    let chosen_version = newest_version(&source.ebuilds)
        .ok_or_else(|| anyhow::anyhow!("no ebuilds found for `{atom}`"))?;

    // Destination honors the downstream `package-dir` nesting (e.g. `ebuilds/`).
    let prefix = project
        .and_then(|p| p.downstream.first())
        .and_then(|d| d.package_dir.clone())
        .unwrap_or_default();
    let prefix = prefix.trim_matches('/');
    let dest = dest_root
        .join(prefix)
        .join(&atom.category.name)
        .join(&atom.package.name);
    std::fs::create_dir_all(&dest)?;

    let mut report = AdoptReport {
        atom: atom.full(),
        version: version
            .map(str::to_string)
            .or_else(|| Some(chosen_version.clone())),
        destination: dest.clone(),
        ebuilds: Vec::new(),
        extra_files: Vec::new(),
        config_path: None,
        archive_template: None,
    };

    // Copy ebuilds (version-filtered).
    for (filename, content) in &source.ebuilds {
        std::fs::write(dest.join(filename), content)?;
        report.ebuilds.push(filename.clone());
    }

    // Copy Manifest / metadata.xml / files/ wholesale — never re-synthesized.
    if let Some(m) = &source.manifest {
        std::fs::write(dest.join("Manifest"), m)?;
        report.extra_files.push("Manifest".to_string());
    }
    if let Some(md) = &source.metadata_xml {
        std::fs::write(dest.join("metadata.xml"), md)?;
        report.extra_files.push("metadata.xml".to_string());
    }
    for (rel, bytes) in &source.patches {
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, bytes)?;
        report.extra_files.push(rel.display().to_string());
    }

    // Write a `.gentooit/<pkg>.yaml` pin when one doesn't exist yet.
    let chosen = source
        .ebuilds
        .iter()
        .find(|(f, _)| {
            f.strip_suffix(".ebuild")
                .and_then(|s| s.split_once('-'))
                .map(|(_, v)| v)
                == Some(chosen_version.as_str())
        })
        .map(|(_, c)| c.as_str())
        .unwrap_or("");
    let config_path = dest_root
        .join(".gentooit")
        .join(format!("{}.yaml", atom.package.name));
    if !config_path.exists() {
        let cfg = render_adopt_config(&atom, &chosen_version, chosen, &source, project);
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(&cfg)?;
        std::fs::write(&config_path, yaml)?;
        report.config_path = Some(config_path);
        report.archive_template = source.archive_template();
    }

    Ok(report)
}

/// Collect everything gentooit copies, reading from the tree.
fn read_source_package(
    src: &Path,
    atom: &Atom,
    version: Option<&str>,
) -> anyhow::Result<SourcePackage> {
    let mut pkg = SourcePackage {
        atom: atom.clone(),
        ebuilds: Vec::new(),
        manifest: None,
        metadata_xml: None,
        patches: Vec::new(),
    };

    for entry in std::fs::read_dir(src)? {
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if name.ends_with(".ebuild") && ebuild_matches_version(&name, atom, version) {
            pkg.ebuilds
                .push((name.clone(), std::fs::read_to_string(&path)?));
        } else if name == "Manifest" {
            pkg.manifest = Some(std::fs::read_to_string(&path)?);
        } else if name == "metadata.xml" {
            pkg.metadata_xml = Some(std::fs::read_to_string(&path)?);
        }
    }

    let files_dir = src.join("files");
    if files_dir.is_dir() {
        for entry in WalkDir::new(&files_dir) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(src)?;
                let bytes = std::fs::read(entry.path())?;
                pkg.patches.push((rel.to_path_buf(), bytes));
            }
        }
    }

    if pkg.ebuilds.is_empty() {
        anyhow::bail!(
            "no ebuilds for `{}` in `{}`{}",
            atom.full(),
            src.display(),
            version
                .map(|v| format!(" matching version {v}"))
                .unwrap_or_default()
        );
    }
    pkg.ebuilds.sort();
    Ok(pkg)
}

/// Whether an ebuild filename matches the atom and an optional version filter.
fn ebuild_matches_version(name: &str, atom: &Atom, version: Option<&str>) -> bool {
    let Some(stem) = name.strip_suffix(".ebuild") else {
        return false;
    };
    // Atom package names can't contain `-`, so the version is after the first
    // `-` (rev/alpha suffixes included).
    let Some((pn, pv)) = stem.split_once('-') else {
        return false;
    };
    pn == atom.package.name
        && version.is_none_or(|v| pv == v || pv.strip_suffix("-r0").unwrap_or(pv) == v)
}

/// Newest version among the copied ebuilds (numeric-token comparison).
fn newest_version(pairs: &[(String, String)]) -> Option<String> {
    pairs
        .iter()
        .filter_map(|(f, _)| f.strip_suffix(".ebuild")?.split_once('-').map(|(_, v)| v))
        .max_by(|a, b| compare_simple(a, b))
        .map(str::to_string)
}

fn compare_simple(a: &str, b: &str) -> std::cmp::Ordering {
    let ta: Vec<&str> = a.split(['.', '-', '_', '+']).collect();
    let tb: Vec<&str> = b.split(['.', '-', '_', '+']).collect();
    for (x, y) in ta.iter().zip(tb.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(xn), Ok(yn)) => xn.cmp(&yn),
            (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
            (Err(_), Ok(_)) => std::cmp::Ordering::Less,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    ta.len().cmp(&tb.len())
}

impl SourcePackage {
    /// Derive the single primary archive URL template from `SRC_URI`, skipping
    /// variable bundles like `${ZBS_DEPENDENCIES_SRC_URI}`, and parameterize
    /// `${P}`/`${PV}` to the `{version}` form gentooit's tooling understands.
    fn archive_template(&self) -> Option<String> {
        let ebuild = self.ebuilds.last().map(|(_, c)| c.as_str())?;
        let block = raw_srcuri_block(ebuild)?;
        // First HTTP(S) URL, skipping generated bundle placeholders.
        let first = block
            .split_whitespace()
            .find(|t| t.starts_with("http") && !t.contains("ZBS_DEPENDENCIES"))?;
        let package = self.atom.package.name.clone();
        Some(
            first
                .replace("${PV}", "{version}")
                .replace("${P}", &format!("{package}-{{version}}"))
                .replace("{version}-{version}", "{version}"),
        )
    }
}

/// Extract the raw quoted `SRC_URI="..."` value, joining continuation lines
/// (then / `${PV}` bundle lines) until the quote balance closes. Returns
/// `None` when an ebuild has no SRC_URI assignment.
fn raw_srcuri_block(ebuild: &str) -> Option<String> {
    let lines: Vec<&str> = ebuild.lines().collect();
    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let Some(value_start) = line.strip_prefix("SRC_URI=") else {
            continue;
        };
        let mut value = value_start.trim().to_string();
        if value.is_empty() {
            return Some(String::new());
        }
        // Join continuation lines while the quote count is unbalanced.
        let mut guard = 0;
        while value.matches('"').count() % 2 == 1 && idx + 1 + guard < lines.len() && guard < 200 {
            guard += 1;
            let next = lines[idx + guard].trim();
            if next.is_empty() || next.starts_with('#') {
                continue;
            }
            value.push(' ');
            value.push_str(next);
        }
        // Clip everything after the closing quote.
        let trimmed = value.trim_matches('"').trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Build the `.gentooit/<pkg>.yaml` contents for the adopted package. Any
/// fields the project config supplies (entry point is in `project`) win;
/// everything else is derived from the imported ebuild/metadata.
fn render_adopt_config(
    atom: &Atom,
    version: &str,
    ebuild: &str,
    source: &SourcePackage,
    project: Option<&ProjectConfig>,
) -> ProjectConfig {
    let meta = crate::ebuild::extract_variables(ebuild).ok();

    // Upstream identity: prefer the metadata.xml remote-id, else the atom's
    // category as a best-effort name.
    let (rid_type, rid_id) = source
        .metadata_xml
        .as_deref()
        .and_then(|xml| crate::metadata::PackageMetadata::parse_xml(xml).ok())
        .and_then(|md| {
            md.remote_ids
                .first()
                .map(|r| (r.r#type.clone(), r.id.clone()))
        })
        .unwrap_or_else(|| ("github".to_string(), atom.package.name.clone()));

    let upstream = UpstreamConfig {
        vcs: Some(rid_type),
        upstream: Some(rid_id.clone()),
        package_name: Some(atom.package.name.clone()),
        version: Some(version.to_string()),
        tag_template: Some("v{version}".to_string()),
        archive_template: source.archive_template(),
        ..Default::default()
    };

    let maintainer_email = project
        .and_then(|p| p.package.as_ref())
        .and_then(|p| p.maintainer_email.clone());
    let maintainer_name = project
        .and_then(|p| p.package.as_ref())
        .and_then(|p| p.maintainer_name.clone());

    let package = PackageConfig {
        description: meta
            .as_ref()
            .and_then(|m| m.description.clone())
            .or_else(|| {
                Some(format!(
                    "{} - adopted from the Gentoo tree",
                    atom.package.name
                ))
            }),
        homepage: meta.as_ref().and_then(|m| m.homepage.clone()),
        license: meta.as_ref().and_then(|m| m.license.clone()),
        slot: meta.as_ref().and_then(|m| m.slot.clone()),
        keywords: meta.as_ref().and_then(|m| m.keywords.clone()),
        maintainer_email,
        maintainer_name,
        ..Default::default()
    };

    let downstream = match project.and_then(|p| p.downstream.first()) {
        Some(d) => vec![d.clone()],
        None => vec![DownstreamConfig::default()],
    };

    ProjectConfig {
        spec_version: Some("1.0".to_string()),
        upstream: Some(upstream),
        package: Some(package),
        downstream,
        open_pull_request: Some(false),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tree(dir: &Path, content: &[(&str, &str)]) {
        for (rel, data) in content {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, data).unwrap();
        }
    }

    #[test]
    fn adopts_full_tree_with_files_and_config() {
        let tree = tempfile::tempdir().unwrap();
        write_tree(
            tree.path(),
            &[
                (
                    "x11-terms/ghostty/ghostty-1.3.1.ebuild",
                    "# Copyright 1999-2026 Gentoo Authors\nEAPI=8\n\
                     DESCRIPTION=\"Terminal emulator\"\n\
                     HOMEPAGE=\"https://ghostty.org\"\n\
                     SRC_URI=\"https://release.files.ghostty.org/${PV}/ghostty-${PV}.tar.gz\"\n\n\
                     LICENSE=\"MIT\"\nSLOT=\"0\"\nKEYWORDS=\"~amd64\"\n\n\
                     src_install() { default }\n",
                ),
                ("x11-terms/ghostty/Manifest", "DIST ghostty-1.3.1.tar.gz 10 SHA256 deadbeef"),
                (
                    "x11-terms/ghostty/metadata.xml",
                    "<?xml version=\"1.0\"?>\n<pkgmetadata>\n<upstream><remote-id type=\"github\">ghostty-org/ghostty</remote-id></upstream>\n</pkgmetadata>\n",
                ),
                (
                    "x11-terms/ghostty/files/ghostty-1.3.0.patch",
                    "--- a/foo\n+++ b/foo\n",
                ),
            ],
        );

        let dest = tempfile::tempdir().unwrap();
        let project = ProjectConfig {
            package: Some(PackageConfig {
                maintainer_email: Some("dev@example.com".to_string()),
                maintainer_name: Some("Dev".to_string()),
                ..Default::default()
            }),
            downstream: vec![DownstreamConfig {
                url: "git@github.com:me/overlay.git".to_string(),
                branch: Some("main".to_string()),
                category: Some("x11-terms".to_string()),
                package_dir: Some("ebuilds".to_string()),
            }],
            ..Default::default()
        };

        let report = adopt_package(
            "x11-terms/ghostty",
            Some("1.3.1"),
            tree.path(),
            dest.path(),
            Some(&project),
        )
        .unwrap();

        assert!(report.destination.join("ghostty-1.3.1.ebuild").is_file());
        assert!(report.destination.join("Manifest").is_file());
        assert!(report.destination.join("metadata.xml").is_file());
        assert!(report
            .destination
            .join("files/ghostty-1.3.0.patch")
            .is_file());

        // Config written under `<root>/.gentooit/ghostty.yaml` with the pin.
        let cfg_path = report.config_path.expect("config written");
        assert_eq!(cfg_path, dest.path().join(".gentooit/ghostty.yaml"));
        let cfg: ProjectConfig =
            serde_yaml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            cfg.upstream.as_ref().unwrap().version.as_deref(),
            Some("1.3.1")
        );
        assert_eq!(
            cfg.upstream.as_ref().unwrap().upstream.as_deref(),
            Some("ghostty-org/ghostty")
        );
        assert_eq!(
            cfg.upstream.as_ref().unwrap().archive_template.as_deref(),
            Some("https://release.files.ghostty.org/{version}/ghostty-{version}.tar.gz")
        );
        assert_eq!(
            cfg.package.as_ref().unwrap().maintainer_email.as_deref(),
            Some("dev@example.com")
        );
        assert_eq!(cfg.open_pull_request, Some(false));
        assert_eq!(
            cfg.downstream[0].package_dir.as_deref(),
            Some("ebuilds"),
            "destination prefix comes from the project's downstream package-dir"
        );
    }

    #[test]
    fn adopt_filters_versions_and_pins_newest() {
        let tree = tempfile::tempdir().unwrap();
        write_tree(
            tree.path(),
            &[
                ("app-misc/foo/foo-1.0.0.ebuild", "EAPI=8\n"),
                ("app-misc/foo/foo-1.5.0.ebuild", "EAPI=8\n"),
                ("app-misc/foo/foo-2.0.0.ebuild", "EAPI=8\n"),
            ],
        );
        let dest = tempfile::tempdir().unwrap();
        let report = adopt_package(
            "app-misc/foo",
            Some("1.5.0"),
            tree.path(),
            dest.path(),
            None,
        )
        .unwrap();
        assert_eq!(
            report.ebuilds,
            vec!["foo-1.5.0.ebuild".to_string()],
            "version filter keeps only the matching ebuild"
        );
    }

    #[test]
    fn adopt_without_filter_copies_all_versions() {
        let tree = tempfile::tempdir().unwrap();
        write_tree(
            tree.path(),
            &[
                ("app-misc/foo/foo-1.0.0.ebuild", "EAPI=8\n"),
                ("app-misc/foo/foo-1.5.0.ebuild", "EAPI=8\n"),
            ],
        );
        let dest = tempfile::tempdir().unwrap();
        let report = adopt_package("app-misc/foo", None, tree.path(), dest.path(), None).unwrap();
        assert_eq!(report.ebuilds.len(), 2);
        assert_eq!(report.version.as_deref(), Some("1.5.0"), "newest pinned");
    }

    #[test]
    fn adopt_missing_package_errors() {
        let tree = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let err = adopt_package("x11-terms/nope", None, tree.path(), dest.path(), None);
        assert!(err.is_err());
    }

    #[test]
    fn compare_simple_orders_numerically() {
        assert_eq!(
            compare_simple("1.10.0", "1.9.0"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_simple("1.1.0", "1.1.0-r1"),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn raw_srcuri_captures_multiline_bundles() {
        let ebuild = concat!(
            "EAPI=8\n",
            "SRC_URI=\"\n",
            "\thttps://release.files.ghostty.org/${PV}/ghostty-${PV}.tar.gz\n",
            "\t${ZBS_DEPENDENCIES_SRC_URI}\n",
            "\"\n",
            "LICENSE=\"MIT\"\n",
        );
        let block = raw_srcuri_block(ebuild).unwrap();
        assert!(block.contains("https://release.files.ghostty.org/${PV}/ghostty-${PV}.tar.gz"));
        assert!(block.contains("${ZBS_DEPENDENCIES_SRC_URI}"));
    }

    #[test]
    fn archive_template_parameterizes_ghostty_srcuri() {
        let tree = tempfile::tempdir().unwrap();
        let ebuild = concat!(
            "EAPI=8\n",
            "SRC_URI=\"\n",
            "\thttps://release.files.ghostty.org/${PV}/ghostty-${PV}.tar.gz\n",
            "\t${ZBS_DEPENDENCIES_SRC_URI}\n",
            "\"\n",
            "DESCRIPTION=\"Terminal emulator\"\n",
        );
        write_tree(
            tree.path(),
            &[("x11-terms/ghostty/ghostty-1.3.1.ebuild", ebuild)],
        );
        let source = read_source_package(
            &tree.path().join("x11-terms/ghostty"),
            &crate::ebuild::Atom::parse("x11-terms/ghostty").unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(
            source.archive_template().as_deref(),
            Some("https://release.files.ghostty.org/{version}/ghostty-{version}.tar.gz")
        );
    }
}
