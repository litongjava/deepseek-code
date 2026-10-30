//! Desktop adapters. Shared types are re-exported for existing integrations.
pub use deepseek_code_core::*;
pub mod bridge;
pub mod api;
pub mod commands;
pub mod updater;
pub mod remotion;
pub mod secure_storage;
