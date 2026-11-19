//! Config integration tests, consolidated into one test binary.

mod comment_writer;
#[cfg(feature = "schema-export")]
mod config_reference;
mod cross_build_config_compat;
mod current;
mod nodes_mdns_config;
mod save_preparation;
mod shared_metadata;

// `#[derive(Configurable)]` expands to `crate::config`, `crate::security` and
// `crate::traits` paths, which resolve to these shims at the test crate root.
mod config {
    pub use reiclaw_config::config::*;
}
mod security {
    pub use reiclaw_config::secrets::SecretStore;
}
mod traits {
    pub use reiclaw_config::traits::*;
}
