//! Path-prefix rewrite rule parsing.

use super::ConfigError;
use super::yaml::{self, Node};

/// One validated rule. An empty destination is the `-` delete-prefix marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteRule {
    pub source: String,
    pub destination: String,
}

/// Parse `target.rewrite`. An omitted field yields no rules; an explicit
/// `null` is an error, matching Go.
pub fn parse(node: Option<&Node>, key_index: usize) -> Result<Vec<RewriteRule>, ConfigError> {
    let Some(node) = node else {
        return Ok(Vec::new());
    };
    let Some(items) = yaml::sequence(node) else {
        return Err(ConfigError::at(
            yaml::line(node),
            format!("keys[{key_index}].target.rewrite must be a list of strings"),
        ));
    };
    let mut rules = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let line = yaml::line(item);
        let Some(rule) = yaml::scalar_string(item) else {
            return Err(ConfigError::at(
                line,
                format!("keys[{key_index}].target.rewrite[{index}] must be a string"),
            ));
        };
        let Some((source, destination)) = split_rule(rule) else {
            return Err(ConfigError::at(
                line,
                invalid_rule_message(key_index, index),
            ));
        };
        if !valid_prefix(source) || (destination != "-" && !valid_prefix(destination)) {
            return Err(ConfigError::at(
                line,
                invalid_rule_message(key_index, index),
            ));
        }
        rules.push(RewriteRule {
            source: source.to_string(),
            destination: if destination == "-" {
                String::new()
            } else {
                destination.to_string()
            },
        });
    }
    Ok(rules)
}

fn split_rule(rule: &str) -> Option<(&str, &str)> {
    if rule.matches(':').count() != 1 {
        return None;
    }
    rule.split_once(':')
}

fn invalid_rule_message(key_index: usize, index: usize) -> String {
    format!(
        "keys[{key_index}].target.rewrite[{index}] must be source:destination with an absolute escaped source path and an absolute escaped destination path or '-' to delete the prefix (no authority, query, fragment or whitespace)"
    )
}

/// A literal escaped path prefix. Go validates this by unescaping and
/// re-escaping with `url.URL.EscapedPath()`, which permits exactly the raw
/// bytes below plus well-formed `%HH` escapes.
pub fn valid_prefix(prefix: &str) -> bool {
    if !prefix.starts_with('/') || prefix.starts_with("//") {
        return false;
    }
    if prefix
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '?' || c == '#')
    {
        return false;
    }
    let bytes = prefix.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
            continue;
        }
        if !allowed_raw_path_byte(byte) {
            return false;
        }
        i += 1;
    }
    true
}

fn allowed_raw_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_'
                | b'.'
                | b'~'
                | b'$'
                | b'&'
                | b'+'
                | b','
                | b'/'
                | b':'
                | b';'
                | b'='
                | b'@'
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::yaml::load;

    fn rules(source: &str) -> Result<Vec<RewriteRule>, ConfigError> {
        let docs = load(source).expect("parse");
        let Some(root) = docs.into_iter().next() else {
            return Ok(Vec::new());
        };
        parse(yaml::field(&root, "rewrite"), 0)
    }

    #[test]
    fn accepts_documented_rules() {
        let parsed =
            rules("rewrite: ['/api:/v1', '/openai:-', '/:/v1/', '/api/a%2Fb:/v2']").unwrap();
        assert_eq!(
            parsed,
            vec![
                RewriteRule {
                    source: "/api".into(),
                    destination: "/v1".into()
                },
                RewriteRule {
                    source: "/openai".into(),
                    destination: String::new()
                },
                RewriteRule {
                    source: "/".into(),
                    destination: "/v1/".into()
                },
                RewriteRule {
                    source: "/api/a%2Fb".into(),
                    destination: "/v2".into()
                },
            ]
        );
        assert!(rules("").unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_rules_with_go_messages() {
        for rule in [
            "rewrite: null",
            "rewrite: ['/api']",
            "rewrite: ['/api:']",
            "rewrite: ['api:/v1']",
            "rewrite: ['//api:/v1']",
            "rewrite: ['/api:/v1?x']",
            "rewrite: ['/api:%zz']",
            "rewrite: [1]",
        ] {
            let err = rules(rule).unwrap_err().to_string();
            assert!(err.contains("line "), "{rule}: {err}");
        }
    }

    #[test]
    fn error_uses_element_line() {
        let source = "a: 1\nb: 2\nc: 3\nd: 4\nrewrite:\n  - '/ok:/v1'\n  - 'bad'\n";
        let err = rules(source).unwrap_err().to_string();
        assert_eq!(
            err,
            "line 7: keys[0].target.rewrite[1] must be source:destination with an absolute escaped source path and an absolute escaped destination path or '-' to delete the prefix (no authority, query, fragment or whitespace)"
        );
    }
}
