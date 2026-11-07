//! Perennos Tempo Vault: an Obsidian-compatible Markdown vault in Rust.
//!
//! Plain `.md` files stay the source of truth. Everything else here
//! (index, links, search, graph, history) is derived from them and can be
//! rebuilt at any time.

pub mod ask;
pub mod canvas;
pub mod cli;
pub mod diagram;
pub mod graph;
pub mod learn;
pub mod math;
pub mod mcp;
pub mod parse;
pub mod publish;
pub mod query;
pub mod render;
pub mod search;
#[cfg(test)]
mod sec_tests;
pub mod server;
pub mod tempo_home;
pub mod util;
pub mod vault;
pub mod write;
pub mod yaml;
