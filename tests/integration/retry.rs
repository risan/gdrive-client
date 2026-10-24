//! Retries, `Retry-After` and the deadline, exercised through `Client::metadata`.

use std::time::{Duration, Instant};

use gdrive_client::Error;

use crate::support::{Env, Fault, Kind, env, env_with};

fn env_with_file() -> (Env, String) {
    let env = env();
    let id = env.fake.add_file("root", "x.bin", b"hello");

    (env, id)
}

fn env_with_file_and(adjust: impl FnOnce(&mut gdrive_client::Options)) -> (Env, String) {
    let env = env_with(adjust);
    let id = env.fake.add_file("root", "x.bin", b"hello");

    (env, id)
}

fn gets(env: &Env) -> usize {
    env.fake.requests_of(Kind::Get).len()
}

#[test]
fn retries_each_transient_status() {
    for status in [429, 500, 502, 503, 504] {
        let (env, id) = env_with_file();

        env.fake.fault(Kind::Get, Fault::status(status));

        assert!(env.client.metadata(&id).is_ok(), "{status}");
        assert_eq!(gets(&env), 2, "{status}");
    }
}

#[test]
fn retries_drive_rate_limit_403() {
    for reason in ["rateLimitExceeded", "userRateLimitExceeded"] {
        let (env, id) = env_with_file();

        env.fake.fault(Kind::Get, Fault::drive_error(403, reason));

        assert!(env.client.metadata(&id).is_ok(), "{reason}");
        assert_eq!(gets(&env), 2, "{reason}");
    }

    let (env, id) = env_with_file();

    env.fake.fault(
        Kind::Get,
        Fault::drive_error(403, "rateLimitExceeded").with_retry_after("1"),
    );

    assert!(env.client.metadata(&id).is_ok());

    let requests = env.fake.requests_of(Kind::Get);

    assert_eq!(requests.len(), 2);
    assert!(requests[1].at - requests[0].at >= Duration::from_secs(1));
}

#[test]
fn retries_a_dropped_connection() {
    let (env, id) = env_with_file();

    env.fake.fault(Kind::Get, Fault::CloseBeforeResponse);

    assert!(env.client.metadata(&id).is_ok());
    assert_eq!(gets(&env), 2);
}

#[test]
fn does_not_retry_other_4xx() {
    let cases = [
        (400, "badRequest"),
        (404, "notFound"),
        (403, "storageQuotaExceeded"),
        (403, "insufficientFilePermissions"),
    ];

    for (status, reason) in cases {
        let (env, id) = env_with_file();

        env.fake.fault(Kind::Get, Fault::drive_error(status, reason));

        let error = env.client.metadata(&id).unwrap_err();

        match error {
            Error::Http {
                status: seen,
                reason: seen_reason,
                ..
            } => {
                assert_eq!(seen, status);
                assert_eq!(seen_reason.as_deref(), Some(reason));
            }
            other => panic!("{status} {reason}: expected Http, got {other:?}"),
        }

        assert_eq!(gets(&env), 1, "{status} {reason}");
    }
}

#[test]
fn honors_retry_after_seconds() {
    let (env, id) = env_with_file();

    env.fake.fault(Kind::Get, Fault::status(503).with_retry_after("1"));

    assert!(env.client.metadata(&id).is_ok());

    let requests = env.fake.requests_of(Kind::Get);

    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].at - requests[0].at >= Duration::from_secs(1),
        "backoff alone is 10 ms"
    );
}

#[test]
fn stops_at_the_deadline() {
    let (env, id) = env_with_file_and(|options| options.deadline = Duration::from_secs(1));

    env.fake.always(Kind::Get, 503);

    let started = Instant::now();
    let error = env.client.metadata(&id).unwrap_err();

    assert!(matches!(error, Error::Deadline { .. }), "{error:?}");
    assert!(gets(&env) > 1);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert!(error.to_string().contains("503"), "{error}");
}

#[test]
fn retry_after_past_the_deadline_fails_at_once() {
    let (env, id) = env_with_file_and(|options| options.deadline = Duration::from_secs(2));

    env.fake.fault(Kind::Get, Fault::status(503).with_retry_after("30"));

    let started = Instant::now();
    let error = env.client.metadata(&id).unwrap_err();

    assert!(matches!(error, Error::Deadline { .. }), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(gets(&env), 1);
}

#[test]
fn token_refresh_is_retried() {
    let (env, id) = env_with_file();

    env.fake.fault(Kind::Token, Fault::CloseBeforeResponse);

    assert!(env.client.metadata(&id).is_ok());
    assert_eq!(env.fake.requests_of(Kind::Token).len(), 2);
}

#[test]
fn a_deadline_too_large_to_represent_does_not_panic() {
    let (env, id) = env_with_file_and(|options| options.deadline = Duration::MAX);

    assert_eq!(env.client.metadata(&id).unwrap().id, id);
}
