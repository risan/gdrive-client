# gdrive-client

A small, blocking Google Drive client for Rust, built for low-memory hosts: upload, download, list and verify files with
memory that does not grow with file size.

- Blocking, no async runtime: [ureq](https://crates.io/crates/ureq) 3 with rustls, `serde_json` and `md-5` are the only
  dependencies.
- Constant memory: a resumable upload keeps one chunk in memory (8 MiB by default, your choice), a download streams
  through a 64 KiB buffer. See [Memory and size](#memory-and-size).
- Static musl builds work (`x86_64-unknown-linux-musl`).
- No logging of its own, and no token or secret in an error or in `Debug` output.
- Unix only: the token store is a mode 600 file.

What it does: log in (device flow, supplied values, or a one-time import from rclone), resolve and create folders, list
and find files, read a file's metadata, upload a new file or new content for an existing id with Drive's resumable
protocol and verify the result, download to a temp file and rename, delete or trash. It does not sync, filter, copy
server-side, throttle bandwidth or support shared drives.

## Install

As a git dependency, pinned to a commit:

```toml
[dependencies]
gdrive-client = { git = "https://github.com/risan/gdrive-client", rev = "<commit>" }
```

The crate is not published on crates.io. The minimum Rust version is 1.99.

## Google Cloud setup

You need an OAuth client of your own. The library asks for one scope, `https://www.googleapis.com/auth/drive.file`.

1. Open the [Google Cloud console](https://console.cloud.google.com/) and create a project, or reuse one. Accounts are
   limited in how many projects they can have, so prefer adding an OAuth client to an existing project over creating a
   new project for every program.
2. Enable the **Google Drive API** for the project (APIs & Services, Library).
3. Configure the **OAuth consent screen**, and **publish it** (move it from "Testing" to "In production"). While the
   consent screen is in testing, Google expires refresh tokens after 7 days and the program has to log in again every
   week. `drive.file` is not a sensitive scope, so publishing a personal project does not normally need a review by
   Google.
4. Create an **OAuth client ID** (APIs & Services, Credentials). The client type goes with the way you log in:

   | Login | Client type |
   |---|---|
   | The device flow of this library (`DeviceLogin`, the example's `login`) | **TVs and Limited Input devices** |
   | Reusing a token that rclone or a desktop program made (`Credentials::from_rclone_config`) | **Desktop app** |

   A refresh token only works with the client id and secret that issued it. To reuse an rclone token you need rclone's
   own client id and secret in the config (a remote made with rclone's built-in client has empty `client_id` and
   `client_secret`, and cannot be imported).

5. Note the client id and the client secret. With the device flow, the library needs nothing else.

### What `drive.file` lets a client see

`drive.file` gives access to the files **created or opened by any client of the same Google Cloud project**. It does not
give access to the rest of the Drive. A client of **another** project sees none of those files: files uploaded with
rclone's built-in client, or by a client in a different project, do not show up in `list_files`, `find_files` or
`folder_id`. Two programs that must see each other's files should use OAuth clients of the same project.

## Logging in

A credential set is a client id, a client secret and a refresh token. There are three ways to get one, and each ends in
`Client::create`, which writes the token store and makes no request.

**Device flow.** The library returns the verification URL and the code for you to show; the user approves on any device.

```rust,no_run
use gdrive_client::{Client, DeviceLogin, Error, Options};

fn log_in(client_id: &str, client_secret: &str) -> Result<Client, Error> {
    let options = Options::default();
    let login = DeviceLogin::start(client_id, client_secret, &options)?;

    println!("Open {} and enter the code {}", login.verification_url(), login.user_code());

    // Polls at Google's interval until the user approves, denies, or the code expires.
    let credentials = login.wait()?;

    Client::create("/var/lib/app/drive-token.json", credentials, options)
}
```

**Values you supply**, for example from your own configuration:

```rust,no_run
use gdrive_client::{Client, Credentials, Error, Options};

fn from_values() -> Result<Client, Error> {
    let credentials = Credentials::new("client-id", "client-secret", "refresh-token");

    Client::create("/var/lib/app/drive-token.json", credentials, Options::default())
}
```

**A one-time import from rclone**, taking `client_id`, `client_secret` and the refresh token inside `token` from a
`drive` section of the rclone config. An encrypted config, a section of another type, and rclone's built-in client are
refused with an error that does not echo any value.

```rust,no_run
use std::path::Path;

use gdrive_client::{Client, Credentials, Error, Options};

fn from_rclone() -> Result<Client, Error> {
    let credentials = Credentials::from_rclone_config(Path::new("/root/.config/rclone/rclone.conf"), "gdrive")?;

    Client::create("/var/lib/app/drive-token.json", credentials, Options::default())
}
```

## The token store

The credentials and the current access token live in one JSON file that you choose:

```json
{
  "version": 1,
  "client_id": "...",
  "client_secret": "...",
  "refresh_token": "...",
  "access_token": "...",
  "access_token_refresh_at": 1790000000
}
```

- The file is written atomically (a temp file in the same directory, fsync, rename, fsync of the directory) and has mode
  600. `Client::open` refuses a file with any other mode (`TokenStoreProblem::InsecureMode`) and never changes
  permissions itself. A missing, unreadable or malformed file is a distinct error too.
- `access_token_refresh_at` is the time the access token was received plus Google's `expires_in` minus 60 seconds, as
  unix seconds. While running, the library measures the same moment on the monotonic clock, so a clock change does not
  matter.
- A new access token is fetched when there is none, when it expires within 60 seconds, and once more when Drive answers
  401. One refresh is in flight at a time, so concurrent calls share it.
- If Google rejects the refresh token, the call fails with `Error::InvalidGrant`: log in again.
- **A failed write of a refreshed access token does not fail the call.** The new token is used from memory for the rest
  of the process, and the failure is kept for you: call `client.take_store_failure()` after a call or a run and log what
  it returns. A full disk, for example, must not stop a mover that exists to free that disk. Losing a cached access token
  costs one extra refresh.
- **A failed write of new credentials does fail the call** (`Client::create`, or a refresh token that Google rotated),
  because losing those can mean logging in again. After a failed write of rotated credentials the client keeps them in
  memory, every call fails with the same error without sending the dead refresh token, and the next call after the
  store becomes writable writes it and carries on.

```rust,no_run
use gdrive_client::{Client, Error, Options};

fn open_and_report() -> Result<(), Error> {
    let client = Client::open("/var/lib/app/drive-token.json", Options::default())?;

    client.metadata("some-file-id")?;

    if let Some(problem) = client.take_store_failure() {
        eprintln!("token store not written: {problem}");
    }

    Ok(())
}
```

## Using the client

`Client` is `Send + Sync`: share one between threads. Every method blocks.

**Folders.** Paths are resolved from My Drive's root. Trashed folders do not count. Two folders with one name under one
parent are an error naming the path (`Error::DuplicateFolder`): the library does not guess. An empty path or an empty
segment (`a//b`) is `Error::InvalidInput`.

```rust,no_run
use gdrive_client::{Client, Error};

fn folders(client: &Client) -> Result<(), Error> {
    // Resolve only: Error::FolderNotFound names the path if it is missing.
    let existing = client.folder_id("traffic-analysis/recordings")?;

    // Resolve, creating the missing levels.
    let ensured = client.ensure_folder("bdgtransit/archive")?;

    println!("{existing} {ensured}");
    Ok(())
}
```

Two processes creating the same folder at the same moment can leave two folders, which a later call reports as
`DuplicateFolder`. Run one job at a time per folder.

**List, find, metadata.** Listing returns the files that are not trashed and not folders, over every page. Names are
matched exactly, with quotes and backslashes escaped; `find_files` returns every match so you decide what duplicates mean.

```rust,no_run
use gdrive_client::{Client, Error};

fn look_around(client: &Client, folder_id: &str) -> Result<(), Error> {
    for file in client.list_files(folder_id)? {
        println!("{} {} {:?} {:?} {:?}", file.id, file.name, file.size, file.md5, file.modified_time);
    }

    let matches = client.find_files(folder_id, "gaps.log")?;
    let one = client.metadata(&matches[0].id)?;

    println!("{} {:?} {:?} trashed: {}", one.name, one.size, one.parents, one.trashed);
    Ok(())
}
```

`size` and `md5` are `None` for files without content, such as Google Docs.

**Upload.** `upload_new` creates a file in a folder; `upload_replace` gives an existing id new content and keeps the id
(as `rclone copyto` does). The chunk size is `Options::chunk_size` (a multiple of 256 KiB, 8 MiB by default), and the
chunk buffer is the only memory that scales with it. After a network error, a 429, a 5xx or a rate-limit 403 in the
middle, the library asks Drive how much it has and continues from there; a session Drive no longer knows (404 or 410)
starts over once.

The result carries the file as Drive reports it, and the size and MD5 of the bytes sent, computed while reading (the
file is read once). **Check `verified()` before you delete anything locally**: it is true when the local file did not
change during the upload (size, modification time and inode, also by path) and Drive holds a file with the local size and
MD5. The library never deletes the local file.

```rust,no_run
use std::path::Path;

use gdrive_client::{Client, Error};

fn upload(client: &Client, folder_id: &str) -> Result<(), Error> {
    let local = Path::new("/srv/cctv/2026-10-24.ts");
    let name = "2026-10-24.ts";

    let upload = match client.find_files(folder_id, name)?.as_slice() {
        [] => client.upload_new(local, folder_id, name)?,
        [existing] => client.upload_replace(local, &existing.id)?,
        several => panic!("{} files named {name}", several.len()),
    };

    if upload.verified() {
        println!("safe to delete {}: Drive holds {:?} bytes, md5 {}", local.display(), upload.file.size, upload.local_md5);
    }

    Ok(())
}
```

If the deadline passes mid-upload, `Error::Deadline` carries `confirmed_bytes`, the bytes Drive confirmed; an unfinished
resumable session creates nothing in Drive.

**Download.** The bytes are streamed to a temp file beside the target while the MD5 is computed, and the file is renamed
over the target only when the download is complete. After any failure the target is untouched and no temp file is left. A
transient failure restarts the download from byte 0.

```rust,no_run
use std::path::Path;

use gdrive_client::{Client, Error};

fn download(client: &Client, file_id: &str) -> Result<(), Error> {
    let download = client.download(file_id, Path::new("/var/cache/app/gaps.log"))?;

    println!("{} bytes, md5 {}", download.size, download.md5);
    Ok(())
}
```

**Delete and trash.**

```rust,no_run
use gdrive_client::{Client, Error};

fn clean_up(client: &Client, old: &str, older: &str) -> Result<(), Error> {
    client.trash(old)?; // recoverable from Drive's trash
    client.delete(older)?; // permanent

    Ok(())
}
```

A delete that Drive applied but whose answer was lost is retried and counts as done when the retry finds the file gone.

## Retries, the deadline and errors

Every request retries 429, 500, 502, 503, 504, network errors, and Drive's rate-limit 403s (`rateLimitExceeded` and
`userRateLimitExceeded`) with exponential backoff and jitter, honoring `Retry-After`. Other 4xx answers are not retried,
including other 403s such as `storageQuotaExceeded`.

`Options` has public fields, so `Options { deadline: ..., ..Options::default() }` is the way to change them:

| Field | Default | Meaning |
|---|---|---|
| `deadline` | 10 minutes | The time budget of each call, measured from its start |
| `chunk_size` | 8 MiB | Upload chunk, a positive multiple of 256 KiB |
| `initial_backoff` | 1 s | First retry delay ceiling; it doubles each retry |
| `max_backoff` | 32 s | The largest ceiling |
| `endpoints` | Google's | Where requests go; `Endpoints::local(url)` points both at one origin (tests) |

The **deadline** bounds network requests, retries, backoff sleeps, token refreshes and waits on other calls. It does not
bound local file I/O: reading the upload source, writing a download, writing the token store and their `sync_all` run to
completion. Someone moving 1 GB files over a slow link should set the deadline to fit the link. `DeviceLogin::wait` is
different: it ends at approval, denial or the expiry of the device code, and bounds only each poll's retries.

**Errors** are one `#[non_exhaustive] Error` enum whose variants say what was being done, the file id or path it was about
(never a URL), and Drive's error reason, for example:

```text
upload of /srv/cctv/x.ts: Drive answered 403 (storageQuotaExceeded): The user's Drive storage quota has been exceeded.
```

Text from Google that enters an error has every secret the call knows replaced with `[redacted]`, and ureq's own error
text (which can print URLs) is never used. `Endpoints` accept `https://` anywhere and `http://` only for `127.0.0.1`,
`localhost` and `[::1]`, and a resumable session URL must be on the Drive endpoint's origin before any chunk is sent to
it, because chunk requests carry the bearer token.

## Memory and size

[MEASUREMENTS.md](MEASUREMENTS.md) has the numbers, measured on a static musl build against the fake server: uploading
a 1 GiB file peaks at about 9.6 MiB with 8 MiB chunks (3.2 MiB with 1 MiB chunks) and downloading it at about 1.5 MiB,
the same as for 64 MiB files. The example binary is 1.84 MB static; the library adds at most 156 KiB over a bare ureq
HTTPS client.

## Building static musl

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The TLS provider, ring, compiles C code, so the build needs a C compiler for the musl target. This repository uses
[Zig](https://ziglang.org/) as that compiler: `tools/zig-cc` and `tools/zig-ar` are two small wrappers, and
`.cargo/config.toml` points `CC_x86_64_unknown_linux_musl` and `AR_x86_64_unknown_linux_musl` at them. On Alpine, the
system gcc (`apk add build-base`) is enough. Cargo does not read the `.cargo/config.toml` of a git dependency, so a crate
that depends on this one keeps its own C compiler setup.

## Logging

This crate logs nothing. ureq logs through the `log` crate, and its `TRACE` level is wire-level debugging that is **not
redacted**: it shows request headers, including the bearer token. Keep `TRACE` off for ureq in anything that handles real
credentials.

## The example program

`examples/gdrive.rs` is a small command line around the library:

```sh
export GDRIVE_CLIENT_SECRET=...        # secrets come from the environment, never from arguments
cargo run --example gdrive -- --store token.json login <client-id>
cargo run --example gdrive -- --store token.json upload ./report.pdf backups/reports   # exit 1 if not verified
cargo run --example gdrive -- --store token.json list backups/reports
cargo run --example gdrive -- --store token.json download <file-id> ./report.pdf
cargo run --example gdrive -- --store token.json trash <file-id>
```

The other commands are `import-rclone <config> <section>`, `set-credentials <client-id>` (with `GDRIVE_REFRESH_TOKEN`)
and `delete <file-id>`. Options: `--endpoint <url>`, `--chunk-mib <n>`, `--deadline-secs <n>`, `--print-peak-rss`.

## Tests

`cargo test --locked` runs the unit tests and an integration suite against an in-process fake of Drive and Google's OAuth
endpoints on 127.0.0.1. The fake is strict where clients usually go wrong: query escaping, `fields`, `Content-Range`
offsets and 256 KiB chunks. No test talks to Google except the opt-in live test, and the tests ignore proxy variables.

Ignored tests, run explicitly:

```sh
# The example binary against the fake
cargo build --example gdrive
GDRIVE_CLIENT_EXAMPLE=target/debug/examples/gdrive cargo test --locked --test integration example:: -- --ignored

# Peak memory, see MEASUREMENTS.md
cargo build --release --locked --target x86_64-unknown-linux-musl --examples
GDRIVE_CLIENT_EXAMPLE=target/x86_64-unknown-linux-musl/release/examples/gdrive \
    cargo test --release --locked --test integration measure:: -- --ignored --nocapture
```

**The live test** (`live::round_trip`) uploads, verifies, lists, downloads and deletes a 300 KiB file in a folder it
creates at My Drive's root, then removes the folder. It is skipped unless you ask for it, with a token store made by the
example's `login`:

```sh
GDRIVE_CLIENT_LIVE=1 GDRIVE_CLIENT_TOKEN_STORE=token.json \
    cargo test --locked --test integration live:: -- --nocapture
```

It only rewrites the store's access token.

## License

MIT, see [LICENSE](LICENSE).
