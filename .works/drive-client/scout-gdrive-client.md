# Scout: gdrive-client (new repository) — facts for the plan

Date: 2026-10-24. Checked by the plan drafter directly (no repo-scout run: the repo is empty).

## Repository
- Worktree `/home/risan/projects/code/gdrive-lite/.claude/worktrees/drive-client`, branch `feat/drive-client` from
  `main` @ cf6e955. Tracked files: `README.md` (title still "gdrive-lite"), `LICENSE` (MIT), `.gitignore` (`/target`).
  `origin` is `https://github.com/risan/gdrive-client.git`. `.works/` is untracked.
- No test command exists yet; the plan creates the crate. The user's other Rust crate is bdgtransit's
  `research/collector/` (worktree `/home/risan/projects/code/bdgtransit/.claude/worktrees/rust-nightly/research/collector`):
  its conventions are the pattern to mirror.

## Pattern to mirror: bdgtransit `research/collector/`
- `Cargo.toml`: edition 2024, `rust-version = "1.99"`, `ureq = { version = "3.5.0", default-features = false,
  features = ["rustls"] }`, release profile `opt-level = "s"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`,
  `strip = "symbols"`; dev-deps include `httparse = "1"`, `tempfile = "3"`, `serde_json` (dev only).
- `rust-toolchain.toml`: `channel = "1.99.1"`, components clippy + rustfmt, `targets = ["x86_64-unknown-linux-musl"]`,
  `profile = "minimal"`. `rustfmt.toml`: `max_width = 120`.
- `.cargo/config.toml`: `[env] CC_x86_64_unknown_linux_musl = { value = "tools/zig-cc", relative = true }` and the same
  for `AR_` with `tools/zig-ar`. `tools/zig-cc` strips cc-rs's `--target=...` and runs
  `zig cc -target x86_64-linux-musl "$@"`; `tools/zig-ar` is `exec zig ar "$@"`.
- Unit tests live in `src/<module>/tests.rs` next to `src/<module>.rs` (for example `src/json.rs` + `src/json/tests.rs`).
- JSON: the collector has its own 410-line scanner (`src/json.rs`) because it must match Go's `encoding/json`; that
  reason does not apply here. Neither caller has `serde_json` as a normal dependency (bdgtransit: dev only;
  traffic-analysis `recorder/Cargo.toml`: only `ureq` 3 and `time` 0.3 with `default-features = false`).
- Fresh shell: bdgtransit runs cargo through `env -i HOME="$HOME" PATH=... bash -lc`; `~/.profile` adds
  `~/.local/bin` (zig 0.17.0) and sources `~/.cargo/env` (rustup, toolchain 1.99.1 with the musl target installed).
  The agent's own shell does not have `~/.cargo/bin` on `PATH`.

## ureq 3.5.0 facts (`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ureq-3.5.0/`)
- `Cargo.toml:101` feature `rustls = ["rustls-no-provider", "_ring", "rustls-webpki-roots"]`; `_ring` (line 63) selects
  rustls's `ring` provider. `src/tls/mod.rs:375`: default `root_certs: RootCerts::WebPki` (compiled-in roots).
  No `gzip` feature, so no `Accept-Encoding` is sent and Drive answers uncompressed.
- `ring` needs a C compiler for the target: a musl build on this host without one fails with
  `failed to find tool "x86_64-linux-musl-gcc"` (tried 2026-10-24); with bdgtransit's `tools/zig-cc` it builds.
- `src/config.rs:322-335`: `max_redirects(0)` returns every 3xx as is, never `TooManyRedirects` (needed for Drive's
  `308 Resume Incomplete`, which has no `Location`). `:520` `http_status_as_error(false)` returns 4xx/5xx as `Ok`.
  `:604` `redirect_auth_headers`, default `Never` (`ureq-proto-0.6.5/src/client/mod.rs:498`): `Authorization` is
  dropped on redirects.
- Timeouts (`src/config.rs:747-930`): `timeout_global` (end to end, including reading the body),
  `timeout_per_read` / `timeout_per_write` (restart per transport read/write), `timeout_connect`. Per-request config:
  `RequestBuilder::config()` (`src/request.rs:368`). `ureq::Timeout` variants (`src/timings.rs`): `PerRead`, `PerWrite`,
  `Global`, `PerCall`, `Resolve`, `Connect`, `SendRequest`, `Await100`, `SendBody`, `RecvResponse`, `RecvBody`.
- Bodies: `send(&[u8])` has a known length, so ureq sets `Content-Length` (`src/run.rs:333-337`, `BodyMode::LengthDelimited`);
  `send_empty()` "is equivalent to `.send(&[])`" (`src/request.rs:571`). `Body::as_reader()` has no size limit
  (`src/body/mod.rs:230`); `body.with_config().limit(n)` (`:421`, `:502`) bounds `read_to_string`.
- `ureq::Error` variants (`src/error.rs`): `StatusCode`, `Http`, `BadUri(String)` (carries the URI text), `Protocol`,
  `Io`, `Timeout`, `HostNotFound`, `RedirectFailed`, `InvalidProxyUrl`, `ConnectionFailed`, `BodyExceedsLimit`,
  `TooManyRedirects`, `Tls`, ...
- Logging: ureq logs through `log`; headers are redacted at debug level, but `TRACE` is "wire level debug. NOT
  REDACTED!" (`src/lib.rs:487`).

## Size and speed measurements (2026-10-24)
Scratch crate `/home/risan/.cache/claude-tmp/claude-1000/-home-risan-projects-code-bdgtransit--claude-worktrees-rust-collector/d4c13d89-da95-4aba-91db-c200c02b47d7/scratchpad/gdc-measure/`,
the callers' release profile, `x86_64-unknown-linux-musl`, zig-cc, Rust 1.99.1. Each binary does one ureq GET; the
variants add only the named code.

| Variant | Bytes | Added to base |
|---|---|---|
| base (ureq GET, read body) | 1,680,216 | — |
| + hand-written JSON DOM parser and string writer (prototype, ~250 lines) | 1,708,888 | +28 KiB |
| + `serde_json::Value` parse and `json!` write (no derive) | 1,721,208 | +40 KiB |
| + `serde` derive structs (6 types, parse + serialize) | 1,749,880 | +68 KiB |
| + `md-5` 0.11.0 hashing a file | 1,688,408 | +8 KiB |
| + `md5` 0.8.1 hashing a file | 1,696,600 | +16 KiB |

MD5 throughput on this host, musl release, 512 MiB in memory: `md5` 0.8.1 805 MB/s, `md-5` 0.11.0 759 MB/s.
Both crates are cached in `~/.cargo/registry`; the crates.io index is reachable.

## Google API facts (fetched 2026-10-24)
- Uploads (developers.google.com/workspace/drive/api/guides/manage-uploads): initiate with
  `POST https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable` (new) or `PATCH .../files/{fileId}?uploadType=resumable`
  (update), JSON metadata with `Content-Type: application/json; charset=UTF-8`, optional `X-Upload-Content-Length`;
  answer `200` with `Location` = session URI (valid one week). Chunks: `PUT` the session URI with `Content-Length` and
  `Content-Range: bytes a-b/total`; chunks are multiples of 256 KiB except the last; `308 Resume Incomplete` with
  `Range: bytes=0-n` (no `Range` = nothing received); "Don't assume that the server received all bytes sent".
  Final: `200`/`201` with the file resource. Status query: empty `PUT` with `Content-Range: bytes */total`; `200`/`201`
  done, `308` continue, `404` session expired → restart. "For any 4xx errors (including 403) during a resumable upload,
  restart the upload."
- Query language (guides/search-files, ref-search-terms): string values in single quotes; escape `'` as `\'` and `\`
  as `\\` (`name contains 'quinn\'s paper\\essay'`).
- Device flow (identity/protocols/oauth2/limited-input-device): `POST https://oauth2.googleapis.com/device/code` with
  `client_id`, `scope`; answer `device_code`, `user_code`, `verification_url`, `expires_in`, `interval`. Poll
  `POST https://oauth2.googleapis.com/token` with `client_id`, `client_secret`, `device_code`,
  `grant_type=urn:ietf:params:oauth:grant-type:device_code`. Errors: `authorization_pending` (428), `slow_down` (403),
  `access_denied` (403), `invalid_client` (401, also "client type must be TVs and Limited Input devices"),
  `invalid_grant` (400), `expired_token` (RFC 8628). Refresh tokens are always returned for devices. Allowed Drive
  scopes: `drive.appdata`, `drive.file`. Refresh: `grant_type=refresh_token` with `client_id`, `client_secret`,
  `refresh_token`; the answer has `access_token`, `expires_in`, `scope`, `token_type` (no new refresh token as a rule).
- rclone `drive` sections are `key = value` lines: `type = drive`, `client_id`, `client_secret`, `scope`, and
  `token = {"access_token":...,"token_type":"Bearer","refresh_token":...,"expiry":...}`. An empty `client_id` means
  rclone's built-in client. An encrypted config starts with `# Encrypted rclone configuration File` and
  `RCLONE_ENCRYPT_V0:`.
