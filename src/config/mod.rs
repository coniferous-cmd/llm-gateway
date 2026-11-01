//! Startup configuration: strict YAML parsing, validation, and defaults.
//!
//! The configuration is immutable after [`load`]. Errors carry the source line
//! whenever one is available and never repeat scalar values, which may be
//! secrets.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use time::OffsetDateTime;

use yaml::Node;

pub mod conversation;
pub mod expire;
pub mod listen;
pub mod origin;
pub mod rewrite;
pub mod schema;
pub mod yaml;

pub use conversation::ConversationLog;
pub use rewrite::RewriteRule;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8080";
/// The encrypted peer port used when `e2ee` is enabled without an explicit
/// listen address. Loopback by default; cross-machine deployments must set it
/// explicitly (and firewall it).
pub const DEFAULT_E2EE_LISTEN: &str = "127.0.0.1:8081";

/// A configuration diagnostic. A missing line means the error is not tied to
/// one node (for example an empty `keys` section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    line: Option<usize>,
    message: String,
}

impl ConfigError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self {
            line: None,
            message: message.into(),
        }
    }

    pub fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
        }
    }

    pub fn line(&self) -> Option<usize> {
        self.line
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// The model name callers put in requests.
    pub id: String,
    /// The model name the provider receives.
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub base_url: String,
    pub api_key: String,
    /// Names an `e2ee.peers` entry; the request is forwarded over a Noise
    /// session instead of plain HTTP. Mutually exclusive with `base_url`.
    pub peer: String,
    /// When set, the target stops being used once the wall clock passes it.
    pub expire: Option<OffsetDateTime>,
    pub rewrite: Vec<RewriteRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub target: Target,
    pub models: Vec<Model>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub base_url: String,
    pub identity_pub: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct E2ee {
    pub listen: String,
    pub identity_key: String,
    pub peers: Vec<Peer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub conversation_log: ConversationLog,
    pub listen: String,
    /// `None` when the gateway has no encrypted peer channel.
    pub e2ee: Option<E2ee>,
    pub keys: HashMap<String, Vec<Entry>>,
}

/// The default directory holding the config, pid and log files.
pub fn default_dir() -> Result<PathBuf, ConfigError> {
    let home = home_dir().ok_or_else(|| {
        ConfigError::msg("resolve home directory: operating system home directory is unavailable")
    })?;
    Ok(home.join(".config").join("llm-gateway"))
}

/// The configuration location used when `--config` is not given.
pub fn default_path() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("config.yaml"))
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let data = std::fs::read_to_string(path)
        .map_err(|err| ConfigError::msg(format!("read config: {err}")))?;
    let mut cfg = parse(&data)?;
    if !cfg.conversation_log.directory.is_absolute() {
        let absolute = std::path::absolute(path)
            .map_err(|err| ConfigError::msg(format!("resolve config directory: {err}")))?;
        let base = absolute
            .parent()
            .ok_or_else(|| ConfigError::msg("resolve config directory: no parent"))?;
        cfg.conversation_log.directory = base.join(&cfg.conversation_log.directory);
    }
    Ok(cfg)
}

pub fn parse(data: &str) -> Result<Config, ConfigError> {
    let mut documents = yaml::load(data)?;
    if documents.is_empty() {
        return Err(ConfigError::msg("config: keys must not be empty"));
    }
    if documents.len() > 1 {
        return Err(ConfigError::msg(
            "parse config: expected a single YAML document",
        ));
    }
    let root = yaml::expand_merges(documents.remove(0))?;
    if yaml::mapping(&root).is_none() {
        return Err(ConfigError::msg(
            "parse config: cannot decode value into a configuration mapping",
        ));
    }
    schema::validate(&root)?;

    let mut cfg = Config {
        conversation_log: ConversationLog::default(),
        listen: DEFAULT_LISTEN.to_string(),
        e2ee: None,
        keys: HashMap::new(),
    };

    if let Some(node) = yaml::field(&root, "listen") {
        let text = yaml::scalar_string(node)
            .ok_or_else(|| ConfigError::at(yaml::line(node), "listen must be a string"))?;
        if !text.is_empty() {
            cfg.listen = text.to_string();
        }
    }
    if !listen::valid(&cfg.listen) {
        return Err(ConfigError::at(
            yaml::field_line(&root, "listen"),
            "listen must be a host:numeric-port address with port 0-65535",
        ));
    }
    cfg.conversation_log = conversation::parse(&root)?;

    let e2ee_node = yaml::field(&root, "e2ee");
    let mut peer_by_name: HashMap<String, Peer> = HashMap::new();
    let mut peer_addrs: Vec<(String, String)> = Vec::new();
    if let Some(node) = e2ee_node.filter(|node| !yaml::is_null(node)) {
        let Some(_) = yaml::mapping(node) else {
            return Err(ConfigError::at(yaml::line(node), "e2ee must be a mapping"));
        };
        let mut e2ee = E2ee {
            listen: DEFAULT_E2EE_LISTEN.to_string(),
            identity_key: String::new(),
            peers: Vec::new(),
        };
        if let Some(listen_node) = yaml::field(node, "listen") {
            let text = yaml::scalar_string(listen_node).ok_or_else(|| {
                ConfigError::at(yaml::line(listen_node), "e2ee.listen must be a string")
            })?;
            if !text.is_empty() {
                e2ee.listen = text.to_string();
            }
        }
        if !listen::valid(&e2ee.listen) {
            return Err(ConfigError::at(
                yaml::field_line(node, "listen"),
                "e2ee.listen must be a host:numeric-port address with port 0-65535",
            ));
        }
        let key = string_field(node, "identity_key", "e2ee")?.unwrap_or_default();
        if key.trim().is_empty() {
            return Err(ConfigError::at(
                yaml::field_line(node, "identity_key"),
                "e2ee.identity_key is required when e2ee is configured",
            ));
        }
        if !valid_base64_32(&key) {
            return Err(ConfigError::at(
                yaml::field_line(node, "identity_key"),
                "e2ee.identity_key must be a base64-encoded 32-byte X25519 private key",
            ));
        }
        e2ee.identity_key = key;

        if let Some(peers_node) = yaml::field(node, "peers").filter(|n| !yaml::is_null(n)) {
            let Some(peers) = yaml::sequence(peers_node) else {
                return Err(ConfigError::at(
                    yaml::line(peers_node),
                    "e2ee.peers must be a list",
                ));
            };
            for (index, peer_node) in peers.iter().enumerate() {
                let line = yaml::line(peer_node);
                if yaml::mapping(peer_node).is_none() {
                    return Err(ConfigError::at(
                        line,
                        format!("e2ee.peers[{index}] must be a mapping"),
                    ));
                }
                let context = format!("e2ee.peers[{index}]");
                let name = string_field(peer_node, "name", &context)?.unwrap_or_default();
                if name.trim().is_empty() {
                    return Err(ConfigError::at(
                        line,
                        format!("e2ee.peers[{index}].name is required"),
                    ));
                }
                if peer_by_name.contains_key(&name) {
                    return Err(ConfigError::at(
                        line,
                        format!("e2ee.peers[{index}].name {name:?} is duplicated"),
                    ));
                }
                let base_raw = string_field(peer_node, "base_url", &context)?.unwrap_or_default();
                let base = origin::parse_origin(&base_raw).map_err(|_| {
                    ConfigError::at(
                        line,
                        format!(
                            "e2ee.peers[{index}].base_url must be an http(s) origin without credentials, path, query or fragment"
                        ),
                    )
                })?;
                let public = string_field(peer_node, "identity_pub", &context)?.unwrap_or_default();
                if public.trim().is_empty() {
                    return Err(ConfigError::at(
                        line,
                        format!("e2ee.peers[{index}].identity_pub is required"),
                    ));
                }
                if !valid_base64_32(&public) {
                    return Err(ConfigError::at(
                        line,
                        format!(
                            "e2ee.peers[{index}].identity_pub must be a base64-encoded 32-byte X25519 public key"
                        ),
                    ));
                }
                let peer = Peer {
                    name,
                    base_url: base,
                    identity_pub: public,
                };
                if let Some(addr) = origin::origin_addr(&peer.base_url) {
                    peer_addrs.push((peer.name.clone(), addr));
                }
                peer_by_name.insert(peer.name.clone(), peer.clone());
                e2ee.peers.push(peer);
            }
        }
        cfg.e2ee = Some(e2ee);
    }

    let Some(keys_node) = yaml::field(&root, "keys") else {
        return Err(ConfigError::msg("config: keys must not be empty"));
    };
    let Some(entries) = yaml::sequence(keys_node) else {
        return Err(ConfigError::msg(
            "parse config: cannot decode value into keys",
        ));
    };
    if entries.is_empty() {
        return Err(ConfigError::msg("config: keys must not be empty"));
    }

    let mut plain_addrs: HashMap<String, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if yaml::mapping(entry).is_none() {
            return Err(ConfigError::at(
                yaml::line(entry),
                format!("keys[{index}] must be a mapping"),
            ));
        }
        let context = format!("keys[{index}]");
        let token = string_field(entry, "token", &context)?.unwrap_or_default();
        if token.trim().is_empty() {
            return Err(ConfigError::at(
                yaml::field_line(entry, "token"),
                format!("keys[{index}].token is required"),
            ));
        }

        let target_node = yaml::field(entry, "target");
        let target = match target_node {
            Some(node) if yaml::is_null(node) => {
                return Err(ConfigError::at(
                    yaml::line(node),
                    format!("keys[{index}].target must set one of base_url or peer"),
                ));
            }
            Some(node) if yaml::mapping(node).is_some() => node,
            Some(node) => {
                return Err(ConfigError::at(
                    yaml::line(node),
                    format!("keys[{index}].target must be a mapping"),
                ));
            }
            None => {
                return Err(ConfigError::at(
                    yaml::line(entry),
                    format!("keys[{index}].target must set one of base_url or peer"),
                ));
            }
        };
        let target_context = format!("keys[{index}].target");
        let base_raw = string_field(target, "base_url", &target_context)?.unwrap_or_default();
        let peer_raw = string_field(target, "peer", &target_context)?.unwrap_or_default();
        let has_base = !base_raw.trim().is_empty();
        let has_peer = !peer_raw.trim().is_empty();
        if has_base && has_peer {
            return Err(ConfigError::at(
                yaml::field_line(target, "base_url"),
                format!("keys[{index}].target must set exactly one of base_url or peer, not both"),
            ));
        }

        let mut parsed_target = Target::default();
        if has_peer {
            let name = peer_raw.trim();
            let Some(peer) = peer_by_name.get(name) else {
                return Err(ConfigError::at(
                    yaml::field_line(target, "peer"),
                    format!("keys[{index}].target.peer {name:?} is not declared in e2ee.peers"),
                ));
            };
            parsed_target.base_url = peer.base_url.clone();
            parsed_target.peer = peer_raw;
        } else if has_base {
            let base = origin::parse_origin(&base_raw).map_err(|_| {
                ConfigError::at(
                    yaml::field_line(target, "base_url"),
                    format!(
                        "keys[{index}].target.base_url must be an http(s) origin without credentials, path, query or fragment"
                    ),
                )
            })?;
            if let Some(addr) = origin::origin_addr(&base) {
                plain_addrs.insert(addr, index);
            }
            parsed_target.base_url = base;
        } else {
            return Err(ConfigError::at(
                yaml::line(target),
                format!("keys[{index}].target must set one of base_url or peer"),
            ));
        }
        parsed_target.api_key =
            string_field(target, "api_key", &target_context)?.unwrap_or_default();
        if let Some(node) = yaml::field(target, "expire") {
            parsed_target.expire = expire::parse(node)?;
        }
        parsed_target.rewrite = rewrite::parse(yaml::field(target, "rewrite"), index)?;

        let prior = cfg.keys.get(&token).cloned();
        let models = parse_models(entry, prior.as_deref(), index)?;
        cfg.keys.entry(token).or_default().push(Entry {
            target: parsed_target,
            models,
        });
    }

    // A peer's e2ee address must never collide with a plaintext target: the
    // transport decides encryption by destination address alone.
    if let Some(node) = e2ee_node {
        for (name, addr) in &peer_addrs {
            if let Some(&index) = plain_addrs.get(addr) {
                return Err(ConfigError::at(
                    yaml::line(node),
                    format!(
                        "e2ee peer {name:?} address {addr} collides with keys[{index}].target.base_url"
                    ),
                ));
            }
        }
    }

    Ok(cfg)
}

fn parse_models(
    entry: &Node,
    prior: Option<&[Entry]>,
    key_index: usize,
) -> Result<Vec<Model>, ConfigError> {
    let Some(node) = yaml::field(entry, "models") else {
        return Ok(Vec::new());
    };
    if yaml::is_null(node) {
        return Ok(Vec::new());
    }
    let Some(items) = yaml::sequence(node) else {
        return Err(ConfigError::at(
            yaml::line(node),
            format!("keys[{key_index}].models must be a list"),
        ));
    };
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(prior) = prior {
        for entry in prior {
            for model in &entry.models {
                seen.insert(model.id.clone());
            }
        }
    }
    let mut models = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let line = yaml::line(item);
        let context = format!("keys[{key_index}].models[{index}]");
        if yaml::mapping(item).is_none() {
            return Err(ConfigError::at(
                line,
                format!("{context} must be a mapping"),
            ));
        }
        let id = string_field(item, "id", &context)?.unwrap_or_default();
        if id.trim().is_empty() {
            return Err(ConfigError::at(
                yaml::field_line(item, "id"),
                format!("{context}.id is required"),
            ));
        }
        let name = string_field(item, "name", &context)?.unwrap_or_default();
        if name.trim().is_empty() {
            return Err(ConfigError::at(
                yaml::field_line(item, "name"),
                format!("{context}.name is required"),
            ));
        }
        if !seen.insert(id.clone()) {
            return Err(ConfigError::at(
                yaml::field_line(item, "id"),
                format!("{context}.id {id:?} is already declared for this token"),
            ));
        }
        models.push(Model { id, name });
    }
    Ok(models)
}

fn string_field(node: &Node, name: &str, context: &str) -> Result<Option<String>, ConfigError> {
    let Some(value) = yaml::field(node, name) else {
        return Ok(None);
    };
    let Some(text) = yaml::scalar_string(value) else {
        return Err(ConfigError::at(
            yaml::line(value),
            format!("{context}.{name} must be a string"),
        ));
    };
    Ok(Some(text.to_string()))
}

fn valid_base64_32(value: &str) -> bool {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .is_ok_and(|bytes| bytes.len() == 32)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
        let drive = std::env::var_os("HOMEDRIVE")?;
        let path = std::env::var_os("HOMEPATH")?;
        let mut home = PathBuf::from(drive);
        home.push(path);
        Some(home)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n";

    #[test]
    fn parses_minimal_valid_config() {
        let cfg = parse(VALID).unwrap();
        assert_eq!(cfg.listen, DEFAULT_LISTEN);
        assert_eq!(cfg.keys.len(), 1);
        let entry = &cfg.keys["dev"][0];
        assert_eq!(entry.target.base_url, "https://example.com");
        assert_eq!(entry.target.api_key, "");
        assert!(entry.models.is_empty());
        assert_eq!(cfg.conversation_log, ConversationLog::default());
    }

    #[test]
    fn requires_non_empty_keys_and_token() {
        for (source, expected) in [
            ("", "keys must not be empty"),
            ("listen: ':9'", "keys must not be empty"),
            ("keys: []\n", "keys must not be empty"),
            (
                "keys:\n  - target:\n      base_url: https://example.com\n",
                "line 2: keys[0].token is required",
            ),
        ] {
            let err = parse(source).unwrap_err().to_string();
            assert!(err.contains(expected), "{source}: {err}");
        }
    }

    #[test]
    fn normalizes_base_url_and_rejects_bad_origins() {
        let cfg =
            parse("keys:\n  - token: dev\n    target:\n      base_url: ' https://example.com/ '\n")
                .unwrap();
        assert_eq!(cfg.keys["dev"][0].target.base_url, "https://example.com");
        for base in [
            "https://example.com/v1",
            "https://example.com?x=1",
            "https://user@example.com",
            "ftp://example.com",
        ] {
            let source = format!("keys:\n  - token: dev\n    target:\n      base_url: '{base}'\n");
            let err = parse(&source).unwrap_err().to_string();
            assert!(err.contains("base_url"), "{base}: {err}");
        }
    }

    #[test]
    fn same_token_appends_in_order_and_rejects_duplicate_model_ids() {
        let cfg = parse(
            "keys:\n  - token: dev\n    target: {base_url: 'https://a.example'}\n    models: [{id: m, name: a}]\n  - token: dev\n    target: {base_url: 'https://b.example'}\n    models: [{id: n, name: b}]\n",
        )
        .unwrap();
        assert_eq!(cfg.keys["dev"].len(), 2);
        assert_eq!(cfg.keys["dev"][1].target.base_url, "https://b.example");

        let err = parse(
            "keys:\n  - token: dev\n    target: {base_url: 'https://a.example'}\n    models: [{id: m, name: a}]\n  - token: dev\n    target: {base_url: 'https://b.example'}\n    models: [{id: m, name: b}]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already declared"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields_at_every_level() {
        for source in [
            "typo: 1\nkeys: []\n",
            "keys:\n  - typo: 1\n",
            "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n      typo: 1\n",
            "keys:\n  - token: dev\n    target: {base_url: https://example.com}\n    models: [{id: m, name: n, typo: 1}]\n",
            "e2ee:\n  typo: 1\n  identity_key: x\nkeys:\n  - token: dev\n    target: {base_url: https://example.com}\n",
            "conversation_log:\n  typo: 1\nkeys:\n  - token: dev\n    target: {base_url: https://example.com}\n",
        ] {
            let err = parse(source).unwrap_err().to_string();
            assert!(
                err.contains("unknown configuration field"),
                "{source}: {err}"
            );
        }
    }

    #[test]
    fn e2ee_peer_resolution_and_collision() {
        let identity = base64::engine::general_purpose::STANDARD.encode([0_u8; 32]);
        let public = base64::engine::general_purpose::STANDARD.encode([1_u8; 32]);
        let source = format!(
            "e2ee:\n  identity_key: '{identity}'\n  peers:\n    - name: c\n      base_url: 'http://127.0.0.1:8081'\n      identity_pub: '{public}'\nkeys:\n  - token: dev\n    target:\n      peer: c\n      api_key: sk-virt-on-c\n"
        );
        let cfg = parse(&source).unwrap();
        let entry = &cfg.keys["dev"][0];
        assert_eq!(entry.target.peer, "c");
        assert_eq!(entry.target.base_url, "http://127.0.0.1:8081");
        assert_eq!(entry.target.api_key, "sk-virt-on-c");

        let collision = format!(
            "e2ee:\n  identity_key: '{identity}'\n  peers:\n    - name: c\n      base_url: 'http://127.0.0.1:8081'\n      identity_pub: '{public}'\nkeys:\n  - token: dev\n    target:\n      base_url: 'http://127.0.0.1:8081'\n"
        );
        let err = parse(&collision).unwrap_err().to_string();
        assert!(err.contains("collides"), "{err}");
    }

    #[test]
    fn merge_keys_are_expanded_before_parsing() {
        let source = "keys:\n  - &base\n    token: dev\n    target:\n      base_url: https://example.com\n  - <<: *base\n    token: other\n";
        let cfg = parse(source).unwrap();
        assert_eq!(cfg.keys["dev"][0].target.base_url, "https://example.com");
        assert_eq!(cfg.keys["other"][0].target.base_url, "https://example.com");
    }
}
