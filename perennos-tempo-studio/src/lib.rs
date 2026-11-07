//! Perennos Tempo studio: open REAPER and mix and master songs (plan A), with a built-in
//! loudness meter, mixdown and mastering engine as plan B when REAPER is not there.
pub mod config;
pub mod loudness;
pub mod master;
pub mod mcp;
pub mod mix;
pub mod ops;
pub mod reaper;
#[cfg(test)]
mod sec_tests;
pub mod util;
pub mod wav;
