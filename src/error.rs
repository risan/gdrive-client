use std::fmt;
use std::io;
use std::path::PathBuf;

/// What a failed call was doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    RefreshToken,
    DeviceLogin,
    ResolveFolder,
    CreateFolder,
    ListFiles,
    FindFiles,
    Metadata,
    Upload,
    Download,
    Delete,
    Trash,
}

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Operation::RefreshToken => "token refresh",
            Operation::DeviceLogin => "device login",
            Operation::ResolveFolder => "folder lookup",
            Operation::CreateFolder => "folder creation",
            Operation::ListFiles => "listing",
            Operation::FindFiles => "file search",
            Operation::Metadata => "metadata",
            Operation::Upload => "upload",
            Operation::Download => "download",
            Operation::Delete => "delete",
            Operation::Trash => "trash",
        };

        formatter.write_str(name)
    }
}

/// Why the token store could not be used.
#[derive(Debug)]
pub enum TokenStoreProblem {
    Missing,
    Unreadable(io::Error),
    /// The file's permission bits; the library never changes them.
    InsecureMode(u32),
    Invalid(String),
    WriteFailed(io::Error),
}

/// Every way a call can fail. `target` is the local path, folder path or file id the call was about, never a URL.
/// Secrets never appear in an error or in its `Debug` output.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Drive answered with a status the call does not accept.
    Http {
        operation: Operation,
        target: String,
        status: u16,
        /// Drive's `error.errors[0].reason`, else `error.status`.
        reason: Option<String>,
        message: Option<String>,
    },
    /// A network failure the library does not retry, such as a TLS error.
    Network {
        operation: Operation,
        target: String,
        detail: String,
    },
    /// The call's time budget ran out. For an upload, `confirmed_bytes` is what Drive had confirmed.
    Deadline {
        operation: Operation,
        target: String,
        confirmed_bytes: Option<u64>,
        last_failure: String,
    },
    /// Google rejected the refresh token: the caller must log in again.
    InvalidGrant {
        operation: Operation,
    },
    LoginDenied,
    LoginExpired,
    OAuth {
        operation: Operation,
        code: String,
        description: Option<String>,
    },
    TokenStore {
        path: PathBuf,
        problem: TokenStoreProblem,
    },
    RcloneConfig {
        path: PathBuf,
        section: String,
        problem: String,
    },
    FolderNotFound {
        path: String,
    },
    DuplicateFolder {
        path: String,
    },
    Local {
        operation: Operation,
        path: PathBuf,
        source: io::Error,
    },
    /// The local file shrank or ended early while it was being uploaded.
    LocalFileChanged {
        path: PathBuf,
    },
    /// An answer that Drive's protocol does not allow.
    Protocol {
        operation: Operation,
        target: String,
        detail: String,
    },
    InvalidInput {
        detail: String,
    },
}

impl Error {
    pub(crate) fn with_confirmed_bytes(self, confirmed: u64) -> Error {
        match self {
            Error::Deadline {
                operation,
                target,
                last_failure,
                ..
            } => Error::Deadline {
                operation,
                target,
                confirmed_bytes: Some(confirmed),
                last_failure,
            },
            other => other,
        }
    }
}

fn write_prefix(formatter: &mut fmt::Formatter<'_>, operation: &Operation, target: &str) -> fmt::Result {
    if target.is_empty() {
        write!(formatter, "{operation}: ")
    } else {
        write!(formatter, "{operation} of {target}: ")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http {
                operation,
                target,
                status,
                reason,
                message,
            } => {
                write_prefix(formatter, operation, target)?;
                write!(formatter, "Drive answered {status}")?;

                if let Some(reason) = reason {
                    write!(formatter, " ({reason})")?;
                }

                if let Some(message) = message {
                    write!(formatter, ": {message}")?;
                }

                Ok(())
            }
            Error::Network {
                operation,
                target,
                detail,
            } => {
                write_prefix(formatter, operation, target)?;
                write!(formatter, "network error: {detail}")
            }
            Error::Deadline {
                operation,
                target,
                confirmed_bytes,
                last_failure,
            } => {
                write_prefix(formatter, operation, target)?;
                write!(formatter, "deadline reached")?;

                if let Some(confirmed) = confirmed_bytes {
                    write!(formatter, " after Drive confirmed {confirmed} bytes")?;
                }

                write!(formatter, "; last failure: {last_failure}")
            }
            Error::InvalidGrant { operation } => {
                write!(
                    formatter,
                    "{operation}: Google rejected the refresh token (invalid_grant); log in again"
                )
            }
            Error::LoginDenied => write!(formatter, "device login: the user denied access"),
            Error::LoginExpired => write!(
                formatter,
                "device login: the device code expired before it was approved"
            ),
            Error::OAuth {
                operation,
                code,
                description,
            } => {
                write!(formatter, "{operation}: Google answered {code}")?;

                if let Some(description) = description {
                    write!(formatter, ": {description}")?;
                }

                Ok(())
            }
            Error::TokenStore { path, problem } => {
                write!(formatter, "token store {}: ", path.display())?;

                match problem {
                    TokenStoreProblem::Missing => write!(formatter, "the file does not exist"),
                    TokenStoreProblem::Unreadable(source) => write!(formatter, "cannot be read: {source}"),
                    TokenStoreProblem::InsecureMode(mode) => {
                        write!(formatter, "mode is {mode:03o}, it must be 600")
                    }
                    TokenStoreProblem::Invalid(detail) => write!(formatter, "invalid: {detail}"),
                    TokenStoreProblem::WriteFailed(source) => write!(formatter, "cannot be written: {source}"),
                }
            }
            Error::RcloneConfig { path, section, problem } => {
                write!(
                    formatter,
                    "rclone config {} section [{section}]: {problem}",
                    path.display()
                )
            }
            Error::FolderNotFound { path } => write!(formatter, "folder not found: {path}"),
            Error::DuplicateFolder { path } => write!(formatter, "more than one folder is named {path}"),
            Error::Local {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} of {}: {source}", path.display()),
            Error::LocalFileChanged { path } => {
                write!(formatter, "{} changed while it was being uploaded", path.display())
            }
            Error::Protocol {
                operation,
                target,
                detail,
            } => {
                write_prefix(formatter, operation, target)?;
                write!(formatter, "unexpected answer: {detail}")
            }
            Error::InvalidInput { detail } => write!(formatter, "invalid input: {detail}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Local { source, .. } => Some(source),
            Error::TokenStore {
                problem: TokenStoreProblem::Unreadable(source) | TokenStoreProblem::WriteFailed(source),
                ..
            } => Some(source),
            _ => None,
        }
    }
}

/// A fixed text for each ureq error. ureq's own `Display` is never used: `BadUri`, `RequireHttpsOnly` and ureq-proto's
/// bad `Location` error print URLs, and a URL can carry a resumable session's `upload_id`.
pub(crate) fn describe(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Io(source) => describe_io(source),
        ureq::Error::Timeout(phase) => format!("timed out ({phase})"),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::Protocol(_) => "HTTP protocol error".to_string(),
        ureq::Error::RequireHttpsOnly(_) => "https required".to_string(),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) | ureq::Error::TlsRequired => {
            "TLS error".to_string()
        }
        ureq::Error::BadUri(_) | ureq::Error::Http(_) => "invalid request".to_string(),
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => "redirect refused".to_string(),
        ureq::Error::LargeResponseHeader(..) => "response header too large".to_string(),
        ureq::Error::BodyExceedsLimit(_) => "response body too large".to_string(),
        ureq::Error::ConnectProxyFailed(_) | ureq::Error::InvalidProxyUrl => "proxy error".to_string(),
        _ => "HTTP client error".to_string(),
    }
}

fn describe_io(error: &io::Error) -> String {
    if error.raw_os_error().is_some() {
        error.to_string()
    } else {
        error.kind().to_string()
    }
}

#[cfg(test)]
mod tests;
