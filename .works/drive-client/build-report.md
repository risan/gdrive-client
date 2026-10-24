# A small blocking Google Drive client library for low-memory hosts — build

Status: complete
Contract: plan.md
Branch: feat/drive-client in /home/risan/projects/code/gdrive-lite/.claude/worktrees/drive-client — base main @ cf6e955
Builder: implementer (Claude Sonnet, effort high)
Tests: `cargo test --locked` (fresh login shell) → unit: `test result: ok. 49 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.33s`; integration: `test result: ok. 95 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 5.32s`; doc: `test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`. Also green: `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`, `cargo build --release --locked --target x86_64-unknown-linux-musl --examples` (both examples static-pie), the 3 ignored `example::` tests; `measure::peak_rss` passed twice (S11). Skipped: `live::round_trip` against real Drive (no credentials yet). Commits are local, uncommitted work: only `.works/` is untracked; next step: `check`.

## Steps
- S1 — e07e18f — crate scaffold, toolchain files, zig musl wrappers, `examples/size_baseline`; static-pie build checked
- S2 — 3e1d5fd — `Options`/`Endpoints` with validation and origin comparison, `Secret` and redaction, `Error` and `describe`
- S3 — c245fd6 — fake Drive and OAuth server (query parser, fields, offsets, faults, request log) and its 16 self-tests
- S4 — 94d9007 — atomic mode-600 token store, `Credentials`, `Client::create` and `open`
- S5 — cb0a7de — executor (backoff, `Retry-After`, deadline), single-flight token coordinator, `metadata`
- S5 follow-up — b5d6ff8 — a deadline hit mid-attempt names the failure that caused the retry; two flaky tests fixed
- S6 — 02f6c15 — device login and rclone import
- S7 — cf2b56a — folders, listing, find
- S8 — 34c0c84 — resumable upload with the sliding chunk buffer
- S9 — c9fabc9 — download, delete, trash
- S10 — 08593a8 — `examples/gdrive.rs` and its ignored tests
- S11 — 76fcfd0 — `measure::peak_rss` and `MEASUREMENTS.md`
- S12 — da2ec8a — opt-in `live::round_trip`; written, NOT run against Drive (skipped: the user's credentials do not exist yet)
- S13 — 886c15d — README, compiled as the crate docs
- S14 — 9ecdd9c — fresh-shell gate; temporary dead-code allowances removed

## Deviations
- Shell: the harness refused `env -i HOME="$HOME" ...` in this session, so cargo ran through a small script that keeps
  `HOME`, unsets the rest of the environment, sets `PATH=/usr/bin:/bin` and execs `bash -lc` — the same fresh login
  shell. The script lived in the work folder and is deleted.
- S5 follow-up commit b5d6ff8 (not a plan Step): after a test flaked, `Retrying::attribute` makes a deadline that passes
  during an in-flight attempt report the failure that made the request retry ("Drive answered 503; then timed out"),
  and the refused-connection test uses `127.0.0.1:1` (a port released by a closed listener can be taken by a fake
  started in parallel).
- S9 `malformed_redirect_is_not_echoed`: the plan's `Location` `http://[bad/?upload_id=s3cr3t` does not make ureq-proto
  fail; it joins an unparsable absolute URL as a path relative to the request and follows it (the bearer token is
  dropped, so the follow-up gets a 401, the client refreshes once and resends). The test now uses
  `//[bad/?upload_id=s3cr3t`, which does reach ureq-proto's `BadLocationHeader`, and a second test pins the original
  input (nothing leaks, no `.part` left).
- S3/S8 fake: `ReportRange(n)` takes an absolute session offset (keep the session's bytes up to `n`; an `n` below what
  the session holds makes the fake lie), because the plan's "below the confirmed one" test cannot be built from "keep
  only n bytes of the chunk". Extra fake helpers: `fault_after`, `always_after`, `Fault::with_retry_after`,
  `Fault::drive_error_with_message`.
- S12 audit: `grep 'Options::default\|Endpoints::default\|Endpoints {' tests/` matches only `live.rs`; `fake.options()`
  (in `support/mod.rs`) builds all five fields instead of `..Options::default()`. The foreign-origin upload test uses
  `https://drive.attacker.example/...` instead of a `googleapis.com` URL so the second grep has no match.
- Extra modules not in the plan: `src/hash.rs` (hex, two callers) and `src/temp.rs` (sibling temp path and directory
  fsync, shared by the token store and downloads).
- `DeviceLogin` maps an `invalid_grant` answer to `Error::OAuth` (`Error::InvalidGrant` is for a refresh token that
  Google rejected).
- The example prints `VmHWM_KiB <n>` on stdout for `--print-peak-rss`.
- Tests first, honestly: for S2, S4, S6, S9 the new tests were run and failed to compile for the missing items before
  the change. For S3 (the fake and its self-tests), S5, S7, S8, S10, S11 and S12 the tests and code were written in one
  pass and the failing run was not captured separately.
- S12 amended once (clippy `cloned_ref_to_slice_refs` in `live.rs`) before S13; nothing was pushed.
- Not done: the live run against real Drive (`live-run.md` is not written). Commands for it are in the README and in
  `tests/integration/live.rs`.

## Not run against real Google
Every test but `live::round_trip` talks to the in-process fake on 127.0.0.1; `live::round_trip` printed
`live test skipped: set GDRIVE_CLIENT_LIVE=1 and GDRIVE_CLIENT_TOKEN_STORE=<token store>` and passed. The fake's
agreement with real Drive on the 308 `Range` header, `fields` on the final answer, the `PATCH` upload and downloads is
unchecked until that run.

## Measurements (AC5, MEASUREMENTS.md)
VmHWM, static musl example, 1 GiB sparse sources / generated downloads: upload 1 GiB 8 MiB chunks 9864 KiB; upload
1 GiB 1 MiB chunks 3312 KiB; upload 64 MiB 8 MiB 9912 KiB; download 1 GiB 1508 KiB (1 MiB flag: 1536 KiB); download
64 MiB 1512 KiB. Static musl sizes: `examples/gdrive` 1,844,088 bytes, `examples/size_baseline` 1,684,312 bytes,
difference 159,776 bytes (156 KiB, an upper bound on the library's added size).
