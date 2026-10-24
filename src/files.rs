use serde_json::Value;

use crate::client::Client;
use crate::error::{Error, Operation};
use crate::http::{Method, Request, encode_component};

/// The fields every file resource is requested with: listing, `metadata` and the upload's final answer.
pub(crate) const FILE_FIELDS: &str = "id,name,mimeType,size,md5Checksum,modifiedTime,parents,trashed";

/// A file or folder as Drive describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    /// Absent for folders and for files without content, such as Google Docs.
    pub size: Option<u64>,
    /// Lowercase hex; absent where there is no content.
    pub md5: Option<String>,
    /// RFC 3339, as Drive sends it.
    pub modified_time: Option<String>,
    pub parents: Vec<String>,
    pub trashed: bool,
}

impl DriveFile {
    pub(crate) fn from_json(value: &Value) -> Result<DriveFile, String> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        let size = match &value["size"] {
            Value::Null => None,
            Value::String(size) => Some(size.parse().map_err(|_| "size is not a number".to_string())?),
            _ => return Err("size is not a string".to_string()),
        };

        Ok(DriveFile {
            id: text("id").ok_or("the file has no id")?,
            name: text("name").ok_or("the file has no name")?,
            mime_type: text("mimeType").unwrap_or_default(),
            size,
            md5: text("md5Checksum"),
            modified_time: text("modifiedTime"),
            parents: value["parents"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|parent| parent.as_str().map(str::to_string))
                .collect(),
            trashed: value["trashed"].as_bool().unwrap_or(false),
        })
    }
}

impl Client {
    /// One file's id, name, size, MD5, parents and trashed flag.
    pub fn metadata(&self, file_id: &str) -> Result<DriveFile, Error> {
        let call = self.call(Operation::Metadata, file_id);
        let url = self.drive_url(
            &format!("/drive/v3/files/{}", encode_component(file_id)),
            &[("fields", FILE_FIELDS)],
        );
        let answer = self.execute(&call, &Request::drive(Method::Get, url))?;
        let value = call.expect_json(&answer, &[200])?;

        DriveFile::from_json(&value).map_err(|detail| call.protocol_error(&detail))
    }
}
