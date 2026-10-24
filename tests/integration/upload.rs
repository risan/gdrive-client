//! Resumable uploads against the fake Drive, which enforces Drive's offsets and 256 KiB chunks.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use gdrive_client::Error;
use md5::{Digest, Md5};

use crate::support::{Env, Fault, Kind, env, env_with};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const CHUNK: u64 = 256 * 1024;

fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 7 + index / 251) % 256) as u8)
        .collect()
}

fn md5_hex(bytes: &[u8]) -> String {
    Md5::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn local_file(env: &Env, name: &str, bytes: &[u8]) -> PathBuf {
    let path = env.dir.path().join(name);

    fs::write(&path, bytes).unwrap();
    path
}

fn chunk_ranges(env: &Env) -> Vec<String> {
    env.fake
        .requests_of(Kind::ChunkPut)
        .into_iter()
        .map(|record| record.content_range.expect("a chunk carries Content-Range"))
        .collect()
}

fn count(env: &Env, kind: Kind) -> usize {
    env.fake.requests_of(kind).len()
}

fn target_folder(env: &Env) -> String {
    env.fake.add_folder("root", "target")
}

#[test]
fn uploads_a_new_file_in_chunks() {
    let env = env();
    let folder = target_folder(&env);
    let data = pattern(MIB + 123);
    let path = local_file(&env, "new.bin", &data);

    let upload = env.client.upload_new(&path, &folder, "new.bin").unwrap();

    assert_eq!(
        chunk_ranges(&env),
        [
            "bytes 0-262143/1048699",
            "bytes 262144-524287/1048699",
            "bytes 524288-786431/1048699",
            "bytes 786432-1048575/1048699",
            "bytes 1048576-1048698/1048699",
        ]
    );

    let stored = env.fake.file(&upload.file.id).expect("the file exists in the fake");

    assert_eq!(stored.content.as_deref(), Some(data.as_slice()));
    assert_eq!(stored.name, "new.bin");
    assert_eq!(stored.size, Some(data.len() as u64));
    assert_eq!(stored.parents, [folder]);
    assert_eq!(upload.file.size, Some(data.len() as u64));
    assert_eq!(upload.file.md5.as_deref(), Some(md5_hex(&data).as_str()));
    assert_eq!(upload.local_size, data.len() as u64);
    assert_eq!(upload.local_md5, md5_hex(&data));
    assert!(!upload.local_file_changed);
    assert!(upload.verified());
    assert_eq!(count(&env, Kind::UploadInit), 1);

    for chunk in env.fake.requests_of(Kind::ChunkPut) {
        assert!(chunk.authorization.is_some_and(|header| header.starts_with("Bearer ")));
    }
}

#[test]
fn replaces_the_content_of_an_existing_id() {
    let env = env();
    let id = env.fake.add_file("root", "gaps.log", b"old content");
    let data = pattern(300 * KIB);
    let path = local_file(&env, "gaps.log", &data);

    let upload = env.client.upload_replace(&path, &id).unwrap();

    let init = env.fake.requests_of(Kind::UploadInit).remove(0);

    assert_eq!(init.method, "PATCH");
    assert!(init.path.ends_with(&id), "{}", init.path);
    assert_eq!(upload.file.id, id);
    assert_eq!(upload.file.name, "gaps.log");
    assert_eq!(env.fake.file(&id).unwrap().content.as_deref(), Some(data.as_slice()));
    assert_eq!(env.fake.files().len(), 1);
    assert!(upload.verified());
}

#[test]
fn resumes_after_a_dropped_connection() {
    let env = env();
    let folder = target_folder(&env);
    let data = pattern(MIB);
    let path = local_file(&env, "drop.bin", &data);

    env.fake
        .fault_after(1, Kind::ChunkPut, Fault::DropAfterBody { persist: 100 * 1024 });

    let upload = env.client.upload_new(&path, &folder, "drop.bin").unwrap();
    let ranges = chunk_ranges(&env);

    assert_eq!(ranges[1], "bytes 262144-524287/1048576");
    assert_eq!(
        ranges[2], "bytes 364544-626687/1048576",
        "continues at 256 KiB + 100 KiB"
    );
    assert_eq!(count(&env, Kind::StatusQuery), 1);
    assert_eq!(
        env.fake.requests_of(Kind::StatusQuery)[0].content_range.as_deref(),
        Some("bytes */1048576")
    );
    assert_eq!(count(&env, Kind::UploadInit), 1);
    assert_eq!(
        env.fake.file(&upload.file.id).unwrap().content.as_deref(),
        Some(data.as_slice())
    );
    assert!(upload.verified());
}

#[test]
fn resumes_after_503_mid_upload() {
    let env = env();
    let folder = target_folder(&env);
    let data = pattern(MIB);
    let path = local_file(&env, "busy.bin", &data);

    env.fake.fault_after(2, Kind::ChunkPut, Fault::status(503));

    let upload = env.client.upload_new(&path, &folder, "busy.bin").unwrap();
    let ranges = chunk_ranges(&env);

    assert_eq!(ranges[2], ranges[3], "nothing was kept, so the same range is resent");
    assert!(upload.verified());

    let env = crate::support::env();
    let folder = target_folder(&env);
    let path = local_file(&env, "applied.bin", &data);

    env.fake.fault_after(1, Kind::ChunkPut, Fault::ApplyThenClose);

    let upload = env.client.upload_new(&path, &folder, "applied.bin").unwrap();
    let ranges = chunk_ranges(&env);

    assert_eq!(ranges[1], "bytes 262144-524287/1048576");
    assert_eq!(
        ranges[2], "bytes 524288-786431/1048576",
        "the applied chunk is not sent again"
    );
    assert!(upload.verified());
}

#[test]
fn status_query_failures_are_retried() {
    let env = env();
    let folder = target_folder(&env);
    let data = pattern(600 * KIB);
    let path = local_file(&env, "query.bin", &data);

    env.fake.fault(Kind::ChunkPut, Fault::CloseBeforeResponse);
    env.fake.fault(Kind::StatusQuery, Fault::status(503));

    let upload = env.client.upload_new(&path, &folder, "query.bin").unwrap();

    assert_eq!(count(&env, Kind::StatusQuery), 2);
    assert!(upload.verified());
}

#[test]
fn rate_limited_chunk_resumes() {
    let env = env();
    let folder = target_folder(&env);
    let data = pattern(MIB);
    let path = local_file(&env, "limited.bin", &data);

    env.fake
        .fault_after(1, Kind::ChunkPut, Fault::drive_error(403, "userRateLimitExceeded"));

    let upload = env.client.upload_new(&path, &folder, "limited.bin").unwrap();

    assert_eq!(count(&env, Kind::UploadInit), 1, "the same session continues");
    assert_eq!(count(&env, Kind::StatusQuery), 1);
    assert!(upload.verified());
}

#[test]
fn partial_confirmation_resends_the_tail() {
    let env = env();
    let folder = target_folder(&env);
    let data = pattern(MIB);
    let path = local_file(&env, "partial.bin", &data);

    env.fake.fault_after(1, Kind::ChunkPut, Fault::ReportRange(384 * 1024));

    let upload = env.client.upload_new(&path, &folder, "partial.bin").unwrap();
    let ranges = chunk_ranges(&env);

    assert_eq!(ranges[1], "bytes 262144-524287/1048576");
    assert_eq!(ranges[2], "bytes 393216-655359/1048576", "384 KiB, then a full 256 KiB");
    assert_eq!(
        env.fake.file(&upload.file.id).unwrap().content.as_deref(),
        Some(data.as_slice())
    );
    assert!(upload.verified());
}

#[test]
fn lost_session_restarts_once() {
    let data = pattern(MIB);

    let env = env();
    let folder = target_folder(&env);
    let path = local_file(&env, "gone.bin", &data);

    env.fake.fault_after(2, Kind::ChunkPut, Fault::SessionGone(404));

    let upload = env.client.upload_new(&path, &folder, "gone.bin").unwrap();

    assert_eq!(count(&env, Kind::UploadInit), 2);
    assert_eq!(
        env.fake.file(&upload.file.id).unwrap().content.as_deref(),
        Some(data.as_slice())
    );
    assert!(upload.verified());

    let env = crate::support::env();
    let folder = target_folder(&env);
    let path = local_file(&env, "gone-on-query.bin", &data);

    env.fake.fault(Kind::ChunkPut, Fault::CloseBeforeResponse);
    env.fake.fault(Kind::StatusQuery, Fault::SessionGone(410));

    let upload = env.client.upload_new(&path, &folder, "gone-on-query.bin").unwrap();

    assert_eq!(count(&env, Kind::UploadInit), 2);
    assert_eq!(
        env.fake.file(&upload.file.id).unwrap().content.as_deref(),
        Some(data.as_slice())
    );
    assert!(upload.verified());
}

#[test]
fn second_session_loss_is_an_error() {
    let env = env();
    let folder = target_folder(&env);
    let path = local_file(&env, "doomed.bin", &pattern(MIB));

    env.fake.fault(Kind::ChunkPut, Fault::SessionGone(404));
    env.fake.fault(Kind::ChunkPut, Fault::SessionGone(404));

    let error = env.client.upload_new(&path, &folder, "doomed.bin").unwrap_err();

    assert!(matches!(error, Error::Http { status: 404, .. }), "{error:?}");
    assert_eq!(count(&env, Kind::UploadInit), 2);
    assert!(env.fake.files().iter().all(|file| file.name != "doomed.bin"));
}

#[test]
fn offset_below_the_confirmed_one_is_a_protocol_error() {
    let env = env();
    let folder = target_folder(&env);
    let path = local_file(&env, "lying.bin", &pattern(MIB));

    env.fake.fault_after(1, Kind::ChunkPut, Fault::ReportRange(100 * 1024));

    let error = env.client.upload_new(&path, &folder, "lying.bin").unwrap_err();

    match &error {
        Error::Protocol { detail, .. } => {
            assert!(detail.contains("102400"), "{detail}");
            assert!(detail.contains("262144"), "{detail}");
        }
        other => panic!("expected Protocol, got {other:?}"),
    }
}

#[test]
fn verify_fails_when_drive_md5_differs() {
    let env = env();
    let folder = target_folder(&env);
    let path = local_file(&env, "corrupt.bin", &pattern(MIB));

    env.fake.fault_after(3, Kind::ChunkPut, Fault::CorruptMd5);

    let upload = env.client.upload_new(&path, &folder, "corrupt.bin").unwrap();

    assert!(!upload.local_file_changed);
    assert!(!upload.verified());
    assert_ne!(upload.file.md5.as_deref(), Some(upload.local_md5.as_str()));
}

#[test]
fn verify_fails_when_the_local_file_changes() {
    let original = pattern(MIB);

    for during_chunk in [1, 3] {
        let env = env();
        let folder = target_folder(&env);
        let path = local_file(&env, "changing.bin", &original);
        let appended = path.clone();

        env.fake.fault_after(
            during_chunk,
            Kind::ChunkPut,
            Fault::hook(move || {
                let mut file = OpenOptions::new().append(true).open(&appended).unwrap();

                file.write_all(b"appended while uploading").unwrap();
            }),
        );

        let upload = env.client.upload_new(&path, &folder, "changing.bin").unwrap();

        assert_eq!(
            env.fake.file(&upload.file.id).unwrap().content.as_deref(),
            Some(original.as_slice()),
            "Drive got the original length (chunk {during_chunk})"
        );
        assert!(upload.local_file_changed, "chunk {during_chunk}");
        assert!(!upload.verified(), "chunk {during_chunk}");
    }
}

#[test]
fn verify_fails_when_the_path_is_replaced() {
    let env = env();
    let folder = target_folder(&env);
    let original = pattern(MIB);
    let path = local_file(&env, "replaced.bin", &original);
    let replacement = local_file(&env, "replacement.bin", &pattern(MIB + 5));
    let target = path.clone();

    env.fake.fault_after(
        1,
        Kind::ChunkPut,
        Fault::hook(move || fs::rename(&replacement, &target).unwrap()),
    );

    let upload = env.client.upload_new(&path, &folder, "replaced.bin").unwrap();

    assert_eq!(
        env.fake.file(&upload.file.id).unwrap().content.as_deref(),
        Some(original.as_slice()),
        "the open handle still uploads the original bytes"
    );
    assert!(upload.local_file_changed);
    assert!(!upload.verified());
}

#[test]
fn shrinking_local_file_stops_the_upload() {
    let env = env();
    let folder = target_folder(&env);
    let path = local_file(&env, "shrinking.bin", &pattern(MIB));
    let target = path.clone();

    env.fake.fault_after(
        1,
        Kind::ChunkPut,
        Fault::hook(move || {
            OpenOptions::new()
                .write(true)
                .open(&target)
                .unwrap()
                .set_len(300 * 1024)
                .unwrap();
        }),
    );

    let error = env.client.upload_new(&path, &folder, "shrinking.bin").unwrap_err();

    assert!(matches!(error, Error::LocalFileChanged { .. }), "{error:?}");
    assert!(env.fake.files().iter().all(|file| file.name != "shrinking.bin"));
}

#[test]
fn missing_fields_in_the_final_answer_are_fetched() {
    let env = env();
    let folder = target_folder(&env);
    let data = pattern(300 * KIB);
    let path = local_file(&env, "bare.bin", &data);

    env.fake.fault_after(1, Kind::ChunkPut, Fault::OmitFields);

    let upload = env.client.upload_new(&path, &folder, "bare.bin").unwrap();

    assert_eq!(count(&env, Kind::Get), 1, "one metadata call fills size and MD5");
    assert_eq!(upload.file.size, Some(data.len() as u64));
    assert!(upload.verified());
}

#[test]
fn empty_file() {
    let env = env();
    let folder = target_folder(&env);
    let path = local_file(&env, "empty.bin", b"");

    let upload = env.client.upload_new(&path, &folder, "empty.bin").unwrap();
    let puts: Vec<_> = env
        .fake
        .requests()
        .into_iter()
        .filter(|record| record.method == "PUT")
        .collect();

    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].content_range.as_deref(), Some("bytes */0"));
    assert_eq!(puts[0].body_len, 0);
    assert_eq!(upload.local_md5, "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(upload.file.size, Some(0));
    assert!(upload.verified());
    assert_eq!(
        env.fake.file(&upload.file.id).unwrap().content.as_deref(),
        Some(&[][..])
    );
}

#[test]
fn deadline_mid_upload_reports_confirmed_bytes() {
    let env = env_with(|options| options.deadline = Duration::from_secs(1));
    let folder = target_folder(&env);
    let path = local_file(&env, "slow.bin", &pattern(MIB));

    env.fake.always_after(2, Kind::ChunkPut, 503);

    let error = env.client.upload_new(&path, &folder, "slow.bin").unwrap_err();

    match &error {
        Error::Deadline { confirmed_bytes, .. } => assert_eq!(*confirmed_bytes, Some(2 * CHUNK)),
        other => panic!("expected Deadline, got {other:?}"),
    }

    assert!(error.to_string().contains("524288"), "{error}");
    assert!(env.fake.files().iter().all(|file| file.name != "slow.bin"));
}

#[test]
fn session_url_on_another_origin_is_refused() {
    let env = env();
    let folder = target_folder(&env);
    let path = local_file(&env, "foreign.bin", &pattern(300 * KIB));
    let fake_url = env.fake.url();
    let fake_port: u16 = fake_url.rsplit(':').next().unwrap().parse().unwrap();

    for foreign in [
        format!("http://127.0.0.1:{}/upload/drive/v3/files?upload_id=x", fake_port + 1),
        format!("{fake_url}.attacker.example/upload/drive/v3/files?upload_id=x"),
        "https://drive.attacker.example/upload/drive/v3/files?upload_id=x".to_string(),
    ] {
        env.fake
            .fault(Kind::UploadInit, Fault::ForeignLocation(foreign.clone()));

        let error = env.client.upload_new(&path, &folder, "foreign.bin").unwrap_err();

        assert!(matches!(error, Error::Protocol { .. }), "{foreign}: {error:?}");
        assert!(!format!("{error} {error:?}").contains("upload_id"));
    }

    assert_eq!(count(&env, Kind::ChunkPut), 0, "nothing may be sent to another origin");
    assert_eq!(count(&env, Kind::StatusQuery), 0);
}

#[test]
fn upload_errors_hide_the_session_url() {
    let env = env_with(|options| options.deadline = Duration::from_millis(500));
    let folder = target_folder(&env);
    let path = local_file(&env, "secret-url.bin", &pattern(MIB));

    env.fake.always_after(1, Kind::ChunkPut, 503);

    let deadline = env.client.upload_new(&path, &folder, "secret-url.bin").unwrap_err();

    assert!(matches!(deadline, Error::Deadline { .. }), "{deadline:?}");

    let env = crate::support::env();
    let folder = target_folder(&env);
    let path = local_file(&env, "secret-url-2.bin", &pattern(MIB));

    env.fake.fault_after(1, Kind::ChunkPut, Fault::ReportRange(1024));

    let protocol = env.client.upload_new(&path, &folder, "secret-url-2.bin").unwrap_err();

    assert!(matches!(protocol, Error::Protocol { .. }), "{protocol:?}");

    for error in [deadline, protocol] {
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("upload_id"), "{text}");
            assert!(!text.contains("s3ss10n"), "{text}");
        }
    }
}
