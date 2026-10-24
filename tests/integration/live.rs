//! The one test that talks to the real Google Drive. It is skipped (and passes) unless `GDRIVE_CLIENT_LIVE=1` and
//! `GDRIVE_CLIENT_TOKEN_STORE` names a token store made by `cargo run --example gdrive -- --store <store> login ...`:
//!
//! ```text
//! GDRIVE_CLIENT_LIVE=1 GDRIVE_CLIENT_TOKEN_STORE=<store> \
//!     cargo test --locked --test integration live:: -- --nocapture
//! ```
//!
//! It works in a folder it creates at My Drive's root and removes it at the end, or on failure. It rewrites the given
//! store's access token (atomically), nothing else.

use std::fs;
use std::path::PathBuf;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use gdrive_client::{Client, Options};
use md5::{Digest, Md5};
use tempfile::TempDir;

const KIB: usize = 1024;

/// Deletes the folder (and with it anything left inside) when a step fails.
struct FolderGuard<'a> {
    client: &'a Client,
    folder_id: String,
    armed: bool,
}

impl Drop for FolderGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.client.delete(&self.folder_id);
        }
    }
}

fn bytes(length: usize, seed: usize) -> Vec<u8> {
    (0..length)
        .map(|index| ((index * 31 + seed * 17 + index / 255) % 256) as u8)
        .collect()
}

fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn round_trip() {
    let token_store = match (
        std::env::var("GDRIVE_CLIENT_LIVE").as_deref(),
        std::env::var_os("GDRIVE_CLIENT_TOKEN_STORE"),
    ) {
        (Ok("1"), Some(store)) => PathBuf::from(store),
        _ => {
            println!("live test skipped: set GDRIVE_CLIENT_LIVE=1 and GDRIVE_CLIENT_TOKEN_STORE=<token store>");
            return;
        }
    };

    // Two chunks of 256 KiB, so the real Drive answers a real 308.
    let options = Options {
        chunk_size: 256 * KIB,
        ..Options::default()
    };
    let client = Client::open(&token_store, options).expect("open the token store");
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let folder_name = format!("gdrive-client-live-{seconds}-{}", process::id());
    let folder_id = client.ensure_folder(&folder_name).expect("create the folder");
    let mut guard = FolderGuard {
        client: &client,
        folder_id: folder_id.clone(),
        armed: true,
    };
    let scratch = TempDir::new().unwrap();
    let local = scratch.path().join("live.bin");
    let first = bytes(300 * KIB, 1);

    fs::write(&local, &first).unwrap();

    let upload = client.upload_new(&local, &folder_id, "live.bin").expect("upload");

    assert!(upload.verified(), "{upload:?}");

    let file_id = upload.file.id.clone();
    let metadata = client.metadata(&file_id).expect("metadata");

    assert_eq!(metadata.size, Some(first.len() as u64));
    assert_eq!(metadata.md5.as_deref(), Some(md5_hex(&first).as_str()));
    assert_eq!(metadata.parents, std::slice::from_ref(&folder_id));

    let listed = client.list_files(&folder_id).expect("list");

    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].md5.as_deref(), Some(md5_hex(&first).as_str()));

    let found = client.find_files(&folder_id, "live.bin").expect("find");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, file_id);

    let second = bytes(310 * KIB, 2);

    fs::write(&local, &second).unwrap();

    let replaced = client.upload_replace(&local, &file_id).expect("replace");

    assert_eq!(replaced.file.id, file_id, "the id is kept");
    assert!(replaced.verified(), "{replaced:?}");

    let downloaded = scratch.path().join("downloaded.bin");
    let download = client.download(&file_id, &downloaded).expect("download");

    assert_eq!(fs::read(&downloaded).unwrap(), second);
    assert_eq!(download.md5, md5_hex(&second));

    client.delete(&file_id).expect("delete the file");
    client.delete(&folder_id).expect("delete the folder");
    guard.armed = false;
}
