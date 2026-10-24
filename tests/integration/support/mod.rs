//! Shared helpers of the integration tests: the fake Drive and OAuth server, a plain ureq agent for talking to it
//! directly, and small file helpers.

mod drive_query;
pub mod fake_drive;
mod wire;

use std::io::{Read, Write};
use std::net::TcpStream;

use std::path::{Path, PathBuf};
use std::time::Duration;

use gdrive_client::{Client, Credentials, Endpoints, Options};
use tempfile::TempDir;

pub use fake_drive::{DeviceConfig, DevicePoll, FakeDrive, Fault, Kind, ROOT_ID};

impl FakeDrive {
    /// Both endpoints at the fake, a 5 s deadline, 10 to 50 ms backoff and 256 KiB chunks.
    pub fn options(&self) -> Options {
        Options {
            deadline: Duration::from_secs(5),
            chunk_size: 256 * 1024,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(50),
            endpoints: Endpoints::local(self.url()),
        }
    }

    /// The client id, client secret and refresh token the fake accepts.
    pub fn valid_credentials(&self) -> Credentials {
        let (client_id, client_secret, refresh_token) = self.credentials();

        Credentials::new(client_id, client_secret, refresh_token)
    }
}

/// A fake, a scratch directory and a client with a fresh token store (no access token yet).
pub struct Env {
    pub fake: FakeDrive,
    pub dir: TempDir,
    pub client: Client,
}

impl Env {
    pub fn store_path(&self) -> PathBuf {
        store_path(&self.dir)
    }
}

pub fn store_path(dir: &TempDir) -> PathBuf {
    dir.path().join("token.json")
}

pub fn env() -> Env {
    env_with(|_| {})
}

/// Like [`env`], with the options adjusted first.
pub fn env_with(adjust: impl FnOnce(&mut Options)) -> Env {
    let fake = FakeDrive::start();
    let dir = TempDir::new().expect("scratch directory");
    let mut options = fake.options();

    adjust(&mut options);

    let client = Client::create(store_path(&dir), fake.valid_credentials(), options).expect("create the client");

    Env { fake, dir, client }
}

/// Whether a read-only directory really stops file creation here (it does not for root).
pub fn read_only_dirs_are_enforced(scratch: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch.join("probe-read-only");

    std::fs::create_dir(&dir).expect("probe dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod probe");

    let enforced = std::fs::File::create(dir.join("probe")).is_err();

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod probe back");
    enforced
}

/// The agent every test uses to talk to the fake directly. It ignores proxy variables (nothing may leave the host),
/// returns 4xx and 5xx as answers, and does not follow redirects: ureq's default of 10 redirects would reject the
/// fake's 308 for lacking a `Location` before a test could read it.
pub fn raw_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(None)
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub fn request(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut builder = ureq::http::Request::builder().method(method).uri(url);

    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }

    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }

    let request = builder.body(body.to_vec()).expect("test request");
    let mut response = agent.run(request).expect("the fake answers");
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect();
    let body = response.body_mut().read_to_vec().expect("answer body");

    Reply { status, headers, body }
}

/// Sends the request text as is and reads the answer until the fake closes the connection.
pub fn raw_http(base_url: &str, request_text: &str) -> Reply {
    let address = base_url.trim_start_matches("http://");
    let mut stream = TcpStream::connect(address).expect("connect to the fake");

    stream.write_all(request_text.as_bytes()).expect("send");

    let mut answer = Vec::new();

    stream.read_to_end(&mut answer).expect("read");

    let split = answer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("answer head");
    let head = String::from_utf8_lossy(&answer[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_string(), value.trim().to_string()))
        .collect();

    Reply {
        status,
        headers,
        body: answer[split + 4..].to_vec(),
    }
}

/// Percent-encodes everything but unreserved characters, for query values.
pub fn encode(text: &str) -> String {
    let mut encoded = String::new();

    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }

    encoded
}
