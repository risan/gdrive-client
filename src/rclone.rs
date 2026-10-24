//! One-time import of a Drive client's credentials from an rclone config file. Errors never echo values.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::credentials::Credentials;
use crate::error::Error;

const ENCRYPTED_MARKER: &str = "RCLONE_ENCRYPT_V0:";

impl Credentials {
    /// Reads `client_id` and `client_secret` of the `drive` section named `section`, and the refresh token inside its
    /// `token` JSON. The access token in the file is ignored: the first call refreshes.
    pub fn from_rclone_config(path: &Path, section: &str) -> Result<Credentials, Error> {
        let fail = |problem: &str| Error::RcloneConfig {
            path: path.to_path_buf(),
            section: section.to_string(),
            problem: problem.to_string(),
        };
        let content =
            fs::read_to_string(path).map_err(|error| fail(&format!("the file cannot be read ({})", error.kind())))?;

        if content.lines().any(|line| line.trim().starts_with(ENCRYPTED_MARKER)) {
            return Err(fail("the config is encrypted; decrypt it first"));
        }

        let values = section_values(&content, section).ok_or_else(|| fail("the section was not found"))?;
        let value = |key: &str| values.get(key).map(String::as_str).unwrap_or("");

        if value("type") != "drive" {
            return Err(fail("the section is not of type drive"));
        }

        if value("client_id").is_empty() || value("client_secret").is_empty() {
            return Err(fail(
                "client_id or client_secret is empty: rclone's built-in client cannot be reused",
            ));
        }

        if value("token").is_empty() {
            return Err(fail("the section has no token"));
        }

        let token: Value = serde_json::from_str(value("token")).map_err(|_| fail("the token is not valid JSON"))?;
        let refresh_token = token["refresh_token"].as_str().unwrap_or("");

        if refresh_token.is_empty() {
            return Err(fail("the token has no refresh_token"));
        }

        Ok(Credentials::new(
            value("client_id"),
            value("client_secret"),
            refresh_token,
        ))
    }
}

/// The `key = value` pairs of one section; `#` and `;` start comment lines. A value may contain `=`.
fn section_values(content: &str, wanted: &str) -> Option<HashMap<String, String>> {
    let mut found = false;
    let mut inside = false;
    let mut values = HashMap::new();

    for line in content.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            inside = name == wanted;
            found |= inside;
            continue;
        }

        if inside && let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    found.then_some(values)
}

#[cfg(test)]
mod tests;
