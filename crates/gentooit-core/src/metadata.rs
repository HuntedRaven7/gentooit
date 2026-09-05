//! metadata.xml handling.
//!
//! Each Gentoo package has a `metadata.xml` (defined by GLEP 68) describing
//! maintainers, the long description, USE flags, and — critically for
//! gentooit — an `<upstream>` element with a `<remote-id>` that links to the
//! upstream VCS (e.g. GitHub).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// An error operating on metadata.xml files.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("XML parse error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown remote-id type `{0}`")]
    UnknownRemoteIdType(String),
}

/// A remote-id linking the package to its upstream project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteId {
    /// The type, e.g. `github`, `gitlab`, `pypi`.
    pub r#type: String,
    /// The project identifier, e.g. `torvalds/linux`.
    pub id: String,
}

/// A structured, lossy representation of the parts of metadata.xml gentooit
/// cares about. We do not attempt to preserve the entire XML verbatim; for the
/// common case of adding a `<remote-id>` we render a fresh metadata.xml.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageMetadata {
    /// Maintainers.
    pub maintainers: Vec<Maintainer>,
    /// Long description per language.
    pub longdescription: Vec<LongDescription>,
    /// The upstream remote-ids.
    pub remote_ids: Vec<RemoteId>,
    /// Upstream changelog URL.
    pub changelog: Option<String>,
    /// Upstream docs URL.
    pub doc: Option<String>,
    /// Upstream bug tracker URL.
    pub bugs_to: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maintainer {
    pub r#type: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongDescription {
    pub lang: Option<String>,
    pub text: String,
}

impl PackageMetadata {
    /// Create an empty package metadata with a single remote-id.
    pub fn with_remote_id(remote_id: RemoteId) -> Self {
        PackageMetadata {
            remote_ids: vec![remote_id],
            ..Default::default()
        }
    }

    /// Parse a metadata.xml file into this structure. Unknown elements are
    /// ignored rather than erroring, since metadata.xml is a stable but
    /// extensible schema.
    pub fn parse_xml(xml: &str) -> Result<PackageMetadata, MetadataError> {
        use quick_xml::events::Event;
        use quick_xml::Reader;

        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut pkg = PackageMetadata::default();
        let mut current: Option<String> = None;
        // For remote-id elements, the type attribute is set on the open tag and
        // the id is the element's text (or a fallback `id` attribute).
        let mut pending_remote_id_type: Option<String> = None;
        let mut pending_remote_id_id: Option<String> = None;

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    current = Some(name.clone());
                    if name == "remote-id" {
                        let mut r_type = String::new();
                        let mut id = String::new();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "type" => r_type = val,
                                "id" => id = val,
                                _ => {}
                            }
                        }
                        pending_remote_id_type = Some(r_type);
                        pending_remote_id_id = Some(id);
                    } else if name == "maintainer" {
                        let mut m_type = None;
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key == "type" {
                                m_type = Some(val);
                            }
                        }
                        pkg.maintainers.push(Maintainer {
                            r#type: m_type,
                            email: None,
                            name: None,
                        });
                    }
                }
                Ok(Event::Empty(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if name == "remote-id" {
                        // Self-closing form `<remote-id type="X" id="Y"/>`.
                        let mut r_type = String::new();
                        let mut id = String::new();
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            match key.as_str() {
                                "type" => r_type = val,
                                "id" => id = val,
                                _ => {}
                            }
                        }
                        if !id.is_empty() {
                            pkg.remote_ids.push(RemoteId { r#type: r_type, id });
                        }
                    }
                }
                Ok(Event::Text(t)) => {
                    let text = String::from_utf8_lossy(&t).to_string();
                    match current.as_deref() {
                        Some("email") => {
                            if let Some(m) = pkg.maintainers.last_mut() {
                                m.email = Some(text);
                            }
                        }
                        Some("name") => {
                            if let Some(m) = pkg.maintainers.last_mut() {
                                m.name = Some(text);
                            }
                        }
                        Some("longdescription") => {
                            if !text.trim().is_empty() {
                                let last_lang =
                                    pkg.longdescription.last().and_then(|l| l.lang.clone());
                                pkg.longdescription.push(LongDescription {
                                    lang: last_lang,
                                    text: text.trim().to_string(),
                                });
                            }
                        }
                        Some("remote-id") => {
                            if !text.trim().is_empty() {
                                let r_type = pending_remote_id_type.take().unwrap_or_default();
                                let id = pending_remote_id_id.take().unwrap_or_default();
                                pkg.remote_ids.push(RemoteId {
                                    r#type: r_type,
                                    id: if id.is_empty() {
                                        text.trim().to_string()
                                    } else {
                                        id
                                    },
                                });
                            }
                        }
                        Some("changelog") => pkg.changelog = Some(text),
                        Some("doc") => pkg.doc = Some(text),
                        Some("bugs-to") => pkg.bugs_to = Some(text),
                        _ => {}
                    }
                }
                Ok(Event::End(_)) => {
                    current = None;
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(MetadataError::Xml(e)),
                _ => {}
            }
        }

        Ok(pkg)
    }

    /// Read and parse metadata.xml from a path.
    pub fn from_path(path: &Path) -> Result<PackageMetadata, MetadataError> {
        let xml = std::fs::read_to_string(path)?;
        PackageMetadata::parse_xml(&xml)
    }

    /// Look up the first remote-id of a given type.
    pub fn remote_id(&self, r#type: &str) -> Option<&RemoteId> {
        self.remote_ids.iter().find(|r| r.r#type == r#type)
    }

    /// Render the metadata.xml text. This produces a valid, minimal
    /// metadata.xml; callers that need to preserve extra elements should edit
    /// the file separately.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        out.push_str("<!DOCTYPE pkgmetadata SYSTEM \"https://www.gentoo.org/dtd/metadata.dtd\">\n");
        out.push_str("<pkgmetadata>\n");
        for m in &self.maintainers {
            let t = m.r#type.as_deref().unwrap_or("person");
            out.push_str(&format!("  <maintainer type=\"{t}\">\n"));
            if let Some(e) = &m.email {
                out.push_str(&format!("    <email>{e}</email>\n"));
            }
            if let Some(n) = &m.name {
                out.push_str(&format!("    <name>{n}</name>\n"));
            }
            out.push_str("  </maintainer>\n");
        }
        for ld in &self.longdescription {
            if let Some(lang) = &ld.lang {
                out.push_str(&format!("  <longdescription lang=\"{lang}\">\n"));
                out.push_str(&format!("    {}\n", ld.text));
                out.push_str("  </longdescription>\n");
            }
        }
        if !self.remote_ids.is_empty() {
            out.push_str("  <upstream>\n");
            if let Some(c) = &self.changelog {
                out.push_str(&format!("    <changelog>{c}</changelog>\n"));
            }
            if let Some(d) = &self.doc {
                out.push_str(&format!("    <doc>{d}</doc>\n"));
            }
            if let Some(b) = &self.bugs_to {
                out.push_str(&format!("    <bugs-to>{b}</bugs-to>\n"));
            }
            for r in &self.remote_ids {
                out.push_str(&format!(
                    "    <remote-id type=\"{}\">{}</remote-id>\n",
                    r.r#type, r.id
                ));
            }
            out.push_str("  </upstream>\n");
        }
        out.push_str("</pkgmetadata>\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_id() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE pkgmetadata SYSTEM "https://www.gentoo.org/dtd/metadata.dtd">
<pkgmetadata>
  <maintainer type="person">
    <email>dev@example.com</email>
    <name>Dev</name>
  </maintainer>
  <upstream>
    <changelog>https://example.com/CHANGELOG</changelog>
    <remote-id type="github">torvalds/linux</remote-id>
  </upstream>
</pkgmetadata>
"#;
        let md = PackageMetadata::parse_xml(xml).unwrap();
        assert_eq!(md.remote_id("github").unwrap().id, "torvalds/linux");
        assert_eq!(md.maintainers.len(), 1);
        assert_eq!(md.maintainers[0].email.as_deref(), Some("dev@example.com"));
        assert_eq!(
            md.changelog.as_deref(),
            Some("https://example.com/CHANGELOG")
        );
    }
}
