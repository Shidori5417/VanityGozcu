//! VanityGözcü core library: configuration, checking, rate limiting, scheduling, state,
//! persistence and notifications.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod checker;
pub mod config;
pub mod model;
pub mod notify;
pub mod pipeline;
pub mod ratelimit;
pub mod scheduler;
pub mod state;
pub mod store;
