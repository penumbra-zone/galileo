use std::collections::BTreeMap;

use penumbra_crypto::asset;

#[derive(Clone, Debug, Default)]
pub struct Balance {
    pub(super) ready: BTreeMap<asset::Id, u64>,
    pub(super) submitted_change: BTreeMap<asset::Id, u64>,
    pub(super) submitted_spend: BTreeMap<asset::Id, u64>,
}
