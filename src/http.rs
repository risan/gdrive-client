//! The request executor: one ureq agent, the retry rules of D7, and the per-call time budget.

use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use ureq::AsSendBody;

use crate::error::{Error, Operation, describe};
use crate::options::Options;
use crate::secret::{Secret, redact};

const SUCCESS_BODY_LIMIT: u64 = 4 * 1024 * 1024;
const ERROR_BODY_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Service {
    Drive,
    OAuth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    fn has_body(self) -> bool {
        !matches!(self, Method::Get | Method::Delete)
    }
}

/// One request, ready to be sent (and resent) with a fresh bearer token each time.
pub(crate) struct Request<'a> {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: &'a [u8],
    pub service: Service,
    pub max_redirects: u32,
}

impl<'a> Request<'a> {
    /// A Drive request, sent with the access token.
    pub fn drive(method: Method, url: String) -> Request<'a> {
        Request {
            method,
            url,
            headers: Vec::new(),
            body: &[],
            service: Service::Drive,
            max_redirects: 0,
        }
    }

    /// A form POST to Google's OAuth endpoints, sent without a token.
    pub fn oauth(url: String, form: &'a str) -> Request<'a> {
        Request {
            method: Method::Post,
            url,
            headers: vec![("Content-Type", "application/x-www-form-urlencoded".to_string())],
            body: form.as_bytes(),
            service: Service::OAuth,
            max_redirects: 0,
        }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Request<'a> {
        self.headers.push((name, value.into()));
        self
    }

    pub fn body(mut self, body: &'a [u8]) -> Request<'a> {
        self.body = body;
        self
    }

    pub fn json_body(self, body: &'a str) -> Request<'a> {
        self.header("Content-Type", "application/json; charset=UTF-8")
            .body(body.as_bytes())
    }

    pub fn max_redirects(mut self, redirects: u32) -> Request<'a> {
        self.max_redirects = redirects;
        self
    }
}

/// What one call is about and how long it may take. Secrets the call knows are removed from any text of Google's that
/// enters an error.
pub(crate) struct Call {
    pub operation: Operation,
    pub target: String,
    deadline: Instant,
    secrets: RefCell<Vec<Secret>>,
}

impl Call {
    pub fn new(operation: Operation, target: impl Into<String>, budget: Duration) -> Call {
        Call {
            operation,
            target: target.into(),
            deadline: Instant::now() + budget,
            secrets: RefCell::new(Vec::new()),
        }
    }

    /// The same budget and secrets, under another operation and target (a token refresh inside an upload, a folder
    /// creation inside a path walk).
    pub fn derive(&self, operation: Operation, target: impl Into<String>) -> Call {
        Call {
            operation,
            target: target.into(),
            deadline: self.deadline,
            secrets: RefCell::new(self.secrets.borrow().clone()),
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_none()
    }

    pub fn know(&self, secret: Secret) {
        self.secrets.borrow_mut().push(secret);
    }

    pub fn know_all(&self, secrets: impl IntoIterator<Item = Secret>) {
        self.secrets.borrow_mut().extend(secrets);
    }

    pub fn redact(&self, text: &str) -> String {
        redact(text, &self.secrets.borrow())
    }

    pub fn deadline_error(&self, last_failure: &str) -> Error {
        Error::Deadline {
            operation: self.operation,
            target: self.target.clone(),
            confirmed_bytes: None,
            last_failure: self.redact(last_failure),
        }
    }

    pub fn protocol_error(&self, detail: &str) -> Error {
        Error::Protocol {
            operation: self.operation,
            target: self.target.clone(),
            detail: self.redact(detail),
        }
    }

    pub fn network_error(&self, detail: &str) -> Error {
        Error::Network {
            operation: self.operation,
            target: self.target.clone(),
            detail: self.redact(detail),
        }
    }

    pub fn http_error(&self, answer: &Answer) -> Error {
        let info = DriveErrorInfo::parse(&answer.body);

        Error::Http {
            operation: self.operation,
            target: self.target.clone(),
            status: answer.status,
            reason: info.reason.map(|reason| self.redact(&reason)),
            message: info.message.map(|message| self.redact(&message)),
        }
    }

    /// An OAuth endpoint's error: a refresh token's `invalid_grant` is its own error, other `error` codes keep Google's description.
    pub fn oauth_error(&self, answer: &Answer) -> Error {
        let body = answer.json();
        let code = body.as_ref().and_then(|value| value["error"].as_str());

        match code {
            Some("invalid_grant") if self.operation == Operation::RefreshToken => Error::InvalidGrant {
                operation: self.operation,
            },
            Some(code) => Error::OAuth {
                operation: self.operation,
                code: self.redact(code),
                description: body
                    .as_ref()
                    .and_then(|value| value["error_description"].as_str())
                    .map(|description| self.redact(description)),
            },
            None => self.http_error(answer),
        }
    }

    /// The JSON body of an OAuth endpoint's 200 answer; any other answer is its error.
    pub fn expect_oauth_json(&self, answer: &Answer) -> Result<Value, Error> {
        if answer.status != 200 {
            return Err(self.oauth_error(answer));
        }

        answer
            .json()
            .ok_or_else(|| self.protocol_error("the answer is not valid JSON"))
    }

    /// The JSON body of an answer whose status is one of `accepted`; any other status is an `Http` error.
    pub fn expect_json(&self, answer: &Answer, accepted: &[u16]) -> Result<Value, Error> {
        if !accepted.contains(&answer.status) {
            return Err(self.http_error(answer));
        }

        answer
            .json()
            .ok_or_else(|| self.protocol_error("the answer is not valid JSON"))
    }
}

/// A complete answer: status, the headers the library reads, and the body.
pub(crate) struct Answer {
    pub status: u16,
    pub location: Option<String>,
    pub range: Option<String>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// Why an attempt did not produce an answer to act on.
pub(crate) enum Failure {
    /// Worth another attempt after a backoff, or after `retry_after`.
    Transient {
        description: String,
        retry_after: Option<Duration>,
    },
    /// The token was refreshed after a 401: resend at once, not counted as a retry.
    Reauthenticated,
    Fatal(Error),
}

pub(crate) struct DriveErrorInfo {
    /// `error.errors[0].reason`, else `error.status`.
    pub reason: Option<String>,
    pub message: Option<String>,
    pub reasons: Vec<String>,
}

impl DriveErrorInfo {
    pub fn parse(body: &[u8]) -> DriveErrorInfo {
        let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let error = &value["error"];
        let reasons: Vec<String> = error["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["reason"].as_str().map(str::to_string))
            .collect();
        let reason = reasons
            .first()
            .cloned()
            .or_else(|| error["status"].as_str().map(str::to_string));

        DriveErrorInfo {
            reason,
            message: error["message"].as_str().map(str::to_string),
            reasons,
        }
    }
}

/// Statuses worth another attempt: 429, 5xx that mean "try again", and Drive's rate-limit 403s. OAuth endpoints'
/// 403s (`slow_down`, `access_denied`) are never Drive rate limits.
pub(crate) fn is_transient(status: u16, service: Service, body: &[u8]) -> bool {
    match status {
        429 | 500 | 502 | 503 | 504 => true,
        403 if service == Service::Drive => DriveErrorInfo::parse(body)
            .reasons
            .iter()
            .any(|reason| matches!(reason.as_str(), "rateLimitExceeded" | "userRateLimitExceeded")),
        _ => false,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ErrorClass {
    Deadline,
    Transient,
    Fatal,
}

pub(crate) fn classify_error(error: &ureq::Error) -> ErrorClass {
    match error {
        ureq::Error::Timeout(ureq::Timeout::Global) => ErrorClass::Deadline,
        ureq::Error::Timeout(_) | ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => {
            ErrorClass::Transient
        }
        _ => ErrorClass::Fatal,
    }
}

/// The ureq agent and the backoff settings.
pub(crate) struct Transport {
    agent: ureq::Agent,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Transport {
    pub fn new(options: &Options) -> Transport {
        let loopback = options.endpoints.is_loopback_http();
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .https_only(!loopback)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .timeout_per_write(Some(Duration::from_secs(60)))
            .user_agent(concat!("gdrive-client/", env!("CARGO_PKG_VERSION")));

        // ureq reads ALL_PROXY, HTTPS_PROXY and HTTP_PROXY by default; a local fake must never be reached through one.
        if loopback {
            config = config.proxy(None);
        }

        Transport {
            agent: config.build().into(),
            initial_backoff: options.initial_backoff,
            max_backoff: options.max_backoff,
        }
    }

    /// Sends the request once and returns the response with its body unread.
    pub fn send(
        &self,
        call: &Call,
        request: &Request<'_>,
        bearer: Option<&Secret>,
    ) -> Result<ureq::http::Response<ureq::Body>, Failure> {
        let remaining = call
            .remaining()
            .ok_or_else(|| Failure::Fatal(call.deadline_error("the deadline passed before the request was sent")))?;
        let mut builder = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(request.url.as_str());

        if let Some(token) = bearer {
            builder = builder.header("Authorization", format!("Bearer {}", token.expose()));
        }

        for (name, value) in &request.headers {
            builder = builder.header(*name, value.as_str());
        }

        let invalid = |_| {
            Failure::Fatal(Error::InvalidInput {
                detail: "the request could not be built".to_string(),
            })
        };
        let outcome = if request.method.has_body() {
            self.run(
                builder.body(request.body).map_err(invalid)?,
                remaining,
                request.max_redirects,
            )
        } else {
            self.run(builder.body(()).map_err(invalid)?, remaining, request.max_redirects)
        };

        outcome.map_err(|error| self.failure(call, &error))
    }

    fn run<S: AsSendBody>(
        &self,
        request: ureq::http::Request<S>,
        remaining: Duration,
        max_redirects: u32,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let request = self
            .agent
            .configure_request(request)
            .timeout_global(Some(remaining))
            .max_redirects(max_redirects)
            .build();

        self.agent.run(request)
    }

    pub fn failure(&self, call: &Call, error: &ureq::Error) -> Failure {
        let description = describe(error);

        match classify_error(error) {
            ErrorClass::Deadline => Failure::Fatal(call.deadline_error(&description)),
            ErrorClass::Transient => Failure::Transient {
                description,
                retry_after: None,
            },
            ErrorClass::Fatal if matches!(error, ureq::Error::Protocol(_)) => {
                Failure::Fatal(call.protocol_error(&description))
            }
            ErrorClass::Fatal => Failure::Fatal(call.network_error(&description)),
        }
    }

    /// Reads the body (success bodies up to 4 MiB, error bodies up to 64 KiB) and judges the status.
    pub fn answer(
        &self,
        call: &Call,
        request: &Request<'_>,
        response: ureq::http::Response<ureq::Body>,
    ) -> Result<Answer, Failure> {
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let retry_after = header("retry-after").and_then(|value| parse_retry_after(&value, unix_now()));
        let location = header("location");
        let range = header("range");
        let limit = if status < 300 {
            SUCCESS_BODY_LIMIT
        } else {
            ERROR_BODY_LIMIT
        };
        let mut body = Vec::new();

        response
            .into_body()
            .as_reader()
            .take(limit)
            .read_to_end(&mut body)
            .map_err(|error| self.failure(call, &ureq::Error::from(error)))?;

        if is_transient(status, request.service, &body) {
            let name = match request.service {
                Service::Drive => "Drive",
                Service::OAuth => "Google's OAuth endpoint",
            };

            return Err(Failure::Transient {
                description: format!("{name} answered {status}"),
                retry_after,
            });
        }

        Ok(Answer {
            status,
            location,
            range,
            body,
        })
    }

    /// One attempt without a token: send, read, judge.
    pub fn exchange(&self, call: &Call, request: &Request<'_>, bearer: Option<&Secret>) -> Result<Answer, Failure> {
        let response = self.send(call, request, bearer)?;

        self.answer(call, request, response)
    }

    /// Sends a request that needs no token (OAuth) until it has an answer to act on or the deadline passes.
    pub fn execute(&self, call: &Call, request: &Request<'_>) -> Result<Answer, Error> {
        let mut retrying = Retrying::new();

        loop {
            match self.exchange(call, request, None) {
                Ok(answer) => return Ok(answer),
                Err(Failure::Transient {
                    description,
                    retry_after,
                }) => retrying.pause(self, call, &description, retry_after)?,
                Err(Failure::Reauthenticated) => {}
                Err(Failure::Fatal(error)) => return Err(retrying.attribute(error)),
            }
        }
    }

    pub fn backoff(&self, attempt: u32) -> Duration {
        backoff_delay(self.initial_backoff, self.max_backoff, attempt, random_u64())
    }
}

/// The retries of one logical request.
pub(crate) struct Retrying {
    attempt: u32,
    /// Whether the one forced token refresh after a 401 has been used.
    pub reauthenticated: bool,
    last_failure: String,
}

impl Retrying {
    pub fn new() -> Retrying {
        Retrying {
            attempt: 0,
            reauthenticated: false,
            last_failure: String::new(),
        }
    }

    /// Records the failure and returns the next jittered backoff.
    pub fn record_failure(&mut self, transport: &Transport, description: &str) -> Duration {
        let backoff = transport.backoff(self.attempt);

        self.attempt += 1;
        self.last_failure = description.to_string();
        backoff
    }

    /// Records the failure and returns the D7 delay: `Retry-After` when the answer had one, else jittered backoff.
    pub fn next_delay(&mut self, transport: &Transport, description: &str, retry_after: Option<Duration>) -> Duration {
        let backoff = self.record_failure(transport, description);

        retry_after.unwrap_or(backoff)
    }

    /// A deadline that passes while an attempt is in flight reports the failure that made the request retry, not only
    /// "timed out".
    pub fn attribute(&self, error: Error) -> Error {
        match error {
            Error::Deadline {
                operation,
                target,
                confirmed_bytes,
                last_failure,
            } if !self.last_failure.is_empty() => Error::Deadline {
                operation,
                target,
                confirmed_bytes,
                last_failure: format!("{}; then {last_failure}", self.last_failure),
            },
            other => other,
        }
    }

    /// Sleeps for the delay, or returns `Deadline` at once when the delay would end past the deadline.
    pub fn sleep(&self, call: &Call, delay: Duration) -> Result<(), Error> {
        match call.remaining() {
            Some(left) if delay < left => {
                thread::sleep(delay);
                Ok(())
            }
            _ => Err(call.deadline_error(&self.last_failure)),
        }
    }

    pub fn pause(
        &mut self,
        transport: &Transport,
        call: &Call,
        description: &str,
        retry_after: Option<Duration>,
    ) -> Result<(), Error> {
        let delay = self.next_delay(transport, description, retry_after);

        self.sleep(call, delay)
    }
}

pub(crate) fn backoff_ceiling(initial: Duration, max: Duration, attempt: u32) -> Duration {
    initial.saturating_mul(1u32 << attempt.min(30)).min(max)
}

/// A delay drawn uniformly from `[ceiling / 2, ceiling]`; `random` is the draw.
pub(crate) fn backoff_delay(initial: Duration, max: Duration, attempt: u32, random: u64) -> Duration {
    let ceiling = backoff_ceiling(initial, max, attempt);
    let floor = ceiling / 2;
    let span = (ceiling - floor).as_nanos();
    let extra = span * u128::from(random) / u128::from(u64::MAX);

    floor + Duration::from_nanos(extra as u64)
}

fn random_u64() -> u64 {
    RandomState::new().build_hasher().finish()
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `Retry-After` as delta-seconds or an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`); any other form is `None`.
pub(crate) fn parse_retry_after(value: &str, now_unix: u64) -> Option<Duration> {
    let value = value.trim();

    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }

    let target = parse_http_date(value)?;

    Some(Duration::from_secs(target.saturating_sub(now_unix)))
}

fn parse_http_date(value: &str) -> Option<u64> {
    let parts: Vec<&str> = value.split(' ').collect();

    if parts.len() != 6 || parts[5] != "GMT" || !parts[0].ends_with(',') || parts[0].len() != 4 {
        return None;
    }

    let day: u64 = parts[1].parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|name| *name == parts[2])? as u64
        + 1;
    let year: u64 = parts[3].parse().ok().filter(|year| (1970..=9999).contains(year))?;
    let mut clock = parts[4].split(':').map(|part| part.parse::<u64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);

    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 || day == 0 || day > days_in_month(year, month)
    {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);

    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_shifted = (month + 9) % 12;
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

/// Percent-encodes everything but the unreserved characters.
pub(crate) fn encode_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());

    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }

    encoded
}

pub(crate) fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", encode_component(name), encode_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn url(base: &str, path: &str, query: &[(&str, &str)]) -> String {
    let mut url = format!("{}{path}", base.trim_end_matches('/'));

    if !query.is_empty() {
        url.push('?');
        url.push_str(&form_body(query));
    }

    url
}

#[cfg(test)]
mod tests;
