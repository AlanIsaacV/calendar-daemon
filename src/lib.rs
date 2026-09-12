//! Local calendar daemon: owns Google authorization, synchronization, reminder
//! scheduling and the local calendar export a visual client reads.
//!
//! The binary in `main.rs` is a thin CLI over these modules. Everything that
//! encodes a rule — which reminders apply, when they fire, what a calendar
//! export looks like — lives here so it can be tested without a network, a
//! keyring or a graphical session.

pub mod auth;
pub mod config;
pub mod control;
pub mod daemon;
pub mod export;
pub mod google;
pub mod model;
pub mod notify;
pub mod reminder;
pub mod store;
pub mod util;
