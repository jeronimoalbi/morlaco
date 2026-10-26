//! Morlaco core: wallet history, pricing and FIFO lot accounting.
//!
//! The crate has no UI code. A front end fetches each address with
//! [`portfolio::fetch`], values them together with [`portfolio::assemble`]
//! and renders the resulting [`portfolio::Portfolio`] values.

pub mod backend;
pub mod chain;
pub mod db;
mod error;
pub mod lots;
pub mod paths;
pub mod portfolio;
pub mod price;
pub mod util;

pub use error::{Error, Result};
