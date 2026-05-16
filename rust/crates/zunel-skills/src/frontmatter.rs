//! Strip YAML frontmatter from skill markdown and parse it into a
//! weakly-typed tree. The parser is intentionally small and only
//! surfaces the fields `ContextBuilder` and the loader actually read.

use serde::Deserialize;

use crate::error::Error;

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Frontmatter {
    #[serde(default)]
    pub description: String,
    /// Optional nested metadata. Accepts either a JSON string or a
    /// map (`MetadataRaw::String` or `::Object`).
    #[serde(default)]
    pub metadata: Option<MetadataRaw>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum MetadataRaw {
    String(String),
    Object(serde_yaml::Value),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedMetadata {
    pub always: bool,
    pub bins: Vec<String>,
    pub env: Vec<String>,
}

impl Frontmatter {
    /// Pull the `zunel` or `openclaw` nested namespace into a
    /// strongly-typed struct. Unknown keys ignored.
    pub fn parsed_metadata(&self) -> ParsedMetadata {
        let raw_yaml: serde_yaml::Value = match &self.metadata {
            Some(MetadataRaw::String(s)) => {
                // Frontmatter authors sometimes write the metadata block as a
                // JSON string instead of a YAML map; try JSON first, then YAML.
                match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(v) => match serde_yaml::to_value(v) {
                        Ok(y) => y,
                        Err(_) => return ParsedMetadata::default(),
                    },
                    Err(_) => match serde_yaml::from_str::<serde_yaml::Value>(s) {
                        Ok(y) => y,
                        Err(_) => return ParsedMetadata::default(),
                    },
                }
            }
            Some(MetadataRaw::Object(v)) => v.clone(),
            None => return ParsedMetadata::default(),
        };
        let Some(mapping) = raw_yaml.as_mapping() else {
            return ParsedMetadata::default();
        };
        let ns = mapping
            .get(serde_yaml::Value::String("zunel".into()))
            .or_else(|| mapping.get(serde_yaml::Value::String("openclaw".into())));
        let Some(ns) = ns.and_then(|v| v.as_mapping()) else {
            return ParsedMetadata::default();
        };
        let always = ns
            .get(serde_yaml::Value::String("always".into()))
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false);
        let (bins, env) = match ns
            .get(serde_yaml::Value::String("requires".into()))
            .and_then(serde_yaml::Value::as_mapping)
        {
            Some(req) => {
                let bins = req
                    .get(serde_yaml::Value::String("bins".into()))
                    .and_then(serde_yaml::Value::as_sequence)
                    .map(|seq| {
                        seq.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let env = req
                    .get(serde_yaml::Value::String("env".into()))
                    .and_then(serde_yaml::Value::as_sequence)
                    .map(|seq| {
                        seq.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                (bins, env)
            }
            None => (Vec::new(), Vec::new()),
        };
        ParsedMetadata { always, bins, env }
    }
}

/// Split a skill body into (frontmatter, stripped_body).
pub fn split(markdown: &str) -> Result<(Frontmatter, String), Error> {
    let trimmed = markdown.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return Ok((Frontmatter::default(), markdown.to_string()));
    }
    let rest = &trimmed[3..];
    let end_marker = "\n---";
    let end_idx = rest.find(end_marker).ok_or_else(|| Error::Frontmatter {
        message: "missing closing ---".into(),
    })?;
    let yaml = &rest[..end_idx];
    let body_start = end_idx + end_marker.len();
    let body = rest[body_start..].trim_start_matches(['\n', '\r']);
    let frontmatter: Frontmatter =
        serde_yaml::from_str(yaml).map_err(|source| Error::Frontmatter {
            message: format!("invalid yaml: {source}"),
        })?;
    Ok((frontmatter, body.to_string()))
}
