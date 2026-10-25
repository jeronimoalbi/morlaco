//! Morlaco core: wallet history, pricing and FIFO lot accounting.
//!
//! The crate has no UI code. A front end calls [`portfolio::build`] and
//! renders the resulting [`portfolio::Portfolio`].

pub mod cache;
pub mod chain;
mod error;
pub mod lots;
pub mod paths;
pub mod portfolio;
pub mod price;
pub mod util;

pub use error::{Error, Result};
