//! HTTP/1.1 on a `TcpStream`: just enough for the fake. Requests carry `Content-Length` bodies only, and every
//! answer closes the connection.

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Request {
    pub method: String,
    pub path: String,
    pub raw_query: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub content_length: Option<usize>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn form(&self) -> Vec<(String, String)> {
        parse_query(&String::from_utf8_lossy(&self.body)).unwrap_or_default()
    }
}

pub enum ReadError {
    Closed,
    Bad(String),
}

pub fn read_request(stream: &mut TcpStream) -> Result<Request, ReadError> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);

        match parsed.parse(&buffer) {
            Ok(httparse::Status::Complete(head_len)) => {
                let method = parsed.method.unwrap_or("").to_string();
                let target = parsed.path.unwrap_or("").to_string();
                let headers: Vec<(String, String)> = parsed
                    .headers
                    .iter()
                    .map(|header| {
                        (
                            header.name.to_string(),
                            String::from_utf8_lossy(header.value).into_owned(),
                        )
                    })
                    .collect();

                return finish_request(stream, method, target, headers, buffer.split_off(head_len));
            }
            Ok(httparse::Status::Partial) => {}
            Err(error) => return Err(ReadError::Bad(format!("unparsable request head: {error}"))),
        }

        match stream.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Closed),
            Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            Err(_) => return Err(ReadError::Closed),
        }
    }
}

fn finish_request(
    stream: &mut TcpStream,
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    mut body: Vec<u8>,
) -> Result<Request, ReadError> {
    let find = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };

    if find("Transfer-Encoding").is_some() {
        return Err(ReadError::Bad("chunked request bodies are not supported".to_string()));
    }

    if find("Expect").is_some_and(|value| value.eq_ignore_ascii_case("100-continue")) {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    let content_length = match find("Content-Length") {
        Some(value) => Some(
            value
                .trim()
                .parse::<usize>()
                .map_err(|_| ReadError::Bad("bad Content-Length".to_string()))?,
        ),
        None => None,
    };

    if let Some(length) = content_length {
        if body.len() > length {
            body.truncate(length);
        }

        let already = body.len();

        body.resize(length, 0);
        if stream.read_exact(&mut body[already..]).is_err() {
            return Err(ReadError::Closed);
        }
    }

    let (path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let query = parse_query(&raw_query).ok_or_else(|| ReadError::Bad("malformed percent-encoding".to_string()))?;

    Ok(Request {
        method,
        path,
        raw_query,
        query,
        headers,
        content_length,
        body,
    })
}

pub fn parse_query(raw: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();

    for part in raw.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));

        pairs.push((percent_decode(key)?, percent_decode(value)?));
    }

    Some(pairs)
}

pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let pair = bytes.get(index + 1..index + 3)?;
                let value = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;

                decoded.push(value);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(decoded).ok()
}

/// Writes the body bytes that start at the given offset into the block.
pub type Filler = Box<dyn Fn(u64, &mut [u8]) + Send>;

pub enum Payload {
    Bytes(Vec<u8>),
    /// A body produced while writing: `length` bytes from `fill`, cut after `cut_after` bytes when set.
    Generated {
        length: u64,
        cut_after: Option<u64>,
        fill: Filler,
    },
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub payload: Payload,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            payload: Payload::Bytes(Vec::new()),
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        Response::new(status)
            .header("Content-Type", "application/json; charset=UTF-8")
            .body(value.to_string().into_bytes())
    }

    pub fn raw_json(status: u16, text: &str) -> Response {
        Response::new(status)
            .header("Content-Type", "application/json; charset=UTF-8")
            .body(text.as_bytes().to_vec())
    }

    pub fn header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, bytes: Vec<u8>) -> Response {
        self.payload = Payload::Bytes(bytes);
        self
    }
}

pub fn write_response(stream: &mut TcpStream, response: Response) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason_phrase(response.status));
    let length = match &response.payload {
        Payload::Bytes(bytes) => bytes.len() as u64,
        Payload::Generated { length, .. } => *length,
    };

    if response.status != 204 {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }

    head.push_str("Connection: close\r\n");

    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }

    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;

    match response.payload {
        Payload::Bytes(bytes) => stream.write_all(&bytes)?,
        Payload::Generated {
            length,
            cut_after,
            fill,
        } => {
            let stop = cut_after.map_or(length, |cut| cut.min(length));
            let mut block = vec![0u8; 64 * 1024];
            let mut position = 0u64;

            while position < stop {
                let count = (stop - position).min(block.len() as u64) as usize;

                fill(position, &mut block[..count]);
                stream.write_all(&block[..count])?;
                position += count as u64;
            }
        }
    }

    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Resume Incomplete",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        411 => "Length Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}
