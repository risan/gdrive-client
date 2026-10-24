use super::*;

const SECOND: Duration = Duration::from_secs(1);
// Sun, 06 Nov 1994 08:49:37 GMT
const SAMPLE_DATE_UNIX: u64 = 784_111_777;

#[test]
fn parses_retry_after_seconds_and_http_date() {
    assert_eq!(parse_retry_after("120", 0), Some(Duration::from_secs(120)));
    assert_eq!(parse_retry_after(" 7 ", 0), Some(Duration::from_secs(7)));
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", SAMPLE_DATE_UNIX - 30),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", SAMPLE_DATE_UNIX + 5),
        Some(Duration::ZERO)
    );
    assert_eq!(parse_retry_after("soon", 0), None);
    assert_eq!(parse_retry_after("", 0), None);
    assert_eq!(parse_retry_after("Sunday, 06-Nov-94 08:49:37 GMT", 0), None);
    assert_eq!(parse_retry_after("Sun, 31 Feb 1994 08:49:37 GMT", 0), None);
}

#[test]
fn backoff_stays_within_the_jittered_ceiling() {
    let initial = SECOND;
    let max = Duration::from_secs(32);

    for attempt in 0..=12 {
        let ceiling = max.min(initial * 2u32.pow(attempt));

        assert_eq!(backoff_ceiling(initial, max, attempt), ceiling, "attempt {attempt}");

        for random in [0, 1, u64::MAX / 3, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
            let delay = backoff_delay(initial, max, attempt, random);

            assert!(delay >= ceiling / 2 && delay <= ceiling, "attempt {attempt}: {delay:?}");
        }
    }

    assert_eq!(
        backoff_ceiling(initial, max, 200),
        max,
        "a huge attempt must not overflow"
    );
}

fn rate_limit_body(reasons: &[&str]) -> Vec<u8> {
    let errors: Vec<_> = reasons
        .iter()
        .map(|reason| serde_json::json!({"reason": reason, "message": "m"}))
        .collect();

    serde_json::json!({"error": {"code": 403, "message": "m", "errors": errors}})
        .to_string()
        .into_bytes()
}

#[test]
fn classifies_transient_failures() {
    for status in [429, 500, 502, 503, 504] {
        assert!(is_transient(status, Service::Drive, b""), "{status}");
        assert!(is_transient(status, Service::OAuth, b""), "{status}");
    }

    for status in [200, 201, 204, 308, 400, 401, 404, 410, 501] {
        assert!(!is_transient(status, Service::Drive, b""), "{status}");
    }

    assert!(is_transient(
        403,
        Service::Drive,
        &rate_limit_body(&["rateLimitExceeded"])
    ));
    assert!(is_transient(
        403,
        Service::Drive,
        &rate_limit_body(&["userRateLimitExceeded"])
    ));
    assert!(
        is_transient(
            403,
            Service::Drive,
            &rate_limit_body(&["somethingElse", "userRateLimitExceeded"])
        ),
        "a rate-limit reason in a later entry counts"
    );
    assert!(!is_transient(
        403,
        Service::Drive,
        &rate_limit_body(&["storageQuotaExceeded"])
    ));
    assert!(!is_transient(
        403,
        Service::Drive,
        &rate_limit_body(&["insufficientFilePermissions"])
    ));
    assert!(!is_transient(403, Service::Drive, b""));
    assert!(!is_transient(403, Service::Drive, b"not json"));
    assert!(
        !is_transient(403, Service::OAuth, &rate_limit_body(&["rateLimitExceeded"])),
        "OAuth 403s (slow_down, access_denied) are not Drive rate limits"
    );
}

#[test]
fn classifies_network_failures() {
    let io = |kind| ureq::Error::Io(std::io::Error::from(kind));

    assert_eq!(
        classify_error(&io(std::io::ErrorKind::UnexpectedEof)),
        ErrorClass::Transient
    );
    assert_eq!(
        classify_error(&io(std::io::ErrorKind::ConnectionReset)),
        ErrorClass::Transient
    );
    assert_eq!(classify_error(&ureq::Error::ConnectionFailed), ErrorClass::Transient);
    assert_eq!(classify_error(&ureq::Error::HostNotFound), ErrorClass::Transient);
    assert_eq!(
        classify_error(&ureq::Error::Timeout(ureq::Timeout::PerRead)),
        ErrorClass::Transient
    );
    assert_eq!(
        classify_error(&ureq::Error::Timeout(ureq::Timeout::Connect)),
        ErrorClass::Transient
    );
    assert_eq!(
        classify_error(&ureq::Error::Timeout(ureq::Timeout::Global)),
        ErrorClass::Deadline
    );
    assert_eq!(classify_error(&ureq::Error::Tls("bad certificate")), ErrorClass::Fatal);
    assert_eq!(classify_error(&ureq::Error::BadUri("x".to_string())), ErrorClass::Fatal);
}

#[test]
fn drive_error_info_reads_reason_status_and_message() {
    let body = br#"{"error":{"code":403,"message":"Quota","status":"PERMISSION_DENIED","errors":[{"reason":"storageQuotaExceeded","message":"Quota"}]}}"#;
    let info = DriveErrorInfo::parse(body);

    assert_eq!(info.reason.as_deref(), Some("storageQuotaExceeded"));
    assert_eq!(info.message.as_deref(), Some("Quota"));

    let without_errors = DriveErrorInfo::parse(br#"{"error":{"code":503,"message":"Down","status":"UNAVAILABLE"}}"#);

    assert_eq!(without_errors.reason.as_deref(), Some("UNAVAILABLE"));

    let nothing = DriveErrorInfo::parse(b"<html>");

    assert_eq!(nothing.reason, None);
    assert_eq!(nothing.message, None);
}

#[test]
fn url_encoding_keeps_only_unreserved_characters() {
    assert_eq!(encode_component("a b&c=d/e'f\\g~-_."), "a%20b%26c%3Dd%2Fe%27f%5Cg~-_.");
    assert_eq!(encode_component("naïve"), "na%C3%AFve");
}

#[test]
fn form_bodies_encode_each_value() {
    assert_eq!(form_body(&[("a", "1 2"), ("b", "x&y")]), "a=1%202&b=x%26y");
}

#[test]
fn a_secret_is_remembered_once() {
    let call = Call::new(Operation::Upload, "file", Duration::from_secs(60));

    for _ in 0..1000 {
        call.know(Secret::new("access-token"));
    }

    call.know_all([Secret::new("access-token"), Secret::new("other")]);
    assert_eq!(call.secrets.borrow().len(), 2);
}
