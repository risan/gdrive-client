# A small blocking Google Drive client library for low-memory hosts

Status: ready
Type: feature
Route: bounded-change — a new library with a fixed set of operations and two known callers, but real design choices (resumable upload, token store, retries, JSON) that need a spec and a reviewed plan
Next: draft-spec

## Request
> Yes, let's create rust drive client! Maybe create a cargo packge for this so @traffic-rust can also reuse this
> library?

The user chose a new public GitHub repo (`github.com/risan/gdrive-client`, created 2026-10-24); both projects use it as
a git dependency pinned to a commit; crates.io maybe later.

Why: in CT 102 (Alpine 3.24, 128 MB RAM, shared by bdgtransit and traffic-analysis) every rclone call costs about
22–26 MB anonymous memory plus about 50 MB of file pages of its 111 MB binary (measured 2026-10-24, 7 MB file, real
Drive). Tuning rclone saves only 2–4 MB.

## Goal
A Rust library that does the few Google Drive operations the two projects need, with constant memory however large
the file, no async runtime, and a static musl build, so neither project needs rclone in the LXC.

## Callers
- bdgtransit nightly archive (`research/collector/` crate, `bdgtransit archive`): per service day, upload or replace
  `fixes-YYYY-MM-DD.sqlite.gz` (about 7 MB) in `bdgtransit/fixes`, read its MD5, list the folder's MD5s, download a
  day's file. Its own OAuth client (type "TVs and Limited Input devices") in the user's existing Google Cloud project
  "Google Drive Sync", the same project as traffic-analysis's client (the user's decision, 2026-10-24: projects are
  limited to 16 per account); logs in with the device flow in the LXC.
- traffic-analysis hourly upload (`cctv-upload`, in its own repo): move `citeureup_*.ts` files (230–257 MB, up to about
  1 GB) to `traffic-analysis/recordings/`, delete the local file only after the upload is verified by MD5 and size;
  overwrite `traffic-analysis/gaps.log` in place every hour (same file id). It starts by reusing its existing "Desktop
  app" OAuth client and refresh token (stored today in `/srv/cctv/rclone.conf`). Later it moves to its own "TVs and
  Limited Input devices" client in the same project (created by the user 2026-10-24); `drive.file` access is per
  Cloud project, so that client should still see the 1,000+ existing files (to be proven by a listing before the
  switch). The Desktop app client is never deleted: the user's PC script `sync-recordings.sh` uses it through rclone,
  which has no device-flow login (traffic-analysis, 2026-10-24).

## Acceptance criteria
- [ ] Auth: device flow login (client type "TVs and Limited Input devices", scope `drive.file`), and existing
      client id, secret and refresh token (including a one-time import from an rclone config section); access tokens
      refreshed automatically; the token store is written atomically with mode 600.
- [ ] Operations: find or create a folder path; list a folder (name, id, size, MD5) with paging; resumable upload of a
      new file and of new content for an existing file id, from a path, resuming an interrupted session within one
      run; download to a file; delete a file; file metadata with size and MD5.
- [ ] Retries with backoff on 429, 5xx and network errors, bounded by a caller-given total deadline.
- [ ] Memory does not grow with file size: peak RSS for a 1 GB upload and download against a local fake server is
      measured and is about the chunk size plus a constant.
- [ ] Tests run without network or Google (a local fake Drive server); an opt-in test against real Drive exists.
- [ ] ureq 3 with `default-features = false, features = ["rustls"]` (shared with both callers), edition 2024,
      Rust 1.99.1, builds static musl; README documents setup (Google Cloud project, client types, publishing the
      consent screen) and every operation.

## Out of scope
- Bandwidth limits, include/exclude filters, `--min-age`, server-side copy, sync or purge: callers do their own file
  selection.
- Wiring the library into bdgtransit or traffic-analysis (separate changes in those repos).
- Publishing to crates.io.

## Assumptions
- Google's device flow allows `drive.file` and always returns a refresh token (checked on 2026-10-24 at
  developers.google.com/identity/protocols/oauth2/limited-input-device).
- License MIT (the repo was created with it; the user's other public code repos have no Rust precedent).
