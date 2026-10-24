//! Streaming downloads (D17): into a temp file beside the target while hashing, renamed over the target only when the
//! whole body has arrived.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use md5::{Digest, Md5};

use crate::client::Client;
use crate::error::{Error, Operation};
use crate::hash::hex;
use crate::http::{Call, Failure, Method, Request, Retrying, encode_component};
use crate::temp;

const BUFFER_SIZE: usize = 64 * 1024;
const MAX_REDIRECTS: u32 = 5;

/// What a download wrote, computed while writing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub size: u64,
    /// Lowercase hex MD5 of the bytes written.
    pub md5: String,
}

impl Client {
    /// Downloads a file's content to `local`. The bytes go to a temp file in the same directory, which replaces
    /// `local` only when the download is complete; after any failure `local` is untouched and no temp file is left. A
    /// transient failure restarts the download from byte 0.
    pub fn download(&self, file_id: &str, local: &Path) -> Result<Download, Error> {
        let call = self.call(Operation::Download, file_id);
        let local_error = |source| Error::Local {
            operation: Operation::Download,
            path: local.to_path_buf(),
            source,
        };
        let temp_path = temp::sibling(local, "part").map_err(local_error)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(local_error)?;
        let outcome = self
            .stream_media(&call, file_id, &mut file, local)
            .and_then(|download| {
                file.sync_all()
                    .and_then(|()| fs::rename(&temp_path, local))
                    .and_then(|()| temp::sync_directory(temp::directory_of(local)))
                    .map(|()| download)
                    .map_err(local_error)
            });

        if outcome.is_err() {
            let _ = fs::remove_file(&temp_path);
        }

        outcome
    }

    fn stream_media(&self, call: &Call, file_id: &str, file: &mut File, local: &Path) -> Result<Download, Error> {
        let local_error = |source| Error::Local {
            operation: Operation::Download,
            path: local.to_path_buf(),
            source,
        };
        let url = self.drive_url(
            &format!("/drive/v3/files/{}", encode_component(file_id)),
            &[("alt", "media")],
        );
        let request = Request::drive(Method::Get, url).max_redirects(MAX_REDIRECTS);
        let mut retrying = Retrying::new();

        loop {
            file.set_len(0).map_err(local_error)?;
            file.seek(SeekFrom::Start(0)).map_err(local_error)?;

            match self.download_attempt(call, &request, &mut retrying, file, local) {
                Ok(download) => return Ok(download),
                Err(Failure::Reauthenticated) => {}
                Err(Failure::Transient {
                    description,
                    retry_after,
                }) => retrying.pause(self.transport(), call, &description, retry_after)?,
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
    }

    fn download_attempt(
        &self,
        call: &Call,
        request: &Request<'_>,
        retrying: &mut Retrying,
        file: &mut File,
        local: &Path,
    ) -> Result<Download, Failure> {
        let response = self.send_authorized(call, request, retrying)?;

        if response.status().as_u16() != 200 {
            let answer = self.transport().answer(call, request, response)?;

            return Err(Failure::Fatal(call.http_error(&answer)));
        }

        let mut reader = response.into_body().into_reader();
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut md5 = Md5::new();
        let mut size = 0u64;

        loop {
            let count = reader
                .read(&mut buffer)
                .map_err(|error| self.transport().failure(call, &ureq::Error::from(error)))?;

            if count == 0 {
                return Ok(Download {
                    size,
                    md5: hex(&md5.finalize()),
                });
            }

            md5.update(&buffer[..count]);
            file.write_all(&buffer[..count]).map_err(|source| {
                Failure::Fatal(Error::Local {
                    operation: Operation::Download,
                    path: local.to_path_buf(),
                    source,
                })
            })?;
            size += count as u64;
        }
    }
}
