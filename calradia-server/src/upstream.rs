//! Where replies come from: an OpenAI-compatible upstream, or a canned text for
//! `--fake-llm` and the text fault modes.
//!
//! The upstream client is a minimal HTTP/1.1 client on std `TcpStream`, so the job
//! deadline bounds every connect, write and read, and a cancel can abort a request by
//! shutting the socket down from another thread.

use crate::prompt::Chat;
use crate::protocol::{
    REASON_CANCELED, REASON_TIMEOUT, REASON_UPSTREAM_ERROR, REASON_UPSTREAM_UNAVAILABLE,
    UPSTREAM_MAX_BODY,
};
use serde_json::{json, Value};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_UPSTREAM: &str = "http://172.17.0.1:8080/v1";
pub const DEFAULT_MODEL: &str = "calradia-qwen3.5-9b";
const MAX_TOKENS: u32 = 220;
const TEMPERATURE: f64 = 0.8;
/// Cap on the upstream response's status line plus headers.
const MAX_RESPONSE_HEAD: usize = 64 * 1024;
/// Cap on one chunk-size line of a chunked body.
const MAX_CHUNK_LINE: usize = 1024;

/// Why a reply could not be produced: `reason` is the job's FAILED token, `detail` is for
/// the log only.
#[derive(Debug)]
pub struct Failure {
    pub reason: &'static str,
    pub detail: String,
}

impl Failure {
    pub fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Failure {
            reason,
            detail: detail.into(),
        }
    }
    pub fn error(detail: impl Into<String>) -> Self {
        Self::new(REASON_UPSTREAM_ERROR, detail)
    }
    fn timeout(during: &str) -> Self {
        Self::new(REASON_TIMEOUT, format!("deadline reached during {during}"))
    }
    fn too_large() -> Self {
        Self::error(format!("response body over {UPSTREAM_MAX_BODY} bytes"))
    }
    fn io(e: io::Error, during: &str) -> Self {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Self::timeout(during),
            io::ErrorKind::UnexpectedEof => {
                Self::error(format!("connection closed during {during}"))
            }
            _ => Self::error(format!("{during}: {e}")),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} ({})", self.reason, self.detail)
    }
}

/// An `http://host[:port][/prefix]` base URL, resolved to the chat completions path.
#[derive(Debug)]
pub struct Endpoint {
    host: String,
    port: u16,
    authority: String,
    path: String,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "http://{}{}", self.authority, self.path)
    }
}

pub fn parse_endpoint(base: &str) -> Result<Endpoint, String> {
    let bad = |why: &str| format!("bad upstream URL {base:?}: {why}");
    let rest = base
        .strip_prefix("http://")
        .ok_or_else(|| bad("must start with http:// (TLS is not supported)"))?;
    let (authority, prefix) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    if authority.is_empty() || authority.contains('@') || prefix.contains(['?', '#']) {
        return Err(bad("expected http://host[:port][/path]"));
    }
    let (host, port) = match authority.strip_prefix('[') {
        // IPv6 literal: [addr] or [addr]:port
        Some(v6) => {
            let (host, after) = v6.split_once(']').ok_or_else(|| bad("unclosed ["))?;
            let port = match after {
                "" => Some(80),
                p => p.strip_prefix(':').and_then(|p| p.parse().ok()),
            };
            (host, port)
        }
        None => match authority.rsplit_once(':') {
            Some((host, p)) => (host, p.parse().ok()),
            None => (authority, Some(80)),
        },
    };
    let port = port.ok_or_else(|| bad("bad port"))?;
    Ok(Endpoint {
        host: host.to_string(),
        port,
        authority: authority.to_string(),
        path: format!("{}/chat/completions", prefix.trim_end_matches('/')),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Canned {
    /// `--fake-llm`: a short in-character reply.
    Reply,
    /// `--fault oversize-N`: a reply of N characters before sanitizing.
    Oversize(usize),
    /// `--fault nonascii`: a reply full of characters the engine cannot show.
    NonAscii,
}

pub enum Backend {
    Upstream {
        endpoint: Endpoint,
        model: String,
        connect_timeout: Duration,
    },
    Canned {
        delay: Duration,
        kind: Canned,
    },
}

impl Backend {
    /// Produces the raw, unsanitized reply to `chat` for job `job`.
    ///
    /// For the upstream, `attach` is called with the connected socket before anything is
    /// sent; it registers a handle for cancel-by-shutdown and returns false if the job has
    /// already left PENDING, in which case the request is abandoned.
    pub fn generate(
        &self,
        chat: &Chat,
        job: u32,
        deadline: Instant,
        attach: &mut dyn FnMut(&TcpStream) -> bool,
    ) -> Result<String, Failure> {
        match self {
            Backend::Upstream {
                endpoint,
                model,
                connect_timeout,
            } => {
                let messages: Vec<Value> = chat
                    .messages
                    .iter()
                    .map(|(role, content)| json!({"role": role, "content": content}))
                    .collect();
                let body = json!({
                    "model": model,
                    "messages": messages,
                    "max_tokens": MAX_TOKENS,
                    "temperature": TEMPERATURE,
                    "chat_template_kwargs": {"enable_thinking": false},
                });
                let conn = connect(endpoint, deadline, *connect_timeout)?;
                if !attach(&conn) {
                    return Err(Failure::new(
                        REASON_CANCELED,
                        "job left PENDING before send",
                    ));
                }
                let (status, body) = post(&conn, endpoint, &body.to_string(), deadline)?;
                parse_completion(status, &body)
            }
            Backend::Canned { delay, kind } => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left < *delay {
                    thread::sleep(left);
                    return Err(Failure::timeout("canned reply"));
                }
                thread::sleep(*delay);
                Ok(match kind {
                    Canned::Reply if !chat.fake_reply.is_empty() => chat.fake_reply.clone(),
                    _ => kind.text(job),
                })
            }
        }
    }
}

impl Canned {
    fn text(self, job: u32) -> String {
        const REPLIES: [&str; 3] = [
            "Quiet road today, captain. I don't trust quiet roads. Keep your blade loose in \
             its scabbard.",
            "Coin's thin and the ale is thinner. Still, I've ridden with worse, and buried \
             most of them.",
            "Rain by nightfall. My knee says so, and it has lied to me less often than any \
             Swadian lord.",
        ];
        match self {
            Canned::Reply => REPLIES[job as usize % REPLIES.len()].to_string(),
            Canned::Oversize(n) => "The road to Praven is long, the ale is thin and the \
                 rain never stops. "
                .chars()
                .cycle()
                .take(n)
                .collect(),
            Canned::NonAscii => "\u{201C}Aye,\u{201D} says Hrodvar \u{2014} the caf\u{E9} \
                 in Sargoth\u{2026} serves na\u{EF}ve Stra\u{DF}e ale \u{1F37A} \u{4F60}\u{597D} \
                 \u{A1}Ol\u{E9}!"
                .to_string(),
        }
    }
}

/// Connects to the first address that accepts, bounded by the connect timeout and the
/// deadline.
fn connect(
    ep: &Endpoint,
    deadline: Instant,
    connect_timeout: Duration,
) -> Result<TcpStream, Failure> {
    // Name resolution has no timeout in std; the default upstream is an IP literal.
    let addrs: Vec<SocketAddr> = (ep.host.as_str(), ep.port)
        .to_socket_addrs()
        .map_err(|e| Failure::new(REASON_UPSTREAM_UNAVAILABLE, format!("resolve: {e}")))?
        .collect();
    let mut last = format!("no address for {}", ep.host);
    for addr in addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Failure::timeout("connect"));
        }
        match TcpStream::connect_timeout(&addr, left.min(connect_timeout)) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("connect {addr}: {e}"),
        }
    }
    if Instant::now() >= deadline {
        return Err(Failure::timeout("connect"));
    }
    Err(Failure::new(REASON_UPSTREAM_UNAVAILABLE, last))
}

/// Sends the POST and reads the whole response. Returns the status and the body.
fn post(
    conn: &TcpStream,
    ep: &Endpoint,
    json: &str,
    deadline: Instant,
) -> Result<(u16, Vec<u8>), Failure> {
    let _ = conn.set_nodelay(true);
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
        ep.path,
        ep.authority,
        json.len()
    );
    let mut io = DeadlineIo { conn, deadline };
    io.write_all(request.as_bytes())
        .map_err(|e| Failure::io(e, "send"))?;
    read_response(&mut BufReader::new(io))
}

/// Reads and writes on a socket without passing the deadline: before every call the
/// socket timeout is set to the time left.
struct DeadlineIo<'a> {
    conn: &'a TcpStream,
    deadline: Instant,
}

impl DeadlineIo<'_> {
    fn time_left(&self) -> io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            Ok(left)
        }
    }
}

impl Read for DeadlineIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.conn.set_read_timeout(Some(self.time_left()?))?;
        let mut conn = self.conn;
        conn.read(buf)
    }
}

impl Write for DeadlineIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.conn.set_write_timeout(Some(self.time_left()?))?;
        let mut conn = self.conn;
        conn.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_response(r: &mut impl BufRead) -> Result<(u16, Vec<u8>), Failure> {
    let mut budget = MAX_RESPONSE_HEAD;
    loop {
        let line = read_line(r, &mut budget)?;
        let mut parts = line.split_whitespace();
        let status = match (parts.next(), parts.next().map(str::parse::<u16>)) {
            (Some(v), Some(Ok(s))) if v.starts_with("HTTP/") => s,
            _ => return Err(Failure::error(format!("bad status line {line:?}"))),
        };
        let mut chunked = false;
        let mut length = None;
        loop {
            let line = read_line(r, &mut budget)?;
            if line.is_empty() {
                break;
            }
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| Failure::error(format!("bad header line {line:?}")))?;
            let (name, value) = (name.trim(), value.trim());
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.to_ascii_lowercase().contains("chunked");
            } else if name.eq_ignore_ascii_case("content-length") {
                let n = value
                    .parse::<usize>()
                    .map_err(|_| Failure::error(format!("bad Content-Length {value:?}")))?;
                length = Some(n);
            }
        }
        // Skip interim responses such as "100 Continue".
        if (100..200).contains(&status) {
            continue;
        }
        let body = if chunked {
            read_chunked(r)?
        } else if let Some(n) = length {
            if n > UPSTREAM_MAX_BODY {
                return Err(Failure::too_large());
            }
            let mut body = vec![0; n];
            r.read_exact(&mut body)
                .map_err(|e| Failure::io(e, "response body"))?;
            body
        } else {
            let mut body = Vec::new();
            r.by_ref()
                .take(UPSTREAM_MAX_BODY as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|e| Failure::io(e, "response body"))?;
            if body.len() > UPSTREAM_MAX_BODY {
                return Err(Failure::too_large());
            }
            body
        };
        return Ok((status, body));
    }
}

fn read_chunked(r: &mut impl BufRead) -> Result<Vec<u8>, Failure> {
    let mut body = Vec::new();
    loop {
        let line = read_line(r, &mut { MAX_CHUNK_LINE })?;
        let hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(hex, 16)
            .map_err(|_| Failure::error(format!("bad chunk size {hex:?}")))?;
        if size == 0 {
            // Trailers, if any, are left unread: the connection is closed anyway.
            return Ok(body);
        }
        if size > UPSTREAM_MAX_BODY - body.len() {
            return Err(Failure::too_large());
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..])
            .map_err(|e| Failure::io(e, "response chunk"))?;
        if !read_line(r, &mut { MAX_CHUNK_LINE })?.is_empty() {
            return Err(Failure::error("missing CRLF after chunk"));
        }
    }
}

/// Reads one line of at most `*budget` bytes (including the line ending), without its
/// CRLF or LF, and charges it to the budget.
fn read_line(r: &mut impl BufRead, budget: &mut usize) -> Result<String, Failure> {
    let mut buf = Vec::new();
    let n = r
        .by_ref()
        .take(*budget as u64)
        .read_until(b'\n', &mut buf)
        .map_err(|e| Failure::io(e, "response head"))?;
    if !buf.ends_with(b"\n") {
        return Err(if n >= *budget {
            Failure::error("response line too long")
        } else {
            Failure::error("connection closed during response head")
        });
    }
    *budget -= n;
    buf.pop();
    if buf.ends_with(b"\r") {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Extracts `choices[0].message.content`, ignoring `reasoning_content`. A missing or null
/// content counts as an empty reply.
fn parse_completion(status: u16, body: &[u8]) -> Result<String, Failure> {
    if !(200..300).contains(&status) {
        return Err(Failure::error(format!("HTTP {status}")));
    }
    let v: Value =
        serde_json::from_slice(body).map_err(|e| Failure::error(format!("bad JSON: {e}")))?;
    let message = v
        .pointer("/choices/0/message")
        .ok_or_else(|| Failure::error("no choices[0].message"))?;
    match message.get("content") {
        Some(Value::String(s)) => Ok(s.clone()),
        None | Some(Value::Null) => Ok(String::new()),
        Some(_) => Err(Failure::error("content is not a string")),
    }
}
