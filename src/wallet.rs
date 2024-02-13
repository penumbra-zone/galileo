use anyhow::Context;
use penumbra_keys::keys::SpendKey;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// A wallet file storing a single spend authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wallet {
    pub spend_key: SpendKey,
}

impl Wallet {
    /// Read the wallet data from the provided path.
    pub fn load(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let config_contents = std::fs::read_to_string(&path).context(
            "pcli config file not found. hint: run 'pcli init soft-kms import-phrase' to import key material",
        )?;
        let config_toml: toml::Table = toml::from_str(&config_contents)?;

        let sk_str = match config_toml["custody"]["spend_key"].as_str() {
            Some(s) => s,
            None => {
                return Err(anyhow::anyhow!(
                    "'spend_key' field not found in custody TOML file"
                ))
            }
        };
        let spend_key = SpendKey::from_str(sk_str)
            .context(format!("Could not create SpendKey from string: {}", sk_str))?;
        Ok(Self { spend_key })
    }
}
