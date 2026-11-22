//! Headless Notion core: API client, models, SQLite mirror, op queue, sync.

pub mod activity;
pub mod api;
pub mod error;
pub mod markdown;
pub mod media;
pub mod model;
pub mod ops;
pub mod paths;
pub mod private;
pub mod props;
pub mod store;
pub mod sync;
pub mod views;

pub use error::{Error, Result};
