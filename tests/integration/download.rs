//! Downloads through a temp file, against the fake Drive.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gdrive_client::Error;
use md5::{Digest, Md5};

use crate::support::{Env, FakeDrive, Fault, Kind, env, env_with};

fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 13 + index / 97) % 256) as u8)
        .collect()
}

fn md5_hex(bytes: &[u8]) -> String {
    Md5::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn destination(env: &Env) -> PathBuf {
    let directory = env.dir.path().join("downloads");

    fs::create_dir_all(&directory).unwrap();
    directory.join("out.bin")
}

fn names_in(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();

    names.sort();
    names
}

#[test]
fn downloads_through_a_temp_file() {
    let env = env();
    let data = pattern(300 * 1024 + 17);
    let id = env.fake.add_file("root", "remote.bin", &data);
    let target = destination(&env);

    let download = env.client.download(&id, &target).unwrap();

    assert_eq!(fs::read(&target).unwrap(), data);
    assert_eq!(download.size, data.len() as u64);
    assert_eq!(download.md5, md5_hex(&data));
    assert_eq!(names_in(&target), ["out.bin"], "no .part file may be left");
}

#[test]
fn truncated_download_restarts_from_the_start() {
    let env = env();
    let data = pattern(300 * 1024);
    let id = env.fake.add_file("root", "remote.bin", &data);
    let target = destination(&env);

    env.fake.fault(Kind::Download, Fault::TruncateBody(100_000));

    let download = env.client.download(&id, &target).unwrap();

    assert_eq!(env.fake.requests_of(Kind::Download).len(), 2);
    assert_eq!(fs::read(&target).unwrap(), data);
    assert_eq!(download.md5, md5_hex(&data));
    assert_eq!(names_in(&target), ["out.bin"]);
}

#[test]
fn interrupted_download_leaves_no_file() {
    let env = env_with(|options| options.deadline = Duration::from_secs(1));
    let id = env.fake.add_file("root", "remote.bin", &pattern(300 * 1024));
    let target = destination(&env);

    for _ in 0..500 {
        env.fake.fault(Kind::Download, Fault::TruncateBody(1000));
    }

    let error = env.client.download(&id, &target).unwrap_err();

    assert!(matches!(error, Error::Deadline { .. }), "{error:?}");
    assert!(names_in(&target).is_empty(), "{:?}", names_in(&target));
}

#[test]
fn failed_download_keeps_the_existing_file() {
    let env = env_with(|options| options.deadline = Duration::from_millis(500));
    let id = env.fake.add_file("root", "remote.bin", &pattern(300 * 1024));
    let target = destination(&env);

    fs::write(&target, b"old content").unwrap();

    for _ in 0..500 {
        env.fake.fault(Kind::Download, Fault::TruncateBody(1000));
    }

    assert!(env.client.download(&id, &target).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"old content");
    assert_eq!(names_in(&target), ["out.bin"]);

    assert!(env.client.download("no-such-id", &target).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"old content");
}

#[test]
fn unknown_id_is_an_error_and_writes_nothing() {
    let env = env();
    let target = destination(&env);

    let error = env.client.download("no-such-id", &target).unwrap_err();

    assert!(matches!(error, Error::Http { status: 404, .. }), "{error:?}");
    assert!(names_in(&target).is_empty());
}

#[test]
fn malformed_redirect_is_not_echoed() {
    let env = env();
    let id = env.fake.add_file("root", "remote.bin", b"data");
    let target = destination(&env);

    // ureq-proto rejects a network-path reference that does not parse as a URL, and prints it in its own error.
    env.fake.fault(
        Kind::Download,
        Fault::Redirect {
            status: 302,
            location: "//[bad/?upload_id=s3cr3t".to_string(),
        },
    );

    let error = env.client.download(&id, &target).unwrap_err();

    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains("s3cr3t"), "{text}");
        assert!(!text.contains("[bad"), "{text}");
    }

    assert!(names_in(&target).is_empty());
}

#[test]
fn a_redirect_to_another_origin_carries_no_bearer_token() {
    let env = env();
    let data = pattern(1000);
    let id = env.fake.add_file("root", "remote.bin", &data);
    let other = FakeDrive::start();
    let other_id = other.add_file("root", "remote.bin", &data);
    let target = destination(&env);

    env.fake.fault(
        Kind::Download,
        Fault::Redirect {
            status: 302,
            location: format!("{}/drive/v3/files/{other_id}?alt=media", other.url()),
        },
    );

    // The other origin answers 401 without a token; the client refreshes once and resends to the Drive endpoint.
    env.client.download(&id, &target).unwrap();

    let followed = other.requests_of(Kind::Download);

    assert_eq!(followed.len(), 1, "{followed:?}");
    assert_eq!(followed[0].authorization, None);
    assert_eq!(fs::read(&target).unwrap(), data);
}

#[test]
fn unparsable_absolute_redirect_leaks_nothing_and_leaves_no_part_file() {
    let env = env();
    let id = env.fake.add_file("root", "remote.bin", b"data");
    let target = destination(&env);

    // ureq-proto reads this as a path relative to the request, so the download goes on without the bearer token.
    env.fake.fault(
        Kind::Download,
        Fault::Redirect {
            status: 302,
            location: "http://[bad/?upload_id=s3cr3t".to_string(),
        },
    );

    let result = env.client.download(&id, &target);

    if let Err(error) = &result {
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("s3cr3t") && !text.contains("[bad"), "{text}");
        }
    }

    assert_eq!(names_in(&target), if result.is_ok() { vec!["out.bin"] } else { vec![] });
}
