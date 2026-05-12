// This library crate exists solely to enable integration testing (`tests/`).
// It is not a stable public API — all public exports are for test access only.
pub mod auth;
pub mod checksum_verify;
pub mod client;
pub mod commands;
pub mod config;
pub mod errors;
pub mod install_detect;
pub mod output;
pub mod prompts;
pub mod push;
pub mod repo;
