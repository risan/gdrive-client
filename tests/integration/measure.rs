//! AC5: peak resident memory (`VmHWM`) of the example moving 1 GiB files and 64 MiB files against the fake, and so
//! whether memory depends on file size. Ignored by default; run it in release against the static musl example:
//!
//! ```text
//! GDRIVE_CLIENT_EXAMPLE=target/x86_64-unknown-linux-musl/release/examples/gdrive \
//!     cargo test --release --locked --test integration measure:: -- --ignored --nocapture
//! ```
//!
//! `VmHWM` counts pages mapped into the process (heap, stack, binary text), not the page cache that `read` and `write`
//! go through, so the disk traffic of a 1 GiB file does not reach the number. The upload sources are sparse files
//! (`File::set_len`, no data blocks), and every download goes to a temp directory that is emptied before the next.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use gdrive_client::Client;
use tempfile::TempDir;

use crate::support::FakeDrive;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const KIB_PER_MIB: i64 = 1024;

struct Case {
    label: &'static str,
    chunk_mib: u64,
    peak_kib: u64,
}

struct Rig {
    example: PathBuf,
    fake: FakeDrive,
    store: PathBuf,
    scratch: TempDir,
}

impl Rig {
    fn run(&self, chunk_mib: u64, command: &[&str]) -> u64 {
        let output = Command::new(&self.example)
            .args(["--store", self.store.to_str().unwrap(), "--endpoint", &self.fake.url()])
            .args(["--chunk-mib", &chunk_mib.to_string(), "--print-peak-rss"])
            .args(command)
            .output()
            .expect("run the example");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(
            output.status.success(),
            "{command:?}\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM_KiB "))
            .and_then(|number| number.trim().parse().ok())
            .unwrap_or_else(|| panic!("no VmHWM line in {stdout}"))
    }

    fn upload(&self, label: &'static str, source: &Path, chunk_mib: u64) -> Case {
        let folder = format!("measure/{label}-{chunk_mib}");

        Case {
            label,
            chunk_mib,
            peak_kib: self.run(chunk_mib, &["upload", source.to_str().unwrap(), &folder]),
        }
    }

    fn download(&self, label: &'static str, file_id: &str, chunk_mib: u64) -> Case {
        let directory = self.scratch.path().join("downloads");

        fs::create_dir_all(&directory).unwrap();

        let target = directory.join("out.bin");
        let peak_kib = self.run(chunk_mib, &["download", file_id, target.to_str().unwrap()]);

        assert!(fs::metadata(&target).unwrap().len() > 0);
        fs::remove_dir_all(&directory).unwrap();
        Case {
            label,
            chunk_mib,
            peak_kib,
        }
    }
}

fn sparse_file(directory: &Path, name: &str, length: u64) -> PathBuf {
    let path = directory.join(name);

    File::create(&path).unwrap().set_len(length).unwrap();
    path
}

fn within(a: u64, b: u64, limit_kib: i64) -> bool {
    (a as i64 - b as i64).abs() <= limit_kib
}

#[test]
#[ignore = "release build and the static musl example; see the module comment"]
fn peak_rss() {
    let example = PathBuf::from(
        std::env::var_os("GDRIVE_CLIENT_EXAMPLE").expect("GDRIVE_CLIENT_EXAMPLE must name the static musl example"),
    );
    let fake = FakeDrive::start();

    fake.store_content(false);

    let scratch = TempDir::new().unwrap();
    let store = scratch.path().join("token.json");

    Client::create(&store, fake.valid_credentials(), fake.options()).unwrap();

    let source_64m = sparse_file(scratch.path(), "source-64m.bin", 64 * MIB);
    let source_1g = sparse_file(scratch.path(), "source-1g.bin", GIB);
    let remote_64m = fake.add_generated_file("root", "remote-64m.bin", 64 * MIB);
    let remote_1g = fake.add_generated_file("root", "remote-1g.bin", GIB);
    let rig = Rig {
        example,
        fake,
        store,
        scratch,
    };

    let upload_1g_8 = rig.upload("upload 1 GiB", &source_1g, 8);
    let upload_1g_1 = rig.upload("upload 1 GiB", &source_1g, 1);
    let upload_64m_8 = rig.upload("upload 64 MiB", &source_64m, 8);
    let download_1g_8 = rig.download("download 1 GiB", &remote_1g, 8);
    let download_1g_1 = rig.download("download 1 GiB", &remote_1g, 1);
    let download_64m_8 = rig.download("download 64 MiB", &remote_64m, 8);

    println!("\n| operation | chunk size | peak RSS (VmHWM) |\n|---|---|---|");

    for case in [
        &upload_1g_8,
        &upload_1g_1,
        &upload_64m_8,
        &download_1g_8,
        &download_1g_1,
        &download_64m_8,
    ] {
        println!(
            "| {} | {} MiB{} | {} KiB |",
            case.label,
            case.chunk_mib,
            if case.label.starts_with("download") {
                " (not used by downloads)"
            } else {
                ""
            },
            case.peak_kib
        );
    }

    assert!(
        within(upload_1g_8.peak_kib, upload_64m_8.peak_kib, KIB_PER_MIB),
        "upload memory depends on file size: {} against {} KiB",
        upload_1g_8.peak_kib,
        upload_64m_8.peak_kib
    );
    assert!(
        within(download_1g_8.peak_kib, download_64m_8.peak_kib, KIB_PER_MIB),
        "download memory depends on file size: {} against {} KiB",
        download_1g_8.peak_kib,
        download_64m_8.peak_kib
    );

    let chunk_effect = upload_1g_8.peak_kib as i64 - upload_1g_1.peak_kib as i64;

    assert!(
        (5 * KIB_PER_MIB..=9 * KIB_PER_MIB).contains(&chunk_effect),
        "8 MiB against 1 MiB chunks should differ by about 7 MiB, not {chunk_effect} KiB"
    );
}
