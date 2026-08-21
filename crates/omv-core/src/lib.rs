//! Shared building blocks for the OpenMedVideo services: configuration,
//! the storage abstraction, playback-token signing, and catalog models.

pub mod config;
pub mod models;
pub mod storage;
pub mod token;

pub use config::Config;
