use super::*;
use std::io::Read;

fn start() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || serve(listener));
    addr
}

fn exchange(addr: std::net::SocketAddr, parts: &[&str]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    for p in parts {
        s.write_all(p.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(20));
    }
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn get(addr: std::net::SocketAddr, target: &str) -> String {
    exchange(
        addr,
        &[&format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n")],
    )
}

fn body_of(req: &str) -> String {
    let req = http::read_request(&mut req.as_bytes()).unwrap();
    match route(&req) {
        Reply::Send(_, b) => b,
        Reply::Close => "<close>".into(),
    }
}

#[test]
fn echo_sanitizes_and_truncates() {
    assert_eq!(
        body_of("GET /echo?msg=a%7Cb%0D%0Ac HTTP/1.1\r\n\r\n"),
        "1|a b  c"
    );
    assert_eq!(body_of("GET /echo HTTP/1.1\r\n\r\n"), "1|(no msg)");
    let long = "%C3%A9".repeat(150); // 300 bytes of 2-byte chars
    let body = body_of(&format!("GET /echo?msg={long} HTTP/1.1\r\n\r\n"));
    assert_eq!(body.len(), 2 + ECHO_MAX);
    assert_eq!(body_of("GET /close HTTP/1.1\r\n\r\n"), "<close>");
}

#[test]
fn parses_args() {
    let a = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
    assert_eq!(a(&[]).unwrap(), DEFAULT_BIND);
    assert_eq!(a(&["--bind", "0.0.0.0:9"]).unwrap(), "0.0.0.0:9");
    assert!(a(&["--bind"]).is_err());
    assert!(a(&["--nope"]).is_err());
}

#[test]
fn serves_routes_over_tcp() {
    let addr = start();
    let ping = get(addr, "/ping");
    assert!(ping.starts_with("HTTP/1.1 200 OK\r\n"), "{ping}");
    assert!(
        ping.contains(&format!("Content-Length: {}\r\n", HELLO.len()))
            && ping.contains("Connection: close\r\n")
    );
    assert!(ping.ends_with("\r\n\r\n1|Hello from the Calradia AI server!"));

    assert!(get(addr, "/echo?msg=a%20b|c").ends_with("\r\n\r\n1|a b c"));
    let split = exchange(
        addr,
        &["GET /echo?msg=sp", "lit HTTP/1.1\r\nHo", "st: t\r\n\r\n"],
    );
    assert!(split.ends_with("\r\n\r\n1|split"), "{split}");

    let err = get(addr, "/error");
    assert!(err.starts_with("HTTP/1.1 500 ") && err.ends_with("\r\n\r\n0|server error test"));
    let nf = get(addr, "/nope");
    assert!(nf.starts_with("HTTP/1.1 404 ") && nf.ends_with("\r\n\r\n0|not found"));

    assert_eq!(get(addr, "/close"), "");
    assert_eq!(exchange(addr, &["garbage\r\n\r\n"]), "");
    // The server is still alive after malformed input.
    assert!(get(addr, "/").ends_with("1|Hello from the Calradia AI server!"));
}
