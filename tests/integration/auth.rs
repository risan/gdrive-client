//! Credentials, the token store and the token lifecycle against the fake OAuth endpoint.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gdrive_client::{Client, Credentials, DeviceLogin, Endpoints, Error, Options, TokenStoreProblem};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{DeviceConfig, DevicePoll, FakeDrive, Fault, Kind, env, read_only_dirs_are_enforced};

fn store_in(dir: &TempDir) -> PathBuf {
    dir.path().join("token.json")
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn sample_credentials() -> Credentials {
    Credentials::new("client-id-1", "client-secret-1", "refresh-token-1")
}

fn make_read_only(dir: &Path) {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
}

fn make_writable(dir: &Path) {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn create_writes_the_store_with_mode_600() {
    let fake = FakeDrive::start();
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);

    Client::create(&store, sample_credentials(), fake.options()).unwrap();

    assert_eq!(mode_of(&store), 0o600);
    assert_eq!(
        read_json(&store),
        json!({
            "version": 1,
            "client_id": "client-id-1",
            "client_secret": "client-secret-1",
            "refresh_token": "refresh-token-1",
            "access_token": null,
            "access_token_refresh_at": null,
        })
    );
    assert!(fake.requests().is_empty(), "create must not make a request");
}

#[test]
fn store_is_replaced_by_rename() {
    let fake = FakeDrive::start();
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);

    Client::create(&store, sample_credentials(), fake.options()).unwrap();

    let old_inode = fs::metadata(&store).unwrap().ino();
    let old_handle = fs::File::open(&store).unwrap();
    let old_content = fs::read(&store).unwrap();

    Client::create(
        &store,
        Credentials::new("other-id", "other-secret", "other-refresh"),
        fake.options(),
    )
    .unwrap();

    let mut seen_by_old_handle = Vec::new();

    std::io::Read::read_to_end(&mut &old_handle, &mut seen_by_old_handle).unwrap();

    assert_ne!(fs::metadata(&store).unwrap().ino(), old_inode);
    assert_eq!(seen_by_old_handle, old_content);
    assert_eq!(read_json(&store)["client_id"], "other-id");

    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    assert_eq!(names, ["token.json"], "a temporary file was left behind");
}

#[test]
fn open_rejects_a_store_that_is_not_mode_600() {
    let fake = FakeDrive::start();
    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);

    Client::create(&store, sample_credentials(), fake.options()).unwrap();
    fs::set_permissions(&store, fs::Permissions::from_mode(0o644)).unwrap();

    let error = Client::open(&store, fake.options()).unwrap_err();

    assert!(
        matches!(
            error,
            Error::TokenStore {
                problem: TokenStoreProblem::InsecureMode(0o644),
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        mode_of(&store),
        0o644,
        "the library must never widen or narrow permissions"
    );
}

#[test]
fn open_reports_missing_unreadable_and_invalid_stores() {
    let fake = FakeDrive::start();
    let dir = TempDir::new().unwrap();
    let problem = |path: &Path| match Client::open(path, fake.options()) {
        Err(Error::TokenStore { problem, .. }) => problem,
        other => panic!("expected a token store error, got {other:?}"),
    };

    assert!(matches!(
        problem(&dir.path().join("absent.json")),
        TokenStoreProblem::Missing
    ));
    assert!(matches!(problem(dir.path()), TokenStoreProblem::Unreadable(_)));

    let write = |name: &str, content: &str| {
        let path = dir.path().join(name);

        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    };

    for (name, content) in [
        ("garbage.json", "this is not json"),
        (
            "missing-field.json",
            r#"{"version":1,"client_id":"a","client_secret":"b","access_token":null,"access_token_refresh_at":null}"#,
        ),
        (
            "version-2.json",
            r#"{"version":2,"client_id":"a","client_secret":"b","refresh_token":"c","access_token":null,"access_token_refresh_at":null}"#,
        ),
    ] {
        let path = write(name, content);

        assert!(matches!(problem(&path), TokenStoreProblem::Invalid(_)), "{name}");
    }
}

#[test]
fn failed_write_keeps_the_old_store() {
    let fake = FakeDrive::start();
    let scratch = TempDir::new().unwrap();

    if !read_only_dirs_are_enforced(scratch.path()) {
        eprintln!("skipped: read-only directories are not enforced (running as root)");
        return;
    }

    let dir = TempDir::new().unwrap();
    let store = store_in(&dir);

    Client::create(&store, sample_credentials(), fake.options()).unwrap();

    let before = fs::read(&store).unwrap();

    make_read_only(dir.path());

    let result = Client::create(
        &store,
        Credentials::new("new-id", "new-secret", "new-refresh"),
        fake.options(),
    );

    make_writable(dir.path());

    assert!(
        matches!(
            result,
            Err(Error::TokenStore {
                problem: TokenStoreProblem::WriteFailed(_),
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(fs::read(&store).unwrap(), before);
}

#[test]
fn credentials_and_client_debug_hide_secrets() {
    let fake = FakeDrive::start();
    let dir = TempDir::new().unwrap();
    let credentials = sample_credentials();
    let client = Client::create(store_in(&dir), credentials.clone(), fake.options()).unwrap();

    for text in [format!("{credentials:?}"), format!("{client:?}")] {
        assert!(text.contains("client-id-1"), "{text}");
        assert!(!text.contains("client-secret-1"), "{text}");
        assert!(!text.contains("refresh-token-1"), "{text}");
    }

    assert_eq!(credentials.client_id(), "client-id-1");
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn token_requests(fake: &FakeDrive) -> usize {
    fake.requests_of(Kind::Token).len()
}

fn seed_file(fake: &FakeDrive) -> String {
    fake.add_file("root", "seed.bin", b"seed")
}

fn bearer_of_last_get(fake: &FakeDrive) -> String {
    fake.requests_of(Kind::Get)
        .last()
        .and_then(|record| record.authorization.clone())
        .and_then(|header| header.strip_prefix("Bearer ").map(str::to_string))
        .expect("a Get request with a bearer token")
}

#[test]
fn first_call_refreshes_and_stores_the_access_token() {
    let env = env();
    let id = seed_file(&env.fake);

    env.client.metadata(&id).unwrap();

    let stored = read_json(&env.store_path());
    let refresh_at = stored["access_token_refresh_at"].as_u64().expect("a refresh time");

    assert_eq!(token_requests(&env.fake), 1);
    assert_eq!(stored["access_token"], bearer_of_last_get(&env.fake));
    assert!(refresh_at.abs_diff(unix_now() + 3540) <= 5, "{refresh_at}");
    assert_eq!(mode_of(&env.store_path()), 0o600);
}

#[test]
fn token_is_reused_until_60_seconds_before_expiry() {
    let long = env();
    let id = seed_file(&long.fake);

    long.fake.set_expires_in(3600);

    for _ in 0..3 {
        long.client.metadata(&id).unwrap();
    }

    assert_eq!(token_requests(&long.fake), 1);

    let short = env();
    let id = seed_file(&short.fake);

    short.fake.set_expires_in(59);

    for _ in 0..3 {
        short.client.metadata(&id).unwrap();
    }

    assert_eq!(token_requests(&short.fake), 3);
}

#[test]
fn stored_token_is_used_after_reopen() {
    let env = env();
    let id = seed_file(&env.fake);

    env.client.metadata(&id).unwrap();

    let reopened = Client::open(env.store_path(), env.fake.options()).unwrap();

    reopened.metadata(&id).unwrap();

    assert_eq!(token_requests(&env.fake), 1);
}

#[test]
fn a_401_refreshes_once_and_resends() {
    let env = env();
    let id = seed_file(&env.fake);

    env.fake.fault(Kind::Get, Fault::Unauthorized);

    env.client.metadata(&id).unwrap();

    assert_eq!(token_requests(&env.fake), 2);
    assert_eq!(env.fake.requests_of(Kind::Get).len(), 2);
}

#[test]
fn a_second_401_is_an_error() {
    let env = env();
    let id = seed_file(&env.fake);

    env.fake.fault(Kind::Get, Fault::Unauthorized);
    env.fake.fault(Kind::Get, Fault::Unauthorized);

    let error = env.client.metadata(&id).unwrap_err();

    assert!(matches!(error, Error::Http { status: 401, .. }), "{error:?}");
    assert_eq!(token_requests(&env.fake), 2);
}

#[test]
fn invalid_grant_is_a_distinct_error() {
    let env = env();
    let (client_id, client_secret, _) = env.fake.credentials();
    let client = Client::create(
        store_in(&env.dir),
        Credentials::new(client_id, client_secret, "revoked-refresh-token"),
        env.fake.options(),
    )
    .unwrap();

    let error = client.metadata("any-id").unwrap_err();

    assert!(matches!(error, Error::InvalidGrant { .. }), "{error:?}");
    assert!(error.to_string().contains("log in again"), "{error}");
}

#[test]
fn failed_access_token_write_does_not_fail_the_call() {
    let env = env();

    if !read_only_dirs_are_enforced(env.dir.path()) {
        eprintln!("skipped: read-only directories are not enforced (running as root)");
        return;
    }

    let id = seed_file(&env.fake);
    let before = fs::read(env.store_path()).unwrap();

    env.fake.set_expires_in(3600);
    make_read_only(env.dir.path());

    let first = env.client.metadata(&id);
    let failure = env.client.take_store_failure();
    let second_failure = env.client.take_store_failure();
    let again = env.client.metadata(&id);

    make_writable(env.dir.path());

    assert!(first.is_ok(), "{first:?}");
    assert!(
        matches!(
            failure,
            Some(Error::TokenStore {
                problem: TokenStoreProblem::WriteFailed(_),
                ..
            })
        ),
        "{failure:?}"
    );
    assert!(second_failure.is_none());
    assert_eq!(
        fs::read(env.store_path()).unwrap(),
        before,
        "the store must be untouched"
    );
    assert!(again.is_ok());
    assert_eq!(token_requests(&env.fake), 1, "the second call uses the in-memory token");

    // The directory is writable again: the next refresh (forced by a 401) writes the newest token.
    env.fake.set_expires_in(59);
    env.fake.fault(Kind::Get, Fault::Unauthorized);

    env.client.metadata(&id).unwrap();

    assert_eq!(
        read_json(&env.store_path())["access_token"],
        bearer_of_last_get(&env.fake)
    );
    assert!(env.client.take_store_failure().is_none());
}

#[test]
fn failed_write_of_rotated_credentials_fails_until_written() {
    let env = env();

    if !read_only_dirs_are_enforced(env.dir.path()) {
        eprintln!("skipped: read-only directories are not enforced (running as root)");
        return;
    }

    let id = seed_file(&env.fake);
    let original_refresh_token = env.fake.credentials().2;

    env.fake.rotate_refresh_tokens(true);
    make_read_only(env.dir.path());

    let first = env.client.metadata(&id);
    let second = env.client.metadata(&id);
    let requests_while_failing = token_requests(&env.fake);

    make_writable(env.dir.path());

    let third = env.client.metadata(&id);
    let stored = read_json(&env.store_path());

    for result in [&first, &second] {
        assert!(
            matches!(
                result,
                Err(Error::TokenStore {
                    problem: TokenStoreProblem::WriteFailed(_),
                    ..
                })
            ),
            "{result:?}"
        );
    }

    assert_eq!(
        requests_while_failing, 1,
        "no refresh with the old, now dead refresh token"
    );
    assert!(third.is_ok(), "{third:?}");
    assert_ne!(stored["refresh_token"], original_refresh_token.as_str());
    assert_eq!(env.fake.refresh_tokens(), [stored["refresh_token"].as_str().unwrap()]);
    assert_eq!(token_requests(&env.fake), 1);
}

fn assert_clean(error: &Error, secrets: &[&str]) {
    for text in [error.to_string(), format!("{error:?}")] {
        for secret in secrets {
            assert!(!text.contains(secret), "{secret} leaked into: {text}");
        }
    }
}

#[test]
fn secrets_absent_from_error_text() {
    let env = env();
    let (_, client_secret, refresh_token) = env.fake.credentials();
    let id = seed_file(&env.fake);

    env.client.metadata(&id).unwrap();

    let access_token = bearer_of_last_get(&env.fake);
    let secrets = [client_secret.as_str(), refresh_token.as_str(), access_token.as_str()];

    // A second 401.
    env.fake.fault(Kind::Get, Fault::Unauthorized);
    env.fake.fault(Kind::Get, Fault::Unauthorized);
    assert_clean(&env.client.metadata(&id).unwrap_err(), &secrets);

    // A Drive 403 whose message echoes every secret; the 401s above replaced the access token.
    let access_token = bearer_of_last_get(&env.fake);
    let secrets = [client_secret.as_str(), refresh_token.as_str(), access_token.as_str()];
    let echo = format!("{client_secret} {refresh_token} {access_token}");

    env.fake
        .fault(Kind::Get, Fault::drive_error_with_message(403, "forbidden", &echo));

    let error = env.client.metadata(&id).unwrap_err();
    let text = error.to_string();

    assert_clean(&error, &secrets);
    assert!(text.contains("[redacted]") && text.contains("forbidden"), "{text}");

    // An OAuth 400 whose description echoes the client secret and the refresh token.
    let oauth = crate::support::env();
    let (_, oauth_secret, oauth_refresh) = oauth.fake.credentials();
    let body =
        json!({"error": "invalid_request", "error_description": format!("bad {oauth_secret} and {oauth_refresh}")});

    oauth
        .fake
        .fault(Kind::Token, Fault::status_with_body(400, &body.to_string()));

    let error = oauth.client.metadata("any").unwrap_err();
    let text = error.to_string();

    assert_clean(&error, &[oauth_secret.as_str(), oauth_refresh.as_str()]);
    assert!(
        text.contains("[redacted]") && text.contains("invalid_request"),
        "{text}"
    );

    // invalid_grant.
    let revoked = Client::create(
        store_in(&oauth.dir),
        Credentials::new(
            oauth.fake.credentials().0,
            oauth_secret.clone(),
            "revoked-refresh-token",
        ),
        oauth.fake.options(),
    )
    .unwrap();

    assert_clean(
        &revoked.metadata("any").unwrap_err(),
        &[oauth_secret.as_str(), "revoked-refresh-token"],
    );

    // A refused connection ends at the deadline. Listening on port 1 needs root, so no fake started by a test running
    // in parallel can own it (a port that was bound and released could be handed to one).
    let options = Options {
        deadline: Duration::from_millis(300),
        endpoints: Endpoints::local("http://127.0.0.1:1"),
        ..oauth.fake.options()
    };
    let dir = TempDir::new().unwrap();
    let refused = Client::create(
        store_in(&dir),
        Credentials::new("id", oauth_secret.clone(), "refresh-secret-value"),
        options,
    )
    .unwrap()
    .metadata("any")
    .unwrap_err();

    assert!(matches!(refused, Error::Deadline { .. }), "{refused:?}");
    assert_clean(&refused, &[oauth_secret.as_str(), "refresh-secret-value"]);
}

#[test]
fn a_slow_refresh_is_shared_not_repeated() {
    let env = crate::support::env_with(|options| options.deadline = Duration::from_secs(3));
    let id = seed_file(&env.fake);

    env.fake
        .fault(Kind::Token, Fault::hook(|| std::thread::sleep(Duration::from_secs(1))));

    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| env.client.metadata(&id));

        std::thread::sleep(Duration::from_millis(100));

        let second = scope.spawn(|| env.client.metadata(&id));

        (first.join().unwrap(), second.join().unwrap())
    });

    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_ok(), "{second:?}");
    assert_eq!(token_requests(&env.fake), 1);

    let bearers: Vec<_> = env
        .fake
        .requests_of(Kind::Get)
        .iter()
        .map(|record| record.authorization.clone())
        .collect();

    assert_eq!(bearers.len(), 2);
    assert_eq!(bearers[0], bearers[1]);
}

#[test]
fn concurrent_calls_refresh_once_with_rotation() {
    let env = env();
    let id = seed_file(&env.fake);
    let original_refresh_token = env.fake.credentials().2;

    env.fake.rotate_refresh_tokens(true);
    env.fake.set_expires_in(3600);

    let barrier = std::sync::Barrier::new(8);
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    env.client.metadata(&id)
                })
            })
            .collect();

        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });

    for result in &results {
        assert!(result.is_ok(), "{result:?}");
    }

    let stored = read_json(&env.store_path());

    assert_eq!(token_requests(&env.fake), 1);
    assert_ne!(stored["refresh_token"], original_refresh_token.as_str());
    assert_eq!(env.fake.refresh_tokens(), [stored["refresh_token"].as_str().unwrap()]);
}

fn device_login(fake: &FakeDrive) -> DeviceLogin {
    let (client_id, client_secret, _) = fake.credentials();

    DeviceLogin::start(&client_id, &client_secret, &fake.options()).unwrap()
}

fn quick_device() -> DeviceConfig {
    DeviceConfig {
        interval: 0,
        ..DeviceConfig::default()
    }
}

#[test]
fn device_login_shows_url_and_code_then_approves() {
    let fake = FakeDrive::start();

    fake.set_device(DeviceConfig {
        user_code: "WXYZ-1234".to_string(),
        verification_url: "https://example.test/device".to_string(),
        expires_in: 900,
        ..quick_device()
    });
    fake.script_device_polls(&[DevicePoll::Pending, DevicePoll::Pending, DevicePoll::Approved]);

    let login = device_login(&fake);

    assert_eq!(login.verification_url(), "https://example.test/device");
    assert_eq!(login.user_code(), "WXYZ-1234");
    assert_eq!(login.expires_in(), Duration::from_secs(900));

    let credentials = login.wait().unwrap();
    let (client_id, client_secret, _) = fake.credentials();
    let polls = fake.requests_of(Kind::DevicePoll);

    assert_eq!(polls.len(), 3);

    for poll in &polls {
        assert_eq!(poll.form_value("client_id").as_deref(), Some(client_id.as_str()));
        assert_eq!(
            poll.form_value("client_secret").as_deref(),
            Some(client_secret.as_str())
        );
        assert_eq!(poll.form_value("device_code").as_deref(), Some("fake-device-code"));
        assert_eq!(
            poll.form_value("grant_type").as_deref(),
            Some("urn:ietf:params:oauth:grant-type:device_code")
        );
    }

    let dir = TempDir::new().unwrap();

    assert_eq!(credentials.client_id(), client_id);
    Client::create(store_in(&dir), credentials, fake.options()).unwrap();
    assert_eq!(read_json(&store_in(&dir))["refresh_token"], fake.device_refresh_token());
}

#[test]
fn device_login_asks_for_drive_file_scope() {
    let fake = FakeDrive::start();

    fake.set_device(quick_device());
    device_login(&fake);

    let requests = fake.requests_of(Kind::DeviceCode);

    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].form_value("scope").as_deref(),
        Some("https://www.googleapis.com/auth/drive.file")
    );
}

#[test]
fn slow_down_adds_five_seconds() {
    let fake = FakeDrive::start();

    fake.set_device(quick_device());
    fake.script_device_polls(&[DevicePoll::SlowDown, DevicePoll::Approved]);

    device_login(&fake).wait().unwrap();

    let polls = fake.requests_of(Kind::DevicePoll);

    assert_eq!(polls.len(), 2);
    assert!(polls[1].at - polls[0].at >= Duration::from_secs(5));
}

#[test]
fn denied_login() {
    let fake = FakeDrive::start();

    fake.set_device(quick_device());
    fake.script_device_polls(&[DevicePoll::Pending, DevicePoll::Denied]);

    assert!(matches!(device_login(&fake).wait(), Err(Error::LoginDenied)));
}

#[test]
fn login_expired_by_google() {
    let fake = FakeDrive::start();

    fake.set_device(quick_device());
    fake.script_device_polls(&[DevicePoll::Expired]);

    assert!(matches!(device_login(&fake).wait(), Err(Error::LoginExpired)));
}

#[test]
fn device_code_lifetime_out_of_range_is_a_protocol_error() {
    let fake = FakeDrive::start();

    fake.set_device(DeviceConfig {
        expires_in: u64::MAX,
        ..DeviceConfig::default()
    });

    let (client_id, client_secret, _) = fake.credentials();
    let result = DeviceLogin::start(&client_id, &client_secret, &fake.options());

    assert!(matches!(result, Err(Error::Protocol { .. })), "{result:?}");
}

#[test]
fn login_expires_locally() {
    let fake = FakeDrive::start();

    fake.set_device(DeviceConfig {
        expires_in: 2,
        interval: 1,
        ..DeviceConfig::default()
    });

    let started = Instant::now();
    let result = device_login(&fake).wait();

    assert!(matches!(result, Err(Error::LoginExpired)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
}

#[test]
fn poll_failures_retry_within_the_deadline() {
    let fake = FakeDrive::start();
    let (client_id, client_secret, _) = fake.credentials();
    let options = Options {
        deadline: Duration::from_secs(1),
        ..fake.options()
    };

    fake.set_device(DeviceConfig {
        expires_in: 60,
        ..quick_device()
    });
    fake.always(Kind::DevicePoll, 503);

    let login = DeviceLogin::start(&client_id, &client_secret, &options).unwrap();
    let started = Instant::now();
    let result = login.wait();

    assert!(matches!(result, Err(Error::Deadline { .. })), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert!(fake.requests_of(Kind::DevicePoll).len() > 1);
}

#[test]
fn poll_retry_waits_the_interval() {
    let fake = FakeDrive::start();

    fake.set_device(DeviceConfig {
        interval: 1,
        ..DeviceConfig::default()
    });
    fake.fault(Kind::DevicePoll, Fault::status(503));
    fake.script_device_polls(&[DevicePoll::Approved]);

    device_login(&fake).wait().unwrap();

    let polls = fake.requests_of(Kind::DevicePoll);

    assert_eq!(polls.len(), 2);
    assert!(
        polls[1].at - polls[0].at >= Duration::from_secs(1),
        "backoff alone is 10 ms"
    );
}

#[test]
fn device_login_debug_hides_code_and_secret() {
    let fake = FakeDrive::start();

    fake.set_device(quick_device());

    let text = format!("{:?}", device_login(&fake));

    assert!(text.contains("fake-client-id"), "{text}");
    assert!(!text.contains("fake-device-code"), "{text}");
    assert!(!text.contains("fake-client-secret"), "{text}");
}

#[test]
fn imported_rclone_credentials_work() {
    let env = env();
    let (client_id, client_secret, refresh_token) = env.fake.credentials();
    let config = format!(
        "[gdrive]\ntype = drive\nclient_id = {client_id}\nclient_secret = {client_secret}\nscope = drive.file\ntoken = {{\"access_token\":\"stale\",\"token_type\":\"Bearer\",\"refresh_token\":\"{refresh_token}\",\"expiry\":\"2020-01-01T00:00:00Z\"}}\n"
    );
    let config_path = env.dir.path().join("rclone.conf");

    fs::write(&config_path, config).unwrap();

    let credentials = Credentials::from_rclone_config(&config_path, "gdrive").unwrap();
    let client = Client::create(env.dir.path().join("imported.json"), credentials, env.fake.options()).unwrap();
    let id = seed_file(&env.fake);

    assert!(client.metadata(&id).is_ok());
}
