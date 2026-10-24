//! Folders, listing, metadata, delete and trash against the fake Drive.

use gdrive_client::{Error, Operation};

use crate::support::{Env, Fault, Kind, ROOT_ID, env};

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

fn count_of(env: &Env, name: &str) -> usize {
    env.fake.files_named(name).len()
}

#[test]
fn resolves_an_existing_nested_path() {
    let env = env();
    let a = env.fake.add_folder("root", "traffic-analysis");
    let b = env.fake.add_folder(&a, "recordings");

    assert_eq!(env.client.folder_id("traffic-analysis").unwrap(), a);
    assert_eq!(env.client.folder_id("traffic-analysis/recordings").unwrap(), b);
    assert_eq!(env.client.ensure_folder("traffic-analysis/recordings").unwrap(), b);
}

#[test]
fn missing_folder_names_the_path() {
    let env = env();

    env.fake.add_folder("root", "a");

    match env.client.folder_id("a/b") {
        Err(Error::FolderNotFound { path }) => assert_eq!(path, "a/b"),
        other => panic!("expected FolderNotFound, got {other:?}"),
    }
}

#[test]
fn ensure_folder_creates_missing_levels() {
    let env = env();
    let a = env.fake.add_folder("root", "a");

    let b = env.client.ensure_folder("a/b").unwrap();

    assert_eq!(env.fake.file(&b).unwrap().parents, [a]);
    assert_eq!(env.fake.file(&b).unwrap().name, "b");

    let creates = env.fake.requests_of(Kind::Create).len();

    assert_eq!(env.client.ensure_folder("a/b").unwrap(), b);
    assert_eq!(
        env.fake.requests_of(Kind::Create).len(),
        creates,
        "a second call must create nothing"
    );

    let top = env.client.ensure_folder("new-top/inner").unwrap();

    assert_eq!(env.fake.file(&top).unwrap().name, "inner");
    assert_eq!(count_of(&env, "new-top"), 1);
}

#[test]
fn duplicate_folders_name_the_path() {
    for page_size in [1000, 1] {
        let env = env();

        env.fake.set_page_size(page_size);
        env.fake.add_folder("root", "a");
        env.fake.add_folder("root", "a");

        match env.client.folder_id("a") {
            Err(Error::DuplicateFolder { path }) => assert_eq!(path, "a"),
            other => panic!("page size {page_size}: expected DuplicateFolder, got {other:?}"),
        }

        assert!(matches!(
            env.client.ensure_folder("a"),
            Err(Error::DuplicateFolder { .. })
        ));
    }
}

#[test]
fn trashed_folders_do_not_count() {
    let env = env();
    let trashed = env.fake.add_folder("root", "a");

    env.fake.set_trashed(&trashed, true);

    assert!(matches!(env.client.folder_id("a"), Err(Error::FolderNotFound { .. })));

    let created = env.client.ensure_folder("a").unwrap();

    assert_ne!(created, trashed);
    assert_eq!(count_of(&env, "a"), 2);
}

#[test]
fn lost_create_answer_does_not_duplicate() {
    let env = env();

    env.fake.fault(Kind::Create, Fault::ApplyThenClose);

    let id = env.client.ensure_folder("a").unwrap();

    assert_eq!(count_of(&env, "a"), 1);
    assert_eq!(env.fake.files_named("a")[0].id, id);
    assert_eq!(
        env.fake.requests_of(Kind::Create).len(),
        1,
        "the lookup finds the applied create"
    );
}

#[test]
fn rejects_empty_path_segments() {
    let env = env();

    for path in ["", "a//b", "/a", "a/"] {
        assert!(
            matches!(env.client.folder_id(path), Err(Error::InvalidInput { .. })),
            "{path:?}"
        );
        assert!(
            matches!(env.client.ensure_folder(path), Err(Error::InvalidInput { .. })),
            "{path:?}"
        );
    }

    assert!(env.fake.requests().is_empty());
}

#[test]
fn lists_every_page() {
    let env = env();
    let folder = env.fake.add_folder("root", "data");

    env.fake.set_page_size(2);

    for index in 0..5 {
        env.fake
            .add_file(&folder, &format!("f{index}.bin"), format!("content {index}").as_bytes());
    }

    let trashed = env.fake.add_file(&folder, "trashed.bin", b"gone");

    env.fake.set_trashed(&trashed, true);
    env.fake.add_folder(&folder, "subfolder");

    let files = env.client.list_files(&folder).unwrap();
    let mut names: Vec<_> = files.iter().map(|file| file.name.clone()).collect();

    names.sort();

    assert_eq!(names, ["f0.bin", "f1.bin", "f2.bin", "f3.bin", "f4.bin"]);
    assert_eq!(env.fake.requests_of(Kind::List).len(), 3);

    for file in &files {
        assert!(file.size.is_some() && file.md5.is_some(), "{file:?}");
    }
}

#[test]
fn find_returns_every_exact_match() {
    let env = env();
    let folder = env.fake.add_folder("root", "data");
    let first = env.fake.add_file(&folder, "x.gz", b"one");
    let second = env.fake.add_file(&folder, "x.gz", b"two");

    env.fake.add_file(&folder, "X.gz", b"case");
    env.fake.add_file(&folder, "x.gz.bak", b"suffix");
    env.fake.add_file("root", "x.gz", b"elsewhere");

    let mut ids: Vec<_> = env
        .client
        .find_files(&folder, "x.gz")
        .unwrap()
        .into_iter()
        .map(|file| file.id)
        .collect();

    ids.sort();

    assert_eq!(ids, [first, second]);
    assert!(env.client.find_files(&folder, "absent").unwrap().is_empty());
}

#[test]
fn quotes_and_backslashes_are_escaped() {
    let env = env();
    let folder_name = r"it's\here";
    let file_name = r"a'b\c.txt";
    let folder = env.fake.add_folder("root", folder_name);
    let file = env.fake.add_file(&folder, file_name, b"x");

    assert_eq!(env.client.folder_id(folder_name).unwrap(), folder);
    assert_eq!(env.client.ensure_folder(folder_name).unwrap(), folder);

    let found = env.client.find_files(&folder, file_name).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, file);

    let created = env.client.ensure_folder(r"it's\here/sub'dir\x").unwrap();

    assert_eq!(env.fake.file(&created).unwrap().name, r"sub'dir\x");
}
