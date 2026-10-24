# Measurements

Peak memory of a 1 GiB upload and download, and the static musl size, as asked for by the spec (R9, AC5). Both come from
the example program (`examples/gdrive.rs`), which is the library plus a small command line.

- Date: 2026-10-24
- Host: AMD Ryzen 9 5900X (12 cores), 20 GB RAM, Linux 6.18 (WSL2), x86_64
- Toolchain: Rust 1.99.1, Zig 0.17.0 as the C compiler for ring (`tools/zig-cc`), target `x86_64-unknown-linux-musl`,
  release profile (`opt-level = "s"`, fat LTO, `panic = "abort"`, stripped)
- Server: the in-process fake Drive of the tests, on 127.0.0.1, with `store_content(false)` (uploads are hashed, not
  kept); the example runs as a child process, so the fake's own memory is not in the numbers

## Commands

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl --examples

GDRIVE_CLIENT_EXAMPLE=target/x86_64-unknown-linux-musl/release/examples/gdrive \
    cargo test --release --locked --test integration measure:: -- --ignored --nocapture

ls -l target/x86_64-unknown-linux-musl/release/examples/{gdrive,size_baseline}
```

## Peak resident memory

`VmHWM` from `/proc/self/status` at the end of the example's run (`--print-peak-rss`). The test makes sparse 64 MiB and
1 GiB sources for the uploads and serves generated 64 MiB and 1 GiB files for the downloads.

| operation | chunk size | peak RSS (VmHWM) |
|---|---|---|
| upload 1 GiB | 8 MiB | 9864 KiB |
| upload 1 GiB | 1 MiB | 3312 KiB |
| upload 64 MiB | 8 MiB | 9912 KiB |
| download 1 GiB | not used by downloads | 1508 KiB |
| download 1 GiB | 1 MiB (same as above: not used) | 1536 KiB |
| download 64 MiB | not used by downloads | 1512 KiB |

What the numbers say:

- Memory does not depend on file size: a 1 GiB and a 64 MiB upload peak within 50 KiB of each other, and so do the two
  downloads (the test asserts a gap of at most 1 MiB).
- For uploads the chunk buffer is what scales: 8 MiB chunks cost about 6.4 MiB more than 1 MiB chunks (the test asserts
  a gap of 5 to 9 MiB). What is left with 1 MiB chunks, about 2.2 MiB beyond the buffer, is the binary, the TLS state
  and the stack.
- Downloads stream through a fixed 64 KiB buffer, so `chunk_size` does not apply to them; the 1 MiB row is there only to
  show it. An earlier run of the same test gave values within 120 KiB of these.

The numbers leave out the page cache. `VmHWM` counts the pages mapped into the process (heap, stack, binary), and the
page cache that `read` and `write` go through is not part of the process. The upload sources are sparse files
(`File::set_len`, no data blocks), and each download goes to a temporary directory that is deleted before the next one.

## Static musl size

| binary | bytes |
|---|---|
| `examples/gdrive` (library, ureq with rustls and ring, serde_json, md-5, and the example's command line) | 1,844,088 |
| `examples/size_baseline` (the same ureq setup and one HTTPS GET, nothing of this crate) | 1,684,312 |
| difference | 159,776 (156 KiB) |

The difference is an upper bound on what the library adds: it includes the example's own code (argument parsing,
printing). Both files are `static-pie linked` per `file(1)`. For scale, the design measured `serde_json::Value` at
+40 KiB and `md-5` at +8 KiB on top of the same baseline.
