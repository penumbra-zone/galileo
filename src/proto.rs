//! Protobuf definitions for Galileo.

/// The file descriptor set.
///
/// See ["Accessing the `protoc` `FileDescriptorSet`"][prost-docs] for more info.
///
/// [prost-docs]: https://docs.rs/prost/latest/prost/#accessing-the-protoc-filedescriptorset
//
//  NB: Using the "no_lfs" suffix prevents matching a catch-all LFS rule.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("proto/gen/proto_descriptor.bin.no_lfs");

pub use self::galileo::*;
mod galileo {
    pub mod faucet {
        pub mod v1 {
            include!("proto/gen/galileo.faucet.v1.rs");
        }
    }
}

/// Vendored Penumbra protobuf types.
///
/// See [`compat`][self::compat] for more information.
pub mod penumbra {
    pub mod core {
        pub mod keys {
            pub mod v1 {
                include!("proto/gen/penumbra.core.keys.v1.rs");
            }
        }
        pub mod txhash {
            pub mod v1 {
                include!("proto/gen/penumbra.core.txhash.v1.rs");
            }
        }
    }
}

/// Compatibility facilities.
///
/// This provides glue to connect our vendored protobuf definitions to the canonical
/// [`penumbra_proto`] definitions.
mod compat {
    // === impl Address ===

    type DomainAddress = penumbra_keys::Address;
    type CanonicalAddress = penumbra_proto::core::keys::v1::Address;
    type VendoredAddress = super::penumbra::core::keys::v1::Address;

    impl Into<CanonicalAddress> for VendoredAddress {
        fn into(self) -> CanonicalAddress {
            let Self { inner, alt_bech32m } = self;
            CanonicalAddress { inner, alt_bech32m }
        }
    }

    impl TryInto<DomainAddress> for VendoredAddress {
        type Error = <DomainAddress as TryFrom<CanonicalAddress>>::Error;
        fn try_into(self) -> Result<DomainAddress, Self::Error> {
            let address: CanonicalAddress = self.into();
            DomainAddress::try_from(address)
        }
    }

    // === impl TransactionId ===

    type DomainTransactionId = penumbra_txhash::TransactionId;
    type CanonicalTransactionId = penumbra_proto::core::txhash::v1::TransactionId;
    type VendoredTransactionId = super::penumbra::core::txhash::v1::TransactionId;

    impl From<CanonicalTransactionId> for VendoredTransactionId {
        fn from(CanonicalTransactionId { inner }: CanonicalTransactionId) -> Self {
            Self { inner }
        }
    }

    impl Into<VendoredTransactionId> for DomainTransactionId {
        fn into(self) -> VendoredTransactionId {
            let id: CanonicalTransactionId = self.into();
            id.into()
        }
    }
}
