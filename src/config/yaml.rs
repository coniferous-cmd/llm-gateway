//! Positioned YAML helpers.
//!
//! `saphyr` resolves aliases and gives every node a source span. The loader
//! does not expand YAML merge keys (`<<`), so this module does that itself,
//! matching the Go implementation's precedence: explicit fields win, and the
//! first mapping in a merge sequence wins over later ones.

use std::collections::HashSet;

use saphyr::{LoadableYamlNode, MarkedYamlOwned, ScalarOwned, YamlDataOwned};

use super::ConfigError;

pub type Node = MarkedYamlOwned;

pub fn load(data: &str) -> Result<Vec<Node>, ConfigError> {
    MarkedYamlOwned::load_from_str(data).map_err(|err| {
        // A self-referential alias is rejected by the loader's alias limit.
        let text = err.to_string();
        if text.to_ascii_lowercase().contains("alias") {
            ConfigError::msg("parse config: cyclic YAML alias")
        } else {
            ConfigError::msg(format!("parse config: {text}"))
        }
    })
}

pub fn line(node: &Node) -> usize {
    node.span.start.line()
}

/// The string value of a scalar node, including tagged scalars.
pub fn scalar_string(node: &Node) -> Option<&str> {
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::String(value)) => Some(value),
        YamlDataOwned::Representation(value, _, _) => Some(value),
        YamlDataOwned::Tagged(_, inner) => scalar_string(inner),
        _ => None,
    }
}

pub fn scalar_bool(node: &Node) -> Option<bool> {
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::Boolean(value)) => Some(*value),
        YamlDataOwned::Representation(value, _, tag) => {
            if tag.as_ref().is_some_and(|tag| tag.suffix == "bool") {
                match value.as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            } else {
                None
            }
        }
        YamlDataOwned::Tagged(_, inner) => scalar_bool(inner),
        _ => None,
    }
}

pub fn scalar_integer(node: &Node) -> Option<i64> {
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::Integer(value)) => Some(*value),
        YamlDataOwned::Representation(value, _, tag) => {
            if tag.as_ref().is_some_and(|tag| tag.suffix == "int") {
                value.parse().ok()
            } else {
                None
            }
        }
        YamlDataOwned::Tagged(_, inner) => scalar_integer(inner),
        _ => None,
    }
}

pub fn is_null(node: &Node) -> bool {
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::Null) => true,
        YamlDataOwned::Tagged(_, inner) => is_null(inner),
        _ => false,
    }
}

pub fn mapping(node: &Node) -> Option<&saphyr::AnnotatedMappingOwned<Node>> {
    match &node.data {
        YamlDataOwned::Mapping(map) => Some(map),
        YamlDataOwned::Tagged(_, inner) => mapping(inner),
        _ => None,
    }
}

pub fn sequence(node: &Node) -> Option<&saphyr::AnnotatedSequenceOwned<Node>> {
    match &node.data {
        YamlDataOwned::Sequence(items) => Some(items),
        YamlDataOwned::Tagged(_, inner) => sequence(inner),
        _ => None,
    }
}

/// A mapping key as a string. Config keys are always plain scalars.
pub fn key_name(node: &Node) -> Option<&str> {
    scalar_string(node)
}

pub fn field<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
    match &node.data {
        YamlDataOwned::Mapping(map) => map.get(&key_node(name)),
        YamlDataOwned::Tagged(_, inner) => field(inner, name),
        _ => None,
    }
}

/// The line of a field, or of its enclosing mapping when it is absent. Go's
/// `field` helper uses the enclosing node so missing-field errors still point
/// at an actionable line.
pub fn field_line(node: &Node, name: &str) -> usize {
    field(node, name).map_or_else(|| line(node), line)
}

fn key_node(name: &str) -> Node {
    Node::value_from_str(name)
}

/// Expand merge keys recursively. Aliases have already been resolved by the
/// loader; a surviving `Alias` node means the document is cyclic.
pub fn expand_merges(node: Node) -> Result<Node, ConfigError> {
    match node.data {
        YamlDataOwned::Sequence(items) => {
            let mut expanded = Vec::with_capacity(items.len());
            for item in items {
                expanded.push(expand_merges(item)?);
            }
            Ok(Node {
                span: node.span,
                data: YamlDataOwned::Sequence(expanded),
            })
        }
        YamlDataOwned::Mapping(map) => {
            let mut explicit = HashSet::new();
            let mut fields: Vec<(Node, Node)> = Vec::new();
            let mut merges: Vec<Node> = Vec::new();
            for (key, value) in map {
                let key = expand_merges(key)?;
                let value = expand_merges(value)?;
                if key_name(&key) == Some("<<") {
                    merges.push(value);
                } else {
                    if let Some(name) = key_name(&key) {
                        explicit.insert(name.to_string());
                    }
                    fields.push((key, value));
                }
            }
            for merge in merges {
                merge_into(&merge, &mut explicit, &mut fields)?;
            }
            Ok(Node {
                span: node.span,
                data: YamlDataOwned::Mapping(fields.into_iter().collect()),
            })
        }
        YamlDataOwned::Tagged(tag, inner) => {
            let inner = expand_merges(*inner)?;
            Ok(Node {
                span: node.span,
                data: YamlDataOwned::Tagged(tag, Box::new(inner)),
            })
        }
        YamlDataOwned::Alias(_) => Err(ConfigError::at(line(&node), "cyclic YAML alias")),
        other => Ok(Node {
            span: node.span,
            data: other,
        }),
    }
}

fn merge_into(
    value: &Node,
    explicit: &mut HashSet<String>,
    fields: &mut Vec<(Node, Node)>,
) -> Result<(), ConfigError> {
    match &value.data {
        YamlDataOwned::Mapping(map) => {
            for (key, item) in map {
                let Some(name) = key_name(key) else {
                    return Err(ConfigError::at(
                        line(key),
                        "YAML merge must reference a mapping",
                    ));
                };
                if name == "<<" {
                    merge_into(item, explicit, fields)?;
                } else if explicit.insert(name.to_string()) {
                    fields.push((key.clone(), item.clone()));
                }
            }
            Ok(())
        }
        YamlDataOwned::Sequence(items) => {
            for item in items {
                merge_into(item, explicit, fields)?;
            }
            Ok(())
        }
        YamlDataOwned::Tagged(_, inner) => merge_into(inner, explicit, fields),
        _ => Err(ConfigError::at(
            line(value),
            "YAML merge must reference a mapping",
        )),
    }
}

/// Reject keys outside `allowed` in one mapping. Only mappings are checked;
/// malformed shapes are reported later by semantic parsing.
pub fn check_unknown(node: &Node, allowed: &[&str]) -> Result<(), ConfigError> {
    let Some(map) = mapping(node) else {
        return Ok(());
    };
    for (key, _) in map {
        let Some(name) = key_name(key) else {
            continue;
        };
        if !allowed.contains(&name) {
            return Err(ConfigError::at(
                line(key),
                format!("unknown configuration field {name:?}"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_one_based_and_merge_keys_expand() {
        let source = "base: &base\n  token: dev\n  target:\n    base_url: https://example.com\nentry:\n  <<: *base\n  token: override\n";
        let docs = load(source).expect("parse");
        let root = expand_merges(docs.into_iter().next().unwrap()).expect("expand");
        let entry = field(&root, "entry").expect("entry");
        assert_eq!(line(entry), 6);
        assert_eq!(
            scalar_string(field(entry, "token").unwrap()).unwrap(),
            "override"
        );
        assert_eq!(line(field(entry, "target").unwrap()), 4);
        // Merged children keep the span of the anchor's original lines.
        let target = field(entry, "target").expect("target");
        assert_eq!(
            scalar_string(field(target, "base_url").unwrap()).unwrap(),
            "https://example.com"
        );
        assert_eq!(line(field(target, "base_url").unwrap()), 4);
    }

    #[test]
    fn merge_sequence_prefers_first_mapping() {
        let source = "a: &a {x: first, y: a}\nb: &b {y: b, z: second}\nc:\n  <<: [*a, *b]\n";
        let docs = load(source).expect("parse");
        let root = expand_merges(docs.into_iter().next().unwrap()).expect("expand");
        let c = field(&root, "c").expect("c");
        assert_eq!(scalar_string(field(c, "x").unwrap()).unwrap(), "first");
        assert_eq!(scalar_string(field(c, "y").unwrap()).unwrap(), "a");
        assert_eq!(scalar_string(field(c, "z").unwrap()).unwrap(), "second");
    }

    #[test]
    fn unknown_fields_report_key_line() {
        let source = "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n      typo: 1\n";
        let docs = load(source).expect("parse");
        let root = expand_merges(docs.into_iter().next().unwrap()).expect("expand");
        let keys = field(&root, "keys").expect("keys");
        let item = &sequence(keys).unwrap()[0];
        let target = field(item, "target").expect("target");
        let err = check_unknown(
            target,
            &["base_url", "api_key", "peer", "expire", "rewrite"],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "line 5: unknown configuration field \"typo\""
        );
    }
}
