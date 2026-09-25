//! Calradia AI IPC proof-of-concept HTTP server (std only, no dependencies).
//!
//! The Mount&Blade Warband engine reaches this server through libcurl when a module
//! script runs `(send_message_to_url, <string>, <encode_url>)`. The engine splits the
//! response body on '|' into integers (reg0..) and strings (s0..), so every body here
//! is a '|'-separated record. Every request is logged in full to stdout, because the
//! exact method and headers the engine sends are not yet known.
//!
//! Usage: `calradia-server [--bind ADDR:PORT]` (default 127.0.0.1:8766).
//!
//! Routes (matched on the decoded path; any method):
//! - `/`, `/ping`   200 `1|Hello from the Calradia AI server!`
//! - `/echo?msg=..` 200 `1|<msg>` ('|', CR, LF become spaces; max 200 bytes)
//! - `/slow?ms=..`  sleeps `ms` (default 5000, max 120000), then answers like `/ping`
//! - `/error`       500 `0|server error test`
//! - `/close`       reads the request, then closes without any response
//! - anything else  404 `0|not found`

mod http;

use http::{format_response, read_request, Request};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};

const DEFAULT_BIND: &str = "127.0.0.1:8766";
const HELLO: &str = "1|Hello from the Calradia AI server!";
const LOG_BODY_MAX: usize = 1024;
const ECHO_MAX: usize = 200;

fn main() {
    let bind = match parse_args(env::args().skip(1)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}\nusage: calradia-server [--bind ADDR:PORT]");
            process::exit(2);
        }
    };
    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot listen on {bind}: {e}");
            process::exit(1);
        }
    };
    log(&format!("calradia-server listening on {bind}"));
    serve(listener);
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut bind = DEFAULT_BIND.to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => bind = args.next().ok_or("--bind needs ADDR:PORT")?,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(bind)
}

/// Accepts connections forever, one thread per connection.
fn serve(listener: TcpListener) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || handle(stream));
            }
            Err(e) => log(&format!("accept error: {e}")),
        }
    }
}

enum Reply {
    Send(u16, String),
    Close,
}

fn route(req: &Request) -> Reply {
    match req.path.as_str() {
        "/" | "/ping" => Reply::Send(200, HELLO.to_string()),
        "/echo" => {
            let msg = req.param("msg").unwrap_or("(no msg)");
            let mut msg = msg.replace(['|', '\r', '\n'], " ");
            let mut end = msg.len().min(ECHO_MAX);
            while !msg.is_char_boundary(end) {
                end -= 1;
            }
            msg.truncate(end);
            Reply::Send(200, format!("1|{msg}"))
        }
        "/slow" => {
            let ms = req.param("ms").and_then(|v| v.parse::<u64>().ok());
            thread::sleep(Duration::from_millis(ms.unwrap_or(5000).min(120_000)));
            Reply::Send(200, HELLO.to_string())
        }
        "/error" => Reply::Send(500, "0|server error test".to_string()),
        "/close" => Reply::Close,
        _ => Reply::Send(404, "0|not found".to_string()),
    }
}

fn handle(mut stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "?".to_string(), |a| a.to_string());
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => return log(&format!("{peer} bad request: {e}; closing")),
    };
    log(&describe(&peer, &req));
    match route(&req) {
        Reply::Close => log(&format!("{peer} {} -> closed without response", req.path)),
        Reply::Send(status, body) => match stream.write_all(&format_response(status, &body)) {
            Ok(()) => log(&format!("{peer} {} -> {status} {body:?}", req.path)),
            Err(e) => log(&format!("{peer} {} -> write error: {e}", req.path)),
        },
    }
}

fn describe(peer: &str, req: &Request) -> String {
    let mut s = format!(
        "request from {peer}\n  {} {}\n  path: {:?}\n",
        req.method, req.target, req.path
    );
    for (k, v) in &req.query {
        s += &format!("  query: {k:?} = {v:?}\n");
    }
    for (k, v) in &req.headers {
        s += &format!("  header: {k}: {v}\n");
    }
    let shown = &req.body[..req.body.len().min(LOG_BODY_MAX)];
    s += &format!(
        "  body ({} bytes): {:?}",
        req.body.len(),
        String::from_utf8_lossy(shown)
    );
    s
}

/// Prints one timestamped block to stdout atomically and flushes.
fn log(msg: &str) {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "[{}.{:03}] {msg}", t.as_secs(), t.subsec_millis());
    let _ = out.flush();
}

#[cfg(test)]
mod tests;
