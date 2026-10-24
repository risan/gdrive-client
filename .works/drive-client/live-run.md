# AC6 live run

- Date: 2026-10-24, about 15:20 WIB, on the user's PC, against real Google Drive.
- Credentials: bdgtransit's own OAuth client ("TVs and Limited Input devices", project "Google Drive Sync"), logged in
  by the user with the example's device flow (`gdrive --store ~/.config/gdrive-client/bdgtransit-token.json login`).
- Command, at head e7bbef7:
  `GDRIVE_CLIENT_LIVE=1 GDRIVE_CLIENT_TOKEN_STORE=~/.config/gdrive-client/bdgtransit-token.json cargo test --locked --test integration live:: -- --nocapture`
- Result: `test live::round_trip ... ok`, then
  `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 104 filtered out; finished in 9.16s`.
  There was no "live test skipped" line, so the test ran. It created a folder at My Drive's root, uploaded a file,
  verified its size and MD5, listed it, downloaded it, deleted the file and removed the folder.
- The token store afterwards: mode 600, 585 bytes (the refreshed access token was written atomically).
