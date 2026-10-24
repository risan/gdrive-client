use super::*;

#[test]
fn debug_hides_the_value() {
    let secret = Secret::new("hunter2");

    assert_eq!(format!("{secret:?}"), "[redacted]");
    assert_eq!(secret.expose(), "hunter2");
}

#[test]
fn redact_replaces_every_known_secret() {
    let secrets = [
        Secret::new("client-secret"),
        Secret::new("1//refresh"),
        Secret::new("ya29.access"),
    ];
    let text = "bad client-secret, bad 1//refresh and ya29.access, then client-secret again";

    let redacted = redact(text, &secrets);

    assert_eq!(
        redacted,
        "bad [redacted], bad [redacted] and [redacted], then [redacted] again"
    );
}

#[test]
fn redact_ignores_empty_secrets() {
    assert_eq!(redact("plain text", &[Secret::new("")]), "plain text");
}

#[test]
fn session_secrets_cover_the_url_and_its_upload_id() {
    let url = "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&upload_id=AEnB2Ur-s3cr3t_1&x=2";
    let secrets = session_secrets(url);

    let redacted = redact(&format!("see {url} or upload_id AEnB2Ur-s3cr3t_1 alone"), &secrets);

    assert_eq!(redacted, "see [redacted] or upload_id [redacted] alone");
}
