//! Landlock sandbox integration tests, consolidated into one test binary.
//!
//! Landlock exists only on Linux and behind the `sandbox-landlock` feature, so
//! the whole binary compiles to nothing elsewhere. The crate is named
//! `landlock`, which keeps the CI `filter: "landlock"` matching every test.

#![cfg(all(feature = "sandbox-landlock", target_os = "linux"))]

mod landlock_contract;
mod landlock_spawn_failure;
mod landlock_workspace_boundary;
