//! Google's device authorization flow (RFC 8628) for limited-input devices.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use crate::credentials::Credentials;
use crate::error::{Error, Operation};
use crate::http::{Answer, Call, Failure, Request, Retrying, Transport, form_body, url};
use crate::options::Options;
use crate::secret::Secret;

const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A started device login: show [`verification_url`](DeviceLogin::verification_url) and
/// [`user_code`](DeviceLogin::user_code) to the user, then [`wait`](DeviceLogin::wait) for the approval.
pub struct DeviceLogin {
    transport: Transport,
    options: Options,
    client_id: String,
    client_secret: Secret,
    device_code: Secret,
    verification_url: String,
    user_code: String,
    expires_in: Duration,
    expires_at: Instant,
    interval: Duration,
}

impl DeviceLogin {
    /// Asks Google for a device code, retrying under `options.deadline`.
    pub fn start(client_id: &str, client_secret: &str, options: &Options) -> Result<DeviceLogin, Error> {
        options.validate()?;

        let transport = Transport::new(options);
        let call = Call::new(Operation::DeviceLogin, "", options.deadline);

        call.know(Secret::new(client_secret));

        let form = form_body(&[("client_id", client_id), ("scope", SCOPE)]);
        let request = Request::oauth(url(&options.endpoints.oauth, "/device/code", &[]), &form);
        let started = Instant::now();
        let answer = transport.execute(&call, &request)?;
        let body = call.expect_oauth_json(&answer)?;
        let text = |key: &str| {
            body[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| call.protocol_error(&format!("the answer has no {key}")))
        };
        let device_code = Secret::new(text("device_code")?);
        let expires_in = Duration::from_secs(
            body["expires_in"]
                .as_u64()
                .ok_or_else(|| call.protocol_error("the answer has no expires_in"))?,
        );
        let interval = body["interval"].as_u64().map_or(DEFAULT_INTERVAL, Duration::from_secs);

        Ok(DeviceLogin {
            transport,
            options: options.clone(),
            client_id: client_id.to_string(),
            client_secret: Secret::new(client_secret),
            verification_url: text("verification_url")?,
            user_code: text("user_code")?,
            device_code,
            expires_in,
            expires_at: started + expires_in,
            interval,
        })
    }

    pub fn verification_url(&self) -> &str {
        &self.verification_url
    }

    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// How long the code stays valid, counted from when it was issued.
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }

    /// Polls at Google's interval until the user approves (the credentials), denies, or the code expires. Each poll's
    /// transient failures are retried until `options.deadline` after that poll began or the code's expiry, whichever
    /// is first.
    pub fn wait(self) -> Result<Credentials, Error> {
        let mut interval = self.interval;
        let form = form_body(&[
            ("client_id", &self.client_id),
            ("client_secret", self.client_secret.expose()),
            ("device_code", self.device_code.expose()),
            ("grant_type", DEVICE_GRANT),
        ]);
        let request = Request::oauth(url(&self.options.endpoints.oauth, "/token", &[]), &form);

        loop {
            self.sleep_until_next_poll(interval)?;

            let answer = self.poll(&request, interval)?;

            if answer.status == 200 {
                return self.credentials_from(&answer);
            }

            match error_code(&answer).as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval = slow_down(interval),
                Some("access_denied") => return Err(Error::LoginDenied),
                Some("expired_token") => return Err(Error::LoginExpired),
                _ => return Err(self.poll_call(Duration::from_secs(1)).oauth_error(&answer)),
            }
        }
    }

    fn poll_call(&self, budget: Duration) -> Call {
        let call = Call::new(Operation::DeviceLogin, "", budget);

        call.know_all([self.client_secret.clone(), self.device_code.clone()]);
        call
    }

    fn sleep_until_next_poll(&self, interval: Duration) -> Result<(), Error> {
        let until_expiry = self.expires_at.saturating_duration_since(Instant::now());

        thread::sleep(interval.min(until_expiry));

        if Instant::now() >= self.expires_at {
            return Err(Error::LoginExpired);
        }

        Ok(())
    }

    /// One poll with its retries.
    fn poll(&self, request: &Request<'_>, interval: Duration) -> Result<Answer, Error> {
        let until_expiry = self.expires_at.saturating_duration_since(Instant::now());
        let expiry_comes_first = until_expiry <= self.options.deadline;
        let call = self.poll_call(until_expiry.min(self.options.deadline));
        let mut retrying = Retrying::new();

        loop {
            let (description, retry_after) = match self.transport.exchange(&call, request, None) {
                Ok(answer) => return Ok(answer),
                Err(Failure::Transient {
                    description,
                    retry_after,
                }) => (description, retry_after),
                Err(Failure::Reauthenticated) => continue,
                Err(Failure::Fatal(error)) => return Err(self.expiry_or(error, expiry_comes_first)),
            };
            let backoff = retrying.record_failure(&self.transport, &description);
            let delay = resend_delay(interval, backoff, retry_after);

            retrying
                .sleep(&call, delay)
                .map_err(|error| self.expiry_or(error, expiry_comes_first))?;
        }
    }

    fn expiry_or(&self, error: Error, expiry_comes_first: bool) -> Error {
        match error {
            Error::Deadline { .. } if expiry_comes_first => Error::LoginExpired,
            other => other,
        }
    }

    fn credentials_from(&self, answer: &Answer) -> Result<Credentials, Error> {
        let call = self.poll_call(Duration::from_secs(1));
        let body = call.expect_oauth_json(answer)?;
        let refresh_token = body["refresh_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| call.protocol_error("the approval carries no refresh_token"))?;

        Ok(Credentials::new(
            &self.client_id,
            self.client_secret.expose(),
            refresh_token,
        ))
    }
}

fn error_code(answer: &Answer) -> Option<String> {
    answer.json()?["error"].as_str().map(str::to_string)
}

/// Google defines the interval as the wait between any two polling requests, so a resend after a failed poll waits
/// the greatest of the interval, the backoff and a valid `Retry-After`; a faster retry would earn `slow_down`.
pub(crate) fn resend_delay(interval: Duration, backoff: Duration, retry_after: Option<Duration>) -> Duration {
    interval.max(backoff).max(retry_after.unwrap_or_default())
}

/// RFC 8628 section 3.5: after `slow_down`, every later wait is 5 seconds longer.
pub(crate) fn slow_down(interval: Duration) -> Duration {
    interval + SLOW_DOWN_STEP
}

impl fmt::Debug for DeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceLogin")
            .field("client_id", &self.client_id)
            .field("verification_url", &self.verification_url)
            .field("user_code", &self.user_code)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
