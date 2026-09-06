//! Configuration for gentooit.
//!
//! There are two layers, mirroring packit:
//!
//! * **Project config** (`.gentooit.yaml` in the repo root) describes a
//!   project: where its upstream lives, how to derive the package name and
//!   version, what to put in the ebuild, and where to push the downstream
//!   change.
//! * **User config** (`~/.config/gentooit/config.yaml`) holds credentials and
//!   the user's default identity.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The default project config filename.
pub const PROJECT_CONFIG: &str = ".gentooit.yaml";
/// The default overlay repo to push to, used only as an example/default.
pub const DEFAULT_OVERLAY: &str = "gentoo/gentoo";

/// User-specific configuration, stored in `~/.config/gentooit/config.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct UserConfig {
    /// GitHub token (classic PAT or fine-grained) used for GitHub API calls.
    #[serde(default)]
    pub github_token: Option<String>,
    /// GitHub App id, if using GitHub App authentication.
    #[serde(default)]
    pub github_app_id: Option<i64>,
    /// Path to the GitHub App private key (PEM).
    #[serde(default)]
    pub github_app_key: Option<PathBuf>,
    /// The git author name used when creating commits in the downstream repo.
    #[serde(default)]
    pub git_author_name: Option<String>,
    /// The git author email used when creating commits.
    #[serde(default)]
    pub git_author_email: Option<String>,
    /// Optional hint for the GitHub username (used to construct fork refs).
    #[serde(default)]
    pub github_username_hint: Option<String>,
    /// Where to clone the downstream overlay repo.
    #[serde(default)]
    pub downstream_dir: Option<PathBuf>,
    /// Use `pkgdev`/`pkgcheck` for QA when present.
    #[serde(default)]
    pub use_pkgdev: Option<bool>,
}

impl UserConfig {
    /// Best-effort GitHub username from the token's environment or config hint.
    pub fn github_username(&self) -> Option<String> {
        std::env::var_os("GENTOOIT_GITHUB_USER")
            .map(|u| u.to_string_lossy().to_string())
            .or_else(|| self.github_username_hint.clone())
    }

    /// Build a UserConfig that authenticates via a GitHub App.
    pub fn for_app(app_id: i64, key_path: &Path) -> Self {
        Self {
            github_app_id: Some(app_id),
            github_app_key: Some(key_path.to_path_buf()),
            ..Self::default()
        }
    }
}

impl UserConfig {
    /// Load the user config from the default location, returning an empty
    /// config if it does not exist.
    pub fn load_default() -> anyhow::Result<UserConfig> {
        let dir = config_dir();
        let path = dir.join("config.yaml");
        UserConfig::load(&path)
    }

    /// Location of the user config directory.
    pub fn config_dir() -> PathBuf {
        config_dir()
    }

    /// Load a user config from `path`. Missing files yield an empty config.
    ///
    /// If no token is present in the file, `GENTOOIT_GITHUB_TOKEN` and then
    /// `GH_TOKEN` are consulted so the token never needs to be written to
    /// disk.
    pub fn load(path: &Path) -> anyhow::Result<UserConfig> {
        if !path.exists() {
            return Ok(user_config_from_env());
        }
        let content = std::fs::read_to_string(path)?;
        let mut cfg: UserConfig = serde_yaml::from_str(&content)?;
        if cfg.github_token.is_none() {
            cfg.github_token = env_token();
        }
        Ok(cfg)
    }
}

/// GitHub token from the environment (`GENTOOIT_GITHUB_TOKEN`, then `GH_TOKEN`).
fn env_token() -> Option<String> {
    std::env::var_os("GENTOOIT_GITHUB_TOKEN")
        .or_else(|| std::env::var_os("GH_TOKEN"))
        .map(|t| t.to_string_lossy().to_string())
}

/// A user config with the token (if any) pulled from the environment.
fn user_config_from_env() -> UserConfig {
    UserConfig {
        github_token: env_token(),
        ..UserConfig::default()
    }
}

/// The values gentooit needs to find an upstream release and derive the
/// ebuild's version and source archive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct UpstreamConfig {
    /// The upstream VCS type: `github` (default) or `gitlab`.
    #[serde(default)]
    pub vcs: Option<String>,
    /// The upstream repository as `owner/name`.
    #[serde(alias = "repo", default)]
    pub upstream: Option<String>,
    /// Override for the package name (PN). Defaults to the repo name.
    #[serde(default)]
    pub package_name: Option<String>,
    /// Template for the version tag, e.g. `v{version}`. Defaults to `{version}`.
    #[serde(default)]
    pub tag_template: Option<String>,
    /// Template for the source tarball basename, e.g. `{package}-{version}.tar.gz`.
    #[serde(default)]
    pub archive_template: Option<String>,
    /// Template for the source tarball *filename*, e.g. `{package}-{version}.tar.gz`.
    /// Defaults to `{package}-{version}.tar.gz`.
    #[serde(default)]
    pub archive_name: Option<String>,
    /// The version to propose. If unset, the latest release is used.
    #[serde(default)]
    pub version: Option<String>,
    /// Override for the extracted source directory when it differs from
    /// `${P}` (for archives whose top-level directory doesn't match the ebuild
    /// basename). May contain `{version}`/`{package}` templates. If unset,
    /// gentooit derives it from the chosen archive's name / GitHub tarball.
    #[serde(default)]
    pub s_dir: Option<String>,
}

impl UpstreamConfig {
    /// The override for the distfile basename, if any.
    pub fn archive_name_override(&self) -> Option<String> {
        self.archive_name.clone()
    }
}

/// A single downstream target (the "dist-git" equivalent): a Gentoo overlay or
/// the main gentoo/gentoo repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DownstreamConfig {
    /// Git remote URL of the downstream repo (e.g.
    /// `https://github.com/USER/overlay.git`).
    #[serde(default)]
    pub url: String,
    /// The git branch to open a PR against.
    #[serde(default)]
    pub branch: Option<String>,
    /// The category (e.g. `sys-apps`). Optional for the propose workflow when
    /// deriving from existing metadata; required for new packages.
    #[serde(default)]
    pub category: Option<String>,
    /// Where the ebuild should be placed within the downstream repo.
    #[serde(default)]
    pub package_dir: Option<String>,
}

impl Default for DownstreamConfig {
    fn default() -> Self {
        DownstreamConfig {
            url: String::new(),
            branch: Some("master".to_string()),
            category: None,
            package_dir: None,
        }
    }
}

/// The full per-project configuration, parsed from `.gentooit.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectConfig {
    /// Spec version of the config file format.
    #[serde(default)]
    pub spec_version: Option<String>,
    /// Upstream provenance.
    #[serde(default)]
    pub upstream: Option<UpstreamConfig>,
    /// Current package metadata (used to fill ebuild variables when we cannot
    /// derive them automatically).
    #[serde(default)]
    pub package: Option<PackageConfig>,
    /// The downstream overlay(s) to sync into.
    #[serde(default)]
    pub downstream: Vec<DownstreamConfig>,
    /// Files to copy/sync between upstream and downstream (packit's
    /// `files_to_sync`).
    #[serde(default)]
    pub files_to_sync: Vec<FileSync>,
    /// Whether to open a PR (true) or push/commit only.
    #[serde(default)]
    pub open_pull_request: Option<bool>,
}

/// Static packaging details gentooit may not be able to infer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PackageConfig {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub slot: Option<String>,
    #[serde(default)]
    pub keywords: Option<String>,
    #[serde(default)]
    pub iuse: Option<String>,
    #[serde(default)]
    pub depend: Option<String>,
    #[serde(default)]
    pub rdepend: Option<String>,
    #[serde(default)]
    pub bdepend: Option<String>,
    #[serde(default)]
    pub restrict: Option<String>,
    /// Eclass(es) to `inherit`, e.g. `meson` or `zig xdg`. Overrides the
    /// build-system detection when set.
    #[serde(default)]
    pub inherit: Option<String>,
    /// Build system preset used to render phase functions when no `inherit`
    /// is configured: `plain`, `cargo`, `meson`, `cmake`, or `zig`. Defaults
    /// to auto-detection from the source archive.
    #[serde(default)]
    pub build_system: Option<String>,
    /// Raw ebuild phase bodies (`src_configure() { ... }`, etc.) appended to
    /// the generated ebuild when the presets aren't enough. Diff-bumps
    /// preserve anything starting from here, so hand-written phases survive
    /// version bumps.
    #[serde(default)]
    pub src_functions: Option<String>,
    #[serde(default)]
    pub maintainer_email: Option<String>,
    #[serde(default)]
    pub maintainer_name: Option<String>,
    #[serde(default)]
    pub remote_id_type: Option<String>,
}

/// A file (or glob) to sync between repos, with src/dest paths.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FileSync {
    #[serde(default)]
    pub src: String,
    #[serde(default)]
    pub dest: String,
    /// Delete dest files not present in src (default false).
    #[serde(default)]
    pub delete: Option<bool>,
}

impl ProjectConfig {
    /// Locate the project config file, walking up from `start_dir`.
    pub fn discover(start_dir: &Path) -> anyhow::Result<Option<(PathBuf, ProjectConfig)>> {
        let mut dir = Some(start_dir);
        while let Some(d) = dir {
            let candidate = d.join(PROJECT_CONFIG);
            if candidate.is_file() {
                let content = std::fs::read_to_string(&candidate)?;
                let cfg: ProjectConfig = serde_yaml::from_str(&content)?;
                return Ok(Some((candidate, cfg)));
            }
            dir = d.parent();
        }
        Ok(None)
    }

    /// Load a specific config path.
    pub fn load(path: &Path) -> anyhow::Result<ProjectConfig> {
        let content = std::fs::read_to_string(path)?;
        let cfg: ProjectConfig = serde_yaml::from_str(&content)?;
        Ok(cfg)
    }

    /// Parse a ProjectConfig from a YAML string.
    pub fn from_yaml(content: &str) -> anyhow::Result<Self> {
        let cfg: Self = serde_yaml::from_str(content)?;
        Ok(cfg)
    }
}

/// The directory used to store gentooit user configuration.
pub fn config_dir() -> PathBuf {
    std::env::var_os("GENTOOIT_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            #[cfg(unix)]
            {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        std::env::var_os("HOME")
                            .map(|h| PathBuf::from(h).join(".config"))
                            .unwrap_or_else(|| PathBuf::from("."))
                    })
                    .join("gentooit")
            }
            #[cfg(not(unix))]
            {
                PathBuf::from(".")
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_project_config() {
        let yaml = r#"
spec-version: "1.0"
upstream:
  vcs: github
  upstream: torvalds/linux
  package-name: linux
  tag-template: "v{version}"
downstream:
  - url: git@github.com:USER/overlay.git
    branch: master
    category: sys-kernel
package:
  license: GPL-2
  keywords: "~amd64"
open-pull-request: true
"#;
        let cfg: ProjectConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.upstream.as_ref().unwrap().upstream.as_deref(),
            Some("torvalds/linux")
        );
        assert_eq!(cfg.downstream.len(), 1);
        assert_eq!(cfg.downstream[0].category.as_deref(), Some("sys-kernel"));
        assert_eq!(
            cfg.package.as_ref().unwrap().license.as_deref(),
            Some("GPL-2")
        );
        assert_eq!(cfg.open_pull_request, Some(true));
    }

    #[test]
    fn user_config_defaults() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.github_token, None);
    }

    #[test]
    fn user_config_kebab_case() {
        let yaml = r#"
github-token: ghp_secret
github-username-hint: me
git-author-name: Test
git-author-email: test@example.com
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.github_token.as_deref(), Some("ghp_secret"));
        assert_eq!(cfg.github_username_hint.as_deref(), Some("me"));
        assert_eq!(cfg.git_author_name.as_deref(), Some("Test"));
    }
}
