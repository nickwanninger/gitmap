//! gitmap as a library, so integration tests can drive the app and render it
//! into a `TestBackend` without a PTY.

pub mod app;
pub mod git;
pub mod input;
pub mod layout;
pub mod render;
pub mod worker;
