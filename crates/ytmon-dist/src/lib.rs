//! Library half of the dist service.
//!
//! The dashboard mounts these same routes under `/update`, so publishing an
//! agent binary no longer needs a second port. `main.rs` keeps the standalone
//! service for hosts that still want one.

pub mod api;
pub mod config;
pub mod files;
