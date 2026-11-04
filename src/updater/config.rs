use ed25519_dalek::VerifyingKey;
use serde::Deserialize;

use super::{Error, Result, feed};

/// Release trust is part of the installed executable. A downloaded config or
/// plan can never substitute a different public key.
#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) manifest_url: String,
    pub(crate) key: VerifyingKey,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbeddedConfig {
    #[serde(default = "enabled_default")]
    enabled: bool,
    manifest_url: String,
    public_key: String,
}

fn enabled_default() -> bool {
    true
}

impl Config {
    /// Disabled builds perform no network activity. Invalid enabled builds
    /// return an error so the application can log it without interrupting UI.
    pub fn embedded() -> Result<Option<Self>> {
        Self::from_json(include_str!("../../Resources/update-config.json"))
    }

    fn from_json(json: &str) -> Result<Option<Self>> {
        let config: EmbeddedConfig = serde_json::from_str(json)?;
        if !config.enabled || (config.manifest_url.is_empty() && config.public_key.is_empty()) {
            return Ok(None);
        }
        feed::validate_https(&config.manifest_url)?;
        let uri: ureq::http::Uri = config
            .manifest_url
            .parse()
            .map_err(|_| Error::new("invalid manifest URL"))?;
        if uri.query().is_some() {
            return Err(Error::new("manifest URL must not contain a query"));
        }
        Ok(Some(Self {
            manifest_url: config.manifest_url,
            key: feed::decode_key(&config.public_key)?,
        }))
    }

    pub fn manifest_url(&self) -> &str {
        &self.manifest_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_and_empty_config_never_construct_a_network_client() {
        assert!(
            Config::from_json(r#"{"enabled":false,"manifest_url":"not a URL","public_key":"bad"}"#)
                .unwrap()
                .is_none()
        );
        assert!(
            Config::from_json(r#"{"manifest_url":"","public_key":""}"#)
                .unwrap()
                .is_none()
        );
        assert!(
            Config::from_json(r#"{"manifest_url":"http://example.com","public_key":"bad"}"#)
                .is_err()
        );
        assert!(
            Config::from_json(
                r#"{"manifest_url":"https://example.com/feed?x=1","public_key":"bad"}"#
            )
            .is_err()
        );
    }
}
