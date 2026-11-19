//! Headless Notion core: API client, models, SQLite mirror, op queue, sync.

pub mod api;
pub mod error;
pub mod model;
pub mod ops;
pub mod store;

pub use error::{Error, Result};
