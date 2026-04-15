// This library crate exists solely to enable integration testing (`tests/`).
// It is not a stable public API — all public exports are for test access only.
pub mod config;
pub mod client;
pub mod output;
pub mod errors;
pub mod auth;
pub mod repo;
pub mod push;
pub mod commands;
