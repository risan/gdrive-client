//! Size baseline for `MEASUREMENTS.md`: one HTTPS GET with the same ureq setup as the library and nothing of this
//! crate, so the static musl size of `examples/gdrive` minus this one bounds the library's added size. Built, never run.

use std::io::Read;

fn main() {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent
        .get("https://www.googleapis.com/discovery/v1/apis")
        .call()
        .expect("request failed");
    let mut body = String::new();
    response
        .body_mut()
        .as_reader()
        .take(4 << 20)
        .read_to_string(&mut body)
        .expect("read failed");
    println!("{} {}", response.status(), body.len());
}
