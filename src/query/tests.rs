use super::*;

#[test]
fn escapes_backslash_then_quote() {
    assert_eq!(escape(r"plain"), "plain");
    assert_eq!(escape(r"a'b"), r"a\'b");
    assert_eq!(escape(r"a\b"), r"a\\b");
    assert_eq!(escape(r"it's\here"), r"it\'s\\here");
    assert_eq!(escape(r"\'"), r"\\\'");
}

#[test]
fn clauses_quote_their_values() {
    assert_eq!(in_parents("fo'lder"), r"'fo\'lder' in parents");
    assert_eq!(name_equals(r"a\b"), r"name = 'a\\b'");
}
