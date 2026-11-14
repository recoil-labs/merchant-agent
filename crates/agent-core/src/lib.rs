//! The RecoilPay Merchant Agent's logic.
//!
//! The agent moves real people's money out of a merchant's bank account
//! because of messages from a server it should not blindly trust. Everything
//! that decides whether money moves lives in this library, so every decision
//! can be tested without a network, a bank or a chain. The binary
//! (`merchant-agent`) only wires it together.
//!
//! The design, and why each piece exists, is in `merchant-agent/docs/design.md`.

pub mod aggregator;
pub mod audit;
pub mod chain;
pub mod config;
pub mod console;
pub mod doctor;
pub mod engine;
pub mod health;
pub mod init;
pub mod logging;
pub mod machine;
pub mod metrics;
pub mod money;
pub mod notify;
pub mod payee;
pub mod pii;
pub mod policy;
pub mod rail;
pub mod runner;
pub mod secrets;
pub mod store;

/// The version of the config file format this build reads.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;
