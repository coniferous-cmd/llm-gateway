//! Unknown-field rejection at every supported configuration level.

use super::ConfigError;
use super::yaml::{self, Node};

pub fn validate(root: &Node) -> Result<(), ConfigError> {
    yaml::check_unknown(root, &["listen", "e2ee", "keys", "conversation_log"])?;

    if let Some(entries) = yaml::field(root, "keys").and_then(yaml::sequence) {
        for entry in entries {
            yaml::check_unknown(entry, &["token", "target", "models"])?;
            if let Some(target) = yaml::field(entry, "target") {
                yaml::check_unknown(
                    target,
                    &["base_url", "api_key", "peer", "expire", "rewrite"],
                )?;
            }
            if let Some(models) = yaml::field(entry, "models").and_then(yaml::sequence) {
                for model in models {
                    yaml::check_unknown(model, &["id", "name"])?;
                }
            }
        }
    }

    if let Some(e2ee) = yaml::field(root, "e2ee") {
        yaml::check_unknown(e2ee, &["listen", "identity_key", "peers"])?;
        if let Some(peers) = yaml::field(e2ee, "peers").and_then(yaml::sequence) {
            for peer in peers {
                yaml::check_unknown(peer, &["name", "base_url", "identity_pub"])?;
            }
        }
    }

    if let Some(log) = yaml::field(root, "conversation_log") {
        yaml::check_unknown(log, &["enabled", "directory", "max_body_bytes"])?;
    }
    Ok(())
}
