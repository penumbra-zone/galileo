//! Protobuf definitions for Galileo.

pub use self::galileo::*;
mod galileo {
    pub mod faucet {
        pub mod v1 {
            include!("proto/gen/galileo.faucet.v1.rs");
        }
    }
}

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
