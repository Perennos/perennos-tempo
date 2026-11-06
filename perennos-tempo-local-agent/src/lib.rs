//! Perennos Tempo local agent: offline agent loop over Ollama with MCP, RAG,
//! guarded file tools and a persistent workflow file.
pub mod agent;
pub mod config;
pub mod conv;
pub mod fstools;
pub mod mcp;
pub mod ollama;
pub mod products;
pub mod rag;
pub mod server;
pub mod tools;
pub mod util;
