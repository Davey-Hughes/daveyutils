//! Core library for nudge. Side-effect-free logic used by the CLI and daemon.

pub mod config;
pub mod daemon;
pub mod detect;
pub mod inject;
pub mod ipc;
pub mod job;
pub mod paths;
pub mod queue;
pub mod scheduler;
pub mod target;
pub mod timespec;
