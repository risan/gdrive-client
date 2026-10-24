//! Credentials, the token store and the token lifecycle against the fake OAuth endpoint.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use gdrive_client::{Client, Credentials, Error, TokenStoreProblem};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{FakeDrive, read_only_dirs_are_enforced};

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
