//! A small blocking Google Drive client.

#![forbid(unsafe_code)]
#![allow(dead_code)] // removed in S14, once every module is wired in

#[cfg(not(unix))]
compile_error!("gdrive-client reads and sets file mode bits and supports Unix only");

mod client;
mod credentials;
mod error;
mod files;
mod http;
mod options;
mod secret;
mod token_store;

pub use client::Client;
pub use credentials::Credentials;
pub use error::{Error, Operation, TokenStoreProblem};
pub use files::DriveFile;
pub use options::{Endpoints, Options};
