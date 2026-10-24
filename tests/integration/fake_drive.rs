//! Self-tests of the fake: it must be as strict as Drive where clients usually go wrong, or the library's tests prove
//! nothing.

use serde_json::{Value, json};

use crate::support::{self, FakeDrive, Fault, Kind, ROOT_ID, encode, raw_agent, raw_http, request};

const KIB: usize = 1024;

struct Fixture {
    fake: FakeDrive,
    agent: ureq::Agent,
    token: String,
}

fn fixture() -> Fixture {
    let fake = FakeDrive::start();
    let token = fake.issue_access_token();

    Fixture {
        fake,
        agent: raw_agent(),
        token,
    }
}

impl Fixture {
    fn get(&self, path_and_query: &str) -> support::Reply {
        let url = format!("{}{path_and_query}", self.fake.url());

        request(&self.agent, "GET", &url, Some(&self.token), &[], &[])
    }

    fn list(&self, query: &str, rest: &str) -> support::Reply {
        self.get(&format!("/drive/v3/files?q={}{rest}", encode(query)))
    }

    fn start_session(&self, total: usize) -> String {
        let url = format!("{}/upload/drive/v3/files?uploadType=resumable", self.fake.url());
        let metadata = json!({"name": "up.bin", "parents": ["root"]}).to_string();
        let total = total.to_string();
        let reply = request(
            &self.agent,
            "POST",
            &url,
            Some(&self.token),
            &[
                ("Content-Type", "application/json; charset=UTF-8"),
                ("X-Upload-Content-Length", &total),
            ],
            metadata.as_bytes(),
        );

        assert_eq!(reply.status, 200, "{}", reply.text());
        reply.header("Location").expect("session location").to_string()
    }

    fn put(&self, session: &str, content_range: &str, body: &[u8]) -> support::Reply {
        request(
            &self.agent,
            "PUT",
            session,
            Some(&self.token),
            &[("Content-Range", content_range)],
            body,
        )
    }
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().expect("an object").keys().cloned().collect();

    keys.sort();
    keys
}

#[test]
fn query_parser_accepts_escaped_quote_and_backslash() {
    let fixture = fixture();

    fixture.fake.add_file("root", r"a'b\c.txt", b"x");

    let reply = fixture.list(r"'root' in parents and name = 'a\'b\\c.txt'", "");

    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["files"].as_array().unwrap().len(), 1);
}

#[test]
fn query_parser_rejects_bad_strings() {
    let fixture = fixture();

    for query in [r"name = 'abc", r"name = 'a\nb'", r"name = 'it's'", r"name contains 'x'"] {
        let reply = fixture.list(query, "");

        assert_eq!(reply.status, 400, "{query}: {}", reply.text());
        assert_eq!(reply.json()["error"]["errors"][0]["reason"], "invalidQuery", "{query}");
    }
}

#[test]
fn default_fields_omit_size_and_md5() {
    let fixture = fixture();
    let id = fixture.fake.add_file("root", "x.bin", b"hello");

    let listed = fixture.get("/drive/v3/files").json();
    let fetched = fixture.get(&format!("/drive/v3/files/{id}")).json();

    assert_eq!(keys(&listed["files"][0]), ["id", "kind", "mimeType", "name"]);
    assert_eq!(keys(&fetched), ["id", "kind", "mimeType", "name"]);
}

#[test]
fn fields_select_the_returned_keys() {
    let fixture = fixture();
    let id = fixture.fake.add_file("root", "x.bin", b"hello");

    let listed = fixture
        .get(&format!(
            "/drive/v3/files?fields={}",
            encode("nextPageToken,files(id,size,md5Checksum)")
        ))
        .json();
    let fetched = fixture
        .get(&format!("/drive/v3/files/{id}?fields={}", encode("id,name,trashed")))
        .json();

    assert_eq!(keys(&listed), ["files"]);
    assert_eq!(keys(&listed["files"][0]), ["id", "md5Checksum", "size"]);
    assert_eq!(listed["files"][0]["size"], "5");
    assert_eq!(listed["files"][0]["md5Checksum"], "5d41402abc4b2a76b9719d911017c592");
    assert_eq!(keys(&fetched), ["id", "name", "trashed"]);
}

#[test]
fn pages_follow_the_page_size_cap() {
    let fixture = fixture();

    fixture.fake.set_page_size(2);

    for index in 0..5 {
        fixture.fake.add_file("root", &format!("f{index}"), b"x");
    }

    let first = fixture.get("/drive/v3/files?pageSize=1000").json();
    let token = first["nextPageToken"].as_str().expect("a next page").to_string();
    let second = fixture
        .get(&format!("/drive/v3/files?pageSize=1000&pageToken={token}"))
        .json();

    assert_eq!(first["files"].as_array().unwrap().len(), 2);
    assert_eq!(second["files"].as_array().unwrap().len(), 2);
    assert_ne!(first["files"][0]["id"], second["files"][0]["id"]);
}

#[test]
fn put_without_content_length_is_411() {
    let fixture = fixture();
    let text = format!(
        "PUT /upload/drive/v3/files?uploadType=resumable&upload_id=x HTTP/1.1\r\nHost: fake\r\nAuthorization: Bearer {}\r\nContent-Range: bytes */0\r\n\r\n",
        fixture.token
    );

    let reply = raw_http(&fixture.fake.url(), &text);

    assert_eq!(reply.status, 411);
}

#[test]
fn chunk_must_start_at_the_persisted_offset() {
    let fixture = fixture();
    let session = fixture.start_session(1024 * KIB);

    let skipped = fixture.put(
        &session,
        &format!("bytes {}-{}/{}", 256 * KIB, 512 * KIB - 1, 1024 * KIB),
        &vec![1; 256 * KIB],
    );
    let first = fixture.put(
        &session,
        &format!("bytes 0-{}/{}", 256 * KIB - 1, 1024 * KIB),
        &vec![1; 256 * KIB],
    );
    let repeated = fixture.put(
        &session,
        &format!("bytes 0-{}/{}", 256 * KIB - 1, 1024 * KIB),
        &vec![1; 256 * KIB],
    );

    assert_eq!(skipped.status, 400);
    assert_eq!(first.status, 308);
    assert_eq!(repeated.status, 400);
}

#[test]
fn non_final_chunk_must_be_a_multiple_of_256_kib() {
    let fixture = fixture();
    let session = fixture.start_session(1024 * KIB);

    let odd = fixture.put(
        &session,
        &format!("bytes 0-{}/{}", 100 * KIB - 1, 1024 * KIB),
        &vec![1; 100 * KIB],
    );

    assert_eq!(odd.status, 400);
    assert_eq!(odd.json()["error"]["errors"][0]["reason"], "badContentRange");
}

#[test]
fn total_must_equal_the_announced_length() {
    let fixture = fixture();
    let session = fixture.start_session(1024 * KIB);

    let reply = fixture.put(
        &session,
        &format!("bytes 0-{}/{}", 256 * KIB - 1, 2048 * KIB),
        &vec![1; 256 * KIB],
    );

    assert_eq!(reply.status, 400);
}

#[test]
fn resume_incomplete_reports_the_persisted_range() {
    let fixture = fixture();
    let session = fixture.start_session(1024 * KIB);
    let total = 1024 * KIB;

    let nothing = fixture.put(&session, &format!("bytes */{total}"), &[]);
    let first = fixture.put(
        &session,
        &format!("bytes 0-{}/{total}", 256 * KIB - 1),
        &vec![1; 256 * KIB],
    );
    let again = fixture.put(&session, &format!("bytes */{total}"), &[]);

    assert_eq!(nothing.status, 308);
    assert_eq!(nothing.header("Range"), None);
    assert_eq!(first.status, 308);
    assert_eq!(first.header("Range"), Some("bytes=0-262143"));
    assert_eq!(again.header("Range"), Some("bytes=0-262143"));
}

#[test]
fn final_chunk_creates_the_file_with_the_sessions_fields() {
    let fixture = fixture();
    let total = 300 * KIB;
    let url = format!(
        "{}/upload/drive/v3/files?uploadType=resumable&fields={}",
        fixture.fake.url(),
        encode("id,size,md5Checksum")
    );
    let init = request(
        &fixture.agent,
        "POST",
        &url,
        Some(&fixture.token),
        &[
            ("Content-Type", "application/json; charset=UTF-8"),
            ("X-Upload-Content-Length", &total.to_string()),
        ],
        json!({"name": "n.bin"}).to_string().as_bytes(),
    );
    let session = init.header("Location").unwrap().to_string();
    let data: Vec<u8> = (0..total).map(|index| (index % 251) as u8).collect();

    fixture.put(
        &session,
        &format!("bytes 0-{}/{total}", 256 * KIB - 1),
        &data[..256 * KIB],
    );
    let done = fixture.put(
        &session,
        &format!("bytes {}-{}/{total}", 256 * KIB, total - 1),
        &data[256 * KIB..],
    );

    assert_eq!(done.status, 201);
    assert_eq!(keys(&done.json()), ["id", "md5Checksum", "size"]);

    let stored = fixture.fake.files_named("n.bin").remove(0);

    assert_eq!(stored.content.as_deref(), Some(data.as_slice()));
    assert_eq!(stored.parents, [ROOT_ID]);
}

#[test]
fn root_alias_maps_to_a_real_id() {
    let fixture = fixture();
    let url = format!("{}/drive/v3/files?fields={}", fixture.fake.url(), encode("id,parents"));
    let created = request(
        &fixture.agent,
        "POST",
        &url,
        Some(&fixture.token),
        &[("Content-Type", "application/json")],
        json!({"name": "a", "mimeType": "application/vnd.google-apps.folder", "parents": ["root"]})
            .to_string()
            .as_bytes(),
    )
    .json();
    let root = fixture
        .get(&format!("/drive/v3/files/root?fields={}", encode("id")))
        .json();

    assert_eq!(created["parents"], json!([ROOT_ID]));
    assert_eq!(root["id"], ROOT_ID);
    assert_ne!(ROOT_ID, "root");
    assert_eq!(
        fixture.list("'root' in parents", "").json()["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fixture.list(&format!("'{ROOT_ID}' in parents"), "").json()["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn bearer_token_is_required() {
    let fixture = fixture();
    let url = format!("{}/drive/v3/files", fixture.fake.url());

    let none = request(&fixture.agent, "GET", &url, None, &[], &[]);
    let wrong = request(&fixture.agent, "GET", &url, Some("not-a-token"), &[], &[]);
    let right = request(&fixture.agent, "GET", &url, Some(&fixture.token), &[], &[]);

    assert_eq!(none.status, 401);
    assert_eq!(wrong.status, 401);
    assert_eq!(right.status, 200);
}

#[test]
fn faults_fire_once_in_order() {
    let fixture = fixture();

    fixture.fake.fault(Kind::List, Fault::status(503));
    fixture.fake.fault(Kind::List, Fault::status(429).with_retry_after("7"));
    fixture
        .fake
        .fault_after(1, Kind::List, Fault::drive_error(403, "storageQuotaExceeded"));

    let statuses: Vec<u16> = (0..5).map(|_| fixture.get("/drive/v3/files").status).collect();

    assert_eq!(statuses, [503, 429, 200, 403, 200]);
    assert_eq!(fixture.fake.requests_of(Kind::List).len(), 5);
}

#[test]
fn always_faults_keep_firing() {
    let fixture = fixture();

    fixture.fake.always_after(1, Kind::Get, 503);

    let statuses: Vec<u16> = (0..3)
        .map(|_| fixture.get(&format!("/drive/v3/files/{ROOT_ID}")).status)
        .collect();

    assert_eq!(statuses, [200, 503, 503]);
}

#[test]
fn token_endpoint_checks_client_and_refresh_token() {
    let fixture = fixture();
    let (client_id, client_secret, refresh_token) = fixture.fake.credentials();
    let url = format!("{}/token", fixture.fake.url());
    let post = |form: &str| {
        request(
            &fixture.agent,
            "POST",
            &url,
            None,
            &[("Content-Type", "application/x-www-form-urlencoded")],
            form.as_bytes(),
        )
    };

    let good = post(&format!(
        "grant_type=refresh_token&client_id={client_id}&client_secret={client_secret}&refresh_token={refresh_token}"
    ));
    let bad_secret = post(&format!(
        "grant_type=refresh_token&client_id={client_id}&client_secret=nope&refresh_token={refresh_token}"
    ));
    let bad_refresh = post(&format!(
        "grant_type=refresh_token&client_id={client_id}&client_secret={client_secret}&refresh_token=nope"
    ));

    assert_eq!(good.status, 200);
    assert_eq!(good.json()["expires_in"], 3600);
    assert_eq!(bad_secret.status, 401);
    assert_eq!(bad_secret.json()["error"], "invalid_client");
    assert_eq!(bad_refresh.status, 400);
    assert_eq!(bad_refresh.json()["error"], "invalid_grant");
}
