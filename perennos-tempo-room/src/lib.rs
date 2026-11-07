//! Perennos Tempo Room: a private group chat where people talk, mention `@tempo`, and Perennos
//! Tempo answers in the thread. Optional bridges to Discord, Telegram and WhatsApp; MCP tools so
//! the agent can read and post; daily transcripts in the Perennos Tempo Vault.

pub mod agent;
pub mod bot;
pub mod bridges;
pub mod cli;
pub mod hub;
pub mod mcp;
pub mod server;
pub mod store;
pub mod team;
pub mod tempo_home;
pub mod util;
pub mod vault;

#[cfg(test)]
pub mod testutil;
