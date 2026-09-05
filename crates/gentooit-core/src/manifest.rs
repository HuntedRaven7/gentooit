//! Manifest file generation and hashing.
//!
//! A Gentoo `Manifest` file (Manifest2, per GLEP 44/74) contains one line per
//! entry in the form:
//!
//! ```text
//! TYPE FILENAME SIZE HASH_TYPE HASH [HASH_TYPE HASH ...]
//! ```
//!
//! Types are `DIST` (distfiles), `EBUILD`, `AUX`, and `MISC`. In a git
//! repository with `thin-manifests = true` (the norm for gentoo/gentoo and
//! overlays), only `DIST` entries are present; git itself tracks the in-tree
//! files.

use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256, Sha512};

/// A single hash algorithm identifier we support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    /// SHA-256 (SHA-2)
    Sha256,
    /// SHA-512 (SHA-2)
    Sha512,
    /// Whirlpool
    Whirlpool,
}

impl HashAlgo {
    pub fn as_str(&self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "SHA256",
            HashAlgo::Sha512 => "SHA512",
            HashAlgo::Whirlpool => "WHIRLPOOL",
        }
    }
}

impl fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The type of a Manifest entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestEntryType {
    Dist,
    Ebuild,
    Aux,
    Misc,
}

impl ManifestEntryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ManifestEntryType::Dist => "DIST",
            ManifestEntryType::Ebuild => "EBUILD",
            ManifestEntryType::Aux => "AUX",
            ManifestEntryType::Misc => "MISC",
        }
    }
}

impl fmt::Display for ManifestEntryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A single Manifest entry line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub entry_type: ManifestEntryType,
    pub filename: String,
    /// Size in bytes.
    pub size: u64,
    /// Hash algorithm -> hex digest pairs.
    pub hashes: Vec<(HashAlgo, String)>,
}

impl ManifestEntry {
    /// Compute a Manifest entry for a distfile by reading and hashing it.
    pub fn for_distfile(path: &Path) -> std::io::Result<ManifestEntry> {
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad filename"))?
            .to_string();
        let data = std::fs::read(path)?;
        let size = data.len() as u64;
        let hashes = hash_bytes(&data, &[HashAlgo::Sha256, HashAlgo::Sha512]);
        Ok(ManifestEntry {
            entry_type: ManifestEntryType::Dist,
            filename,
            size,
            hashes,
        })
    }
}

impl fmt::Display for ManifestEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = vec![
            self.entry_type.as_str().to_string(),
            self.filename.clone(),
            self.size.to_string(),
        ];
        for (algo, digest) in &self.hashes {
            parts.push(algo.as_str().to_string());
            parts.push(digest.clone());
        }
        write!(f, "{}", parts.join(" "))
    }
}

/// An error that can occur while working with Manifests.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed Manifest line `{line}`: {msg}")]
    MalformedLine { line: String, msg: String },
}

/// Compute the given hash algorithms over a byte slice, returning
/// `(algo, lowercase-hex-digest)` pairs.
pub fn hash_bytes(data: &[u8], algos: &[HashAlgo]) -> Vec<(HashAlgo, String)> {
    algos
        .iter()
        .map(|algo| {
            let digest = match algo {
                HashAlgo::Sha256 => hex::encode(Sha256::digest(data)),
                HashAlgo::Sha512 => hex::encode(Sha512::digest(data)),
                HashAlgo::Whirlpool => {
                    use whirlpool::Digest as _;
                    hex::encode(whirlpool::Whirlpool::digest(data))
                }
            };
            (*algo, digest)
        })
        .collect()
}

/// Compute hashes of a file on disk.
pub fn hash_file(path: &Path, algos: &[HashAlgo]) -> std::io::Result<Vec<(HashAlgo, String)>> {
    let data = std::fs::read(path)?;
    Ok(hash_bytes(&data, algos))
}

/// A parsed Manifest file.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    /// All entries, in file order.
    pub entries: Vec<ManifestEntry>,
}

impl Manifest {
    /// Parse Manifest content into entries. Blank lines and comments are ignored.
    pub fn parse(content: &str) -> Result<Manifest, ManifestError> {
        let mut entries = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            entries.push(parse_line(line)?);
        }
        Ok(Manifest { entries })
    }

    /// Read a Manifest from a path.
    pub fn from_path(path: &Path) -> Result<Manifest, ManifestError> {
        let content = std::fs::read_to_string(path)?;
        Manifest::parse(&content)
    }

    /// Serialize to string content, one entry per line, sorted for determinism.
    pub fn to_string_sorted(&self) -> String {
        let mut entries = self.entries.clone();
        entries.sort_by(|a, b| {
            a.entry_type
                .as_str()
                .cmp(b.entry_type.as_str())
                .then_with(|| a.filename.cmp(&b.filename))
        });
        let mut out = String::new();
        for e in entries {
            out.push_str(&e.to_string());
            out.push('\n');
        }
        out
    }

    /// Look up an entry by filename (and optionally type).
    pub fn get(&self, filename: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.filename == filename)
    }

    /// Set or replace an entry, keyed by type+filename.
    pub fn upsert(&mut self, entry: ManifestEntry) {
        self.entries
            .retain(|e| !(e.entry_type == entry.entry_type && e.filename == entry.filename));
        self.entries.push(entry);
    }
}

fn parse_line(line: &str) -> Result<ManifestEntry, ManifestError> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        return Err(ManifestError::MalformedLine {
            line: line.to_string(),
            msg: "expected at least TYPE FILENAME SIZE HASH".to_string(),
        });
    }

    let entry_type = match parts[0] {
        "DIST" => ManifestEntryType::Dist,
        "EBUILD" => ManifestEntryType::Ebuild,
        "AUX" => ManifestEntryType::Aux,
        "MISC" => ManifestEntryType::Misc,
        other => {
            return Err(ManifestError::MalformedLine {
                line: line.to_string(),
                msg: format!("unknown type `{other}`"),
            })
        }
    };
    let filename = parts[1].to_string();
    let size: u64 = parts[2].parse().map_err(|_| ManifestError::MalformedLine {
        line: line.to_string(),
        msg: format!("invalid size `{}`", parts[2]),
    })?;

    let mut hashes = Vec::new();
    let mut i = 3;
    while i + 1 < parts.len() {
        let algo = match parts[i] {
            "SHA256" => HashAlgo::Sha256,
            "SHA512" => HashAlgo::Sha512,
            "WHIRLPOOL" => HashAlgo::Whirlpool,
            // Ignore algorithms we don't recognize (e.g. RMD160, MD5).
            other => {
                i += 2;
                let _ = other;
                continue;
            }
        };
        hashes.push((algo, parts[i + 1].to_string()));
        i += 2;
    }

    Ok(ManifestEntry {
        entry_type,
        filename,
        size,
        hashes,
    })
}

/// Convenience: build the full set of hashes for distfile paths, returning
/// Manifest entries. Used by the `propose-downstream` workflow after it
/// downloads the upstream archives.
pub fn manifest_for_distfiles(paths: &[PathBuf]) -> std::io::Result<Vec<ManifestEntry>> {
    let mut entries = Vec::new();
    for p in paths {
        entries.push(ManifestEntry::for_distfile(p)?);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn round_trip_manifest() {
        let content = "DIST foo-1.2.3.tar.gz 1234 SHA256 abcdef SHA512 123456\n";
        let m = Manifest::parse(content).unwrap();
        assert_eq!(m.entries.len(), 1);
        let e = &m.entries[0];
        assert_eq!(e.filename, "foo-1.2.3.tar.gz");
        assert_eq!(e.size, 1234);
        assert_eq!(e.hashes.len(), 2);
        assert_eq!(m.to_string_sorted(), content);
    }

    #[test]
    fn hash_known_bytes() {
        let data = b"hello world";
        let hashes = hash_bytes(data, &[HashAlgo::Sha256]);
        assert_eq!(
            hashes[0].1,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn manifest_for_distfile() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "test content").unwrap();
        let entry = ManifestEntry::for_distfile(file.path()).unwrap();
        assert_eq!(entry.entry_type, ManifestEntryType::Dist);
        assert_eq!(entry.size, 13);
        assert!(entry.hashes.len() >= 2);
    }
}
