//! A small blocking Google Drive client.

#![forbid(unsafe_code)]
#![allow(dead_code)] // removed in S14, once every module is wired in

#[cfg(not(unix))]
compile_error!("gdrive-client reads and sets file mode bits and supports Unix only");

mod client;
mod credentials;
mod device_login;
mod download;
mod error;
mod files;
mod hash;
mod http;
mod options;
mod query;
mod rclone;
mod secret;
mod temp;
mod token_store;
mod upload;

pub use client::Client;
pub use credentials::Credentials;
pub use device_login::DeviceLogin;
pub use download::Download;
pub use error::{Error, Operation, TokenStoreProblem};
pub use files::DriveFile;
pub use options::{Endpoints, Options};
pub use upload::Upload;
