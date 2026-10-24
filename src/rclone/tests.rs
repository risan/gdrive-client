use std::fs;

use tempfile::TempDir;

use super::*;

const CONFIG: &str = r#"# comment
[other]
type = s3
client_id = not-this-one

; another comment
[gdrive]
type = drive
client_id = the-client-id
client_secret = the-client-secret
scope = drive.file
token = {"access_token":"ya29.old","token_type":"Bearer","refresh_token":"1//the-refresh-token","expiry":"2026-01-01T00:00:00Z"}
"#;

fn import(content: &str, section: &str) -> Result<Credentials, Error> {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("rclone.conf");

    fs::write(&path, content).unwrap();
    Credentials::from_rclone_config(&path, section)
}

fn problem_of(result: Result<Credentials, Error>) -> String {
    match result {
        Err(Error::RcloneConfig { problem, .. }) => problem,
        other => panic!("expected an RcloneConfig error, got {other:?}"),
    }
}

#[test]
fn imports_the_named_drive_section() {
    let credentials = import(CONFIG, "gdrive").unwrap();

    assert_eq!(credentials.client_id(), "the-client-id");
    assert_eq!(credentials.client_secret.expose(), "the-client-secret");
    assert_eq!(credentials.refresh_token.expose(), "1//the-refresh-token");
}

#[test]
fn imports_values_that_contain_equals_signs() {
    let config = "[d]\ntype=drive\nclient_id = a=b\nclient_secret=c==\ntoken={\"refresh_token\":\"r=1\"}\n";
    let credentials = import(config, "d").unwrap();

    assert_eq!(credentials.client_id(), "a=b");
    assert_eq!(credentials.client_secret.expose(), "c==");
    assert_eq!(credentials.refresh_token.expose(), "r=1");
}

#[test]
fn rejects_a_missing_section() {
    assert!(problem_of(import(CONFIG, "nope")).contains("not found"));
}

#[test]
fn rejects_a_section_that_is_not_drive() {
    assert!(problem_of(import(CONFIG, "other")).contains("not of type drive"));
}

#[test]
fn rejects_rclones_builtin_client() {
    let config = CONFIG.replace("client_id = the-client-id", "client_id =");

    assert!(problem_of(import(&config, "gdrive")).contains("built-in"));
}

#[test]
fn rejects_a_token_without_refresh_token() {
    let without = CONFIG.replace(",\"refresh_token\":\"1//the-refresh-token\"", "");
    let not_json = CONFIG.replace("token = {", "token = [");
    let absent: String = CONFIG
        .lines()
        .filter(|line| !line.starts_with("token"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(problem_of(import(&without, "gdrive")).contains("refresh_token"));
    assert!(problem_of(import(&not_json, "gdrive")).contains("token"));
    assert!(problem_of(import(&absent, "gdrive")).contains("token"));
}

#[test]
fn rejects_an_encrypted_config() {
    let encrypted = "# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nc2VjcmV0\n";

    assert!(problem_of(import(encrypted, "gdrive")).contains("encrypted"));
}

#[test]
fn errors_do_not_echo_values() {
    let broken = [
        CONFIG.replace("type = drive", "type = secret-type-value"),
        CONFIG.replace("client_id = the-client-id", "client_id ="),
        CONFIG.replace("\"refresh_token\":\"1//the-refresh-token\"", "\"refresh_token\":\"\""),
    ];

    for config in broken {
        let error = import(&config, "gdrive").unwrap_err();
        let text = format!("{error} {error:?}");

        for value in [
            "the-client-secret",
            "1//the-refresh-token",
            "ya29.old",
            "secret-type-value",
        ] {
            assert!(!text.contains(value), "{value} leaked into {text}");
        }
    }
}

#[test]
fn a_missing_file_is_reported_without_a_panic() {
    let error = Credentials::from_rclone_config(Path::new("/nonexistent/rclone.conf"), "gdrive").unwrap_err();

    assert!(matches!(error, Error::RcloneConfig { .. }), "{error:?}");
}
