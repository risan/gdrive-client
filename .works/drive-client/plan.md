# gdrive-client: a small blocking Google Drive client library — plan

Status: ready
Spec: spec.md
Scout: scout-gdrive-client.md
Review: converged — 6 rounds (Codex sol, effort high). Round 6 confirmed round 5's two Major fixes (single-flight
refresh under token rotation; rotated credentials kept across a deadline) and raised one Minor test-setup fix, applied;
both push-backs (rounds 1 and 4) conceded by Codex

## Approach
One library crate, `gdrive-client` (lib `gdrive_client`), whose only dependencies are blocking ureq 3.5 (rustls with
ring, compiled-in webpki roots), `serde_json` and `md-5`. A `Client` opened from a token-store path owns one ureq
`Agent`, the credential set and the current access token. Every Drive and OAuth request goes through one executor
that refreshes the token (60 s margin, once more on a 401), retries 429, 5xx, Drive's rate-limit 403s and network
errors with jittered
exponential backoff or `Retry-After`, and stops at the operation's deadline. The resumable upload reads the file once,
in order, through one chunk-sized buffer that slides forward as Drive confirms bytes; the MD5 consumes exactly the
confirmed prefix, so a resume from any offset Drive reports needs no second read. Tests run against an in-process fake
Drive and OAuth server (plain HTTP on 127.0.0.1) that is strict where clients usually go wrong: query escaping,
`fields`, `Content-Range` offsets, 256 KiB chunks. Crate layout, toolchain files, unit-test placement and the zig musl
setup mirror bdgtransit's `research/collector/` (scout, "Pattern to mirror").

## Decisions
- D1 — JSON: `serde_json` 1 with `serde_json::Value`, no serde derive. Measured on a static musl build (scout, "Size and
  speed"): a hand-written parser and writer adds 28 KiB, `serde_json::Value` 40 KiB, serde derive 68 KiB. The 12 KiB over
  a hand parser buys a tested parser and string escaper instead of about 250 lines and their tests to maintain; the
  library reads about 15 fields in total, so typed structs gain little. Rejected: a hand parser (bdgtransit has one only
  because it must match Go's `encoding/json`); serde derive (+28 KiB and proc-macro build dependencies for both callers).
- D2 — MD5: `md-5` 0.11 (RustCrypto; its lib name is `md5`, so code says `use md5::{Digest, Md5}`): +8 KiB against
  +16 KiB for `md5` 0.8.1, same speed (759 against 805 MB/s here). Lowercase hex is formatted by a private helper; no
  `hex` crate. Rejected: `md5` 0.8.1.
- D3 — Public API. All methods are blocking; `Client` is `Send + Sync` (token state behind a `Mutex`).
  - `Client::create(token_store: impl Into<PathBuf>, credentials: Credentials, options: Options) -> Result<Client, Error>`
    writes a new store (replacing any old one) and makes no request; `Client::open(token_store, options)` reads an
    existing one.
  - `folder_id(&self, path: &str) -> Result<String, Error>` (resolve only), `ensure_folder(&self, path: &str)` (resolve,
    creating missing levels), `list_files(&self, folder_id: &str) -> Result<Vec<DriveFile>, Error>`,
    `find_files(&self, folder_id: &str, name: &str) -> Result<Vec<DriveFile>, Error>`,
    `metadata(&self, file_id: &str) -> Result<DriveFile, Error>`,
    `upload_new(&self, local: &Path, folder_id: &str, name: &str) -> Result<Upload, Error>`,
    `upload_replace(&self, local: &Path, file_id: &str) -> Result<Upload, Error>`,
    `download(&self, file_id: &str, local: &Path) -> Result<Download, Error>`, `delete(&self, file_id: &str)`,
    `trash(&self, file_id: &str)`. Two methods instead of a `create: bool` flag, and `delete` / `trash` instead of a
    mode flag, so call sites read as what they do.
  - `take_store_failure(&self) -> Option<Error>` returns the latest `TokenStore { WriteFailed }` from writing a
    refreshed access token (D8; R2: such a failure does not fail the call, and is reported beside its result) and
    clears it. A caller checks it after a call or a run, for example to log that the token store could not be
    written. The failure belongs to the client, not to one call: with several threads, any of them may take it.
    Rejected: a wrapper such as `Reported<T> { value, store_failure }` around every method's result (every call site
    pays for a rare condition); a callback in `Options` (makes `Options` non-`Clone` and runs caller code inside the
    library's locks).
  - `Credentials::new(client_id, client_secret, refresh_token)`, `Credentials::from_rclone_config(path: &Path,
    section: &str) -> Result<Credentials, Error>`, `Credentials::client_id(&self) -> &str`.
  - `DeviceLogin::start(client_id: &str, client_secret: &str, options: &Options) -> Result<DeviceLogin, Error>`;
    `verification_url(&self) -> &str`, `user_code(&self) -> &str`, `expires_in(&self) -> Duration`,
    `wait(self) -> Result<Credentials, Error>`.
  - `Options { pub deadline: Duration /* 10 min */, pub chunk_size: usize /* 8 MiB */, pub initial_backoff: Duration
    /* 1 s */, pub max_backoff: Duration /* 32 s */, pub endpoints: Endpoints }` with `Default`. `deadline` is the time
    budget of each `Client` call and of `DeviceLogin::start` (spec R8 "total deadline"), measured from the call's start:
    as R8 states, it bounds every network request (ureq `timeout_global` set to the time left), every retry and backoff
    sleep, token refreshes, and any wait on another call (the D9 commit gate), not local file I/O. Reading the upload
    source, writing a download, the token-store write and their `sync_all` run to completion, as blocking std calls
    must and as ureq documents for its own timeouts (`ureq-3.5.0/src/config.rs:755`). A caller moving 1 GB
    files sets the deadline to fit its link. `DeviceLogin::wait` ends at approval, denial or the device code's expiry
    (R1), each poll's retries bounded per D10.
  - `Endpoints { pub drive: String /* https://www.googleapis.com */, pub oauth: String /* https://oauth2.googleapis.com */ }`
    with `Default` = production and `Endpoints::local(base_url)` pointing both at one origin (the fake).
  - `DriveFile { id, name, mime_type: String, size: Option<u64>, md5: Option<String>, modified_time: Option<String>,
    parents: Vec<String>, trashed: bool }` (all `pub`; Drive's string `size` parsed to `u64`; files without content,
    such as Google Docs, have `None`).
  - `Upload { pub file: DriveFile, pub local_size: u64, pub local_md5: String, pub local_file_changed: bool }` with
    `verified(&self) -> bool` = not changed, and Drive's size and MD5 equal the local ones. `Download { pub size: u64,
    pub md5: String }`, computed while writing.
  - No logging anywhere in the crate.
  Rejected: a builder for `Options` (public fields with `..Options::default()` are enough); `&mut self` methods (a
  `Mutex` that is never held across a request, D9, costs nothing and lets callers share a client).
- D4 — Errors: one `#[non_exhaustive] pub enum Error` (`Debug` + `Display` + `std::error::Error`) whose variants carry
  their context, plus `pub enum Operation { RefreshToken, DeviceLogin, ResolveFolder, CreateFolder, ListFiles,
  FindFiles, Metadata, Upload, Download, Delete, Trash }`:
  `Http { operation, target, status, reason: Option<String>, message: Option<String> }` (Drive's
  `error.errors[0].reason`, else `error.status`, and `error.message`); `Network { operation, target, detail }` (not
  retried, e.g. TLS); `Deadline { operation, target, confirmed_bytes: Option<u64>, last_failure: String }`;
  `InvalidGrant { operation }` (Display: the refresh token was rejected, log in again); `LoginDenied`; `LoginExpired`;
  `OAuth { operation, code, description: Option<String> }`; `TokenStore { path, problem: TokenStoreProblem }` with
  `TokenStoreProblem { Missing, Unreadable(io::Error), InsecureMode(u32), Invalid(String), WriteFailed(io::Error) }`;
  `RcloneConfig { path, section, problem: String }`; `FolderNotFound { path }`; `DuplicateFolder { path }`;
  `Local { operation, path, source: io::Error }`; `LocalFileChanged { path }`; `Protocol { operation, target, detail }`
  (an answer Drive's protocol does not allow); `InvalidInput { detail }`. `target` is the local path, folder path or file
  id the call was about, never a URL. Display reads like
  `upload of /srv/cctv/x.ts: Drive answered 403 (storageQuotaExceeded): The user's Drive storage quota has been exceeded.`
  Rejected: a struct with an `ErrorKind` (the user asked for a typed error enum).
- D5 — Secrets: a private `Secret(String)` newtype whose `Debug` prints `[redacted]` holds the client secret, refresh
  token, access token, device code and the upload session URL. `Credentials`, `DeviceLogin` and `Client` implement
  `Debug` by hand (client id and store path shown). Two rules keep secrets and URLs out of errors:
  - ureq's `Display` is never used: a private `describe(&ureq::Error) -> String` maps each variant to a fixed text
    (`Io` → its `io::ErrorKind` and OS message, which carry no URL; `Timeout(phase)` → `timed out (<phase>)`;
    `ConnectionFailed`, `HostNotFound` → fixed words; every other variant → a fixed name such as `HTTP protocol error`,
    `https required`, `TLS error`), because `BadUri`, `RequireHttpsOnly` (`ureq-3.5.0/src/error.rs:265`) and
    ureq-proto's `BadLocationHeader` (`ureq-proto-0.6.5/src/error.rs:71`) print URLs and `Location` values.
  - Text from Google (Drive's `reason` and `message`, OAuth's `error` and `error_description`) passes through a
    redaction that replaces each secret the call knows (client secret, refresh token, access token, device code, session
    URL and its `upload_id`) with `[redacted]` before it enters an error.
  Request URLs never enter error values.
- D6 — Endpoint safety: `Options` validation (on `Client::create`, `Client::open`, `DeviceLogin::start`) accepts
  `https://` endpoints anywhere and `http://` only for `127.0.0.1`, `localhost` and `[::1]` (the fake); anything else is
  `InvalidInput`. The agent sets `https_only(true)` unless an endpoint is loopback http. A resumable session URL from
  `Location` must have the configured `drive` endpoint's origin, else `Protocol` before anything is sent to it, because
  chunk requests carry the bearer token. Both checks parse with `ureq::http::Uri` (ureq re-exports `http`,
  `ureq-3.5.0/src/lib.rs:583`) and compare scheme, host (ASCII case-insensitive) and effective port (443 / 80 when
  absent); any userinfo (`@` in the authority) is refused. A string-prefix check is not enough
  (`https://www.googleapis.com.attacker.example/` starts with the Drive origin). Rejected: a test-only feature flag for
  the override (the example binary needs it too).
- D7 — Request executor (`src/http.rs`): one agent with `http_status_as_error(false)`, `max_redirects(0)` (Drive's 308
  has no `Location`), `timeout_connect` 30 s, `timeout_per_read` and `timeout_per_write` 60 s (a stalled connection
  becomes a retryable network error instead of eating the budget), user agent `gdrive-client/<version>`. Each attempt
  sets `timeout_global` to the time left before the operation's deadline (none left: `Deadline` without sending).
  When the endpoints are loopback http (the fake) the agent sets `proxy(None)`: ureq's default reads `ALL_PROXY`,
  `HTTPS_PROXY` and `HTTP_PROXY` from the environment (`ureq-3.5.0/src/config.rs:1073`, `src/proxy.rs:232`), which
  would send test traffic, bearer tokens included, through a proxy. Production endpoints keep ureq's environment
  proxy support.
  - Retried: statuses 429, 500, 502, 503, 504; ureq `Io` (a peer that disconnects is `Io(UnexpectedEof)`,
    `ureq-3.5.0/src/error.rs:210`), `ConnectionFailed`, `HostNotFound`, and every `Timeout` except `Global`.
    `Timeout(Global)` is `Deadline`. ureq `Protocol` (a malformed answer, such as a bad `Location`) is `Protocol`, and
    the other ureq errors are `Network`; neither is retried. A 403 from Drive is retried like a 429 when any
    `error.errors[].reason` in its body is `rateLimitExceeded` or `userRateLimitExceeded` (R8): the executor reads a
    403's body once (64 KiB limit), decides, and keeps the parsed body for the `Http` error when it does not retry.
    Other 4xx, including other 403 reasons such as `storageQuotaExceeded`, return to the caller, which maps them to
    `Http`. OAuth endpoints are not Drive: their 403s (`slow_down`, `access_denied`) are handled by D10, never by this
    rule.
  - Delay: `Retry-After` when the answer has one (delta-seconds, or an IMF-fixdate such as
    `Sun, 06 Nov 1994 08:49:37 GMT`, parsed by a small private function; any other form falls back to backoff);
    otherwise `ceiling = min(max_backoff, initial_backoff * 2^attempt)` and a delay drawn uniformly from
    `[ceiling / 2, ceiling]`, the randomness taken from `std::hash::RandomState` (no `rand` crate). A delay that would
    end past the deadline returns `Deadline` at once with the last failure, without sleeping.
  - A 401 forces one token refresh per logical request and resends at once (not counted as a retry); a second 401 is
    `Http`.
  - Bodies: JSON answers are read with `with_config().limit(4 MiB)`, error bodies with 64 KiB; downloads stream.
  - Downloads alone allow 5 redirects per request; ureq's default `RedirectAuthHeaders::Never` drops the bearer token
    on them.
- D8 — Token store (`src/token_store.rs`), Unix only (mode bits): a JSON object
  `{"version":1,"client_id":…,"client_secret":…,"refresh_token":…,"access_token":…|null,"access_token_refresh_at":<unix seconds>|null}`.
  `access_token_refresh_at` is the wall-clock time of receipt plus Google's `expires_in` minus 60 s (spec: stored as a
  wall-clock time with the margin); in memory the same moment is kept as an `Instant` taken at receipt, so a clock jump
  while running does not matter. Write: temp file `.<name>.<pid>.<counter>.tmp` in the same directory opened with
  `create_new` and mode 0o600, then `set_permissions(0o600)` on the handle (umask cannot leave it at anything else),
  `write_all`, `sync_all`, `rename` over the store, `sync_all` on the directory; any failure removes the temp file.
  Load: not found → `Missing`; `mode & 0o777 != 0o600` → `InsecureMode` (the file is never chmod-ed); read error →
  `Unreadable`; bad JSON, a missing field or `version != 1` → `Invalid`. Failed writes, per R2:
  - `Client::create` (login, rclone import, supplied values): the write failure is the call's error
    (`TokenStore { WriteFailed }`); no client is returned.
  - A refresh that changed only the access token: the call goes on with the new token, which stays in memory for the
    rest of the process; the failure is recorded for `take_store_failure` (D3). Later refreshes try the write again.
    This keeps traffic-analysis's mover uploading, and so freeing space, on a full disk.
  - A refresh whose answer carries a different refresh token (Google rotated it): the rotated credentials are kept in
    memory as soon as the answer arrives (D9; the old refresh token may no longer work, so dropping them could force a
    new login), the store is marked as owing credentials, and a failed write fails the call with
    `TokenStore { WriteFailed }` (a deadline reached before the write is `Deadline`, with the same state). While the
    store owes credentials, every call first writes the store again and fails until a write succeeds; then calls go
    on normally.
  Rejected: failing the call on every failed write (the spec now makes access-token writes best-effort); dropping
  rotated credentials after a failed write (could lose the only working refresh token). Rejected: the `time` crate for
  an RFC 3339 expiry (unix seconds need no parser; `modified_time` stays Drive's string for callers to parse).
- D9 — Refresh (`src/credentials.rs`): before each request the executor refreshes when there is no access token or
  `now >= refresh_at`; the refresh is `POST <oauth>/token` with `grant_type=refresh_token`, `client_id`,
  `client_secret`, `refresh_token`, sent through the same retry rules (without a bearer token) under the calling
  operation's deadline. `{"error":"invalid_grant"}` is `InvalidGrant`; other OAuth errors are `OAuth`. Coordination:
  - State, behind a `Mutex` that is never held across network or disk I/O: the access token, its `refresh_at`, a
    generation number (+1 per new token), the credentials, a `refreshing` flag, what the store owes (nothing, the
    access token, or credentials) and the latest store failure for `take_store_failure`.
  - One refresh in flight per client. A caller that needs a refresh (no token, `now >= refresh_at`, or a 401 on
    generation `g`) locks the state: if the generation is already past the one it saw, it uses the current token;
    if `refreshing` is set it unlocks, sleeps 2 ms and looks again, until its own deadline (then `Deadline`, without a
    token request); otherwise it sets `refreshing`, takes the refresh token, unlocks and sends the refresh. A guard
    clears `refreshing` on every exit path. With rotation possible, a second concurrent refresh would send the refresh
    token the first one just invalidated and get `invalid_grant`, so callers never refresh side by side.
  - The answer is published at once, before any deadline check: lock, store the new access token (generation + 1)
    and, when Google sent a different refresh token, the rotated credentials; mark the store as owing the access token,
    or credentials when rotated; clear `refreshing`; unlock. A rotated refresh token is therefore never lost, even
    when the call then runs out of time.
  - Then the refresher writes the store through a write gate, a second `Mutex` taken with `try_lock` in a loop
    (2 ms sleeps) bounded by its deadline, so it never waits past its deadline on another call's write. Under the gate
    it snapshots the current state (always the newest, so writes can never land out of order) and writes it (D8). On
    success it clears what the store owed up to that generation. On failure, or when its deadline passes before it
    gets the gate: if only the access token is owed, record the failure (a deadline skip records nothing) and go on;
    if credentials are owed, return `TokenStore { WriteFailed }` (or `Deadline`), leaving them in memory and owed.
  - Before each request, a call that finds credentials owed writes the store first, the same way, and fails with
    `WriteFailed` or `Deadline` until a write succeeds (D8).
  Refresh and store writer are passed to the coordinator as closures, so unit tests drive it with a refresher or a
  writer that blocks, fails or rotates. Rejected: concurrent refreshes (unsafe with rotation, as above); holding the
  state lock during the refresh (ureq's timeouts cover network I/O only, `ureq-3.5.0/src/config.rs:755`, so waiting
  callers would overrun their deadlines); a candidate-ordering scheme (a single refresher writing the current state
  needs none); a `Condvar` with `wait_timeout` (more code than a `try_lock` loop and 2 ms polls for waits that are
  rare and short).
- D10 — Device login (`src/device_login.rs`): `start` posts `client_id` and
  `scope=https://www.googleapis.com/auth/drive.file` to `<oauth>/device/code` (retry rules under `options.deadline`).
  `wait` polls `<oauth>/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`, sleeping `interval`
  between polls (5 s when absent); `slow_down` adds 5 s to every later wait (RFC 8628 §3.5); `authorization_pending`
  keeps polling; `access_denied` is `LoginDenied`; `expired_token`, or local time past `expires_in`, is `LoginExpired`;
  other OAuth errors are `OAuth` (`invalid_client` gets Google's description, which names the client type). Each poll
  is one request under R8: its 429, 5xx and network failures retry until `options.deadline` after that poll began or
  the code's expiry, whichever comes first (then `Deadline`, or `LoginExpired` at expiry), and every resend, retries
  included, waits the greatest of the current polling interval (with any `slow_down` increase), the D7 backoff and a
  valid `Retry-After`, because Google defines the interval as the wait between any two polling requests
  (`gdc-docs/device.txt:203-207`) and a faster retry would earn `slow_down`. The polling loop
  itself ends only as R1 says, at approval, denial or expiry, because the user needs the code's whole lifetime to
  approve (Google's codes live 30 min; the default deadline is 10 min). An approval without `refresh_token` is
  `Protocol`.
- D11 — rclone import (`src/rclone.rs`): lines `[section]` and `key = value` (split at the first `=`, both sides
  trimmed), `#` and `;` comments. Errors (`RcloneConfig`, never echoing values): encrypted config
  (`RCLONE_ENCRYPT_V0:`), section absent, `type` not `drive`, empty `client_id` or `client_secret` (rclone's built-in
  client, which cannot be reused), `token` absent, not JSON, or without `refresh_token`. Only the refresh token is taken
  from `token`; the first call refreshes.
- D12 — Folders (`src/files.rs`): the path is split on `/`; an empty path or empty segment is `InvalidInput`. Per
  segment, from the alias `root`: `files.list` with
  `q='<parent>' in parents and name = '<segment>' and mimeType = 'application/vnd.google-apps.folder' and trashed = false`
  and a client-side byte-exact name filter (Drive does not document `name =` as exact), through the same paginated
  query as `find_files` (D13): exact matches are collected over every page, and the count is judged only after the last
  page, so two folders on separate pages are still a duplicate. 0 → `FolderNotFound { path so far }` or create,
  1 → descend, 2+ → `DuplicateFolder { path so far }`. Create is
  `POST <drive>/drive/v3/files` with `{"name","mimeType":"application/vnd.google-apps.folder","parents":[parent]}` sent
  once per attempt; after a retryable failure the executor backs off and the lookup runs again before any new POST, so a
  create Drive applied but whose answer was lost does not leave a duplicate. Rejected: retrying the POST blindly.
- D13 — Listing and find: `q='<folder>' in parents and trashed = false and mimeType != 'application/vnd.google-apps.folder'`
  (find adds `name = '<name>'` and the exact filter), `pageSize=1000`, `fields=nextPageToken,files(<FILE_FIELDS>)`,
  following `nextPageToken` until absent. `FILE_FIELDS` = `id,name,mimeType,size,md5Checksum,modifiedTime,parents,trashed`,
  also used by `metadata` and as the upload's `fields`. Subfolders are left out because the spec lists "a folder's
  files" and both callers want data files. Query strings escape `\` as `\\` and `'` as `\'` (`src/query.rs`). Rejected:
  listing subfolders too; a callback or iterator API (bdgtransit's folder grows by one file a day; a `Vec` is enough).
- D14 — Resumable upload (`src/upload.rs`):
  1. Open the local file; `fstat` gives `total` and the file's identity (dev, ino, len, mtime with nanoseconds).
  2. Initiate: `POST <drive>/upload/drive/v3/files?uploadType=resumable&fields=<FILE_FIELDS>` with
     `{"name","parents":[folder]}` (new) or `PATCH <drive>/upload/drive/v3/files/<id>?uploadType=resumable&fields=…`
     with an empty body (replace: same id, metadata kept), `Content-Type: application/json; charset=UTF-8` when there
     is a body, `X-Upload-Content-Length: total`; expect 200 and a `Location` (D6 origin check). The initiation is
     retried like any request: a session whose answer was lost is never used and creates nothing.
  3. State: `confirmed` (bytes Drive holds), an MD5 over exactly `[0, confirmed)`, and one buffer allocated once with
     `chunk_size` capacity holding `[confirmed, confirmed + len)`; the file's read position is always
     `confirmed + len`. Refill the buffer to `min(chunk_size, total - confirmed)` bytes with sequential reads (EOF
     before that → `LocalFileChanged`), then `PUT` it with `Content-Length` and
     `Content-Range: bytes <confirmed>-<confirmed+len-1>/<total>` (an empty file: one empty `PUT` with
     `Content-Range: bytes */0`).
  4. The answer gives `received`: 308 with `Range: bytes=0-<n>` → `n + 1`, 308 without `Range` → 0; 200/201 → `total`
     and the file resource. A retryable failure (D7, including a rate-limit 403) backs off, then sends a status query
     (empty `PUT`,
     `Content-Length: 0`, `Content-Range: bytes */<total>`) that retries the same way and whose 308/200/201 gives
     `received`. A 401 refreshes once per chunk and then queries. 404 or 410, on a chunk or a query, means the session
     is gone: start over once (new session, seek to 0, new MD5, `confirmed = 0`); a second loss is `Http`. Other 4xx
     are `Http`.
  5. Apply `received`: it must satisfy `confirmed <= received <= confirmed + len` (else `Protocol` naming both
     numbers: Drive dropped bytes it had confirmed, or claims bytes never sent). Feed
     `buffer[..received - confirmed]` to the MD5, move the unconfirmed tail to the buffer's front (`copy_within`, no
     reallocation), `confirmed = received`. The tail is resent first and the next refill reads only new bytes, so the
     file is read once and the MD5 covers exactly what Drive confirmed, in order. A 200/201 with
     `confirmed + len != total` is `Protocol`.
  6. Done on 200/201. The deadline, at any point, is `Deadline { confirmed_bytes: Some(confirmed) }`; an unfinished
     session creates nothing in Drive.
  The arithmetic in step 5 and the `Range` parsing are pure functions with unit tests. Rejected: hashing bytes as they
  are sent (a resume below the sent offset would need to un-hash); re-reading the confirmed prefix from disk on resume
  (a second pass, against R5); seeking back in the file to resend (the sliding buffer already holds the tail).
- D15 — Local change check: once Drive has answered the upload 200/201 and any D16 `metadata` call is done,
  immediately before the `Upload` is built, the handle's `fstat` and `std::fs::metadata(path)` must both still match
  the identity from step 1 (dev, ino, len, mtime); any difference sets `local_file_changed` and `verified()` is false.
  Checking only then (not after the last read) also catches a change made while the final chunk was in flight. Needed because the MD5 is of the bytes sent, so it matches Drive even when the file changed under the upload;
  the identity check is what catches the spec's "file changed while uploading". The path check also catches a file
  replaced by rename, which the caller would otherwise delete by path.
- D16 — Upload result: the final 200/201 body is parsed as `DriveFile`; if it lacks `size` or `md5Checksum`, one
  `metadata` call fills them (Drive should return the initiation's `fields`; this keeps the verify check honest if it
  does not).
- D17 — Download (`src/download.rs`): `GET <drive>/drive/v3/files/<id>?alt=media`, streamed with a 64 KiB buffer into
  `.<name>.<pid>.<counter>.part` (`create_new`) in the target's directory while hashing. A retryable failure truncates
  the temp file (`set_len(0)`, seek to 0), resets the MD5 and starts again from byte 0. Success: `sync_all`, `rename`
  over the target, `sync_all` on the directory. Any final failure removes the temp file; an existing target is untouched
  until the rename. Non-200 is `Http`. Rejected: `Range` resume (more code for bdgtransit's 7 MB files;
  traffic-analysis does not download on the host).
- D18 — Delete and trash: `DELETE <drive>/drive/v3/files/<id>` (204); a 404 answered to a retry of the same delete
  counts as done (the earlier attempt was applied, its answer lost); a 404 on the first attempt is `Http`. Trash is
  `PATCH <drive>/drive/v3/files/<id>?fields=id` with `{"trashed":true}`.
- D19 — Fake server (`tests/integration/support/fake_drive.rs`): `std::net::TcpListener` on `127.0.0.1:0`, a thread
  per connection, `httparse` for request heads, `Content-Length` bodies only (chunked → 400), `Connection: close` on
  every answer, state behind `Arc<Mutex<…>>`.
  - Drive rules it enforces: `q` parsed by a strict parser of the clause forms the library sends (`'<id>' in parents`,
    `name =`, `mimeType =` / `!=`, `trashed =`, joined by `and`; unterminated strings or escapes other than `\'` and
    `\\` → 400 `invalidQuery`); the alias `root` maps to a real-looking root id (`fake-root-0001`), so code that expects
    `root` in `parents` fails; `fields` honored (a top-level list and one `files(...)` level; without it Drive's
    defaults `kind,id,name,mimeType`, so a missing `fields` loses size and MD5); `pageSize` capped by a settable page
    size with `nextPageToken`; `PUT` without `Content-Length` → 411; a chunk must start at the persisted offset and a
    non-final chunk must be a multiple of 256 KiB (else 400); `total` must equal `X-Upload-Content-Length`; 308 carries
    `Range: bytes=0-<persisted-1>` (none when 0); the final answer is the file filtered by the session's `fields`.
  - OAuth: `/token` checks `client_id` and `client_secret` (else 401 `invalid_client`); a known refresh token gets an
    access token with a settable `expires_in`, an unknown one 400 `invalid_grant`; with `rotate_refresh_tokens(true)`
    each refresh answer also carries a new refresh token and the old one stops working; device-code polls answer from a
    script (`authorization_pending` 428, `slow_down` 403, `access_denied` 403, `expired_token` 400, approved);
    `/device/code` returns a device code, user code, URL, `expires_in` and `interval` set by the test. Bearer tokens
    are checked on Drive requests (401 otherwise).
  - Faults, one-shot and in order, matched by request kind (`Token`, `DeviceCode`, `DevicePoll`, `List`, `Get`,
    `Create`, `Patch`, `Delete`, `UploadInit`, `ChunkPut`, `StatusQuery`, `Download`): `Status { code, retry_after,
    body }`, `CloseBeforeResponse`, `ApplyThenClose` (do the work, then close without answering), `DropAfterBody
    { persist }` (keep the first `persist` bytes of a chunk, close), `ReportRange(n)` (keep only `n` bytes),
    `SessionGone(404 | 410)`, `ForeignLocation(url)` (an initiation answered with that `Location`),
    `Redirect { status, location }` (any 3xx with that raw `Location`), `TruncateBody(n)`, `Unauthorized`,
    `OmitFields`, `CorruptMd5`, `Hook(Box<dyn FnOnce() + Send>)` (runs before answering, without holding the fake's
    state lock, so a slow hook never blocks other connections; tests change the local file mid-upload or delay a
    refresh); plus `always(kind, status)` for deadline tests. A `Status` body is sent as given, so a test can make
    Google's text echo a secret.
  - Seeding and inspection: `add_folder`, `add_file`, `add_generated_file(size)` (bytes made on the fly, MD5 computed
    once), trashed flags, page size, `store_content(false)` (uploads hashed, not kept; for the measurement), a request
    log (kind, method, path, query, `Content-Range`, body length, time) and file lookup by id.
  - `fake.options()` gives `Endpoints::local(fake)`, deadline 5 s, backoff 10 ms to 50 ms, chunk 256 KiB.
    `support::raw_agent()` gives the self-tests a ureq agent with `proxy(None)`, `http_status_as_error(false)` and
    `max_redirects(0)` (ureq's default of 10 redirects, `ureq-3.5.0/src/config.rs:1075`, would reject the fake's 308
    for lacking a `Location`, `ureq-proto-0.6.5/src/client/redirect.rs:26`, before a self-test could read it); no
    test builds an agent any other way.
  Rejected: a mock HTTP crate (extra dependencies; none enforces Drive's rules); an external fake process for tests.
- D20 — Test layout: one integration target, `tests/integration/main.rs` (cargo's automatic target `integration`),
  with modules `support`, `fake_drive`, `auth`, `files`, `retry`, `upload`, `download`, `example`, `measure`, `live`.
  One binary compiles the fake once, and no test binary warns about fake helpers it does not use (clippy `-D warnings`).
  Pure functions have unit tests in `src/<module>/tests.rs` (bdgtransit's pattern).
- D21 — Memory measurement (AC5): `measure::peak_rss`, `#[ignore]`, run in release with `GDRIVE_CLIENT_EXAMPLE` naming
  the static musl example binary. It starts the fake in-process with `store_content(false)` and generated 64 MiB and
  1 GiB files, makes sparse 64 MiB and 1 GiB sources with `File::set_len` (no data blocks), creates a token store with
  `Client::create`, and runs the example as a child for each case: upload 1 GiB with 8 MiB and with 1 MiB chunks,
  upload 64 MiB with 8 MiB; download 1 GiB with 8 MiB and with 1 MiB, download 64 MiB. The example's
  `--print-peak-rss` prints `VmHWM` from `/proc/self/status` at exit. Disk churn does not reach the number: `VmHWM`
  counts pages mapped into the process (heap, stack, binary text), not the page cache that `read` and `write` go
  through; the sources are sparse, and each download goes to a temp dir and is deleted before the next. Asserts: 1 GiB
  and 64 MiB within 1 MiB of each other, for upload and for download (memory independent of file size); the 8 MiB and
  1 MiB upload results 5 to 9 MiB apart (the chunk buffer is what scales). It prints the Markdown table that goes into
  `MEASUREMENTS.md`. Download memory does not depend on `chunk_size` (fixed 64 KiB buffer); the table says so.
  Rejected: measuring from a parent with `wait4` (needs `libc` and reports `ru_maxrss`, not `VmHWM` by name); a shell
  script (it would need the fake as a separate binary).
- D22 — Live test (AC6): `live::round_trip` passes with a printed "live test skipped: …" unless `GDRIVE_CLIENT_LIVE=1`
  and `GDRIVE_CLIENT_TOKEN_STORE` names a token store; it is the only test with production endpoints. It creates
  `gdrive-client-live-<unix seconds>-<pid>` at My Drive's root, uploads 300 KiB of generated bytes with 256 KiB chunks
  (two chunks, so a real 308), checks `verified()`, `metadata`, `list_files` (one entry, same MD5) and `find_files`,
  replaces the content (same id, verified), downloads (same bytes), deletes the file and then the folder; a drop guard
  deletes the folder if a step fails. It rewrites the given store's access token (atomically), nothing else.
- D23 — Example (`examples/gdrive.rs`), hand-parsed arguments (no clap): global `--store <path>` (required),
  `--endpoint <url>`, `--chunk-mib <n>`, `--deadline-secs <n>`, `--print-peak-rss`; commands `login <client-id>`
  (device flow, prints the URL and code), `import-rclone <config> <section>`, `set-credentials <client-id>`,
  `upload <local> <folder-path>` (`ensure_folder`, then `find_files` by the local name: none → `upload_new`, one →
  `upload_replace`, more → error; prints id, size, MD5 and the verify result, exit 1 when not verified),
  `list <folder-path>`, `download <file-id> <local>`, `delete <file-id>`, `trash <file-id>`. The client secret and
  refresh token come from `GDRIVE_CLIENT_SECRET` and `GDRIVE_REFRESH_TOKEN`, so they never show in `ps` or shell
  history. After every command it prints `warning: token store not written: …` to stderr when `take_store_failure`
  returns a failure (exit code unchanged). Rejected: a `[[bin]]` (the spec says lib only plus an example).
- D24 — README examples compile: `#![doc = include_str!("../README.md")]` in `src/lib.rs`, every Rust block in the
  README marked `rust,no_run`, so `cargo test` compiles an example of every operation without running it.
- D25 — Static musl: ring needs a C compiler for the target (scout: the build fails without one). Copy bdgtransit's
  `tools/zig-cc`, `tools/zig-ar` and its `.cargo/config.toml` `[env]` entries. A git dependency's `.cargo/config.toml`
  is not read by its callers, so each caller keeps its own C setup (bdgtransit: zig; traffic-analysis: Alpine's gcc).
  Rejected: a pure-Rust TLS provider (the spec pins `features = ["rustls"]`, which is ring).

## Tests
- Command: `cargo test --locked`, at the repo root (worktree
  `/home/risan/projects/code/gdrive-lite/.claude/worktrees/drive-client`), from S1 on. Also
  `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`, and
  `cargo build --release --locked --target x86_64-unknown-linux-musl --examples`.
- Shell: run cargo in a login shell, which puts `~/.cargo/bin` (rustup; `rust-toolchain.toml` selects 1.99.1) and
  `~/.local/bin` (zig 0.17.0) on `PATH`: `env -i HOME="$HOME" PATH=/usr/bin:/bin bash -lc '<command>'`. The agent's own
  shell lacks `~/.cargo/bin`.
- Ignored tests, run explicitly: `example::` (needs `GDRIVE_CLIENT_EXAMPLE`, S10), `measure::` (release build and a
  musl example, S11).
- No test reaches beyond 127.0.0.1 except `live::round_trip` with `GDRIVE_CLIENT_LIVE=1`, which this build never sets
  (the user's constraint). Commits: one per Step, no `Co-Authored-By: Claude` or `Claude-Session:` trailers.

## Steps
### S1 — Crate scaffold, toolchain and a static musl build [AC7, support]
- Check (nothing can fail first: the repo has no crate): before, `cargo build` finds no `Cargo.toml`; after,
  `cargo build --release --locked --target x86_64-unknown-linux-musl --example size_baseline` succeeds and
  `file target/x86_64-unknown-linux-musl/release/examples/size_baseline` says `static-pie linked` or
  `statically linked`; `cargo test --locked`, clippy and fmt pass.
- Change: `Cargo.toml` (new: `name = "gdrive-client"`, `version = "0.1.0"`, edition 2024, `rust-version = "1.99"`,
  `license = "MIT"`, `repository = "https://github.com/risan/gdrive-client"`, `publish = false`; dependencies
  `ureq = { version = "3.5", default-features = false, features = ["rustls"] }`, `serde_json = "1"`, `md-5 = "0.11"`;
  dev-dependencies `httparse = "1"`, `tempfile = "3"`; bdgtransit's release profile); `Cargo.lock` (new, committed);
  `rust-toolchain.toml`, `rustfmt.toml` (new, bdgtransit's content); `.cargo/config.toml`, `tools/zig-cc`,
  `tools/zig-ar` (new, copied from bdgtransit `research/collector/`, executable bits kept); `src/lib.rs` (new:
  `#![forbid(unsafe_code)]`, a placeholder crate doc until S13); `examples/size_baseline.rs` (new: one HTTPS GET with
  the same ureq setup and nothing of this crate, so `MEASUREMENTS.md` can state the library's added size; its doc
  comment says so; built, never run).
- Verify: the check above, in the login shell.

### S2 — Options, secrets and errors [AC1, support]
- Tests first:
  - `src/options/tests.rs` (new) — `default_options_match_the_spec`: deadline 10 min, chunk 8 MiB, backoff 1 s / 32 s,
    endpoints `https://www.googleapis.com` and `https://oauth2.googleapis.com`. `chunk_size_must_be_a_positive_multiple_of_256_kib`:
    0, 100 KiB and 300 KiB rejected, 256 KiB and 8 MiB accepted. `plain_http_only_on_loopback`: `http://127.0.0.1:1`,
    `http://localhost:1`, `http://[::1]:1` and any `https://` accepted; `http://example.com`, `ftp://x` and
    `https://user@www.googleapis.com` rejected. `same_origin_compares_scheme_host_and_port` (against
    `https://www.googleapis.com`): `https://WWW.googleapis.com:443/upload/…` accepted;
    `https://www.googleapis.com.attacker.example/…`, `https://www.googleapis.com:8443/…`, `http://www.googleapis.com/…`,
    `https://www.googleapis.com@evil.example/…` and `https://u:p@www.googleapis.com/…` rejected.
  - `src/secret/tests.rs` (new) — `debug_hides_the_value`; `redact_replaces_every_known_secret` (each occurrence, and
    the `upload_id` value of a session URL).
  - `src/error/tests.rs` (new) — `display_names_operation_target_and_reason` (an `Http` 403 `storageQuotaExceeded` for
    an upload of a path shows all four); `deadline_display_reports_confirmed_bytes`;
    `describe_never_prints_urls` (`ureq::Error::BadUri` and `RequireHttpsOnly` built with a URL holding `upload_id=s3cr3t`
    → the description has neither the URL nor `s3cr3t`).
- Change: `src/options.rs`, `src/secret.rs`, `src/error.rs` (new) per D3–D6; re-exports in `src/lib.rs`.
- Verify: `cargo test --locked --lib` → the new tests pass.

### S3 — Fake Drive and OAuth server [support]
- Tests first: `tests/integration/fake_drive.rs` (new), raw ureq requests against the fake —
  `query_parser_accepts_escaped_quote_and_backslash`, `query_parser_rejects_bad_strings` (unterminated, `\n` escape →
  400 `invalidQuery`), `default_fields_omit_size_and_md5`, `fields_select_the_returned_keys`,
  `put_without_content_length_is_411`, `chunk_must_start_at_the_persisted_offset`,
  `non_final_chunk_must_be_a_multiple_of_256_kib`, `resume_incomplete_reports_the_persisted_range` (no `Range` at 0),
  `root_alias_maps_to_a_real_id`, `bearer_token_is_required`, `faults_fire_once_in_order`.
- Change: `tests/integration/main.rs`, `tests/integration/support/mod.rs` (`fake.options()`, temp dirs, file
  helpers), `tests/integration/support/fake_drive.rs` (new) per D19 and D20.
- Verify: `cargo test --locked --test integration fake_drive::` → pass.

### S4 — Token store, credentials, `Client::create` and `open` [AC1]
- Tests first: `tests/integration/auth.rs` (new) —
  - `create_writes_the_store_with_mode_600`: mode is exactly 0o600 (the test leaves the process umask alone); the
    JSON has `version` 1, the three credentials and a null access token.
  - `store_is_replaced_by_rename`: a second write changes the inode; a handle opened before still reads the old, whole
    content; no `.tmp` file is left in the directory.
  - `open_rejects_a_store_that_is_not_mode_600`: chmod 644 → `TokenStore { InsecureMode(0o644) }`, and the mode is
    still 644 afterwards.
  - `open_reports_missing_unreadable_and_invalid_stores`: no file → `Missing`; a directory at the path → `Unreadable`;
    garbage, a missing field, or `version` 2 → `Invalid`.
  - `failed_write_keeps_the_old_store` (skipped when running as root): directory made read-only → `create` returns
    `TokenStore { WriteFailed }` and the old store is byte-identical.
  - `credentials_and_client_debug_hide_secrets`: `Debug` shows the client id, not the secret or the refresh token.
- Change: `src/token_store.rs`, `src/credentials.rs`, `src/client.rs` (new) per D3, D5, D8.
- Verify: `cargo test --locked --test integration auth::` → pass.

### S5 — Request executor, token refresh, retries and `metadata` [AC1, AC2, AC4]
- Tests first:
  - `tests/integration/auth.rs` — `first_call_refreshes_and_stores_the_access_token` (store gains the access token
    and `access_token_refresh_at`, still mode 600); `token_is_reused_until_60_seconds_before_expiry` (`expires_in`
    3600: three calls, one token request; `expires_in` 59: three calls, three token requests);
    `stored_token_is_used_after_reopen` (no token request); `a_401_refreshes_once_and_resends` (fault `Unauthorized`
    → success, two token requests); `a_second_401_is_an_error` (`Unauthorized` twice → `Http` 401, exactly two token
    requests); `invalid_grant_is_a_distinct_error` (`InvalidGrant`, Display says to log in again);
    `failed_access_token_write_does_not_fail_the_call` (skipped as root: directory read-only after `open` →
    `metadata` returns `Ok`; `take_store_failure()` returns `TokenStore { WriteFailed }` once, then `None`; the store
    is byte-identical; a second call uses the in-memory token, with no second token request; after the directory is
    writable again, the next refresh (`expires_in` 59) writes the store with the newest token);
    `failed_write_of_rotated_credentials_fails_until_written` (skipped as root; `rotate_refresh_tokens(true)`,
    directory read-only → the call fails with `TokenStore { WriteFailed }`; the next call fails the same way without
    a token request, so it does not refresh with the old, now dead refresh token; after the directory is writable the
    next call writes the store, now holding the rotated refresh token, and succeeds); `secrets_absent_from_error_text` (Display and Debug of the errors from `invalid_grant`, a
    second 401, a refused connection, a deadline, and an OAuth 400 and a Drive 403 whose `error_description` /
    `message` echo the client secret, refresh token and access token contain none of them, but do contain
    `[redacted]` and Google's code); `a_slow_refresh_is_shared_not_repeated` (deadline 3 s; a `Hook` holds the first
    token request for 1 s; a second thread's `metadata` on the same client, started 100 ms later, waits and returns
    `Ok` with the first refresh's token; exactly one token request; the bounded wait itself, when the refresh outlasts
    the waiter's deadline, is `only_one_refresh_in_flight` below, because one client has one deadline);
    `concurrent_calls_refresh_once_with_rotation` (`rotate_refresh_tokens(true)`, `expires_in` 3600 so a new token is
    not already inside the 60 s margin, a fresh store with no access token; eight threads call `metadata` at once →
    exactly one token request, no `InvalidGrant`, and the store holds the rotated refresh token; overlapping callers
    sharing one blocked refresh within their own deadlines is `only_one_refresh_in_flight` below).
  - `src/credentials/tests.rs` (new) — `refresh_at_is_receipt_plus_expires_in_minus_60`;
    `only_one_refresh_in_flight` (two callers need a refresh; the injected refresher blocks; the second caller waits,
    then uses the first one's token without calling the refresher; with a 200 ms deadline it returns `Deadline` before
    the first finishes, again without calling the refresher); `write_outcomes_follow_d8` (a successful write clears
    what was owed; a failed write of an access token only records the failure and the call goes on; a failed write of
    rotated credentials returns `WriteFailed`, keeps them in memory and owed; the next call writes them first);
    `rotated_credentials_survive_a_deadline` (the refresher returns a rotated token just as the deadline passes, and in
    a second case the write gate is held by a stalled writer → the call returns `Deadline`, the state holds the rotated
    refresh token and owes credentials, and the next call writes them before anything else);
    `writes_take_the_latest_state` (two writes in a row through the gate; the second writes the newer generation, and
    no write ever stores an older generation than the file already holds);
    `a_stalled_write_does_not_hold_another_call_past_its_deadline` (thread A's writer blocks until released; thread B,
    owing credentials, waits for the gate with a 200 ms deadline → B returns `Deadline` before A is released).
  - `tests/integration/files.rs` (new) — `metadata_returns_every_field` (id, name, size, MD5, parents, trashed, MIME
    type, modified time; the request carried `fields`); `metadata_of_an_unknown_id_names_id_and_reason` (`Http` 404,
    reason `notFound`, target the id).
  - `tests/integration/retry.rs` (new) — `retries_each_transient_status` (429, 500, 502, 503, 504, each once → success
    after two requests); `retries_drive_rate_limit_403` (a 403 whose `errors[0].reason` is `rateLimitExceeded`, and
    one with `userRateLimitExceeded` → success after two requests; with `Retry-After: 1` the resend waits at least
    1 s); `retries_a_dropped_connection` (`CloseBeforeResponse`); `does_not_retry_other_4xx` (400, 404, and 403 with
    reasons `storageQuotaExceeded` and `insufficientFilePermissions` → `Http` after one request, reason kept); `honors_retry_after_seconds` (503 with `Retry-After: 1` → the second request comes
    at least 1 s after the first, though backoff is 10 ms); `stops_at_the_deadline` (`always(Get, 503)`, deadline 1 s →
    `Deadline` after more than one attempt, under 1.5 s in total); `retry_after_past_the_deadline_fails_at_once`
    (`Retry-After: 30`, deadline 2 s → `Deadline` within 0.5 s); `token_refresh_is_retried` (`CloseBeforeResponse` on
    `Token`).
  - `src/http/tests.rs` (new) — `parses_retry_after_seconds_and_http_date` (`120`; an IMF-fixdate against a given
    now; a past date → zero; garbage → none); `backoff_stays_within_the_jittered_ceiling` (attempts 0 to 12, capped);
    `classifies_transient_failures` (D7's lists, including a 403 body with a rate-limit reason in a later `errors`
    entry, and one without a body).
- Change: `src/http.rs` (new) per D7; refresh in `src/credentials.rs` per D9; `src/files.rs` (new: `DriveFile` from
  `serde_json::Value`, `metadata`); `src/client.rs`.
- Verify: `cargo test --locked` → pass.

### S6 — Device login and rclone import [AC1]
- Tests first:
  - `tests/integration/auth.rs` — `device_login_shows_url_and_code_then_approves` (script pending, pending, approved;
    the URL and code match the fake's; polls carry `client_secret`, `device_code` and the device grant type; the
    credentials hold the fake's refresh token); `device_login_asks_for_drive_file_scope` (scope is exactly
    `https://www.googleapis.com/auth/drive.file`); `slow_down_adds_five_seconds` (interval 0; slow_down, approved → the
    second poll at least 5 s after the first; the suite's one slow test); `denied_login` (`LoginDenied`);
    `login_expired_by_google` (`expired_token` → `LoginExpired`); `login_expires_locally` (`expires_in` 2, interval 1,
    always pending → `LoginExpired` within 3 s); `poll_failures_retry_within_the_deadline` (deadline 1 s, code
    lifetime 60 s, `always(DevicePoll, 503)` → `Deadline` within 1.5 s); `poll_retry_waits_the_interval` (interval
    1 s, backoff 10 ms; one 503, then approved → the resend comes at least 1 s after the failed poll); `device_login_debug_hides_code_and_secret`;
    `imported_rclone_credentials_work` (import a sample config → `Client::create` → `metadata` succeeds).
  - `src/rclone/tests.rs` (new) — `imports_the_named_drive_section` (two sections; the named one's id, secret and the
    token JSON's refresh token); `rejects_a_missing_section`; `rejects_a_section_that_is_not_drive`;
    `rejects_rclones_builtin_client` (empty `client_id`); `rejects_a_token_without_refresh_token`;
    `rejects_an_encrypted_config`; `errors_do_not_echo_values`.
  - `src/device_login/tests.rs` (new) — `poll_delay_is_the_greatest_of_interval_backoff_and_retry_after` (including an
    interval raised by `slow_down`, so the integration suite needs no second 5 s wait).
- Change: `src/device_login.rs`, `src/rclone.rs` (new) per D10, D11.
- Verify: `cargo test --locked` → pass.

### S7 — Folders, listing and find [AC2]
- Tests first:
  - `tests/integration/files.rs` — `resolves_an_existing_nested_path`; `missing_folder_names_the_path`
    (`FolderNotFound { "a/b" }`); `ensure_folder_creates_missing_levels` (creates `b` under `a`, with `a`'s id as
    parent; a second call creates nothing); `duplicate_folders_name_the_path` (`DuplicateFolder { "a" }`, also with
    page size 1 so the two folders come on separate pages);
    `trashed_folders_do_not_count` (only a trashed `a` → `folder_id` is `FolderNotFound`, `ensure_folder` creates a new
    one); `lost_create_answer_does_not_duplicate` (`ApplyThenClose` on `Create` → exactly one folder);
    `rejects_empty_path_segments` (`""`, `a//b`, `/a` → `InvalidInput`, no request); `lists_every_page` (page size 2,
    five files → three list requests, five files with size and MD5; a trashed file and a subfolder left out);
    `find_returns_every_exact_match` (two `x.gz`, plus `X.gz` and `x.gz.bak` → the two);
    `quotes_and_backslashes_are_escaped` (folder `it's\here` and file `a'b\c.txt` resolve and are found; the fake's
    strict parser would answer 400 otherwise).
  - `src/query/tests.rs` (new) — `escapes_backslash_then_quote`.
- Change: `src/query.rs` (new), `src/files.rs` per D12, D13.
- Verify: `cargo test --locked` → pass.

### S8 — Resumable upload [AC3]
- Tests first (`tests/integration/upload.rs`, new; chunk 256 KiB unless noted):
  - `uploads_a_new_file_in_chunks`: 1 MiB + 123 bytes → five chunk `PUT`s with the exact `Content-Range` sequence;
    the fake holds the same bytes; `Upload.file` has Drive's id, size and MD5; `verified()`.
  - `replaces_the_content_of_an_existing_id`: `PATCH` initiation; same id and name, new bytes; `verified()`.
  - `resumes_after_a_dropped_connection`: `DropAfterBody { persist: 100 KiB }` on the second chunk → a status query,
    then a `PUT` starting at 356 KiB; final bytes and MD5 right.
  - `resumes_after_503_mid_upload`: a 503 on the third chunk with nothing kept → the same range resent; with
    `ApplyThenClose` on a chunk → the next range follows.
  - `status_query_failures_are_retried`: the status query answers 503 once, then 308.
  - `rate_limited_chunk_resumes`: a 403 `userRateLimitExceeded` on the second chunk → back off, status query, the
    upload continues from the reported offset (the same session; not a restart) and is `verified()`.
  - `partial_confirmation_resends_the_tail`: `ReportRange` keeps 128 KiB of the second chunk → the next `PUT` starts at
    384 KiB with a full 256 KiB.
  - `lost_session_restarts_once`: `SessionGone(404)` on the third chunk, and in a second case `SessionGone(410)` on a
    status query → two initiations, right bytes; `second_session_loss_is_an_error` (two losses → `Http`, no file).
  - `offset_below_the_confirmed_one_is_a_protocol_error`: `ReportRange` below what was already confirmed → `Protocol`
    naming both numbers.
  - `verify_fails_when_drive_md5_differs`: `CorruptMd5` → `Ok`, `verified()` false.
  - `verify_fails_when_the_local_file_changes`: a `Hook` appends to the local file during the second chunk → Drive got
    the original length, `local_file_changed`, `verified()` false; the same with the `Hook` on the final chunk (after
    the last read).
  - `verify_fails_when_the_path_is_replaced`: during the second chunk a `Hook` renames another file over the path →
    `local_file_changed`, `verified()` false (the open handle still uploads the original bytes).
  - `shrinking_local_file_stops_the_upload`: during the second chunk a `Hook` truncates the 1 MiB file to 300 KiB
    (below the 512 KiB already read) → the next refill hits EOF → `LocalFileChanged`, no file in the fake.
  - `missing_fields_in_the_final_answer_are_fetched`: `OmitFields` → one extra `metadata` request; `verified()`.
  - `empty_file`: one `PUT` with `Content-Length: 0` and `Content-Range: bytes */0`; MD5
    `d41d8cd98f00b204e9800998ecf8427e`; `verified()`.
  - `deadline_mid_upload_reports_confirmed_bytes`: two chunks accepted, then `always(ChunkPut, 503)` with status
    queries answering 308 → `Deadline { confirmed_bytes: Some(512 KiB) }`; no file in the fake.
  - `session_url_on_another_origin_is_refused`: `ForeignLocation` with the fake's host and another port, and with
    `<fake origin>.attacker.example` → `Protocol`, nothing sent there (the fake's request log has no `ChunkPut`).
  - `upload_errors_hide_the_session_url`: Display and Debug of a mid-upload `Deadline` and a `Protocol` error contain no
    `upload_id`.
  - `src/upload/tests.rs` (new) — `parses_the_range_header` (`bytes=0-42` → 43; absent → 0; malformed → error);
    `confirmation_moves_the_window` (the MD5 sees exactly the confirmed bytes; the tail moves to the front; values
    outside `[confirmed, confirmed + len]` are rejected).
- Change: `src/upload.rs` (new) per D14–D16; `upload_new` / `upload_replace` in `src/client.rs`.
- Verify: `cargo test --locked` → pass.

### S9 — Download, delete and trash [AC4]
- Tests first:
  - `tests/integration/download.rs` (new) — `downloads_through_a_temp_file` (same bytes; `Download` size and MD5 right;
    no `.part` left); `truncated_download_restarts_from_the_start` (`TruncateBody` once → two GETs, right bytes);
    `interrupted_download_leaves_no_file` (truncated every time, deadline 1 s → `Deadline`; neither the target nor a
    `.part` exists); `failed_download_keeps_the_existing_file` (the old target bytes are intact after a failure);
    `unknown_id_is_an_error_and_writes_nothing` (`Http` 404); `malformed_redirect_is_not_echoed` (`Redirect { 302,
    location: "http://[bad/?upload_id=s3cr3t" }` on `Download` → an error whose Display and Debug lack the `Location`
    value and `s3cr3t`; no file written).
  - `tests/integration/files.rs` — `delete_removes_the_file`; `retried_delete_that_was_applied_succeeds`
    (`ApplyThenClose` on `Delete` → `Ok`); `delete_of_an_unknown_id_fails` (`Http` 404); `trash_moves_to_the_trash`
    (`metadata` shows trashed; `list_files` leaves it out).
- Change: `src/download.rs` (new) per D17; delete and trash in `src/files.rs` per D18.
- Verify: `cargo test --locked` → pass.

### S10 — Example program [AC8, AC5 support]
- Tests first: `tests/integration/example.rs` (new) — `cli_round_trip`, `#[ignore]`, binary from
  `GDRIVE_CLIENT_EXAMPLE`: `set-credentials`, then `upload` (new file), `upload` again (same id, replaced), `list` (one
  file and its MD5), `download` (same bytes), `delete`, all against the fake with `--endpoint`, exit 0; with
  `CorruptMd5` the upload exits 1 and prints that it is not verified. Every child runs with `ALL_PROXY`, `HTTPS_PROXY`
  and `HTTP_PROXY` set to `http://127.0.0.1:9` (nothing listens) and `NO_PROXY` removed, so the round trip passes only
  if loopback endpoints ignore environment proxies (D7).
- Change: `examples/gdrive.rs` (new) per D23.
- Verify: `cargo build --locked --example gdrive` then
  `GDRIVE_CLIENT_EXAMPLE=target/debug/examples/gdrive cargo test --locked --test integration example:: -- --ignored`
  → pass.

### S11 — Memory and size measurements [AC5]
- Tests first: `tests/integration/measure.rs` (new) — `peak_rss`, `#[ignore]`, per D21.
- Change: `MEASUREMENTS.md` (new): date, host, toolchain, the commands below, the table from `peak_rss` (VmHWM in KiB
  per case), the static musl sizes of `examples/gdrive` and `examples/size_baseline` and their difference (an upper
  bound on the library's added size: it includes the example's own code), and why the numbers exclude the page cache.
- Verify: `df -h target` shows at least 2 GiB free; `cargo build --release --locked --target x86_64-unknown-linux-musl
  --examples`; `GDRIVE_CLIENT_EXAMPLE=target/x86_64-unknown-linux-musl/release/examples/gdrive cargo test --release
  --locked --test integration measure:: -- --ignored --nocapture` → passes and prints the table;
  `ls -l target/x86_64-unknown-linux-musl/release/examples/{gdrive,size_baseline}` matches `MEASUREMENTS.md`.

### S12 — Opt-in live test [AC6]
- Tests first: `tests/integration/live.rs` (new) — `round_trip` per D22.
- Change: none beyond the test.
- Verify (never against Drive in this build): `cargo test --locked --test integration live:: -- --nocapture` without
  `GDRIVE_CLIENT_LIVE` → passes and prints `live test skipped: …`;
  an audit of where tests get their request destinations: `grep -rn 'Options::default\|Endpoints::default\|Endpoints {'
  tests/` matches only `tests/integration/live.rs` and `fake.options()` in `tests/integration/support/mod.rs`
  (`..Options::default()` after `endpoints: Endpoints::local(…)`), and `grep -rn 'googleapis' tests/ | grep -v
  'googleapis.com/auth/drive.file'` has no match outside `live.rs` (the scope literal in S6's
  `device_login_asks_for_drive_file_scope` is not a destination); every other test talks to the fake. The later live
  run, when the user's bdgtransit client exists:
  `cargo run --example gdrive -- --store <store> login <client-id>` (secret from `GDRIVE_CLIENT_SECRET`), then
  `GDRIVE_CLIENT_LIVE=1 GDRIVE_CLIENT_TOKEN_STORE=<store> cargo test --locked --test integration live:: -- --nocapture`,
  recorded in `.works/drive-client/live-run.md`.

### S13 — README [AC8]
- Check (no test can fail first; docs): before, `README.md` is three lines titled `gdrive-lite`; after, with
  `#![doc = include_str!("../README.md")]` in `src/lib.rs`, `cargo test --locked --doc` compiles every README example,
  and `grep -n '^## ' README.md` lists the sections below.
- Change: `README.md` rewritten, titled `gdrive-client`: what it is (blocking, constant memory, static musl, no
  logging); install as a git dependency pinned to a commit (`gdrive-client = { git =
  "https://github.com/risan/gdrive-client", rev = "<commit>" }`); Google Cloud setup: create a project or reuse one,
  enable the Drive API, the OAuth consent screen (publish it, or refresh tokens expire after 7 days), add an OAuth
  client to an existing project (accounts are limited in projects), which client type goes with which login (device
  flow: "TVs and Limited Input devices"; reusing an rclone or desktop token: "Desktop app"), what `drive.file` lets a
  client see (files created or opened by any client of the same project; a client of another project sees none of
  them); logging in (device flow, supplied values, one-time rclone import); the token store (format, mode 600, atomic
  writes, `InvalidGrant` means log in again, a failed write of a refreshed access token is reported by
  `take_store_failure` while the call goes on, and a failed write of new credentials fails the call); one example per
  operation (folder resolve and create, list, find, metadata, upload new and replace with the verify check, download,
  delete, trash); retries (429, 5xx, Drive's rate-limit 403s, network errors), the deadline (it covers network calls,
  retries, backoff, refreshes and waits on other calls, not local file I/O) and errors;
  memory and size (link to `MEASUREMENTS.md`); building static musl (ring needs a C compiler: zig as in
  `tools/zig-cc`, or Alpine's gcc); a warning that ureq's `TRACE` log level is not redacted and shows tokens; the
  example binary; tests (fake server; the opt-in live test and its variables); license.
- Verify: `cargo test --locked --doc` → pass; the `grep` shows the sections.

### S14 — Fresh-shell gate [AC7]
- Check: in `env -i HOME="$HOME" PATH=/usr/bin:/bin bash -lc '…'` at the repo root: `cargo test --locked`;
  `cargo clippy --locked --all-targets -- -D warnings`; `cargo fmt --check`;
  `cargo build --release --locked --target x86_64-unknown-linux-musl --examples` and `file` on both examples →
  statically linked; `cargo build --locked --example gdrive` and the `example::` ignored test; `git status --short`
  shows `Cargo.lock` tracked and nothing untracked outside `.works/`.
- Change: none, unless a check fails (then fix in the Step that owns the code and rerun).
- Verify: every command above exits 0.

## Risks
- ring needs a C compiler for musl — S1 builds a static musl binary before any library code exists (D25).
- The fake drifts from real Drive — the fake is strict where clients go wrong (D19), and the AC6 live run checks the
  real 308, `fields` on the final answer, the `PATCH` upload and downloads; until it runs, the check reports it skipped.
- Drive may confirm offsets that are not multiples of 256 KiB — the sliding buffer accepts any offset in
  `[confirmed, confirmed + len]` and keeps every chunk a multiple of 256 KiB in length (D14).
- An upload that Drive completed but whose answer and status query were both lost, with the query answering 404, would
  start over and create a second file — Drive documents 200/201 for finished sessions, so this needs Drive to misbehave;
  `find_files` returns duplicates for the caller to see.
- A rate-limit 403 mistaken for a permanent one, or the reverse — D7 decides from the body's `errors[].reason`, and
  S5 pins both sides (`retries_drive_rate_limit_403`, `does_not_retry_other_4xx` with `storageQuotaExceeded`).
- Two processes creating the same folder at once can make duplicates, which later resolves report as
  `DuplicateFolder` — the callers run one job at a time; README says so.
- A test reaching real Google or a proxy — every test except `live::round_trip` builds options from `fake.options()`
  (the grep in S12 pins that), `Endpoints` validation refuses plain http off loopback, and loopback endpoints and
  `support::raw_agent()` ignore environment proxies (D7, D19; S10 runs the example with proxy variables pointing at a
  dead port).
- A store that cannot be written goes unnoticed — the call goes on, but `take_store_failure` reports it (D3, D8);
  README tells callers to check it after a run. A process that keeps failing to write refreshes once per token
  lifetime, as before.
- Timing tests flake on a loaded host — they assert lower bounds or generous upper bounds (1.5 s for a 1 s deadline),
  and only `slow_down_adds_five_seconds` waits 5 s.
- ureq's `send(&[])` might not send `Content-Length: 0` on a status query — the fake answers 411 without it
  (`put_without_content_length_is_411`, `empty_file`), so S8 would fail and the code would set the header itself.
- The 1 GiB download in S11 writes 1 GiB — it goes to a temp dir that is removed; S11 checks free space first.

## Coverage
| Criterion | Steps |
|---|---|
| AC1 | S2 (secret `Debug`), S4, S5 (incl. R2's failed-write rules), S6 |
| AC2 | S5 (`metadata`), S7 |
| AC3 | S8 |
| AC4 | S5 (backoff, `Retry-After`, deadline, rate-limit 403s retried, no retry of other 4xx), S8 (rate-limited chunk), S9 |
| AC5 | S10 (support), S11 |
| AC6 | S12 |
| AC7 | S1, S14 |
| AC8 | S10, S13 |
| Edge: file changed while uploading | S8 (`verify_fails_when_the_local_file_changes`, `shrinking_local_file_stops_the_upload`) |
| Edge: empty file | S8 (`empty_file`) |
| Edge: quotes and backslashes in names | S7 (`quotes_and_backslashes_are_escaped`) |
| Edge: token file missing, unreadable, not mode 600 | S4 |
| Edge: clock skew | S5 (`refresh_at_is_receipt_plus_expires_in_minus_60`, `token_is_reused_until_60_seconds_before_expiry`) |
| Edge: deadline mid-upload | S8 (`deadline_mid_upload_reports_confirmed_bytes`) |

## Changes after build
- 9c02db1 (deep pass) — `refresh_moments` and `Coordinator::new` use `checked_add`: an `expires_in` or stored
  `access_token_refresh_at` too large for an `Instant` means the token is due now instead of a panic; `DeviceLogin::start`
  answers `Protocol` for an unrepresentable `expires_in`; `slow_down` saturates. Tests
  `an_expiry_too_far_to_represent_means_refresh_now`, `device_code_lifetime_out_of_range_is_a_protocol_error`.
- 269c703 (deep pass) — new test `download::a_redirect_to_another_origin_carries_no_bearer_token` pins D7's
  redirect rule (ureq's `RedirectAuthHeaders::Never`); no code change.
- 8a491bf (deep pass) — `examples/gdrive.rs` prints the `take_store_failure` warning after a failed command too (D23
  "after every command"); the client commands moved into `run_with_client`.
- cbb3082 (check) — seven verify findings: (1) `Client::refresh` registers the credentials it sends on the call, so an
  OAuth error echoing a refresh token another thread rotated is redacted (test
  `a_refresh_error_hides_a_token_another_call_rotated`); (2) `Call::know` keeps each secret once (an upload no longer
  adds an access-token entry per chunk); (3) `DeviceLogin::start` counts the code's lifetime from the answer that
  carries it, not from the retries before it (D10); (5) a deadline while fetching the size or MD5 Drive left out of
  the final answer carries `confirmed_bytes` (D14.6); (6) a token store with `access_token` or
  `access_token_refresh_at` absent is `Invalid` (D8: a missing field); (7) `Call::new` cuts a budget above ten
  years to ten years, so `Options::deadline = Duration::MAX` ("no deadline") no longer panics (D3/D7; documented on
  `Options::deadline`). Finding 4, conceded by Codex in re-verify: a device-login poll whose retry delay (`Retry-After`,
  interval or backoff) would end after the code's expiry fails at once with `LoginExpired` instead of sleeping to a
  certain end (D10 "or `LoginExpired` at expiry" includes this; documented on `DeviceLogin::wait`).

## Review round 1 — 2026-10-24
- Accepted (Major): the session-URL origin check was a string prefix (`https://www.googleapis.com.attacker.example`
  passes). D6 now parses with `ureq::http::Uri` and compares scheme, host and effective port, refusing userinfo; S2
  `same_origin_compares_scheme_host_and_port`, S8 `session_url_on_another_origin_is_refused` cover suffix hosts, other
  ports and userinfo.
- Accepted (Major): redaction covered only `BadUri`; `RequireHttpsOnly` (`ureq-3.5.0/src/error.rs:265`) and
  `BadLocationHeader` (`ureq-proto-0.6.5/src/error.rs:71`) print URLs, and Google's error texts were copied verbatim.
  D5 now maps every ureq variant to a fixed text and redacts known secrets from Google's texts; tests in S2
  (`describe_never_prints_urls`, `redact_replaces_every_known_secret`), S5 (`secrets_absent_from_error_text` with
  echoed secrets) and S9 (`malformed_redirect_is_not_echoed`). Also: ureq `Protocol` errors are no longer retried
  (D7; a disconnect is `Io(UnexpectedEof)`, `ureq-3.5.0/src/error.rs:210`), so a malformed answer fails at once.
- Accepted (Major): folder lookups had no pagination, so duplicates on separate pages could resolve. D12 uses the
  paginated find query and judges the count after the last page; S7 duplicate test also runs with page size 1.
- Accepted (Major): the token mutex was held during a refresh, so a call could wait past its deadline. D9 never holds
  it across a request or the store write (two stale callers may both refresh, harmlessly); S5
  `a_slow_refresh_does_not_hold_up_another_call`.
- Accepted (Major): the local-file identity check ran "after the last read", missing changes during the final chunk.
  D15 checks after Drive's final answer and any D16 call, right before the result; S8 adds the final-chunk and
  path-replacement cases.
- Accepted (Major, raised as a product escalation): D8 ignored a failed access-token write, against R2. Resolved by
  following R2 literally: the write failure is `TokenStore { WriteFailed }` and the token is not kept; S5 test renamed
  `failed_access_token_write_is_an_error`. The full-disk trade-off (the mover cannot upload when the store cannot be
  written) is an open item for the spec, in Risks.
- Pushed back (Major, raised as a product escalation): "device polling ignores `options.deadline`". The spec settles
  it: R1 says the library "polls at Google's interval until approval, denial or expiry", and R8 bounds the retries of
  every request. D10 now states both: each poll's transient failures retry until `options.deadline` after that poll
  began or the code's expiry, whichever is first; the loop ends only at approval, denial or expiry. D3's deadline
  wording now names `Client` calls and `DeviceLogin::start`. S6 adds `poll_failures_retry_within_the_deadline`.
- Escalated: none open (the two raised escalations were resolved within the spec, above).
- status: in-progress

## Review round 2 — 2026-10-24
- Codex conceded the round 1 push-back on device polling (R1 allows polling through the code's lifetime).
- Accepted (Major): D9 published a refreshed token before writing the store, so another thread could use a token the
  file never held, and concurrent commits could land out of order. D9 now serializes commits with a second mutex held
  only for the local write, publishes only after a successful write, and drops a candidate older than the committed
  token; unit tests `commit_publishes_only_after_the_write`, `older_candidate_never_replaces_a_newer_commit`; S5's
  failed-write test now checks that the next call makes its own token request.
- Accepted (Major): a device poll's transient-failure retry could resend sooner than Google's interval. D10 now waits
  the greatest of the current interval, the backoff and `Retry-After` before every resend; S6
  `poll_retry_waits_the_interval` and unit `poll_delay_is_the_greatest_of_interval_backoff_and_retry_after`.
- Accepted (Minor): ureq reads proxy variables by default (`ureq-3.5.0/src/config.rs:1073`), so test traffic could
  leave the host. D7 sets `proxy(None)` for loopback endpoints, D19 adds `support::raw_agent()` with no proxy, and S10
  runs the example with proxy variables pointing at a dead port.
- Pushed back: none.
- Escalated: none.
- status: in-progress

## Review round 3 — 2026-10-24
- Codex kept its concession on device polling and confirmed the origin, pagination and write-before-publish fixes.
- Accepted (Major): the commit gate was taken with a plain `lock()`, so a write stalled in another thread (a slow
  `sync_all`) could hold a call past its deadline. D9 now takes the gate with a `try_lock` loop bounded by the call's
  deadline (a timed-out caller returns `Deadline` and its candidate is neither written nor published), rechecks the
  deadline before writing, and takes the store writer as a closure so unit tests can stall or fail it; S5 adds
  `a_stalled_write_does_not_hold_another_commit_past_its_deadline` and `commit_after_the_deadline_writes_nothing`.
- Pushed back: none.
- Escalated: none.
- status: in-progress

## Review round 4 — 2026-10-24
- Codex kept its concession on device polling and called the origin checks, folder pagination, upload identity checks
  and write-before-publish ordering sound.
- Accepted (Minor): S12's audit grep would also match the required scope literal
  `https://www.googleapis.com/auth/drive.file` in S6's test. S12 now audits request destinations (`Options::default`,
  `Endpoints::default`, `Endpoints {` only in `live.rs` and `fake.options()`) and greps `googleapis` minus the scope
  literal.
- Pushed back (Major, NEW): "the persisting caller's own store write can run past its deadline; move persistence to a
  worker thread". R8 bounds retries of requests ("every request retries … until a total deadline"); local file I/O is
  never interruptible with blocking std calls, and the same holds for the upload's source reads and the download's
  1 GB of writes and `sync_all`, which nobody proposes to thread. ureq documents its own timeouts the same way
  (`ureq-3.5.0/src/config.rs:755`: they "do not interrupt application code"). Round 3's fix was different in kind: it
  removed a wait on another call, which costs a `try_lock` loop; a writer thread per commit adds thread and late-result
  handling for a sub-kilobyte write. D3 now states the deadline's scope exactly (network requests, retries, backoff,
  refreshes and waits on other calls; not local file I/O), so the plan no longer claims more. Unconfirmed: the cap of
  four rounds was reached, so Codex has not re-argued this.
- Escalated: none.
- status: converged (round 4 accepted no new Blocker or Major; the default cap of 4 is reached; one Major push-back is
  unconfirmed, above)

## Review round 5 — 2026-10-24
- Context: one single-shot round after the spec changed R2 (a failed write of a refreshed access token is reported, not
  fatal; new credentials still fail) and R8 (403 `rateLimitExceeded` / `userRateLimitExceeded` retried like 429; the
  deadline excludes local file I/O). Before the round the plan was amended in D3 (`take_store_failure`, deadline
  wording), D7, D8, D9, D19, D23, S5, S8, S13, Risks and Coverage.
- Codex conceded the round 4 push-back (R8 now excludes local file I/O) and kept its concession on device polling.
- Accepted (Major, NEW): concurrent refreshes are unsafe with token rotation (a second refresh sends the refresh token
  the first just invalidated and gets `invalid_grant`). D9 now allows one refresh in flight per client; other callers
  poll the state every 2 ms within their own deadline and then use the new token. S5's
  `a_slow_refresh_does_not_hold_up_another_call` became `a_slow_refresh_is_shared_not_repeated`; new
  `concurrent_calls_refresh_once_with_rotation` and unit `only_one_refresh_in_flight`.
- Accepted (Major, NEW): a deadline check could drop a rotated refresh token received just before expiry. D9 now
  publishes every refresh answer to memory at once, marking the store as owing credentials when rotated, before any
  deadline check; writes snapshot the current state under the write gate, so they cannot land out of order (the
  candidate-ordering scheme is gone); a call that times out leaves the rotated credentials owed, and the next call
  writes them first. Unit tests `rotated_credentials_survive_a_deadline`, `writes_take_the_latest_state`,
  `write_outcomes_follow_d8`, `a_stalled_write_does_not_hold_another_call_past_its_deadline`.
- Accepted (Minor, NEW): `support::raw_agent()` left ureq's 10 redirects on, so the fake's 308 would be rejected for
  lacking `Location` (`ureq-proto-0.6.5/src/client/redirect.rs:26`). D19 now sets `max_redirects(0)`.
- Pushed back: none.
- Escalated: none.
- status: capped-open (the coordinator set one round; it accepted two new Majors, fixed above but not confirmed by
  Codex)

## Review round 6 — 2026-10-24
- Single-shot round requested by the coordinator to confirm round 5. Codex confirmed both round 5 Major fixes
  (refreshes serialized; rotated credentials published before deadline checks; writes of the latest state under a
  deadline-bounded gate) and found no Blocker or Major.
- Accepted (Minor, NEW): `concurrent_calls_refresh_once_with_rotation` used `expires_in` 59, so a new token was
  already inside the 60 s margin and a later thread could correctly refresh again, failing a correct build. The test
  now uses `expires_in` 3600 on a fresh store; the sequential 59 s test stays; overlap within deadlines stays with
  the unit test `only_one_refresh_in_flight`. A deterministic reviewer-prescribed fix, closed by this edit.
- Pushed back: none.
- Escalated: none.
- status: converged
