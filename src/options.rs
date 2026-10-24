use std::time::Duration;

use ureq::http::Uri;

use crate::error::Error;

const CHUNK_UNIT: usize = 256 * 1024;

/// Settings shared by every call of a [`Client`](crate::Client) and by [`DeviceLogin`](crate::DeviceLogin).
#[derive(Clone, Debug)]
pub struct Options {
    /// Time budget of each call, measured from its start. It bounds network requests, retries, backoff sleeps, token
    /// refreshes and waits on other calls, not local file I/O.
    pub deadline: Duration,
    /// Upload chunk size in bytes: a positive multiple of 256 KiB. The chunk buffer is the only memory that scales
    /// with it.
    pub chunk_size: usize,
    /// First retry delay ceiling; it doubles up to `max_backoff`.
    pub initial_backoff: Duration,
    /// Largest retry delay ceiling.
    pub max_backoff: Duration,
    /// Where Drive and OAuth requests go.
    pub endpoints: Endpoints,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            deadline: Duration::from_secs(10 * 60),
            chunk_size: 8 * 1024 * 1024,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(32),
            endpoints: Endpoints::default(),
        }
    }
}

impl Options {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.chunk_size == 0 || !self.chunk_size.is_multiple_of(CHUNK_UNIT) {
            return Err(Error::InvalidInput {
                detail: "chunk_size must be a positive multiple of 256 KiB".to_string(),
            });
        }
        self.endpoints.validate()
    }
}

/// The origins of the Drive API and of Google's OAuth endpoints.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// Origin of the Drive API, `https://www.googleapis.com` in production.
    pub drive: String,
    /// Origin of the OAuth endpoints, `https://oauth2.googleapis.com` in production.
    pub oauth: String,
}

impl Default for Endpoints {
    fn default() -> Endpoints {
        Endpoints {
            drive: "https://www.googleapis.com".to_string(),
            oauth: "https://oauth2.googleapis.com".to_string(),
        }
    }
}

impl Endpoints {
    /// Points both endpoints at one origin, for a local fake of Drive and OAuth.
    pub fn local(base_url: impl Into<String>) -> Endpoints {
        let base_url = base_url.into();

        Endpoints {
            drive: base_url.clone(),
            oauth: base_url,
        }
    }

    fn validate(&self) -> Result<(), Error> {
        for endpoint in [&self.drive, &self.oauth] {
            let acceptable = match parse_origin(endpoint) {
                Some(origin) => origin.scheme == "https" || origin.is_loopback_http(),
                None => false,
            };

            if !acceptable {
                return Err(Error::InvalidInput {
                    detail: "endpoints must be https, or http on 127.0.0.1, localhost or [::1], without credentials"
                        .to_string(),
                });
            }
        }

        Ok(())
    }

    pub(crate) fn is_loopback_http(&self) -> bool {
        [&self.drive, &self.oauth]
            .into_iter()
            .any(|endpoint| parse_origin(endpoint).is_some_and(|origin| origin.is_loopback_http()))
    }
}

/// Whether `candidate` has the origin of `endpoint`: same scheme, host (ASCII case-insensitive) and effective port.
/// A string prefix check is not enough: `https://www.googleapis.com.attacker.example/` starts with the Drive origin.
pub(crate) fn same_origin(candidate: &str, endpoint: &str) -> bool {
    match (parse_origin(candidate), parse_origin(endpoint)) {
        (Some(candidate), Some(endpoint)) => candidate == endpoint,
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    fn is_loopback_http(&self) -> bool {
        self.scheme == "http" && matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "[::1]")
    }
}

fn parse_origin(url: &str) -> Option<Origin> {
    let uri: Uri = url.parse().ok()?;
    let authority = uri.authority()?;

    if authority.as_str().contains('@') {
        return None;
    }

    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };

    Some(Origin {
        port: authority.port_u16().unwrap_or(default_port),
        host: authority.host().to_ascii_lowercase(),
        scheme,
    })
}

#[cfg(test)]
mod tests;
