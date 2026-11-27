use super::{invalid, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use unicode_normalization::UnicodeNormalization;

pub const MAX_FILE_BYTES: usize = 64 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    User,
    Feedback,
    Project,
    Reference,
}

/// Retains unknown frontmatter fields and the complete fact body.
#[derive(Debug, Clone)]
pub struct MemoryDocument {
    pub name: String,
    pub description: String,
    pub kind: MemoryKind,
    pub body: String,
    pub frontmatter: Mapping,
}

pub(super) fn name_key(text: &str) -> String {
    let mut folded = String::new();
    for c in text.nfc() {
        if let Some(expansion) = super::name_fold::special(c) {
            folded.push_str(expansion);
        } else {
            folded.extend(c.to_lowercase());
        }
    }
    folded.nfc().collect()
}

fn string<'a>(map: &'a Mapping, key: &str) -> Result<&'a str> {
    map.get(Value::String(key.into()))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing/string field: {key}")))
}

fn line(text: &str, max: usize, field: &str) -> Result<()> {
    if text.trim().is_empty() || text.chars().count() > max || text.chars().any(char::is_control) {
        return Err(invalid(format!("invalid {field}")));
    }
    Ok(())
}

// Inspect parser events before constructing a Value: aliases are never expanded,
// while punctuation in ordinary quoted scalars/comments remains valid YAML.
pub(crate) fn yaml_subset(text: &str) -> Result<()> {
    use std::mem::MaybeUninit;
    use unsafe_libyaml as yaml;
    struct Parser(Box<MaybeUninit<yaml::yaml_parser_t>>);
    impl Drop for Parser {
        fn drop(&mut self) {
            // SAFETY: constructed only after successful initialization; the Box
            // keeps libyaml's self-referential parser at its original address.
            unsafe { yaml::yaml_parser_delete(self.0.as_mut_ptr()) };
        }
    }
    let mut storage = Box::new(MaybeUninit::uninit());
    // SAFETY: input is valid UTF-8, bounded to 8 KiB by parse(), and outlives the
    // parser. Events are accessed only after successful parse, then deleted once.
    unsafe {
        if yaml::yaml_parser_initialize(storage.as_mut_ptr()).fail {
            return Err(invalid("frontmatter parser initialization failed"));
        }
        let mut parser = Parser(storage);
        yaml::yaml_parser_set_input_string(parser.0.as_mut_ptr(), text.as_ptr(), text.len() as u64);
        let mut depth = 0usize;
        for _ in 0..8192 {
            let mut event = MaybeUninit::<yaml::yaml_event_t>::uninit();
            if yaml::yaml_parser_parse(parser.0.as_mut_ptr(), event.as_mut_ptr()).fail {
                return Err(invalid("invalid frontmatter YAML"));
            }
            let mut event = event.assume_init();
            let forbidden = match event.type_ {
                yaml::YAML_ALIAS_EVENT => true,
                yaml::YAML_SCALAR_EVENT => {
                    !event.data.scalar.anchor.is_null() || !event.data.scalar.tag.is_null()
                }
                yaml::YAML_SEQUENCE_START_EVENT => {
                    depth += 1;
                    !event.data.sequence_start.anchor.is_null()
                        || !event.data.sequence_start.tag.is_null()
                }
                yaml::YAML_MAPPING_START_EVENT => {
                    depth += 1;
                    !event.data.mapping_start.anchor.is_null()
                        || !event.data.mapping_start.tag.is_null()
                }
                yaml::YAML_SEQUENCE_END_EVENT | yaml::YAML_MAPPING_END_EVENT => {
                    depth = depth.saturating_sub(1);
                    false
                }
                _ => false,
            };
            let end = event.type_ == yaml::YAML_STREAM_END_EVENT;
            yaml::yaml_event_delete(&mut event);
            if forbidden || depth > 8 {
                return Err(invalid(
                    "YAML aliases, anchors, tags or depth above 8 are unsupported",
                ));
            }
            if end {
                return Ok(());
            }
        }
    }
    Err(invalid("too many frontmatter YAML events"))
}

pub(crate) fn validate_value(value: &Value, depth: usize) -> Result<()> {
    if depth > 8 {
        return Err(invalid("frontmatter depth exceeds 8"));
    }
    match value {
        Value::Number(number) if number.as_f64().is_some_and(|n| !n.is_finite()) => {
            return Err(invalid("non-finite frontmatter number"));
        }
        Value::Tagged(_) => return Err(invalid("YAML tags are not supported")),
        Value::Mapping(map) => {
            for (key, value) in map {
                if !key.is_string() {
                    return Err(invalid("frontmatter keys must be strings"));
                }
                validate_value(value, depth + 1)?;
            }
        }
        Value::Sequence(items) => {
            for value in items {
                validate_value(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl MemoryDocument {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(invalid("memory exceeds 64 KiB"));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| invalid("memory is not UTF-8"))?;
        // Normalize line endings, not fact whitespace. The raw input remains in
        // Entry.bytes and transaction backups; invalid external files stay put.
        let text = text.replace("\r\n", "\n");
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| invalid("missing frontmatter"))?;
        let end = rest
            .find("\n---\n")
            .ok_or_else(|| invalid("unterminated frontmatter"))?;
        let yaml = &rest[..end];
        if yaml.len() > MAX_FRONTMATTER_BYTES {
            return Err(invalid("frontmatter exceeds 8 KiB"));
        }
        yaml_subset(yaml)?;
        let value: Value =
            serde_yaml::from_str(yaml).map_err(|_| invalid("invalid or duplicate YAML fields"))?;
        validate_value(&value, 0)?;
        let frontmatter = value
            .as_mapping()
            .ok_or_else(|| invalid("frontmatter must be a mapping"))?
            .clone();
        let name = string(&frontmatter, "name")?.to_owned();
        let description = string(&frontmatter, "description")?.to_owned();
        line(&name, 80, "name")?;
        line(&description, 240, "description")?;
        if description.len() > 512 {
            return Err(invalid("description exceeds 512 UTF-8 bytes"));
        }
        let metadata = frontmatter
            .get("metadata")
            .and_then(Value::as_mapping)
            .ok_or_else(|| invalid("missing metadata"))?;
        let kind = match string(metadata, "type")? {
            "user" => MemoryKind::User,
            "feedback" => MemoryKind::Feedback,
            "project" => MemoryKind::Project,
            "reference" => MemoryKind::Reference,
            _ => return Err(invalid("invalid metadata.type")),
        };
        for field in ["created_at", "updated_at"] {
            if let Some(value) = metadata.get(field) {
                chrono::DateTime::parse_from_rfc3339(
                    value
                        .as_str()
                        .ok_or_else(|| invalid("timestamp must be a string"))?,
                )
                .map_err(|_| invalid("invalid RFC3339 timestamp"))?;
            }
        }
        if let Some(id) = metadata.get("id") {
            let id = id.as_str().ok_or_else(|| invalid("invalid stable id"))?;
            if id.len() > 128
                || id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(invalid("invalid stable id"));
            }
        }
        let body = rest[end + 5..].to_owned();
        if body.trim().is_empty() {
            return Err(invalid("empty memory body"));
        }
        if matches!(kind, MemoryKind::Feedback | MemoryKind::Project) {
            for heading in ["## Why", "## How to apply"] {
                let mut inside = false;
                let mut content = false;
                let mut fence = None;
                for line in body.lines().map(str::trim) {
                    if line.starts_with("```") || line.starts_with("~~~") {
                        let marker = &line[..3];
                        if fence == Some(marker) {
                            fence = None;
                        } else if fence.is_none() {
                            fence = Some(marker);
                        }
                        continue;
                    }
                    if fence.is_some() {
                        continue;
                    }
                    if line.starts_with("# ") || line.starts_with("## ") {
                        inside = line == heading;
                    } else if inside && !line.is_empty() {
                        content = true;
                    }
                }
                if !content {
                    return Err(invalid(format!("missing/nonempty {heading} section")));
                }
            }
        }
        if super::super::ContextManager::contains_credentials(&text) {
            return Err(invalid("credential-like content is not active memory"));
        }
        Ok(Self {
            name,
            description,
            kind,
            body,
            frontmatter,
        })
    }

    pub fn id(&self) -> Option<&str> {
        self.frontmatter.get("metadata")?.get("id")?.as_str()
    }

    pub fn links(&self) -> Vec<String> {
        static LINKS: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
            regex::Regex::new(r"\[\[([^\]\r\n]{1,160})\]\]").unwrap()
        });
        LINKS
            .captures_iter(&self.body)
            .map(|c| c[1].to_owned())
            .collect()
    }

    pub(super) fn prepare(mut self, previous: Option<&Self>) -> Result<Vec<u8>> {
        let metadata = self
            .frontmatter
            .get_mut("metadata")
            .and_then(Value::as_mapping_mut)
            .unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        for (field, fallback) in [
            ("id", uuid::Uuid::new_v4().to_string()),
            ("created_at", now.clone()),
        ] {
            let old = previous
                .and_then(|d| d.frontmatter.get("metadata")?.get(field))
                .cloned();
            if let Some(old) = old {
                if metadata.get(field).is_some_and(|new| new != &old) {
                    return Err(invalid(format!("{field} is immutable")));
                }
                metadata.insert(Value::String(field.into()), old);
            } else if !metadata.contains_key(field) {
                metadata.insert(Value::String(field.into()), Value::String(fallback));
            }
        }
        metadata.insert(Value::String("updated_at".into()), Value::String(now));
        let yaml = serde_yaml::to_string(&self.frontmatter)
            .map_err(|_| invalid("frontmatter serialization failed"))?;
        let bytes = format!("---\n{yaml}\n---\n{}", self.body).into_bytes();
        Self::parse(&bytes)?;
        Ok(bytes)
    }
}
