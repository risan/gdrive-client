//! An in-process fake of Drive v3 and Google's OAuth endpoints on 127.0.0.1, strict where clients usually go wrong:
//! query escaping, `fields`, `Content-Range` offsets and 256 KiB chunks.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use md5::{Digest, Md5};
use serde_json::{Map, Value, json};

use super::drive_query::{self, Clause};
use super::wire::{self, Filler, Payload, ReadError, Request, Response};

pub const ROOT_ID: &str = "fake-root-0001";
pub const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const CHUNK_UNIT: u64 = 256 * 1024;
const DEFAULT_FILE_FIELDS: [&str; 4] = ["kind", "id", "name", "mimeType"];
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Token,
    DeviceCode,
    DevicePoll,
    List,
    Get,
    Create,
    Patch,
    Delete,
    UploadInit,
    ChunkPut,
    StatusQuery,
    Download,
    Other,
}

pub enum Fault {
    Status {
        code: u16,
        retry_after: Option<String>,
        body: String,
    },
    CloseBeforeResponse,
    /// Do the work, then close without answering.
    ApplyThenClose,
    /// Keep the first `persist` bytes of a chunk, then close.
    DropAfterBody {
        persist: u64,
    },
    /// Keep the session's bytes up to the absolute offset `n` and answer 308 for `n`. An offset below what the session
    /// already holds makes the fake lie: it reports `n` and keeps what it had.
    ReportRange(u64),
    SessionGone(u16),
    /// Answer an upload initiation with this `Location`.
    ForeignLocation(String),
    Redirect {
        status: u16,
        location: String,
    },
    /// Send headers with the full length and only `n` bytes of the body.
    TruncateBody(u64),
    Unauthorized,
    /// Answer a file resource with Drive's default fields only.
    OmitFields,
    CorruptMd5,
    /// Runs before the answer, without the fake's state lock.
    Hook(Box<dyn FnOnce() + Send>),
}

impl Fault {
    pub fn status(code: u16) -> Fault {
        let reason = match code {
            401 => "authError",
            403 => "forbidden",
            404 => "notFound",
            429 => "rateLimitExceeded",
            400 => "badRequest",
            _ => "backendError",
        };

        Fault::drive_error(code, reason)
    }

    pub fn drive_error(code: u16, reason: &str) -> Fault {
        Fault::Status {
            code,
            retry_after: None,
            body: drive_error_body(code, reason, &format!("fake {reason}")),
        }
    }

    /// A Drive error body whose message is the given text, for tests that make Google echo a secret.
    pub fn drive_error_with_message(code: u16, reason: &str, message: &str) -> Fault {
        Fault::Status {
            code,
            retry_after: None,
            body: drive_error_body(code, reason, message),
        }
    }

    pub fn with_retry_after(self, value: &str) -> Fault {
        match self {
            Fault::Status { code, body, .. } => Fault::Status {
                code,
                retry_after: Some(value.to_string()),
                body,
            },
            other => other,
        }
    }

    pub fn status_with_body(code: u16, body: &str) -> Fault {
        Fault::Status {
            code,
            retry_after: None,
            body: body.to_string(),
        }
    }

    pub fn hook(action: impl FnOnce() + Send + 'static) -> Fault {
        Fault::Hook(Box::new(action))
    }
}

fn drive_error_body(code: u16, reason: &str, message: &str) -> String {
    json!({"error": {"code": code, "message": message, "errors": [{"message": message, "domain": "global", "reason": reason}]}})
        .to_string()
}

#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub kind: Kind,
    pub method: String,
    pub path: String,
    pub raw_query: String,
    pub content_range: Option<String>,
    pub authorization: Option<String>,
    pub body_len: usize,
    /// The body when it is small enough to keep (64 KiB), else empty.
    pub body: Vec<u8>,
    pub at: Instant,
}

impl RequestRecord {
    pub fn param(&self, name: &str) -> Option<String> {
        wire::parse_query(&self.raw_query)?
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn form_value(&self, name: &str) -> Option<String> {
        wire::parse_query(&String::from_utf8_lossy(&self.body))?
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Debug)]
pub struct StoredFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub parents: Vec<String>,
    pub trashed: bool,
    pub size: Option<u64>,
    pub md5: Option<String>,
    /// The bytes, unless the content was generated or `store_content(false)` was set.
    pub content: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl Default for DeviceConfig {
    fn default() -> DeviceConfig {
        DeviceConfig {
            device_code: "fake-device-code".to_string(),
            user_code: "ABCD-EFGH".to_string(),
            verification_url: "https://www.google.com/device".to_string(),
            expires_in: 1800,
            interval: 5,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum DevicePoll {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Approved,
}

#[derive(Clone)]
enum Content {
    Bytes(Arc<Vec<u8>>),
    Generated,
    /// Hashed while uploaded and not kept (`store_content(false)`).
    Hashed,
}

#[derive(Clone)]
struct FileEntry {
    id: String,
    name: String,
    mime_type: String,
    parents: Vec<String>,
    trashed: bool,
    modified_time: String,
    content: Option<(Content, u64, String)>,
}

enum Target {
    New { name: String, parents: Vec<String> },
    Replace { id: String },
}

struct Session {
    target: Target,
    fields: Option<String>,
    total: Option<u64>,
    persisted: u64,
    bytes: Vec<u8>,
    hasher: Md5,
    completed: Option<(String, u16)>,
}

struct FaultEntry {
    kind: Kind,
    fault: Option<Fault>,
    always: Option<(u16, String)>,
    skip: usize,
}

#[derive(Default)]
struct Modifier {
    drop_after: Option<u64>,
    report_range: Option<u64>,
    foreign_location: Option<String>,
    truncate_body: Option<u64>,
    omit_fields: bool,
    corrupt_md5: bool,
}

enum Action {
    Respond(Response),
    Close,
}

struct State {
    files: BTreeMap<String, FileEntry>,
    sessions: HashMap<String, Session>,
    faults: Vec<FaultEntry>,
    log: Vec<RequestRecord>,
    refresh_tokens: HashSet<String>,
    access_tokens: HashSet<String>,
    device: DeviceConfig,
    device_script: VecDeque<DevicePoll>,
    device_refresh_token: String,
    client_id: String,
    client_secret: String,
    page_size: usize,
    store_content: bool,
    expires_in: u64,
    rotate: bool,
    counter: u64,
}

struct Shared {
    state: Mutex<State>,
    base_url: String,
}

pub struct FakeDrive {
    shared: Arc<Shared>,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
}

impl FakeDrive {
    pub fn start() -> FakeDrive {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake");
        let address = listener.local_addr().expect("fake address");
        let mut files = BTreeMap::new();

        files.insert(
            ROOT_ID.to_string(),
            FileEntry {
                id: ROOT_ID.to_string(),
                name: "My Drive".to_string(),
                mime_type: FOLDER_MIME.to_string(),
                parents: Vec::new(),
                trashed: false,
                modified_time: modified_time(0),
                content: None,
            },
        );

        let mut refresh_tokens = HashSet::new();

        refresh_tokens.insert("fake-refresh-token-1".to_string());

        let state = State {
            files,
            sessions: HashMap::new(),
            faults: Vec::new(),
            log: Vec::new(),
            refresh_tokens,
            access_tokens: HashSet::new(),
            device: DeviceConfig::default(),
            device_script: VecDeque::new(),
            device_refresh_token: "fake-device-refresh-token".to_string(),
            client_id: "fake-client-id".to_string(),
            client_secret: "fake-client-secret".to_string(),
            page_size: 1000,
            store_content: true,
            expires_in: 3600,
            rotate: false,
            counter: 0,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            base_url: format!("http://{address}"),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);

            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }

                    if let Ok(stream) = stream {
                        let shared = Arc::clone(&shared);

                        std::thread::spawn(move || serve(&shared, stream));
                    }
                }
            })
        };

        FakeDrive {
            shared,
            address,
            stop,
            acceptor: Some(acceptor),
        }
    }

    pub fn url(&self) -> String {
        self.shared.base_url.clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap()
    }

    /// `(client_id, client_secret, refresh_token)` the fake accepts.
    pub fn credentials(&self) -> (String, String, String) {
        let state = self.state();

        (
            state.client_id.clone(),
            state.client_secret.clone(),
            "fake-refresh-token-1".to_string(),
        )
    }

    /// An access token the fake accepts on Drive requests, issued without a token request.
    pub fn issue_access_token(&self) -> String {
        self.state().new_access_token()
    }

    pub fn set_expires_in(&self, seconds: u64) {
        self.state().expires_in = seconds;
    }

    pub fn rotate_refresh_tokens(&self, rotate: bool) {
        self.state().rotate = rotate;
    }

    pub fn add_refresh_token(&self, token: &str) {
        self.state().refresh_tokens.insert(token.to_string());
    }

    pub fn refresh_tokens(&self) -> Vec<String> {
        let mut tokens: Vec<String> = self.state().refresh_tokens.iter().cloned().collect();

        tokens.sort();
        tokens
    }

    pub fn device_refresh_token(&self) -> String {
        self.state().device_refresh_token.clone()
    }

    pub fn set_device(&self, device: DeviceConfig) {
        self.state().device = device;
    }

    pub fn script_device_polls(&self, polls: &[DevicePoll]) {
        self.state().device_script = polls.iter().copied().collect();
    }

    pub fn set_page_size(&self, size: usize) {
        self.state().page_size = size;
    }

    pub fn store_content(&self, store: bool) {
        self.state().store_content = store;
    }

    pub fn add_folder(&self, parent: &str, name: &str) -> String {
        let mut state = self.state();
        let parent = state.resolve_alias(parent);

        state.add_entry(name, FOLDER_MIME, vec![parent], None)
    }

    pub fn add_file(&self, parent: &str, name: &str, bytes: &[u8]) -> String {
        let mut state = self.state();
        let parent = state.resolve_alias(parent);
        let md5 = hex(&Md5::digest(bytes));
        let content = (Content::Bytes(Arc::new(bytes.to_vec())), bytes.len() as u64, md5);

        state.add_entry(name, "application/octet-stream", vec![parent], Some(content))
    }

    /// A file whose bytes are made on the fly from their position; the MD5 is computed once here.
    pub fn add_generated_file(&self, parent: &str, name: &str, size: u64) -> String {
        let mut hasher = Md5::new();
        let mut block = vec![0u8; 1 << 20];
        let mut position = 0u64;

        while position < size {
            let count = (size - position).min(block.len() as u64) as usize;

            fill_generated(position, &mut block[..count]);
            hasher.update(&block[..count]);
            position += count as u64;
        }

        let mut state = self.state();
        let parent = state.resolve_alias(parent);
        let content = (Content::Generated, size, hex(&hasher.finalize()));

        state.add_entry(name, "application/octet-stream", vec![parent], Some(content))
    }

    pub fn set_trashed(&self, id: &str, trashed: bool) {
        if let Some(entry) = self.state().files.get_mut(id) {
            entry.trashed = trashed;
        }
    }

    pub fn file(&self, id: &str) -> Option<StoredFile> {
        let state = self.state();

        state.files.get(id).map(stored_file)
    }

    /// Every file and folder except the root.
    pub fn files(&self) -> Vec<StoredFile> {
        self.state()
            .files
            .values()
            .filter(|entry| entry.id != ROOT_ID)
            .map(stored_file)
            .collect()
    }

    pub fn files_named(&self, name: &str) -> Vec<StoredFile> {
        self.files().into_iter().filter(|file| file.name == name).collect()
    }

    /// A fault for the next request of `kind` that no earlier fault claims.
    pub fn fault(&self, kind: Kind, fault: Fault) {
        self.fault_after(0, kind, fault);
    }

    /// A fault that lets `skip` requests of `kind` pass first.
    pub fn fault_after(&self, skip: usize, kind: Kind, fault: Fault) {
        self.state().faults.push(FaultEntry {
            kind,
            fault: Some(fault),
            always: None,
            skip,
        });
    }

    /// Every request of `kind` from now on answers `status`.
    pub fn always(&self, kind: Kind, status: u16) {
        self.always_after(0, kind, status);
    }

    pub fn always_after(&self, skip: usize, kind: Kind, status: u16) {
        let body = match Fault::status(status) {
            Fault::Status { body, .. } => body,
            _ => String::new(),
        };

        self.state().faults.push(FaultEntry {
            kind,
            fault: None,
            always: Some((status, body)),
            skip,
        });
    }

    pub fn requests(&self) -> Vec<RequestRecord> {
        self.state().log.clone()
    }

    pub fn requests_of(&self, kind: Kind) -> Vec<RequestRecord> {
        self.requests()
            .into_iter()
            .filter(|record| record.kind == kind)
            .collect()
    }

    pub fn clear_requests(&self) {
        self.state().log.clear();
    }
}

impl Drop for FakeDrive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);

        let _ = TcpStream::connect(self.address);

        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
    }
}

fn stored_file(entry: &FileEntry) -> StoredFile {
    let (size, md5, content) = match &entry.content {
        Some((content, size, md5)) => {
            let bytes = match content {
                Content::Bytes(bytes) => Some(bytes.as_ref().clone()),
                _ => None,
            };

            (Some(*size), Some(md5.clone()), bytes)
        }
        None => (None, None, None),
    };

    StoredFile {
        id: entry.id.clone(),
        name: entry.name.clone(),
        mime_type: entry.mime_type.clone(),
        parents: entry.parents.clone(),
        trashed: entry.trashed,
        size,
        md5,
        content,
    }
}

pub fn fill_generated(offset: u64, block: &mut [u8]) {
    for (index, byte) in block.iter_mut().enumerate() {
        let position = offset + index as u64;

        *byte = (position ^ (position >> 8) ^ (position >> 17)) as u8;
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn modified_time(counter: u64) -> String {
    format!(
        "2026-01-01T{:02}:{:02}:{:02}.000Z",
        counter / 3600 % 24,
        counter / 60 % 60,
        counter % 60
    )
}

fn serve(shared: &Shared, mut stream: TcpStream) {
    let request = match wire::read_request(&mut stream) {
        Ok(request) => request,
        Err(ReadError::Bad(message)) => {
            let body = drive_error_body(400, "badRequest", &message);
            let _ = wire::write_response(&mut stream, Response::raw_json(400, &body));
            return;
        }
        Err(ReadError::Closed) => return,
    };
    let kind = classify(&request);
    let fault = shared.record(&request, kind);

    match shared.dispatch(&request, kind, fault) {
        Action::Respond(response) => {
            let _ = wire::write_response(&mut stream, response);
            let _ = stream.shutdown(Shutdown::Both);
        }
        Action::Close => {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn classify(request: &Request) -> Kind {
    let path = request.path.as_str();
    let method = request.method.as_str();

    if method == "POST" && path == "/token" {
        let is_device = request
            .form()
            .iter()
            .any(|(key, value)| key == "grant_type" && value == DEVICE_GRANT);

        return if is_device { Kind::DevicePoll } else { Kind::Token };
    }

    if method == "POST" && path == "/device/code" {
        return Kind::DeviceCode;
    }

    if path == "/upload/drive/v3/files" || path.starts_with("/upload/drive/v3/files/") {
        return match method {
            "POST" | "PATCH" => Kind::UploadInit,
            "PUT" => {
                let is_status_query = request
                    .header("Content-Range")
                    .is_some_and(|value| value.trim_start().starts_with("bytes */"));

                if is_status_query {
                    Kind::StatusQuery
                } else {
                    Kind::ChunkPut
                }
            }
            _ => Kind::Other,
        };
    }

    if path == "/drive/v3/files" {
        return match method {
            "GET" => Kind::List,
            "POST" => Kind::Create,
            _ => Kind::Other,
        };
    }

    if path.starts_with("/drive/v3/files/") {
        return match method {
            "GET" if request.param("alt") == Some("media") => Kind::Download,
            "GET" => Kind::Get,
            "PATCH" => Kind::Patch,
            "DELETE" => Kind::Delete,
            _ => Kind::Other,
        };
    }

    Kind::Other
}

impl Shared {
    /// Logs the request and takes the fault that claims it, if any.
    fn record(&self, request: &Request, kind: Kind) -> Option<Fault> {
        let mut state = self.state.lock().unwrap();

        state.log.push(RequestRecord {
            kind,
            method: request.method.clone(),
            path: request.path.clone(),
            raw_query: request.raw_query.clone(),
            content_range: request.header("Content-Range").map(str::to_string),
            authorization: request.header("Authorization").map(str::to_string),
            body_len: request.body.len(),
            body: if request.body.len() <= 64 * 1024 {
                request.body.clone()
            } else {
                Vec::new()
            },
            at: Instant::now(),
        });

        let position = state.faults.iter().position(|entry| entry.kind == kind)?;
        let entry = &mut state.faults[position];

        if entry.skip > 0 {
            entry.skip -= 1;
            return None;
        }

        if let Some((code, body)) = &entry.always {
            return Some(Fault::Status {
                code: *code,
                retry_after: None,
                body: body.clone(),
            });
        }

        state.faults.remove(position).fault
    }

    fn dispatch(&self, request: &Request, kind: Kind, fault: Option<Fault>) -> Action {
        let mut modifier = Modifier::default();
        let mut close_after = false;

        match fault {
            None => {}
            Some(Fault::Hook(hook)) => hook(),
            Some(Fault::Status {
                code,
                retry_after,
                body,
            }) => {
                let mut response = Response::raw_json(code, &body);

                if let Some(retry_after) = retry_after {
                    response = response.header("Retry-After", &retry_after);
                }

                return Action::Respond(response);
            }
            Some(Fault::Redirect { status, location }) => {
                return Action::Respond(Response::new(status).header("Location", &location));
            }
            Some(Fault::Unauthorized) => return Action::Respond(unauthorized()),
            Some(Fault::CloseBeforeResponse) => return Action::Close,
            Some(Fault::SessionGone(code)) => {
                if let Some(upload_id) = request.param("upload_id") {
                    self.state.lock().unwrap().sessions.remove(upload_id);
                }

                return Action::Respond(Response::raw_json(
                    code,
                    &drive_error_body(code, "notFound", "Session not found"),
                ));
            }
            Some(Fault::ApplyThenClose) => close_after = true,
            Some(Fault::DropAfterBody { persist }) => modifier.drop_after = Some(persist),
            Some(Fault::ReportRange(kept)) => modifier.report_range = Some(kept),
            Some(Fault::ForeignLocation(url)) => modifier.foreign_location = Some(url),
            Some(Fault::TruncateBody(kept)) => modifier.truncate_body = Some(kept),
            Some(Fault::OmitFields) => modifier.omit_fields = true,
            Some(Fault::CorruptMd5) => modifier.corrupt_md5 = true,
        }

        let action = self
            .state
            .lock()
            .unwrap()
            .handle(request, kind, &modifier, &self.base_url);

        if close_after { Action::Close } else { action }
    }
}

fn unauthorized() -> Response {
    Response::raw_json(401, &drive_error_body(401, "authError", "Invalid Credentials"))
}

fn error_response(code: u16, reason: &str, message: &str) -> Action {
    Action::Respond(Response::raw_json(code, &drive_error_body(code, reason, message)))
}

fn oauth_error(code: u16, error: &str, description: &str) -> Action {
    Action::Respond(Response::json(
        code,
        &json!({"error": error, "error_description": description}),
    ))
}

#[derive(Default)]
struct Fields {
    top: Vec<String>,
    nested: HashMap<String, Vec<String>>,
}

fn parse_fields(spec: &str) -> Result<Fields, String> {
    let mut fields = Fields::default();
    let mut depth = 0;
    let mut piece = String::new();
    let mut pieces = Vec::new();

    for character in spec.chars() {
        match character {
            '(' => {
                depth += 1;
                piece.push(character);
            }
            ')' => {
                depth -= 1;
                piece.push(character);
            }
            ',' if depth == 0 => pieces.push(std::mem::take(&mut piece)),
            other => piece.push(other),
        }
    }

    pieces.push(piece);

    for piece in pieces {
        match piece.split_once('(') {
            Some((name, inner)) => {
                let inner = inner.strip_suffix(')').ok_or("unbalanced fields")?;

                if inner.contains('(') {
                    return Err("nested fields deeper than one level".to_string());
                }

                fields
                    .nested
                    .insert(name.to_string(), inner.split(',').map(str::to_string).collect());
                fields.top.push(name.to_string());
            }
            None if piece.is_empty() => return Err("empty field".to_string()),
            None => fields.top.push(piece),
        }
    }

    Ok(fields)
}

fn project(full: &Map<String, Value>, keys: &[String]) -> Value {
    let mut projected = Map::new();

    for key in keys {
        if let Some(value) = full.get(key) {
            projected.insert(key.clone(), value.clone());
        }
    }

    Value::Object(projected)
}

fn default_keys() -> Vec<String> {
    DEFAULT_FILE_FIELDS.iter().map(|key| key.to_string()).collect()
}

fn bad_request(message: &str) -> Action {
    error_response(400, "badRequest", message)
}

impl State {
    fn new_access_token(&mut self) -> String {
        self.counter += 1;

        let token = format!("ya29.fake-access-{}", self.counter);

        self.access_tokens.insert(token.clone());
        token
    }

    fn resolve_alias(&self, id: &str) -> String {
        if id == "root" {
            ROOT_ID.to_string()
        } else {
            id.to_string()
        }
    }

    fn add_entry(
        &mut self,
        name: &str,
        mime_type: &str,
        parents: Vec<String>,
        content: Option<(Content, u64, String)>,
    ) -> String {
        self.counter += 1;

        let id = format!("fake-file-{:04}", self.counter);

        self.files.insert(
            id.clone(),
            FileEntry {
                id: id.clone(),
                name: name.to_string(),
                mime_type: mime_type.to_string(),
                parents,
                trashed: false,
                modified_time: modified_time(self.counter),
                content,
            },
        );

        id
    }

    fn authorized(&self, request: &Request) -> bool {
        request
            .header("Authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| self.access_tokens.contains(token))
    }

    fn handle(&mut self, request: &Request, kind: Kind, modifier: &Modifier, base_url: &str) -> Action {
        match kind {
            Kind::Token | Kind::DevicePoll => return self.token_endpoint(request),
            Kind::DeviceCode => return self.device_code(request),
            Kind::Other => return error_response(404, "notFound", "no such endpoint"),
            _ => {}
        }

        if !self.authorized(request) {
            return Action::Respond(unauthorized());
        }

        match kind {
            Kind::List => self.list(request, modifier),
            Kind::Get => self.get(request, modifier),
            Kind::Create => self.create(request, modifier),
            Kind::Patch => self.patch(request, modifier),
            Kind::Delete => self.delete(request),
            Kind::Download => self.download(request, modifier),
            Kind::UploadInit => self.upload_init(request, modifier, base_url),
            Kind::ChunkPut | Kind::StatusQuery => self.upload_put(request, modifier),
            _ => error_response(404, "notFound", "no such endpoint"),
        }
    }

    // OAuth

    fn token_endpoint(&mut self, request: &Request) -> Action {
        let is_form = request
            .header("Content-Type")
            .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));

        if !is_form {
            return oauth_error(
                400,
                "invalid_request",
                "the body must be application/x-www-form-urlencoded",
            );
        }

        let form: HashMap<String, String> = request.form().into_iter().collect();
        let field = |name: &str| form.get(name).map(String::as_str).unwrap_or("");

        if field("client_id") != self.client_id || field("client_secret") != self.client_secret {
            return oauth_error(401, "invalid_client", "The OAuth client was not found.");
        }

        match field("grant_type") {
            "refresh_token" => {
                if !self.refresh_tokens.contains(field("refresh_token")) {
                    return oauth_error(400, "invalid_grant", "Token has been expired or revoked.");
                }

                let old = field("refresh_token").to_string();
                let access = self.new_access_token();
                let mut answer = json!({
                    "access_token": access,
                    "expires_in": self.expires_in,
                    "scope": "https://www.googleapis.com/auth/drive.file",
                    "token_type": "Bearer",
                });

                if self.rotate {
                    self.counter += 1;

                    let rotated = format!("fake-refresh-rotated-{}", self.counter);

                    self.refresh_tokens.remove(&old);
                    self.refresh_tokens.insert(rotated.clone());
                    answer["refresh_token"] = Value::String(rotated);
                }

                Action::Respond(Response::json(200, &answer))
            }
            DEVICE_GRANT => {
                if field("device_code") != self.device.device_code {
                    return oauth_error(400, "invalid_grant", "Bad Request");
                }

                match self.device_script.pop_front().unwrap_or(DevicePoll::Pending) {
                    DevicePoll::Pending => oauth_error(428, "authorization_pending", "Precondition Required"),
                    DevicePoll::SlowDown => oauth_error(403, "slow_down", "Forbidden"),
                    DevicePoll::Denied => oauth_error(403, "access_denied", "Forbidden"),
                    DevicePoll::Expired => oauth_error(400, "expired_token", "Bad Request"),
                    DevicePoll::Approved => {
                        let refresh = self.device_refresh_token.clone();
                        let access = self.new_access_token();

                        self.refresh_tokens.insert(refresh.clone());
                        Action::Respond(Response::json(
                            200,
                            &json!({
                                "access_token": access,
                                "expires_in": self.expires_in,
                                "refresh_token": refresh,
                                "scope": "https://www.googleapis.com/auth/drive.file",
                                "token_type": "Bearer",
                            }),
                        ))
                    }
                }
            }
            _ => oauth_error(400, "unsupported_grant_type", "Invalid grant_type"),
        }
    }

    fn device_code(&mut self, request: &Request) -> Action {
        let form: HashMap<String, String> = request.form().into_iter().collect();

        if form.get("client_id") != Some(&self.client_id) {
            return oauth_error(401, "invalid_client", "The OAuth client was not found.");
        }

        if !form.contains_key("scope") {
            return oauth_error(400, "invalid_request", "Missing required parameter: scope");
        }

        Action::Respond(Response::json(
            200,
            &json!({
                "device_code": self.device.device_code,
                "user_code": self.device.user_code,
                "verification_url": self.device.verification_url,
                "expires_in": self.device.expires_in,
                "interval": self.device.interval,
            }),
        ))
    }

    // Drive files

    fn file_json(&self, entry: &FileEntry, modifier: &Modifier) -> Map<String, Value> {
        let mut map = Map::new();

        map.insert("kind".to_string(), json!("drive#file"));
        map.insert("id".to_string(), json!(entry.id));
        map.insert("name".to_string(), json!(entry.name));
        map.insert("mimeType".to_string(), json!(entry.mime_type));
        map.insert("parents".to_string(), json!(entry.parents));
        map.insert("trashed".to_string(), json!(entry.trashed));
        map.insert("modifiedTime".to_string(), json!(entry.modified_time));

        if let Some((_, size, md5)) = &entry.content {
            let md5 = if modifier.corrupt_md5 {
                corrupt(md5)
            } else {
                md5.clone()
            };

            map.insert("size".to_string(), json!(size.to_string()));
            map.insert("md5Checksum".to_string(), json!(md5));
        }

        map
    }

    fn render_file(&self, entry: &FileEntry, fields: Option<&str>, modifier: &Modifier) -> Result<Value, String> {
        let full = self.file_json(entry, modifier);

        match fields {
            Some(spec) if !modifier.omit_fields => Ok(project(&full, &parse_fields(spec)?.top)),
            _ => Ok(project(&full, &default_keys())),
        }
    }

    fn lookup(&self, id: &str) -> Option<&FileEntry> {
        self.files.get(&self.resolve_alias(id))
    }

    fn file_id_from_path(request: &Request, prefix: &str) -> String {
        request.path.strip_prefix(prefix).unwrap_or("").to_string()
    }

    fn not_found(id: &str) -> Action {
        error_response(404, "notFound", &format!("File not found: {id}."))
    }

    fn list(&self, request: &Request, modifier: &Modifier) -> Action {
        let clauses = match request.param("q") {
            Some(query) => match drive_query::parse(query) {
                Ok(clauses) => clauses,
                Err(message) => return error_response(400, "invalidQuery", &format!("Invalid Value: {message}")),
            },
            None => Vec::new(),
        };
        let requested = match request.param("pageSize") {
            Some(text) => match text.parse::<usize>() {
                Ok(size) if (1..=1000).contains(&size) => size,
                _ => return bad_request("Invalid value for pageSize"),
            },
            None => 100,
        };
        let page_size = requested.min(self.page_size);
        let start = match request.param("pageToken") {
            Some(token) => match token.strip_prefix('p').and_then(|text| text.parse::<usize>().ok()) {
                Some(start) => start,
                None => return bad_request("Invalid Value: pageToken"),
            },
            None => 0,
        };
        let fields = match request.param("fields").map(parse_fields).transpose() {
            Ok(fields) => fields,
            Err(message) => return bad_request(&format!("Invalid field selection: {message}")),
        };

        let matching: Vec<&FileEntry> = self
            .files
            .values()
            .filter(|entry| entry.id != ROOT_ID && self.matches(entry, &clauses))
            .collect();
        let page: Vec<&&FileEntry> = matching.iter().skip(start).take(page_size).collect();
        let next = (start + page.len() < matching.len()).then(|| format!("p{}", start + page.len()));

        let file_keys = match &fields {
            Some(fields) if !modifier.omit_fields => fields.nested.get("files").cloned().unwrap_or_else(default_keys),
            _ => default_keys(),
        };
        let files: Vec<Value> = page
            .iter()
            .map(|entry| project(&self.file_json(entry, modifier), &file_keys))
            .collect();

        let mut answer = Map::new();

        answer.insert("kind".to_string(), json!("drive#fileList"));
        answer.insert("incompleteSearch".to_string(), json!(false));

        if let Some(token) = next {
            answer.insert("nextPageToken".to_string(), json!(token));
        }

        answer.insert("files".to_string(), Value::Array(files));

        let body = match &fields {
            Some(fields) => project(&answer, &fields.top),
            None => Value::Object(answer),
        };

        Action::Respond(Response::json(200, &body))
    }

    fn matches(&self, entry: &FileEntry, clauses: &[Clause]) -> bool {
        clauses.iter().all(|clause| match clause {
            Clause::InParents(folder) => entry.parents.contains(&self.resolve_alias(folder)),
            Clause::NameEquals(name) => entry.name.eq_ignore_ascii_case(name),
            Clause::MimeTypeEquals(mime) => &entry.mime_type == mime,
            Clause::MimeTypeNotEquals(mime) => &entry.mime_type != mime,
            Clause::Trashed(trashed) => entry.trashed == *trashed,
        })
    }

    fn get(&self, request: &Request, modifier: &Modifier) -> Action {
        let id = State::file_id_from_path(request, "/drive/v3/files/");

        match self.lookup(&id) {
            Some(entry) => self.file_response(entry, request.param("fields"), modifier, 200),
            None => State::not_found(&id),
        }
    }

    fn file_response(&self, entry: &FileEntry, fields: Option<&str>, modifier: &Modifier, status: u16) -> Action {
        match self.render_file(entry, fields, modifier) {
            Ok(value) => Action::Respond(Response::json(status, &value)),
            Err(message) => bad_request(&format!("Invalid field selection: {message}")),
        }
    }

    fn json_body(request: &Request) -> Result<Value, Action> {
        let is_json = request
            .header("Content-Type")
            .is_some_and(|value| value.starts_with("application/json"));

        if !is_json {
            return Err(bad_request("the body must be application/json"));
        }

        serde_json::from_slice(&request.body).map_err(|_| bad_request("the body is not JSON"))
    }

    fn create(&mut self, request: &Request, modifier: &Modifier) -> Action {
        let body = match State::json_body(request) {
            Ok(body) => body,
            Err(action) => return action,
        };
        let Some(name) = body["name"].as_str() else {
            return bad_request("name is required");
        };
        let mime = body["mimeType"].as_str().unwrap_or("application/octet-stream");
        let parents = match self.parents_from(&body) {
            Ok(parents) => parents,
            Err(action) => return action,
        };
        let id = self.add_entry(name, mime, parents, None);
        let entry = self.files[&id].clone();

        self.file_response(&entry, request.param("fields"), modifier, 200)
    }

    fn parents_from(&self, body: &Value) -> Result<Vec<String>, Action> {
        let Some(list) = body["parents"].as_array() else {
            return Ok(vec![ROOT_ID.to_string()]);
        };
        let mut parents = Vec::new();

        for parent in list {
            let id = self.resolve_alias(parent.as_str().unwrap_or(""));

            if !self.files.contains_key(&id) {
                return Err(State::not_found(&id));
            }

            parents.push(id);
        }

        Ok(parents)
    }

    fn patch(&mut self, request: &Request, modifier: &Modifier) -> Action {
        let id = State::file_id_from_path(request, "/drive/v3/files/");
        let id = self.resolve_alias(&id);
        let body = match State::json_body(request) {
            Ok(body) => body,
            Err(action) => return action,
        };
        let Some(entry) = self.files.get_mut(&id) else {
            return State::not_found(&id);
        };

        if let Some(trashed) = body["trashed"].as_bool() {
            entry.trashed = trashed;
        }

        if let Some(name) = body["name"].as_str() {
            entry.name = name.to_string();
        }

        let entry = entry.clone();

        self.file_response(&entry, request.param("fields"), modifier, 200)
    }

    fn delete(&mut self, request: &Request) -> Action {
        let id = State::file_id_from_path(request, "/drive/v3/files/");
        let id = self.resolve_alias(&id);

        if self.files.remove(&id).is_some() {
            Action::Respond(Response::new(204))
        } else {
            State::not_found(&id)
        }
    }

    fn download(&self, request: &Request, modifier: &Modifier) -> Action {
        let id = State::file_id_from_path(request, "/drive/v3/files/");
        let Some(entry) = self.lookup(&id) else {
            return State::not_found(&id);
        };
        let Some((content, size, _)) = &entry.content else {
            return error_response(
                403,
                "fileNotDownloadable",
                "Only files with binary content can be downloaded.",
            );
        };
        let size = *size;
        let fill: Filler = match content {
            Content::Bytes(bytes) => {
                let bytes = Arc::clone(bytes);

                Box::new(move |offset, block| {
                    let start = offset as usize;

                    block.copy_from_slice(&bytes[start..start + block.len()]);
                })
            }
            Content::Generated => Box::new(fill_generated),
            Content::Hashed => return error_response(500, "backendError", "content was not stored"),
        };

        Action::Respond(Response {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/octet-stream".to_string())],
            payload: Payload::Generated {
                length: size,
                cut_after: modifier.truncate_body,
                fill,
            },
        })
    }

    // Resumable uploads

    fn upload_init(&mut self, request: &Request, modifier: &Modifier, base_url: &str) -> Action {
        if request.param("uploadType") != Some("resumable") {
            return bad_request("uploadType must be resumable");
        }

        let total = match request.header("X-Upload-Content-Length") {
            Some(value) => match value.trim().parse::<u64>() {
                Ok(total) => Some(total),
                Err(_) => return bad_request("bad X-Upload-Content-Length"),
            },
            None => None,
        };
        let target = if request.method == "POST" {
            if request.path != "/upload/drive/v3/files" {
                return error_response(404, "notFound", "no such endpoint");
            }

            let body = match State::json_body(request) {
                Ok(body) => body,
                Err(action) => return action,
            };
            let Some(name) = body["name"].as_str() else {
                return bad_request("name is required");
            };
            let parents = match self.parents_from(&body) {
                Ok(parents) => parents,
                Err(action) => return action,
            };

            Target::New {
                name: name.to_string(),
                parents,
            }
        } else {
            let id = State::file_id_from_path(request, "/upload/drive/v3/files/");
            let id = self.resolve_alias(&id);

            if !self.files.contains_key(&id) {
                return State::not_found(&id);
            }

            if !request.body.is_empty() {
                return bad_request("a content update takes no metadata here");
            }

            Target::Replace { id }
        };

        self.counter += 1;

        let upload_id = format!("fake-upload-{:04}-s3ss10n", self.counter);

        self.sessions.insert(
            upload_id.clone(),
            Session {
                target,
                fields: request.param("fields").map(str::to_string),
                total,
                persisted: 0,
                bytes: Vec::new(),
                hasher: Md5::new(),
                completed: None,
            },
        );

        let location = modifier
            .foreign_location
            .clone()
            .unwrap_or_else(|| format!("{base_url}/upload/drive/v3/files?uploadType=resumable&upload_id={upload_id}"));

        Action::Respond(Response::new(200).header("Location", &location))
    }

    fn upload_put(&mut self, request: &Request, modifier: &Modifier) -> Action {
        if request.content_length.is_none() {
            return Action::Respond(Response::raw_json(
                411,
                &drive_error_body(411, "lengthRequired", "Length Required"),
            ));
        }

        let Some(upload_id) = request.param("upload_id") else {
            return bad_request("upload_id is required");
        };

        if !self.sessions.contains_key(upload_id) {
            return error_response(404, "notFound", "Session not found");
        }

        let Some(range) = request.header("Content-Range").and_then(ContentRange::parse) else {
            return error_response(400, "badContentRange", "Content-Range is missing or malformed");
        };
        let upload_id = upload_id.to_string();
        let session = self.sessions.get_mut(&upload_id).unwrap();

        if session.total.is_some_and(|total| total != range.total) {
            return error_response(400, "badContentRange", "total differs from X-Upload-Content-Length");
        }

        session.total = Some(range.total);

        if let Some((file_id, status)) = &session.completed {
            let (file_id, status) = (file_id.clone(), *status);

            return self.completed_answer(&upload_id, &file_id, status, modifier);
        }

        let body = &request.body;
        let persisted_before = session.persisted;

        match range.span {
            None => {
                if !body.is_empty() {
                    return error_response(400, "badContentRange", "a status query takes no body");
                }
            }
            Some((first, last)) => {
                if first != session.persisted {
                    return error_response(
                        400,
                        "badContentRange",
                        &format!("chunk starts at {first}, the session holds {}", session.persisted),
                    );
                }

                if last >= range.total || body.len() as u64 != last - first + 1 {
                    return error_response(400, "badContentRange", "Content-Range does not match the body");
                }

                if last + 1 < range.total && !(body.len() as u64).is_multiple_of(CHUNK_UNIT) {
                    return error_response(
                        400,
                        "badContentRange",
                        "a non-final chunk must be a multiple of 256 KiB",
                    );
                }

                let kept = match (modifier.drop_after, modifier.report_range) {
                    (Some(persist), _) => (persist as usize).min(body.len()),
                    (None, Some(offset)) => (offset.saturating_sub(session.persisted) as usize).min(body.len()),
                    (None, None) => body.len(),
                };

                session.hasher.update(&body[..kept]);
                session.persisted += kept as u64;

                if self.store_content {
                    session.bytes.extend_from_slice(&body[..kept]);
                }

                if modifier.drop_after.is_some() {
                    return Action::Close;
                }
            }
        }

        if session.persisted == range.total {
            return self.finalize(&upload_id, modifier);
        }

        let reported = match modifier.report_range {
            Some(offset) if offset < persisted_before => offset,
            _ => session.persisted,
        };
        let mut response = Response::new(308);

        if reported > 0 {
            response = response.header("Range", &format!("bytes=0-{}", reported - 1));
        }

        Action::Respond(response)
    }

    fn finalize(&mut self, upload_id: &str, modifier: &Modifier) -> Action {
        let session = self.sessions.get_mut(upload_id).unwrap();
        let size = session.persisted;
        let md5 = hex(&session.hasher.clone().finalize());
        let content = if self.store_content {
            Content::Bytes(Arc::new(std::mem::take(&mut session.bytes)))
        } else {
            Content::Hashed
        };
        let content = Some((content, size, md5));

        self.counter += 1;

        let (file_id, status) = match &self.sessions[upload_id].target {
            Target::New { name, parents } => {
                let (name, parents) = (name.clone(), parents.clone());
                let id = self.add_entry(&name, "application/octet-stream", parents, content);

                (id, 201)
            }
            Target::Replace { id } => {
                let id = id.clone();
                let modified = modified_time(self.counter);
                let entry = self.files.get_mut(&id).unwrap();

                entry.content = content;
                entry.modified_time = modified;
                (id, 200)
            }
        };

        self.sessions.get_mut(upload_id).unwrap().completed = Some((file_id.clone(), status));
        self.completed_answer(upload_id, &file_id, status, modifier)
    }

    fn completed_answer(&self, upload_id: &str, file_id: &str, status: u16, modifier: &Modifier) -> Action {
        let fields = self.sessions[upload_id].fields.clone();

        match self.files.get(file_id) {
            Some(entry) => self.file_response(entry, fields.as_deref(), modifier, status),
            None => State::not_found(file_id),
        }
    }
}

fn corrupt(md5: &str) -> String {
    let mut chars: Vec<char> = md5.chars().collect();

    if let Some(first) = chars.first_mut() {
        *first = if *first == '0' { '1' } else { '0' };
    }

    chars.into_iter().collect()
}

struct ContentRange {
    /// `None` for `bytes */total`.
    span: Option<(u64, u64)>,
    total: u64,
}

impl ContentRange {
    fn parse(header: &str) -> Option<ContentRange> {
        let rest = header.trim().strip_prefix("bytes ")?;
        let (span, total) = rest.split_once('/')?;
        let total = total.parse().ok()?;

        if span == "*" {
            return Some(ContentRange { span: None, total });
        }

        let (first, last) = span.split_once('-')?;

        Some(ContentRange {
            span: Some((first.parse().ok()?, last.parse().ok()?)),
            total,
        })
    }
}
