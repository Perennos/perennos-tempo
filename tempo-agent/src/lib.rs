//! Perennos Tempo local agent, a Rust port of `tempo-agent/tempo_agent.py` and `mcphost.py`.
//!
//! An agent loop over Ollama `/api/chat` tool calling with fast and heavy lanes, a
//! repeat-call guard, a step-cap summary and a persistent workflow file, plus an MCP
//! stdio host that offers the tools of locally configured MCP servers to the model.
pub mod config;
pub mod error;
pub mod http;
pub mod mcp;
pub mod ollama;
pub mod text;
pub mod workflow;

#[cfg(test)]
pub(crate) mod testutil;

pub use error::{Error, Result};
