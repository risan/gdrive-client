use std::fmt;

const REDACTED: &str = "[redacted]";

/// A value that must never appear in logs, errors or `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// Replaces every occurrence of each secret in `text`, for text that came from Google and may echo one.
pub(crate) fn redact(text: &str, secrets: &[Secret]) -> String {
    let mut redacted = text.to_string();

    for secret in secrets {
        if !secret.0.is_empty() {
            redacted = redacted.replace(&secret.0, REDACTED);
        }
    }

    redacted
}

/// A resumable session URL and its `upload_id`: either one lets whoever holds it upload to the session.
pub(crate) fn session_secrets(session_url: &str) -> Vec<Secret> {
    let mut secrets = vec![Secret::new(session_url)];
    let upload_id = session_url
        .split_once('?')
        .map(|(_, query)| query)
        .into_iter()
        .flat_map(|query| query.split('&'))
        .find_map(|pair| pair.strip_prefix("upload_id="));

    if let Some(upload_id) = upload_id {
        secrets.push(Secret::new(upload_id));
    }

    secrets
}

#[cfg(test)]
mod tests;
