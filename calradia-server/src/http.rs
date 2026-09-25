//! Minimal, tolerant HTTP/1.x request parsing and response formatting.

use std::io::Read;

/// Maximum size of the request line plus headers.
pub const MAX_HEAD: usize = 64 * 1024;
/// Maximum accepted request body (from Content-Length).
pub const MAX_BODY: usize = 1024 * 1024;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Decodes `%XX` escapes (invalid escapes are kept literally); optionally maps '+' to space.
pub fn percent_decode(s: &str, plus_as_space: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' if plus_as_space => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `a=1&b=x%20y` into decoded key/value pairs ('+' means space).
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k, true), percent_decode(v, true))
        })
        .collect()
}

/// Returns (end of head, start of body); accepts CRLFCRLF or bare LFLF.
fn head_end(buf: &[u8]) -> Option<(usize, usize)> {
    (0..buf.len()).find_map(|i| {
        if buf[i..].starts_with(b"\r\n\r\n") {
            Some((i, i + 4))
        } else if buf[i..].starts_with(b"\n\n") {
            Some((i, i + 2))
        } else {
            None
        }
    })
}

/// Parses the request line and headers into a body-less `Request`.
pub fn parse_head(head: &[u8]) -> Result<Request, String> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let (method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) if m.bytes().all(|c| c.is_ascii_alphabetic()) => (m, t),
        _ => return Err(format!("malformed request line {first:?}")),
    };
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| format!("malformed header line {line:?}"))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    // Tolerate absolute-form targets ("http://host/path?q") as sent to proxies.
    let origin = match target.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => target,
    };
    let (raw_path, raw_query) = origin.split_once('?').unwrap_or((origin, ""));
    Ok(Request {
        method: method.to_string(),
        target: target.to_string(),
        path: percent_decode(raw_path, false),
        query: parse_query(raw_query),
        headers,
        body: Vec::new(),
    })
}

/// Reads one request (head, then Content-Length body) from `r`, across as many reads as needed.
pub fn read_request<R: Read>(r: &mut R) -> Result<Request, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_len, body_start) = loop {
        if let Some(found) = head_end(&buf) {
            break found;
        }
        if buf.len() > MAX_HEAD {
            return Err(format!("headers exceed {MAX_HEAD} bytes"));
        }
        let n = r.read(&mut chunk).map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            return Err(format!(
                "connection closed after {} bytes, before end of headers",
                buf.len()
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let mut req = parse_head(&buf[..head_len])?;
    let len = match req.header("content-length") {
        None => 0,
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| format!("bad Content-Length {v:?}"))?,
    };
    if len > MAX_BODY {
        return Err(format!("body of {len} bytes exceeds {MAX_BODY}"));
    }
    let mut body = buf.split_off(body_start);
    while body.len() < len {
        let n = r
            .read(&mut chunk)
            .map_err(|e| format!("read error in body: {e}"))?;
        if n == 0 {
            return Err(format!(
                "connection closed after {} of {len} body bytes",
                body.len()
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Formats a complete HTTP/1.1 response with a plain-text body.
pub fn format_response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reader that hands out one pre-split chunk per `read` call.
    struct Chunks(Vec<Vec<u8>>);
    impl Read for Chunks {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let c = self.0.remove(0);
            out[..c.len()].copy_from_slice(&c);
            Ok(c.len())
        }
    }

    fn chunks(parts: &[&str]) -> Chunks {
        Chunks(parts.iter().map(|p| p.as_bytes().to_vec()).collect())
    }

    #[test]
    fn decodes_percent_and_plus() {
        assert_eq!(percent_decode("a%20b%7Cc", false), "a b|c");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert_eq!(percent_decode("a+b", true), "a b");
        assert_eq!(percent_decode("%C3%A9", false), "é");
        assert_eq!(percent_decode("100%", false), "100%");
        assert_eq!(percent_decode("%zz%4", false), "%zz%4");
        assert_eq!(percent_decode("%41", false), "A");
        assert_eq!(percent_decode("%FF", false), "\u{FFFD}");
    }

    #[test]
    fn parses_query() {
        let q = parse_query("msg=hi+there&x=%26&&flag");
        assert_eq!(
            q,
            vec![
                ("msg".into(), "hi there".into()),
                ("x".into(), "&".into()),
                ("flag".into(), "".into())
            ]
        );
    }

    #[test]
    fn parses_simple_get() {
        let mut r = chunks(&["GET /echo%21?msg=a%20b HTTP/1.1\r\nHost: x\r\nAccept: */*\r\n\r\n"]);
        let req = read_request(&mut r).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.target, "/echo%21?msg=a%20b");
        assert_eq!(req.path, "/echo!");
        assert_eq!(req.param("msg"), Some("a b"));
        assert_eq!(req.header("HOST"), Some("x"));
        assert_eq!(req.headers.len(), 2);
        assert!(req.body.is_empty());
    }

    #[test]
    fn parses_request_split_across_reads_with_body() {
        let mut r = chunks(&[
            "POST /ping HTTP/1.1\r\nContent-Le",
            "ngth: 5\r\n\r\nhe",
            "llo",
        ]);
        let req = read_request(&mut r).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/ping");
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn parses_absolute_form_and_bare_lf() {
        let mut r = chunks(&["GET http://127.0.0.1:8765/echo?msg=x HTTP/1.1\nHost: h\n\n"]);
        let req = read_request(&mut r).unwrap();
        assert_eq!(req.path, "/echo");
        assert_eq!(req.param("msg"), Some("x"));
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(read_request(&mut chunks(&["garbage\r\n\r\n"])).is_err());
        assert!(read_request(&mut chunks(&["\u{1}\u{2} / HTTP/1.1\r\n\r\n"])).is_err());
        assert!(read_request(&mut chunks(&["GET / HTTP/1.1\r\nno colon\r\n\r\n"])).is_err());
        assert!(read_request(&mut chunks(&["GET / HTTP/1.1\r\nHost: x\r\n"])).is_err());
        assert!(read_request(&mut chunks(&[
            "GET / HTTP/1.1\r\nContent-Length: nope\r\n\r\n"
        ]))
        .is_err());
        assert!(read_request(&mut chunks(&[
            "GET / HTTP/1.1\r\nContent-Length: 9\r\n\r\nabc"
        ]))
        .is_err());
        let huge = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(MAX_HEAD + 10));
        assert!(read_request(&mut Chunks(
            huge.into_bytes().chunks(4096).map(<[u8]>::to_vec).collect()
        ))
        .is_err());
    }

    #[test]
    fn formats_response() {
        let r = String::from_utf8(format_response(404, "0|not found")).unwrap();
        assert_eq!(
            r,
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 11\r\nConnection: close\r\n\r\n0|not found"
        );
    }
}
