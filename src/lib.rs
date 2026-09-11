//! duTime — track disk usage over time.
//!
//! `du` answers "what is big *now*". duTime answers "what *grew*, and *when*" —
//! the question you actually have when a disk fills up.

pub mod config;
pub mod model;
pub mod scan;
pub mod store;
