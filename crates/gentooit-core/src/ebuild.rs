//! Ebuild model: parsing, representing, and generating Gentoo ebuild files.
//!
//! An ebuild is a bash script that declares metadata variables and optional
//! phase functions. gentooit treats it primarily as a set of metadata variables
//! it can read and update, while preserving the rest of the file structure.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An error that can occur while parsing or manipulating an ebuild.
#[derive(Debug, thiserror::Error)]
pub enum EbuildError {
    #[error("invalid ebuild filename `{0}`: expected `<name>-<version>.ebuild`")]
    InvalidFilename(String),
    #[error("invalid package name `{0}`")]
    InvalidPackageName(String),
    #[error("invalid version `{0}`: {1}")]
    InvalidVersion(String, String),
    #[error("malformed ebuild header in `{path}`: {msg}")]
    MalformedHeader { path: PathBuf, msg: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A parsed Gentoo version. Gentoo versions have the form
/// `<version>[._+p]-suffixes and revisions, e.g. `1.2.3-r1`, `2.1_pre2021`.
///
/// gentooit does not need to fully implement the entire Gentoo version
/// comparison algorithm (that is in `portage` / `pkgcore`), but it does need to
/// recognize and round-trip versions so ebuilds can be named and matched.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GentooVersion {
    /// The version string, including any suffix (`-r1`, `_pre`, `+`, etc.).
    pub raw: String,
}

impl GentooVersion {
    /// Parse a version from its string form.
    pub fn new(s: &str) -> Result<Self, EbuildError> {
        if s.is_empty() {
            return Err(EbuildError::InvalidVersion(
                s.to_string(),
                "version is empty".to_string(),
            ));
        }
        Ok(GentooVersion { raw: s.to_string() })
    }

    /// The full package-version string, e.g. `foo-1.2.3-r1`.
    pub fn pf(&self, pn: &str) -> String {
        format!("{pn}-{}", self.raw)
    }
}

impl fmt::Display for GentooVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

/// A package name (PN), e.g. `foo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageName {
    pub name: String,
}

impl PackageName {
    pub fn new(s: &str) -> Result<Self, EbuildError> {
        if s.is_empty()
            || s.bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'+'))
        {
            return Err(EbuildError::InvalidPackageName(s.to_string()));
        }
        Ok(PackageName {
            name: s.to_string(),
        })
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl FromStr for PackageName {
    type Err = EbuildError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PackageName::new(s)
    }
}

/// A category name, e.g. `sys-apps` or `app-editors`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
}

impl Category {
    pub fn new(s: &str) -> Result<Self, EbuildError> {
        if s.is_empty() {
            return Err(EbuildError::InvalidPackageName(
                "empty category".to_string(),
            ));
        }
        Ok(Category {
            name: s.to_string(),
        })
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// The atom `<category>/<package>`, e.g. `sys-apps/foo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Atom {
    pub category: Category,
    pub package: PackageName,
}

impl Atom {
    pub fn new(category: &str, package: &str) -> Result<Self, EbuildError> {
        Ok(Atom {
            category: Category::new(category)?,
            package: PackageName::new(package)?,
        })
    }

    /// Parse a string like `sys-apps/foo`.
    pub fn parse(s: &str) -> Result<Self, EbuildError> {
        let mut it = s.splitn(2, '/');
        let category = it.next().ok_or_else(|| {
            EbuildError::InvalidPackageName(format!("atom `{s}` missing category"))
        })?;
        let package = it.next().ok_or_else(|| {
            EbuildError::InvalidPackageName(format!("atom `{s}` missing package name"))
        })?;
        Atom::new(category, package)
    }

    /// Full name including category: `sys-apps/foo`.
    pub fn full(&self) -> String {
        format!("{}/{}", self.category, self.package)
    }
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.full())
    }
}

impl FromStr for Atom {
    type Err = EbuildError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Atom::parse(s)
    }
}

/// The well-known metadata variables gentooit reads from / writes to ebuilds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EbuildMetadata {
    pub eapi: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub src_uri: Option<String>,
    pub license: Option<String>,
    pub slot: Option<String>,
    pub keywords: Option<String>,
    pub iuse: Option<String>,
    pub depend: Option<String>,
    pub rdepend: Option<String>,
    pub bdepend: Option<String>,
    pub s: Option<String>,
}

/// The parsed representation of an ebuild file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ebuild {
    /// Atom this ebuild belongs to, from its filename/path.
    pub atom: Atom,
    /// The version (PV), from its filename.
    pub version: GentooVersion,
    /// Extracted metadata variables.
    pub metadata: EbuildMetadata,
    /// The original raw content, useful for preservation when editing.
    pub raw: String,
}

impl Ebuild {
    /// The filename, e.g. `foo-1.2.3.ebuild`.
    pub fn filename(&self) -> String {
        format!("{}.ebuild", self.version.pf(&self.atom.package.name))
    }

    /// Parse an ebuild from its filename and content.
    pub fn parse(
        category: &str,
        package: &str,
        filename: &str,
        content: &str,
    ) -> Result<Self, EbuildError> {
        let (expected_pn, pv) = parse_ebuild_filename(filename)?;
        // Verify the package name in the filename matches the given package.
        if expected_pn != package {
            return Err(EbuildError::InvalidPackageName(format!(
                "filename package `{expected_pn}` does not match given atom package `{package}`"
            )));
        }
        let atom = Atom::new(category, package)?;
        let metadata = extract_variables(content)?;
        Ok(Ebuild {
            atom,
            version: GentooVersion::new(&pv)?,
            metadata,
            raw: content.to_string(),
        })
    }

    /// Parse an ebuild from a path on disk.
    pub fn from_path(path: &Path, category: &str, package: &str) -> Result<Self, EbuildError> {
        let content = std::fs::read_to_string(path)?;
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| EbuildError::InvalidFilename(path.display().to_string()))?;
        Self::parse(category, package, filename, &content)
    }

    /// The source URI pattern, defaulting to the common upstream tarball form.
    pub fn default_src_uri(&self) -> String {
        format!(
            "https://github.com/{}/releases/download/${{PV}}/${{P}}.tar.gz",
            self.atom.package.name
        )
    }
}

/// Parse a `<name>-<version>.ebuild` filename into (package name, version).
pub fn parse_ebuild_filename(filename: &str) -> Result<(String, String), EbuildError> {
    let stem = filename
        .strip_suffix(".ebuild")
        .ok_or_else(|| EbuildError::InvalidFilename(filename.to_string()))?;

    // Find the boundary between the package name and the version.
    // Rule (covers the overwhelming majority of real ebuilds): the version
    // begins at the first `-` whose following character is a digit. This
    // correctly handles revision suffixes (`foo-1.2.3-r1`) and hyphenated
    // package names (`gobject-introspection-1.2.3`).
    let bytes = stem.as_bytes();
    let mut digit_split = None;
    for i in 0..stem.len() {
        if bytes[i] == b'-' {
            if let Some(&next) = bytes.get(i + 1) {
                if next.is_ascii_digit() {
                    digit_split = Some(i);
                    break;
                }
            }
        }
    }

    match digit_split {
        Some(i) => {
            let pn = &stem[..i];
            let pv = &stem[i + 1..];
            if pn.is_empty() || pv.is_empty() {
                return Err(EbuildError::InvalidFilename(filename.to_string()));
            }
            Ok((pn.to_string(), pv.to_string()))
        }
        // No digit-leading version found (e.g. alpha-versioned). Fall back to
        // the last `-` that yields a plausible version.
        None => {
            let mut split = None;
            for i in (0..stem.len()).rev() {
                if bytes[i] == b'-' {
                    let pn = &stem[..i];
                    let pv = &stem[i + 1..];
                    if !pn.is_empty() && !pv.is_empty() && looks_like_version(pv) {
                        split = Some((pn.to_string(), pv.to_string()));
                        break;
                    }
                }
            }
            split.ok_or_else(|| EbuildError::InvalidFilename(filename.to_string()))
        }
    }
}

fn looks_like_version(v: &str) -> bool {
    let first = match v.chars().next() {
        Some(c) => c,
        None => return false,
    };
    // A version usually starts with a digit, occasionally a letter (e.g. `alpha1`).
    first.is_ascii_alphanumeric()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | 'p'))
}

/// Extract known metadata variables from an ebuild's raw content.
///
/// This is intentionally a best-effort parser: ebuilds are bash, and arbitrary
/// bash expressions (conditionals, loops, `$(...)` command substitution) make a
/// general parser infeasible. We look for top-level `NAME="value"` assignments
/// that appear at the start of a line. Multi-line values (with parentheses) are
/// supported for the dependency variables.
pub fn extract_variables(content: &str) -> Result<EbuildMetadata, EbuildError> {
    let mut meta = EbuildMetadata::default();

    // Normalize line endings.
    let normalized = content.replace("\r\n", "\n");

    let lines_vec: Vec<&str> = normalized.lines().collect();
    let mut i = 0;

    // Track the current variable being accumulated across lines (for
    // parenthesized lists in DEPEND etc.).
    let mut pending: Option<(String, String)> = None;

    while i < lines_vec.len() {
        let line = lines_vec[i];

        if let Some((name, mut value)) = pending.take() {
            // Continue accumulating a multi-line value until the parenthesis closes.
            value.push(' ');
            value.push_str(line.trim());
            let open = value.matches('(').count();
            let close = value.matches(')').count();
            if open <= close {
                if open > close {
                    // still open, keep going
                    pending = Some((name, value));
                } else {
                    set_variable(&mut meta, &name, &value);
                }
            } else {
                pending = Some((name, value));
            }
            i += 1;
            continue;
        }

        let trimmed = line.trim_start();
        // Skip comments and empty lines.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        // Match `NAME="..."` or `NAME=...` at the start of a line.
        if let Some(eq) = trimmed.find('=') {
            let name = trimmed[..eq].trim();
            if is_known_variable(name) && name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                let mut value = trimmed[eq + 1..].trim().to_string();
                // Strip surrounding quotes.
                if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
                    || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
                {
                    let v = &value[1..value.len() - 1];
                    value = v.to_string();
                }
                // Check if the value has unbalanced parens -> continues on next lines.
                let open = value.matches('(').count();
                let close = value.matches(')').count();
                if open > close {
                    pending = Some((name.to_string(), value));
                } else {
                    set_variable(&mut meta, name, &value);
                }
            }
        }
        i += 1;
    }

    // Flush any pending multi-line value at EOF.
    if let Some((name, value)) = pending {
        if value.matches('(').count() > value.matches(')').count() {
            // Unbalanced at EOF; store as-is.
            set_variable(&mut meta, &name, &value);
        }
    }

    Ok(meta)
}

fn is_known_variable(name: &str) -> bool {
    matches!(
        name,
        "EAPI"
            | "DESCRIPTION"
            | "HOMEPAGE"
            | "SRC_URI"
            | "LICENSE"
            | "SLOT"
            | "KEYWORDS"
            | "IUSE"
            | "DEPEND"
            | "RDEPEND"
            | "BDEPEND"
            | "S"
    )
}

fn set_variable(meta: &mut EbuildMetadata, name: &str, value: &str) {
    let clean = value.trim().trim_matches('"').trim().to_string();
    match name {
        "EAPI" => meta.eapi = Some(normalize_eapi(clean)),
        "DESCRIPTION" => meta.description = Some(clean),
        "HOMEPAGE" => meta.homepage = Some(clean),
        "SRC_URI" => meta.src_uri = Some(clean),
        "LICENSE" => meta.license = Some(clean),
        "SLOT" => meta.slot = Some(clean),
        "KEYWORDS" => meta.keywords = Some(clean),
        "IUSE" => meta.iuse = Some(clean),
        "DEPEND" => meta.depend = Some(clean),
        "RDEPEND" => meta.rdepend = Some(clean),
        "BDEPEND" => meta.bdepend = Some(clean),
        "S" => meta.s = Some(clean),
        _ => {}
    }
}

fn normalize_eapi(v: String) -> String {
    v.trim_matches(|c| c == '"' || c == '\'').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_ebuild() {
        let ebuild = r#"# Copyright 1999-2024 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

DESCRIPTION="A test package"
HOMEPAGE="https://example.com"
SRC_URI="https://example.com/${P}.tar.gz"

LICENSE="MIT"
SLOT="0"
KEYWORDS="~amd64 ~x86"
IUSE=""

DEPEND="dev-libs/foo"
RDEPEND="${DEPEND}"
BDEPEND=""

src_install() {
    default
}
"#;
        let e = Ebuild::parse("app-misc", "testpkg", "testpkg-1.2.3.ebuild", ebuild).unwrap();
        assert_eq!(e.atom.full(), "app-misc/testpkg");
        assert_eq!(e.version.raw, "1.2.3");
        assert_eq!(e.metadata.description.as_deref(), Some("A test package"));
        assert_eq!(e.metadata.eapi.as_deref(), Some("8"));
        assert_eq!(e.metadata.homepage.as_deref(), Some("https://example.com"));
        assert_eq!(e.metadata.depend.as_deref(), Some("dev-libs/foo"));
        assert_eq!(e.metadata.rdepend.as_deref(), Some("${DEPEND}"));
        assert_eq!(
            e.metadata.src_uri.as_deref(),
            Some("https://example.com/${P}.tar.gz")
        );
    }

    #[test]
    fn parse_revision_and_suffix_version() {
        let (pn, pv) = parse_ebuild_filename("foo-1.2.3-r1.ebuild").unwrap();
        assert_eq!(pn, "foo");
        assert_eq!(pv, "1.2.3-r1");
        let (pn, pv) = parse_ebuild_filename("bar-2.0_pre2021.ebuild").unwrap();
        assert_eq!(pn, "bar");
        assert_eq!(pv, "2.0_pre2021");
    }

    #[test]
    fn parse_atom() {
        let atom: Atom = "sys-apps/foo".parse().unwrap();
        assert_eq!(atom.full(), "sys-apps/foo");
        assert_eq!(atom.package.name, "foo");
        assert_eq!(atom.category.name, "sys-apps");
    }

    #[test]
    fn invalid_filename() {
        assert!(parse_ebuild_filename("foo.ebuild").is_err());
    }
}
