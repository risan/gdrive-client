//! The token store: one JSON file with the credentials and the current access token. Unix only, because it reads and
//! sets mode bits.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::credentials::Credentials;
use crate::error::{Error, TokenStoreProblem};
use crate::secret::Secret;

const VERSION: u64 = 1;
const STORE_MODE: u32 = 0o600;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// What the file holds. `access_token_refresh_at` is a wall-clock time in unix seconds, with the 60 s margin applied.
pub(crate) struct StoredTokens {
    pub credentials: Credentials,
    pub access_token: Option<Secret>,
    pub access_token_refresh_at: Option<u64>,
}

pub(crate) fn load(path: &Path) -> Result<StoredTokens, Error> {
    let fail = |problem| Error::TokenStore {
        path: path.to_path_buf(),
        problem,
    };
    let mut file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            fail(TokenStoreProblem::Missing)
        } else {
            fail(TokenStoreProblem::Unreadable(error))
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| fail(TokenStoreProblem::Unreadable(error)))?;

    if !metadata.is_file() {
        return Err(fail(TokenStoreProblem::Unreadable(io::Error::other(
            "not a regular file",
        ))));
    }

    let mode = metadata.mode() & 0o777;

    if mode != STORE_MODE {
        return Err(fail(TokenStoreProblem::InsecureMode(mode)));
    }

    let mut content = Vec::new();

    file.read_to_end(&mut content)
        .map_err(|error| fail(TokenStoreProblem::Unreadable(error)))?;
    parse(&content).map_err(|detail| fail(TokenStoreProblem::Invalid(detail)))
}

fn parse(content: &[u8]) -> Result<StoredTokens, String> {
    let value: Value = serde_json::from_slice(content).map_err(|_| "not valid JSON".to_string())?;

    if value["version"].as_u64() != Some(VERSION) {
        return Err(format!("version must be {VERSION}"));
    }

    let text = |key: &str| {
        value[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{key} is missing or not a string"))
    };
    let access_token = match &value["access_token"] {
        Value::Null => None,
        Value::String(token) => Some(Secret::new(token.as_str())),
        _ => return Err("access_token must be a string or null".to_string()),
    };
    let access_token_refresh_at = match &value["access_token_refresh_at"] {
        Value::Null => None,
        other => Some(
            other
                .as_u64()
                .ok_or("access_token_refresh_at must be unix seconds or null")?,
        ),
    };

    Ok(StoredTokens {
        credentials: Credentials::new(text("client_id")?, text("client_secret")?, text("refresh_token")?),
        access_token,
        access_token_refresh_at,
    })
}

/// Writes the store: temp file in the same directory (mode 600), fsync, rename over the store, fsync the directory.
pub(crate) fn save(path: &Path, tokens: &StoredTokens) -> Result<(), Error> {
    let document = json!({
        "version": VERSION,
        "client_id": tokens.credentials.client_id,
        "client_secret": tokens.credentials.client_secret.expose(),
        "refresh_token": tokens.credentials.refresh_token.expose(),
        "access_token": tokens.access_token.as_ref().map(Secret::expose),
        "access_token_refresh_at": tokens.access_token_refresh_at,
    });
    let mut bytes = document.to_string().into_bytes();

    bytes.push(b'\n');
    write_atomically(path, &bytes).map_err(|error| Error::TokenStore {
        path: path.to_path_buf(),
        problem: TokenStoreProblem::WriteFailed(error),
    })
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path has no file name"))?;
    let temp: PathBuf = directory.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    if let Err(error) = write_temp_then_rename(&temp, path, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    File::open(directory)?.sync_all()
}

fn write_temp_then_rename(temp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(STORE_MODE)
        .open(temp)?;

    // The umask can only remove bits from the mode given to open, but set it on the handle so nothing depends on it.
    file.set_permissions(fs::Permissions::from_mode(STORE_MODE))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temp, path)
}
