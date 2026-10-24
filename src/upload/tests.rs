use std::io::Cursor;

use md5::{Digest, Md5};

use super::*;

fn hex(bytes: &[u8]) -> String {
    Md5::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn data(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

#[test]
fn parses_the_range_header() {
    assert_eq!(parse_range(Some("bytes=0-42")), Ok(43));
    assert_eq!(parse_range(Some("bytes=0-262143")), Ok(262_144));
    assert_eq!(parse_range(None), Ok(0));

    for malformed in [
        "",
        "bytes=1-42",
        "bytes=0-",
        "bytes=a-b",
        "0-42",
        "bytes=0-42-1",
        "bytes 0-42",
    ] {
        assert!(parse_range(Some(malformed)).is_err(), "{malformed}");
    }
}

#[test]
fn confirmation_moves_the_window() {
    let bytes = data(1000);
    let mut source = Cursor::new(bytes.clone());
    let mut window = Window::new(400);

    assert!(window.fill_to(400, &mut source).unwrap());
    assert_eq!(window.pending(), &bytes[..400]);

    // Drive confirms 150 of the 400 bytes: the MD5 sees exactly those, the tail moves to the front.
    window.confirm(150).unwrap();

    assert_eq!(window.confirmed, 150);
    assert_eq!(window.pending(), &bytes[150..400]);
    assert_eq!(window.digest(), hex(&bytes[..150]));

    // The next refill reads only new bytes.
    assert!(window.fill_to(400, &mut source).unwrap());
    assert_eq!(window.pending(), &bytes[150..550]);

    window.confirm(550).unwrap();

    assert_eq!(window.confirmed, 550);
    assert!(window.pending().is_empty());
    assert_eq!(window.digest(), hex(&bytes[..550]));
}

#[test]
fn offsets_outside_the_window_are_rejected() {
    let bytes = data(1000);
    let mut source = Cursor::new(bytes);
    let mut window = Window::new(400);

    window.fill_to(400, &mut source).unwrap();
    window.confirm(100).unwrap();
    window.fill_to(400, &mut source).unwrap();

    // The window now holds [100, 500).
    let below = window.confirm(99).unwrap_err();
    let above = window.confirm(501).unwrap_err();

    assert_eq!((below.received, below.confirmed, below.sent_end), (99, 100, 500));
    assert_eq!((above.received, above.confirmed, above.sent_end), (501, 100, 500));
    assert!(below.to_string().contains("99") && below.to_string().contains("100"));
    assert!(window.confirm(100).is_ok(), "no progress is allowed");
    assert!(window.confirm(500).is_ok());
}

#[test]
fn a_source_that_ends_early_is_reported() {
    let mut source = Cursor::new(data(100));
    let mut window = Window::new(400);

    assert!(!window.fill_to(400, &mut source).unwrap());
}
