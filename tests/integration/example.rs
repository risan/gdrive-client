//! The example binary against the fake. Ignored by default because it needs the built example:
//! `GDRIVE_CLIENT_EXAMPLE=target/debug/examples/gdrive cargo test --test integration example:: -- --ignored`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::support::{FakeDrive, Fault, Kind};

struct Example {
    binary: PathBuf,
    fake: FakeDrive,
    dir: TempDir,
}

impl Example {
    fn new() -> Example {
        let binary = std::env::var_os("GDRIVE_CLIENT_EXAMPLE")
            .map(PathBuf::from)
            .expect("GDRIVE_CLIENT_EXAMPLE must name the built example");

        Example {
            binary,
            fake: FakeDrive::start(),
            dir: TempDir::new().unwrap(),
        }
    }

    fn store(&self) -> PathBuf {
        self.dir.path().join("token.json")
    }

    /// Runs the example with proxy variables that point at a dead port: the round trip passes only if loopback
    /// endpoints ignore environment proxies.
    fn run(&self, arguments: &[&str]) -> Output {
        let (_, client_secret, refresh_token) = self.fake.credentials();
        let url = self.fake.url();
        let store = self.store();

        Command::new(&self.binary)
            .args(["--store", store.to_str().unwrap(), "--endpoint", &url])
            .args(arguments)
            .env("GDRIVE_CLIENT_SECRET", client_secret)
            .env("GDRIVE_REFRESH_TOKEN", refresh_token)
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .expect("run the example")
    }

    fn succeed(&self, arguments: &[&str]) -> String {
        let output = self.run(arguments);

        assert!(
            output.status.success(),
            "{arguments:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn local(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.path().join(name);

        fs::write(&path, bytes).unwrap();
        path
    }
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
#[ignore = "needs GDRIVE_CLIENT_EXAMPLE"]
fn cli_round_trip() {
    let example = Example::new();
    let (client_id, _, _) = example.fake.credentials();
    let first: Vec<u8> = (0..700_000u32).map(|index| (index % 251) as u8).collect();
    let second = b"second version".to_vec();
    let local = example.local("report.bin", &first);

    example.succeed(&["set-credentials", &client_id]);

    let output = example.succeed(&["upload", text(&local), "backups/daily"]);
    let files = example.fake.files_named("report.bin");

    assert!(output.contains("verified"), "{output}");
    assert!(!output.contains("not verified"), "{output}");
    assert_eq!(files.len(), 1);

    let id = files[0].id.clone();

    fs::write(&local, &second).unwrap();

    let output = example.succeed(&["upload", text(&local), "backups/daily"]);
    let files = example.fake.files_named("report.bin");

    assert!(output.contains(&id), "the same id is kept: {output}");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].content.as_deref(), Some(second.as_slice()));

    let listing = example.succeed(&["list", "backups/daily"]);

    assert!(
        listing.contains("report.bin") && listing.contains(files[0].md5.as_deref().unwrap()),
        "{listing}"
    );

    let downloaded = example.dir.path().join("downloaded.bin");

    example.succeed(&["download", &id, text(&downloaded)]);
    assert_eq!(fs::read(&downloaded).unwrap(), second);

    example.succeed(&["delete", &id]);
    assert!(example.fake.file(&id).is_none());

    let trashed_local = example.local("old.bin", b"to the trash");

    example.succeed(&["upload", text(&trashed_local), "backups/daily"]);

    let trashed = example.fake.files_named("old.bin").remove(0).id;

    example.succeed(&["trash", &trashed]);
    assert!(example.fake.file(&trashed).unwrap().trashed);
}

#[test]
#[ignore = "needs GDRIVE_CLIENT_EXAMPLE"]
fn upload_with_a_wrong_md5_exits_1() {
    let example = Example::new();
    let (client_id, _, _) = example.fake.credentials();
    let local = example.local("corrupt.bin", b"some bytes");

    example.succeed(&["set-credentials", &client_id]);
    example.fake.fault(Kind::ChunkPut, Fault::CorruptMd5);

    let output = example.run(&["upload", text(&local), "backups"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("not verified"), "{stdout}");
}

#[test]
#[ignore = "needs GDRIVE_CLIENT_EXAMPLE"]
fn print_peak_rss_prints_the_high_water_mark() {
    let example = Example::new();
    let (client_id, _, _) = example.fake.credentials();

    example.fake.add_folder("root", "backups");
    example.succeed(&["set-credentials", &client_id]);

    let output = example.succeed(&["--print-peak-rss", "list", "backups"]);
    let kib: u64 = output
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM_KiB "))
        .and_then(|number| number.trim().parse().ok())
        .unwrap_or_else(|| panic!("no VmHWM line in {output}"));

    assert!(kib > 0);
}
