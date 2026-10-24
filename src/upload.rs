//! Resumable uploads (D14 to D16). The file is read once, in order, through one chunk-sized buffer that slides forward
//! as Drive confirms bytes; the MD5 consumes exactly the confirmed prefix, so a resume from any offset Drive reports
//! needs no second read.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use md5::{Digest, Md5};
use serde_json::json;

use crate::client::Client;
use crate::error::{Error, Operation};
use crate::files::{DriveFile, FILE_FIELDS};
use crate::hash::hex;
use crate::http::{Answer, Call, Failure, Method, Request, Retrying, encode_component};
use crate::options::same_origin;
use crate::secret::{Secret, session_secrets};

/// The result of an upload: the file as Drive reports it, and what the local file looked like.
#[derive(Clone, Debug)]
pub struct Upload {
    pub file: DriveFile,
    pub local_size: u64,
    /// Lowercase hex MD5 of the bytes sent, computed while reading.
    pub local_md5: String,
    /// The local file or its path changed while it was uploaded (size, modification time or inode).
    pub local_file_changed: bool,
}

impl Upload {
    /// Whether the local file did not change and Drive holds a file with the local size and MD5.
    pub fn verified(&self) -> bool {
        !self.local_file_changed
            && self.file.size == Some(self.local_size)
            && self.file.md5.as_deref() == Some(self.local_md5.as_str())
    }
}

/// What identifies a local file: if any part differs afterwards, the file or its path changed during the upload.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Identity {
        Identity {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
}

enum Destination<'a> {
    New { folder_id: &'a str, name: &'a str },
    Replace { file_id: &'a str },
}

enum SessionEnd {
    Finished {
        answer: Answer,
        md5: String,
    },
    /// Drive no longer knows the session (404 or 410).
    Gone(Answer),
}

enum Reply {
    Finished(Answer),
    Received(u64),
    Gone(Answer),
}

impl Client {
    /// Uploads `local` as a new file called `name` in the folder. The result carries Drive's id, size and MD5.
    pub fn upload_new(&self, local: &Path, folder_id: &str, name: &str) -> Result<Upload, Error> {
        self.upload(local, Destination::New { folder_id, name })
    }

    /// Uploads `local` as the new content of an existing file, keeping its id and metadata.
    pub fn upload_replace(&self, local: &Path, file_id: &str) -> Result<Upload, Error> {
        self.upload(local, Destination::Replace { file_id })
    }

    fn upload(&self, local: &Path, destination: Destination<'_>) -> Result<Upload, Error> {
        let call = self.call(Operation::Upload, local.display().to_string());
        let local_error = |source: io::Error| Error::Local {
            operation: Operation::Upload,
            path: local.to_path_buf(),
            source,
        };
        let mut source = File::open(local).map_err(local_error)?;
        let before = Identity::of(&source.metadata().map_err(local_error)?);
        let total = before.length;
        let mut restarted = false;

        let (answer, local_md5) = loop {
            match self.run_session(&call, &mut source, local, total, &destination)? {
                SessionEnd::Finished { answer, md5 } => break (answer, md5),
                SessionEnd::Gone(_) if !restarted => {
                    restarted = true;
                    source.seek(SeekFrom::Start(0)).map_err(local_error)?;
                }
                SessionEnd::Gone(answer) => return Err(call.http_error(&answer)),
            }
        };

        let value = call.expect_json(&answer, &[200, 201])?;
        let mut file = DriveFile::from_json(&value).map_err(|detail| call.protocol_error(&detail))?;

        if file.size.is_none() || file.md5.is_none() {
            file = self.fetch_metadata(&call.derive(Operation::Metadata, file.id.clone()), &file.id)?;
        }

        Ok(Upload {
            file,
            local_size: total,
            local_md5,
            local_file_changed: local_file_changed(&source, local, before),
        })
    }

    fn run_session(
        &self,
        call: &Call,
        source: &mut File,
        local: &Path,
        total: u64,
        destination: &Destination<'_>,
    ) -> Result<SessionEnd, Error> {
        let capacity = (self.chunk_size() as u64).min(total) as usize;
        let mut window = Window::new(capacity);

        self.drive_session(call, source, local, total, destination, &mut window)
            .map_err(|error| error.with_confirmed_bytes(window.confirmed))
    }

    fn drive_session(
        &self,
        call: &Call,
        source: &mut File,
        local: &Path,
        total: u64,
        destination: &Destination<'_>,
        window: &mut Window,
    ) -> Result<SessionEnd, Error> {
        let session_url = self.initiate(call, total, destination)?;

        call.know_all(session_secrets(session_url.expose()));

        loop {
            let target = (self.chunk_size() as u64).min(total - window.confirmed) as usize;
            let filled = window.fill_to(target, source).map_err(|source| Error::Local {
                operation: Operation::Upload,
                path: local.to_path_buf(),
                source,
            })?;

            if !filled {
                return Err(Error::LocalFileChanged {
                    path: local.to_path_buf(),
                });
            }

            match self.send_chunk(call, &session_url, window, total)? {
                Reply::Finished(answer) => {
                    if window.confirmed + window.len as u64 != total {
                        return Err(call.protocol_error("Drive finished the upload before every byte was sent"));
                    }

                    window
                        .confirm(total)
                        .map_err(|problem| call.protocol_error(&problem.to_string()))?;

                    return Ok(SessionEnd::Finished {
                        answer,
                        md5: window.digest(),
                    });
                }
                Reply::Gone(answer) => return Ok(SessionEnd::Gone(answer)),
                Reply::Received(received) => {
                    window
                        .confirm(received)
                        .map_err(|problem| call.protocol_error(&problem.to_string()))?;

                    if window.confirmed == total {
                        return Err(call.protocol_error("Drive reports every byte received but did not finish"));
                    }
                }
            }
        }
    }

    /// Starts a resumable session and returns its URL, after checking that it is on the Drive endpoint's origin:
    /// chunk requests carry the bearer token.
    fn initiate(&self, call: &Call, total: u64, destination: &Destination<'_>) -> Result<Secret, Error> {
        let parameters = [("uploadType", "resumable"), ("fields", FILE_FIELDS)];
        let length = total.to_string();
        let body;
        let request = match destination {
            Destination::New { folder_id, name } => {
                body = json!({"name": name, "parents": [folder_id]}).to_string();
                Request::drive(Method::Post, self.drive_url("/upload/drive/v3/files", &parameters)).json_body(&body)
            }
            Destination::Replace { file_id } => Request::drive(
                Method::Patch,
                self.drive_url(
                    &format!("/upload/drive/v3/files/{}", encode_component(file_id)),
                    &parameters,
                ),
            ),
        }
        .header("X-Upload-Content-Length", length);
        let answer = self.execute(call, &request)?;

        if answer.status != 200 {
            return Err(call.http_error(&answer));
        }

        let location = answer
            .location
            .ok_or_else(|| call.protocol_error("the answer has no session URL"))?;

        if !same_origin(&location, self.drive_endpoint()) {
            return Err(call.protocol_error("the session URL is not on the Drive endpoint's origin"));
        }

        Ok(Secret::new(location))
    }

    /// Sends the window's bytes. After a retryable failure (or a token refresh) it asks Drive how much it has instead
    /// of resending blindly.
    fn send_chunk(&self, call: &Call, session_url: &Secret, window: &Window, total: u64) -> Result<Reply, Error> {
        let chunk_request = Request::drive(Method::Put, session_url.expose().to_string())
            .header("Content-Range", content_range(window.confirmed, window.len, total))
            .body(window.pending());
        let status_query = Request::drive(Method::Put, session_url.expose().to_string())
            .header("Content-Range", format!("bytes */{total}"));
        let mut retrying = Retrying::new();
        let mut querying = false;

        loop {
            let request = if querying { &status_query } else { &chunk_request };

            match self.attempt(call, request, &mut retrying) {
                Ok(answer) => return interpret(call, answer),
                Err(Failure::Reauthenticated) => querying = true,
                Err(Failure::Transient {
                    description,
                    retry_after,
                }) => {
                    retrying.pause(self.transport(), call, &description, retry_after)?;
                    querying = true;
                }
                Err(Failure::Fatal(error)) => return Err(retrying.attribute(error)),
            }
        }
    }
}

fn interpret(call: &Call, answer: Answer) -> Result<Reply, Error> {
    match answer.status {
        200 | 201 => Ok(Reply::Finished(answer)),
        308 => parse_range(answer.range.as_deref())
            .map(Reply::Received)
            .map_err(|detail| call.protocol_error(&detail)),
        404 | 410 => Ok(Reply::Gone(answer)),
        _ => Err(call.http_error(&answer)),
    }
}

fn content_range(first: u64, length: usize, total: u64) -> String {
    if length == 0 {
        format!("bytes */{total}")
    } else {
        format!("bytes {first}-{}/{total}", first + length as u64 - 1)
    }
}

/// How many bytes Drive holds: `Range: bytes=0-<n>` is `n + 1`, no `Range` is 0.
fn parse_range(header: Option<&str>) -> Result<u64, String> {
    let Some(header) = header else {
        return Ok(0);
    };
    let malformed = || "Drive sent a malformed Range header".to_string();
    let last = header
        .strip_prefix("bytes=0-")
        .and_then(|rest| rest.parse::<u64>().ok())
        .ok_or_else(malformed)?;

    last.checked_add(1).ok_or_else(malformed)
}

fn local_file_changed(source: &File, path: &Path, before: Identity) -> bool {
    let unchanged =
        |metadata: io::Result<fs::Metadata>| metadata.is_ok_and(|metadata| Identity::of(&metadata) == before);

    !(unchanged(source.metadata()) && unchanged(fs::metadata(path)))
}

/// The bytes sent and not yet confirmed, in a buffer allocated once.
struct Window {
    buffer: Vec<u8>,
    len: usize,
    /// Bytes Drive holds; the MD5 has consumed exactly these.
    confirmed: u64,
    md5: Md5,
}

#[derive(Debug)]
struct ConfirmError {
    received: u64,
    confirmed: u64,
    sent_end: u64,
}

impl fmt::Display for ConfirmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Drive reports {} bytes received, outside the {} it had confirmed and the {} sent",
            self.received, self.confirmed, self.sent_end
        )
    }
}

impl Window {
    fn new(capacity: usize) -> Window {
        Window {
            buffer: vec![0; capacity],
            len: 0,
            confirmed: 0,
            md5: Md5::new(),
        }
    }

    fn pending(&self) -> &[u8] {
        &self.buffer[..self.len]
    }

    /// Reads from the source until the window holds `target` bytes; false when the source ends first.
    fn fill_to(&mut self, target: usize, source: &mut impl Read) -> io::Result<bool> {
        while self.len < target {
            match source.read(&mut self.buffer[self.len..target]) {
                Ok(0) => return Ok(false),
                Ok(count) => self.len += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }

        Ok(true)
    }

    /// Drive holds `received` bytes: feed the MD5 the newly confirmed ones and slide the unconfirmed tail to the
    /// front. Drive may confirm anywhere in `[confirmed, confirmed + len]`; anything else is a protocol violation.
    fn confirm(&mut self, received: u64) -> Result<(), ConfirmError> {
        let sent_end = self.confirmed + self.len as u64;

        if received < self.confirmed || received > sent_end {
            return Err(ConfirmError {
                received,
                confirmed: self.confirmed,
                sent_end,
            });
        }

        let advance = (received - self.confirmed) as usize;

        self.md5.update(&self.buffer[..advance]);
        self.buffer.copy_within(advance..self.len, 0);
        self.len -= advance;
        self.confirmed = received;
        Ok(())
    }

    fn digest(&self) -> String {
        hex(&self.md5.clone().finalize())
    }
}

#[cfg(test)]
mod tests;
