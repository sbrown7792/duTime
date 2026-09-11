//! duTime — track disk usage over time.
//!
//! `du` answers "what is big *now*". duTime answers "what *grew*, and *when*" —
//! the question you actually have when a disk fills up.

pub mod api;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod diag;
pub mod model;
pub mod scan;
pub mod store;
pub mod web;
