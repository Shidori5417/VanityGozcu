//! VanityGözcü core library: configuration, checking, rate limiting, scheduling, state,
//! persistence, notifications, the engine and the terminal UI.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod checker;
pub mod claimer;
pub mod config;
pub mod config_edit;
pub mod engine;
pub mod model;
pub mod notify;
pub mod pipeline;
pub mod ratelimit;
pub mod scheduler;
pub mod state;
pub mod store;
pub mod tui;
