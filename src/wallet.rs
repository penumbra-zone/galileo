use anyhow::Context;
use penumbra_crypto::keys::SpendKey;
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
        let custody_json: serde_json::Value =
            serde_json::from_slice(std::fs::read(path)?.as_slice())?;
        let sk_str = match custody_json["spend_key"].as_str() {
            Some(s) => s,
            None => {
                return Err(anyhow::anyhow!(
                    "'spend_key' field not found in custody JSON file"
                ))
            }
        };
        let spend_key = SpendKey::from_str(sk_str)
            .context(format!("Could not create SpendKey from string: {}", sk_str))?;
        Ok(Self { spend_key })
    }
}
