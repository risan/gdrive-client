//! A small blocking Google Drive client.

#![forbid(unsafe_code)]
#![allow(dead_code)] // removed in S14, once every module is wired in

mod error;
mod options;
mod secret;

pub use error::{Error, Operation, TokenStoreProblem};
pub use options::{Endpoints, Options};
