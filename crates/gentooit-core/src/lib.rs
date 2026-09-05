//! gentooit-core: core library implementing gentooit workflows.
//!
//! gentooit is a packit-like tool for Gentoo Linux. It automates packaging
//! upstream projects into Gentoo ebuilds: proposing downsteam changes (upstream
//! release -> ebuild PR), building/testing ebuilds, and syncing changes from the
//! downstream ebuild repository back to the upstream project.
//!
//! This crate contains the platform-agnostic core logic. The `gentooit` binary
//! provides the CLI and `gentooit-service` provides the GitHub App/service that
//! automates these workflows.

pub mod build;
pub mod config;
pub mod ebuild;
pub mod github;
pub mod manifest;
pub mod metadata;
pub mod propose;
pub mod repo;
pub mod sync;

pub mod error {
    pub use crate::ebuild::EbuildError;
    pub use crate::manifest::ManifestError;
}
