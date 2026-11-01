//! Parsing for the opt-in `conversation_log` section.

use std::path::PathBuf;

use super::ConfigError;
use super::yaml::{self, Node};

pub const DEFAULT_MAX_BODY_BYTES: usize = 8 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationLog {
    pub enabled: bool,
    pub directory: PathBuf,
    pub max_body_bytes: usize,
}

impl Default for ConversationLog {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: PathBuf::from("logs/conversations"),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

pub fn parse(root: &Node) -> Result<ConversationLog, ConfigError> {
    let mut out = ConversationLog::default();
    let Some(node) = yaml::field(root, "conversation_log") else {
        return Ok(out);
    };
    let Some(map) = yaml::mapping(node) else {
        return Err(ConfigError::at(
            yaml::line(node),
            "conversation_log must be a mapping",
        ));
    };
    for (key, value) in map {
        let Some(name) = yaml::key_name(key) else {
            continue;
        };
        let invalid = || {
            ConfigError::at(
                yaml::line(value),
                format!("invalid conversation_log.{name}"),
            )
        };
        match name {
            "enabled" => {
                out.enabled = yaml::scalar_bool(value).ok_or_else(invalid)?;
            }
            "directory" => {
                let directory = yaml::scalar_string(value).ok_or_else(invalid)?;
                if directory.trim().is_empty() {
                    return Err(invalid());
                }
                out.directory = PathBuf::from(directory);
            }
            "max_body_bytes" => {
                let limit = yaml::scalar_integer(value).ok_or_else(invalid)?;
                if limit <= 0 {
                    return Err(invalid());
                }
                out.max_body_bytes = usize::try_from(limit).map_err(|_| invalid())?;
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::yaml::load;

    fn parse_source(source: &str) -> Result<ConversationLog, ConfigError> {
        let docs = load(source).expect("parse");
        let root = docs.into_iter().next().unwrap();
        parse(&root)
    }

    #[test]
    fn defaults_when_absent() {
        assert_eq!(
            parse_source("keys: []\n").unwrap(),
            ConversationLog::default()
        );
    }

    #[test]
    fn accepts_enabled_overrides() {
        let parsed = parse_source(
            "conversation_log:\n  enabled: true\n  directory: out\n  max_body_bytes: 1024\n",
        )
        .unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.directory, PathBuf::from("out"));
        assert_eq!(parsed.max_body_bytes, 1024);
    }

    #[test]
    fn rejects_invalid_values_without_leaking_them() {
        for (source, expected) in [
            (
                "conversation_log: []\n",
                "conversation_log must be a mapping",
            ),
            (
                "conversation_log:\n  enabled: secret-value\n",
                "invalid conversation_log.enabled",
            ),
            (
                "conversation_log:\n  max_body_bytes: 0\n",
                "invalid conversation_log.max_body_bytes",
            ),
            (
                "conversation_log:\n  max_body_bytes: -1\n",
                "invalid conversation_log.max_body_bytes",
            ),
            (
                "conversation_log:\n  directory: '   '\n",
                "invalid conversation_log.directory",
            ),
        ] {
            let err = parse_source(source).unwrap_err().to_string();
            assert!(err.contains(expected), "{source}: {err}");
            assert!(!err.contains("secret-value"), "{err}");
        }
    }
}
