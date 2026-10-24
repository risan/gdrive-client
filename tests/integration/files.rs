//! Folders, listing, metadata, delete and trash against the fake Drive.

use gdrive_client::{Error, Operation};

use crate::support::{Kind, ROOT_ID, env};

#[test]
fn metadata_returns_every_field() {
    let env = env();
    let id = env.fake.add_file("root", "x.bin", b"hello");

    let file = env.client.metadata(&id).unwrap();

    assert_eq!(file.id, id);
    assert_eq!(file.name, "x.bin");
    assert_eq!(file.mime_type, "application/octet-stream");
    assert_eq!(file.size, Some(5));
    assert_eq!(file.md5.as_deref(), Some("5d41402abc4b2a76b9719d911017c592"));
    assert_eq!(file.parents, [ROOT_ID]);
    assert!(!file.trashed);
    assert!(
        file.modified_time
            .as_deref()
            .is_some_and(|time| time.starts_with("2026-"))
    );

    let requested = env.fake.requests_of(Kind::Get).remove(0).param("fields").unwrap();

    for field in [
        "id",
        "name",
        "mimeType",
        "size",
        "md5Checksum",
        "modifiedTime",
        "parents",
        "trashed",
    ] {
        assert!(
            requested.split(',').any(|name| name == field),
            "{field} missing from {requested}"
        );
    }
}

#[test]
fn metadata_of_a_folder_has_no_size_or_md5() {
    let env = env();
    let id = env.fake.add_folder("root", "docs");

    let folder = env.client.metadata(&id).unwrap();

    assert_eq!(folder.size, None);
    assert_eq!(folder.md5, None);
    assert_eq!(folder.mime_type, "application/vnd.google-apps.folder");
}

#[test]
fn metadata_of_an_unknown_id_names_id_and_reason() {
    let env = env();

    let error = env.client.metadata("no-such-id").unwrap_err();

    match &error {
        Error::Http {
            operation,
            target,
            status,
            reason,
            ..
        } => {
            assert_eq!(*operation, Operation::Metadata);
            assert_eq!(target, "no-such-id");
            assert_eq!(*status, 404);
            assert_eq!(reason.as_deref(), Some("notFound"));
        }
        other => panic!("expected Http, got {other:?}"),
    }

    assert!(error.to_string().contains("no-such-id"), "{error}");
}
