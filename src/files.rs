use serde_json::{Value, json};

use crate::client::Client;
use crate::error::{Error, Operation};
use crate::http::{Call, Failure, Method, Request, Retrying, encode_component};
use crate::query;

/// The fields every file resource is requested with: listing, `metadata` and the upload's final answer.
pub(crate) const FILE_FIELDS: &str = "id,name,mimeType,size,md5Checksum,modifiedTime,parents,trashed";
const PAGE_SIZE: &str = "1000";
const ROOT_ALIAS: &str = "root";

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
        self.fetch_metadata(&self.call(Operation::Metadata, file_id), file_id)
    }

    pub(crate) fn fetch_metadata(&self, call: &Call, file_id: &str) -> Result<DriveFile, Error> {
        let url = self.drive_url(
            &format!("/drive/v3/files/{}", encode_component(file_id)),
            &[("fields", FILE_FIELDS)],
        );
        let answer = self.execute(call, &Request::drive(Method::Get, url))?;
        let value = call.expect_json(&answer, &[200])?;

        DriveFile::from_json(&value).map_err(|detail| call.protocol_error(&detail))
    }

    /// The id of the folder at `path` below My Drive's root (`a/b/c`). Trashed folders do not count; two folders with
    /// one name under one parent are a `DuplicateFolder` error.
    pub fn folder_id(&self, path: &str) -> Result<String, Error> {
        self.walk_folders(path, false)
    }

    /// Like [`folder_id`](Client::folder_id), creating the folders that are missing.
    pub fn ensure_folder(&self, path: &str) -> Result<String, Error> {
        self.walk_folders(path, true)
    }

    /// The files in a folder that are not trashed and not folders, over every page.
    pub fn list_files(&self, folder_id: &str) -> Result<Vec<DriveFile>, Error> {
        let call = self.call(Operation::ListFiles, folder_id);
        let search = query::all(&[
            query::in_parents(folder_id),
            query::not_trashed(),
            query::is_not_folder(),
        ]);

        self.search(&call, &search)
    }

    /// Every file in the folder whose name is exactly `name`; the caller decides what duplicates mean.
    pub fn find_files(&self, folder_id: &str, name: &str) -> Result<Vec<DriveFile>, Error> {
        let call = self.call(Operation::FindFiles, folder_id);
        let search = query::all(&[
            query::in_parents(folder_id),
            query::name_equals(name),
            query::not_trashed(),
            query::is_not_folder(),
        ]);
        let mut files = self.search(&call, &search)?;

        // Drive does not document `name =` as an exact match.
        files.retain(|file| file.name == name);
        Ok(files)
    }

    /// Runs a `files.list` query and follows `nextPageToken` to the end.
    fn search(&self, call: &Call, search: &str) -> Result<Vec<DriveFile>, Error> {
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut parameters = vec![("q", search), ("pageSize", PAGE_SIZE), ("fields", fields.as_str())];

            if let Some(token) = &page_token {
                parameters.push(("pageToken", token.as_str()));
            }

            let url = self.drive_url("/drive/v3/files", &parameters);
            let answer = self.execute(call, &Request::drive(Method::Get, url))?;
            let page = call.expect_json(&answer, &[200])?;

            for entry in page["files"].as_array().into_iter().flatten() {
                files.push(DriveFile::from_json(entry).map_err(|detail| call.protocol_error(&detail))?);
            }

            match page["nextPageToken"].as_str() {
                Some(token) => page_token = Some(token.to_string()),
                None => return Ok(files),
            }
        }
    }

    fn find_folders(&self, call: &Call, parent_id: &str, name: &str) -> Result<Vec<DriveFile>, Error> {
        let search = query::all(&[
            query::in_parents(parent_id),
            query::name_equals(name),
            query::is_folder(),
            query::not_trashed(),
        ]);
        let mut folders = self.search(call, &search)?;

        folders.retain(|folder| folder.name == name);
        Ok(folders)
    }

    fn walk_folders(&self, path: &str, create: bool) -> Result<String, Error> {
        let segments: Vec<&str> = path.split('/').collect();

        if segments.iter().any(|segment| segment.is_empty()) {
            return Err(Error::InvalidInput {
                detail: "a folder path is non-empty segments separated by single slashes".to_string(),
            });
        }

        let call = self.call(Operation::ResolveFolder, path);
        let mut parent_id = ROOT_ALIAS.to_string();

        for (index, segment) in segments.iter().enumerate() {
            let path_so_far = segments[..=index].join("/");
            let mut matches = self.find_folders(&call, &parent_id, segment)?;

            parent_id = match matches.len() {
                0 if create => self.create_folder(&call, &parent_id, segment, &path_so_far)?,
                0 => return Err(Error::FolderNotFound { path: path_so_far }),
                1 => matches.remove(0).id,
                _ => return Err(Error::DuplicateFolder { path: path_so_far }),
            };
        }

        Ok(parent_id)
    }

    /// Creates a folder with one POST per attempt. After a retryable failure the lookup runs again before the next
    /// POST, so a create that Drive applied but whose answer was lost does not leave a duplicate.
    fn create_folder(&self, call: &Call, parent_id: &str, name: &str, path_so_far: &str) -> Result<String, Error> {
        let call = call.derive(Operation::CreateFolder, path_so_far);
        let body = json!({"name": name, "mimeType": query::FOLDER_MIME_TYPE, "parents": [parent_id]}).to_string();
        let url = self.drive_url("/drive/v3/files", &[("fields", "id")]);
        let request = Request::drive(Method::Post, url).json_body(&body);
        let mut retrying = Retrying::new();
        let mut look_first = false;

        loop {
            if look_first {
                let mut existing = self.find_folders(&call, parent_id, name)?;

                match existing.len() {
                    0 => {}
                    1 => return Ok(existing.remove(0).id),
                    _ => {
                        return Err(Error::DuplicateFolder {
                            path: path_so_far.to_string(),
                        });
                    }
                }
            }

            match self.attempt(&call, &request, &mut retrying) {
                Ok(answer) => {
                    let value = call.expect_json(&answer, &[200])?;

                    return value["id"]
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| call.protocol_error("the created folder has no id"));
                }
                Err(Failure::Reauthenticated) => look_first = false,
                Err(Failure::Transient {
                    description,
                    retry_after,
                }) => {
                    retrying.pause(self.transport(), &call, &description, retry_after)?;
                    look_first = true;
                }
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
    }
}
