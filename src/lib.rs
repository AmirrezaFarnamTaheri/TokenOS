//! TokenOS — Token-Optimal Agent Execution Kernel (library crate).
//!
//! Deterministic, zero-token routing for LLM agents: route locally, spend
//! upstream tokens only when a cheaper local action cannot finish the task.
//!
//! The binary (`src/main.rs`) is a thin CLI over this library; every
//! subsystem is public so TokenOS can be embedded as a kernel inside other
//! agent runtimes.

pub mod config;
pub mod contextidx;
pub mod engine;
pub mod jsonrescue;
pub mod kernel;
pub mod loopdetect;
pub mod maskcodec;
pub mod payload;
pub mod pricing;
pub mod provider;
pub mod recorder;
pub mod store;
pub mod tokenizer;
pub mod verify;
pub mod webui;
