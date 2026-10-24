//! Drive's `q` search language: string values are single-quoted, with `\` written `\\` and `'` written `\'`.

pub(crate) const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

pub(crate) fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

pub(crate) fn in_parents(folder_id: &str) -> String {
    format!("'{}' in parents", escape(folder_id))
}

pub(crate) fn name_equals(name: &str) -> String {
    format!("name = '{}'", escape(name))
}

pub(crate) fn is_folder() -> String {
    format!("mimeType = '{FOLDER_MIME_TYPE}'")
}

pub(crate) fn is_not_folder() -> String {
    format!("mimeType != '{FOLDER_MIME_TYPE}'")
}

pub(crate) fn not_trashed() -> String {
    "trashed = false".to_string()
}

pub(crate) fn all(clauses: &[String]) -> String {
    clauses.join(" and ")
}

#[cfg(test)]
mod tests;
