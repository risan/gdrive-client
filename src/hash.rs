/// Lowercase hex, the form Drive reports MD5 checksums in.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
