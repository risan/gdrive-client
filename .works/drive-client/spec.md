# A small blocking Google Drive client library for low-memory hosts

Status: ready
Scout: none — new repository; the callers' needs are in brief.md, the Drive API v3 and OAuth docs are the reference.

## Goal
A Rust library that does the few Google Drive operations bdgtransit and traffic-analysis need, with constant memory
however large the file, no async runtime, and a static musl build, so neither needs rclone in the LXC.

## In scope
- The library crate `gdrive-client` (lib only), its tests with a local fake Drive server, an opt-in real-Drive test,
  a small example program, README.
- Two measurements: peak RSS of a 1 GB upload and download against the fake server; the library's added binary size
  in a static musl build.

## Out of scope
- Callers' integration; crates.io; bandwidth limits, filters, sync, purge, server-side copy, shared drives.

## Requirements
- R1 — Credentials: a credential set is a client id, a client secret and a refresh token. It can come from (a) the
  device flow (Google's limited-input flow, scope `https://www.googleapis.com/auth/drive.file`: the library returns
  the verification URL and user code for the caller to show, then polls at Google's interval until approval, denial
  or expiry), (b) values the caller supplies, or (c) a one-time import from an rclone config file section of type
  `drive` (`client_id`, `client_secret`, and the `token` JSON's `refresh_token`).
- R2 — Token store: the credential set and the current access token with its expiry live in one JSON file chosen by
  the caller. Writes are atomic (temp file in the same directory, fsync, rename) and the file is mode 600. A new
  access token is fetched when the stored one expires within 60 s or a call answers 401 (once per call); a refresh
  token Google rejects (`invalid_grant`) is a distinct error telling the caller to log in again. Tokens and secrets
  never appear in errors or `Debug` output. A failed write of a refreshed access token does not fail the call: the
  new token is used in memory for the rest of the process, and the failure is reported to the caller beside the
  call's result (orchestrator decision 2026-10-24: a full disk must not stop traffic-analysis's mover, which is what
  frees the disk; losing a cached access token only costs a refresh). A failed write of new credentials (login,
  import, or a refresh token Google rotated) still fails the call, because losing those means logging in again.
- R3 — Folders: resolve a folder path like `traffic-analysis/recordings` from My Drive's root, optionally creating
  missing folders; only folders not in the trash count. Two folders with the same name under one parent are an error
  naming the path (no guessing).
- R4 — Listing: list a folder's files (not trashed) with id, name, size, MD5 and modified time, following every page;
  find files by exact name in a folder (all matches returned, so the caller decides about duplicates).
- R5 — Upload: upload a local file as a new file in a folder, or as new content for an existing file id (same id kept,
  as `rclone copyto` does for `gaps.log`), with Drive's resumable protocol. The chunk size is the caller's (a multiple
  of 256 KiB, default 8 MiB), and the chunk buffer is the only memory that scales with it. After a network error,
  429 or 5xx in the middle, the library asks Drive how much it has and continues from there, within the deadline; a
  session Drive no longer knows (404/410) starts over once. The result carries the file's id, size and MD5 as Drive
  reports them, and a check that they equal the local file's size and MD5 (computed while reading, no second pass).
- R6 — Download and delete: download a file id to a local path (streamed, written to a temp file and renamed only
  when complete); delete a file id permanently, or move it to the trash (caller's choice).
- R7 — Metadata: get one file's id, name, size, MD5, parents and trashed flag by id.
- R8 — Retries: every request retries 429, 500, 502, 503, 504 and network errors with exponential backoff and jitter
  (honoring `Retry-After`), until a total deadline the caller sets per client (default 10 minutes). A 403 whose Drive
  reason is `rateLimitExceeded` or `userRateLimitExceeded` is retried the same way, as Google's Drive API guide says
  (orchestrator decision 2026-10-24). Other 4xx are not retried. The deadline bounds network calls, retries, backoff,
  refreshes and waits on other calls, not local file I/O. Errors say which operation, which file or path, and Drive's error reason.
- R9 — Memory and size: no async runtime; blocking ureq 3 (`default-features = false, features = ["rustls"]`) with
  compiled-in webpki roots; memory independent of file size; peak RSS of a 1 GB upload and of a 1 GB download against
  the fake server and the added static-musl binary size are measured and recorded in the repo.
- R10 — Toolchain: edition 2024, `rust-version = "1.99"`, Rust 1.99.1 pinned; builds for
  `x86_64-unknown-linux-musl`; `cargo test`, `clippy -D warnings`, `fmt --check` pass; `Cargo.lock` committed for the
  tests and example; MIT.
- R11 — Docs: README covers creating the Google Cloud project, enabling the Drive API, the consent screen (publish it,
  or refresh tokens expire after 7 days), which client type goes with which login (device flow: "TVs and Limited Input
  devices"; reusing an rclone/desktop token: "Desktop app"), adding a second OAuth client to an existing project
  (accounts are limited in projects), what `drive.file` lets a client see (files created or opened by any client of
  the same project; a client of another project sees none of them), and an example of every operation. A small example binary shows login, upload with verify, list,
  download.

## Edge cases and failures
- File changed while uploading (size or MD5 differs from what was read) → the verify check fails; nothing is deleted
  by the library.
- Upload of an empty file → works (Drive allows it), MD5 of the empty input.
- A name with quotes or backslashes → escaped in Drive's query language.
- Token file missing, unreadable, or not mode 600 → a clear error; the library never widens permissions.
- Clock skew → expiry uses the `expires_in` Google returns, relative to the local monotonic clock at receipt, stored
  as a wall-clock time with the 60 s margin.
- Deadline passed mid-upload → error with the bytes Drive confirmed; no partial file is visible in Drive (an
  unfinished resumable session creates nothing).

## Must not change
- Nothing exists yet.

## Acceptance criteria
- [ ] AC1 [R1, R2] — Tests against the fake OAuth endpoint: device flow (pending, slow_down, approved, denied,
      expired), refresh on expiry and on 401, `invalid_grant` error, rclone import of a sample config, token file
      written atomically with mode 600, secrets absent from `Debug` and error text.
- [ ] AC2 [R3, R4, R7] — Fake-server tests: folder path resolve and create, duplicate folder error, trashed items
      ignored, listing over several pages, find by name with duplicates, query escaping, metadata.
- [ ] AC3 [R5] — Fake-server tests: new file and new content for an existing id; the server drops the connection or
      answers 503 mid-upload and the library resumes from Drive's reported offset; a 404 session restarts once; the
      verify check passes and fails as it should; empty file.
- [ ] AC4 [R6, R8] — Fake-server tests: download to temp-then-rename, an interrupted download leaves no final file;
      delete and trash; backoff honors `Retry-After`, stops at the deadline, does not retry other 4xx.
- [ ] AC5 [R9] — `MEASUREMENTS.md` gives peak RSS (VmHWM) of the example uploading and downloading a 1 GB file
      against the fake server with 8 MiB and 1 MiB chunks, and the static musl size of the example binary.
- [ ] AC6 [R1, R5, R6] — An opt-in test (`GDRIVE_CLIENT_LIVE=1` plus a token file path) uploads, verifies, lists,
      downloads and deletes a small file in a folder it creates, and removes the folder. It is run once against the
      real Drive with the user's bdgtransit credentials when they exist, recorded in the work folder; until then it
      is skipped and the check says so.
- [ ] AC7 [R10] — In a fresh shell, test, clippy, fmt and a static musl build pass.
- [ ] AC8 [R11] — README sections as listed; the example builds.
