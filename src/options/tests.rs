use super::*;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

fn with_endpoint(url: &str) -> Options {
    Options {
        endpoints: Endpoints::local(url),
        ..Options::default()
    }
}

#[test]
fn default_options_match_the_spec() {
    let options = Options::default();

    assert_eq!(options.deadline, Duration::from_secs(600));
    assert_eq!(options.chunk_size, 8 * MIB);
    assert_eq!(options.initial_backoff, Duration::from_secs(1));
    assert_eq!(options.max_backoff, Duration::from_secs(32));
    assert_eq!(options.endpoints.drive, "https://www.googleapis.com");
    assert_eq!(options.endpoints.oauth, "https://oauth2.googleapis.com");
    assert!(options.validate().is_ok());
}

#[test]
fn chunk_size_must_be_a_positive_multiple_of_256_kib() {
    for rejected in [0, 100 * KIB, 300 * KIB] {
        let options = Options {
            chunk_size: rejected,
            ..Options::default()
        };

        assert!(
            matches!(options.validate(), Err(Error::InvalidInput { .. })),
            "{rejected} was accepted"
        );
    }

    for accepted in [256 * KIB, 8 * MIB] {
        let options = Options {
            chunk_size: accepted,
            ..Options::default()
        };

        assert!(options.validate().is_ok(), "{accepted} was rejected");
    }
}

#[test]
fn plain_http_only_on_loopback() {
    for accepted in [
        "http://127.0.0.1:1",
        "http://localhost:1",
        "http://[::1]:1",
        "https://example.com",
        "https://www.googleapis.com",
    ] {
        assert!(with_endpoint(accepted).validate().is_ok(), "{accepted} was rejected");
    }

    for rejected in [
        "http://example.com",
        "ftp://x",
        "https://user@www.googleapis.com",
        "not a url",
    ] {
        assert!(
            matches!(with_endpoint(rejected).validate(), Err(Error::InvalidInput { .. })),
            "{rejected} was accepted"
        );
    }
}

#[test]
fn loopback_http_is_detected_for_both_endpoints() {
    assert!(with_endpoint("http://127.0.0.1:9").endpoints.is_loopback_http());
    assert!(!Endpoints::default().is_loopback_http());
}

#[test]
fn same_origin_compares_scheme_host_and_port() {
    let drive = "https://www.googleapis.com";

    assert!(same_origin(
        "https://WWW.googleapis.com:443/upload/drive/v3/files?upload_id=x",
        drive
    ));
    assert!(same_origin("https://www.googleapis.com/upload", drive));

    for rejected in [
        "https://www.googleapis.com.attacker.example/upload",
        "https://www.googleapis.com:8443/upload",
        "http://www.googleapis.com/upload",
        "https://www.googleapis.com@evil.example/upload",
        "https://u:p@www.googleapis.com/upload",
        "/relative",
    ] {
        assert!(!same_origin(rejected, drive), "{rejected} was accepted");
    }
}

#[test]
fn local_endpoints_compare_by_port() {
    let fake = "http://127.0.0.1:4000";

    assert!(same_origin("http://127.0.0.1:4000/upload?x=1", fake));
    assert!(!same_origin("http://127.0.0.1:4001/upload", fake));
    assert!(!same_origin("http://127.0.0.1:4000.attacker.example/upload", fake));
}
