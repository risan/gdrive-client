use super::*;

#[test]
fn display_names_operation_target_and_reason() {
    let error = Error::Http {
        operation: Operation::Upload,
        target: "/srv/cctv/x.ts".to_string(),
        status: 403,
        reason: Some("storageQuotaExceeded".to_string()),
        message: Some("The user's Drive storage quota has been exceeded.".to_string()),
    };

    assert_eq!(
        error.to_string(),
        "upload of /srv/cctv/x.ts: Drive answered 403 (storageQuotaExceeded): The user's Drive storage quota has been exceeded."
    );
}

#[test]
fn display_without_reason_or_message() {
    let error = Error::Http {
        operation: Operation::Metadata,
        target: "abc".to_string(),
        status: 500,
        reason: None,
        message: None,
    };

    assert_eq!(error.to_string(), "metadata of abc: Drive answered 500");
}

#[test]
fn deadline_display_reports_confirmed_bytes() {
    let error = Error::Deadline {
        operation: Operation::Upload,
        target: "/srv/x.ts".to_string(),
        confirmed_bytes: Some(524_288),
        last_failure: "Drive answered 503".to_string(),
    };

    let text = error.to_string();

    assert!(text.contains("524288 bytes"), "{text}");
    assert!(text.contains("Drive answered 503"), "{text}");
}

#[test]
fn invalid_grant_tells_the_caller_to_log_in_again() {
    let text = Error::InvalidGrant {
        operation: Operation::RefreshToken,
    }
    .to_string();

    assert!(text.contains("log in again"), "{text}");
}

#[test]
fn describe_never_prints_urls() {
    let url = "https://www.googleapis.com/upload?upload_id=s3cr3t";
    let errors = [
        ureq::Error::BadUri(url.to_string()),
        ureq::Error::RequireHttpsOnly(url.to_string()),
        ureq::Error::ConnectProxyFailed(url.to_string()),
    ];

    for error in errors {
        let description = describe(&error);

        assert!(!description.contains("s3cr3t"), "{description}");
        assert!(!description.contains("googleapis"), "{description}");
        assert!(!description.is_empty());
    }
}

#[test]
fn describe_keeps_io_kind_and_timeout_phase() {
    let refused = ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
    let timeout = ureq::Error::Timeout(ureq::Timeout::PerRead);

    assert!(
        describe(&refused).contains("connection refused"),
        "{}",
        describe(&refused)
    );
    assert_eq!(describe(&timeout), "timed out (per read)");
}
