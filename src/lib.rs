//! VanityGözcü core library: configuration, checking, rate limiting and scheduling.

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod checker;
pub mod config;
pub mod model;
pub mod ratelimit;
pub mod scheduler;
