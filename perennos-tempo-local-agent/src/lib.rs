//! Perennos Tempo local agent: offline agent loop over Ollama with MCP, RAG,
//! guarded file tools and a persistent workflow file.
pub mod agent;
pub mod budget;
pub mod catalog;
pub mod checkpoint;
pub mod config;
pub mod conv;
pub mod fleet_tools;
pub mod fstools;
pub mod mcp;
pub mod ollama;
pub mod patch;
pub mod products;
pub mod project;
pub mod rag;
pub mod server;
pub mod soundcheck;
pub mod soundcheck_mcp;
pub mod tempo_home;
pub mod tools;
pub mod util;
pub mod voice;
