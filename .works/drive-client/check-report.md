# A small blocking Google Drive client library for low-memory hosts — check

Status: fixed
Diff: main (cf6e955)...d389680
Suite: `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked && cargo build --release --locked --target x86_64-unknown-linux-musl --examples` → fmt and clippy exit 0; unit `test result: ok. 51 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.32s`; integration `test result: ok. 101 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 5.31s`; doc `test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`; musl release examples build exit 0
Run: the `gdrive` example, built debug and as the static musl release binary, run against the in-process fake on 127.0.0.1 by `GDRIVE_CLIENT_EXAMPLE=<binary> cargo test --locked --test integration example:: -- --ignored` (`cli_round_trip`, `upload_with_a_wrong_md5_exits_1`, `print_peak_rss_prints_the_high_water_mark`) → `test result: ok. 3 passed; 0 failed` for both binaries; nothing contacted Google. AC6 (`live::round_trip`) is pending the user's live run (`GDRIVE_CLIENT_LIVE` unset, skipped)
Deep pass: fixed — the library holds OAuth secrets and refresh tokens (auth) and serializes token refreshes across threads (concurrency)
Verify: codex-plan-verify, 2 rounds (r1 full pass, r2 re-verify) — "the six fixes pass re-verification; update the two documented contracts before full plan-conformance sign-off" (both documentation items done in d389680 and the plan, closed by diff evidence, not re-sent)

## Fixed
- 9c02db1 — the library panicked on `Instant` overflow in three places: a token store whose
  `access_token_refresh_at` is far in the future (`Coordinator::new`, on `Client::open`), a refresh answer with a huge
  `expires_in` (`refresh_moments`), and a device code with a huge `expires_in` (`DeviceLogin::start`). The first two
  now treat the token as due at once. The third is now a `Protocol` error. `slow_down` saturates. Both new tests failed
  (panicked) before the fix.
- 269c703 — added the test `download::a_redirect_to_another_origin_carries_no_bearer_token`: a 302 to a second fake on
  another port reaches it with no `Authorization` header. It pins D7's redirect rule, which comes from ureq's
  `RedirectAuthHeaders::Never` default. No code change.
- 8a491bf — `examples/gdrive.rs` skipped the `take_store_failure` warning when a command failed (D23: "after every
  command"). It now prints it on both paths.
- cbb3082 — verify round 1 found two Major and five Minor; six fixed, one pushed back:
  `Client::refresh` registers the credentials it sends on the call, so an OAuth error echoing a refresh token another
  thread rotated is redacted (Major); `Call::know` keeps each secret once, so an upload no longer adds an access-token
  entry per chunk (Major); a device code's lifetime counts from the answer that carries it, not from the retries
  before it; a deadline while fetching the size or MD5 Drive left out of the final answer carries `confirmed_bytes`;
  a token store with `access_token` or `access_token_refresh_at` absent is `Invalid` (D8); `Call::new` cuts a budget
  above ten years to ten years, so `Options::deadline = Duration::MAX` no longer panics. Each new test fails with its
  fix removed (checked) or failed before the change.
- d389680 — re-verify round 2 asked for two documentation updates: the ten-year cap on `Options::deadline` and the
  fail-fast `LoginExpired` on `DeviceLogin::wait`; the plan's `## Changes after build` records cbb3082 and both.

## Pushed back
- Finding 4 (a device-login poll whose retry delay cannot fit is reported `LoginExpired` at once) — when the delay
  would end after the code's expiry no retry can land before the code is dead, so `LoginExpired` is accurate and
  failing fast avoids sleeping to a certain end; D10 says "`LoginExpired` at expiry" — conceded by Codex in round 2
  (code unchanged; documented on `wait`).

## Open
- Pending, not a finding: AC6 `live::round_trip` against real Drive is not run (the user runs it later).

