//! Server tests: protocol v1 end to end over TCP, against a scripted fake upstream.
//! Every response body is checked against the frame guarantees by `parse_frame`.

use super::*;
use crate::protocol::*;
use crate::upstream::parse_endpoint;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;

// ---------------------------------------------------------------- fake upstream

#[derive(Clone)]
enum Up {
    /// 200 with a completion whose content is this text, sized by Content-Length.
    Reply(&'static str),
    /// Like `Reply`, but with an owned text.
    ReplyOwned(String),
    /// Like `Reply`, after a pause.
    Slow(u64, &'static str),
    /// Like `ReplyOwned`, with a chunked body.
    Chunked(String),
    /// 200 with this body as-is.
    Body(String),
    /// This status, with a small JSON error body.
    Status(u16),
    /// Read the request and never answer; count the connection once the client closes it.
    Hang,
}

struct Fake {
    url: String,
    /// Requests fully received.
    hits: Arc<AtomicUsize>,
    /// Hung connections that the client closed.
    closed: Arc<AtomicUsize>,
    /// (path, JSON body) of every request.
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Fake {
    fn hits(&self) -> usize {
        self.hits.load(SeqCst)
    }
    fn closed(&self) -> usize {
        self.closed.load(SeqCst)
    }
    fn request(&self, i: usize) -> (String, Value) {
        self.requests.lock().unwrap()[i].clone()
    }
    fn user_text(&self, i: usize) -> String {
        self.request(i).1["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn system_prompt(&self, i: usize) -> String {
        self.request(i).1["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// Starts a fake upstream; connection i gets `script[i]`, and the last entry repeats.
fn fake(script: Vec<Up>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let f = Fake {
        url: format!("http://{}/v1", listener.local_addr().unwrap()),
        hits: Arc::default(),
        closed: Arc::default(),
        requests: Arc::default(),
    };
    let (hits, closed, requests) = (f.hits.clone(), f.closed.clone(), f.requests.clone());
    thread::spawn(move || {
        for (i, conn) in listener.incoming().enumerate() {
            let Ok(stream) = conn else { continue };
            let mode = script[i.min(script.len() - 1)].clone();
            let (hits, closed, requests) = (hits.clone(), closed.clone(), requests.clone());
            thread::spawn(move || serve_fake(stream, mode, &hits, &closed, &requests));
        }
    });
    f
}

fn completion(content: &str) -> String {
    json!({
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": content,
                "reasoning_content": "The captain asks about the road. Be brief.",
            },
        }],
    })
    .to_string()
}

fn ok_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn serve_fake(
    mut s: TcpStream,
    mode: Up,
    hits: &AtomicUsize,
    closed: &AtomicUsize,
    requests: &Mutex<Vec<(String, Value)>>,
) {
    let Ok(req) = http::read_request(&mut s) else {
        return;
    };
    let body = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    requests.lock().unwrap().push((req.path.clone(), body));
    hits.fetch_add(1, SeqCst);
    let reply = match mode {
        Up::Reply(text) => ok_response(&completion(text)),
        Up::ReplyOwned(text) => ok_response(&completion(&text)),
        Up::Slow(ms, text) => {
            thread::sleep(Duration::from_millis(ms));
            ok_response(&completion(text))
        }
        Up::Chunked(text) => {
            let json = completion(&text);
            let mut r = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            for (i, piece) in json.as_bytes().chunks(4000).enumerate() {
                let ext = if i == 0 { ";name=value" } else { "" };
                r.extend(format!("{:x}{ext}\r\n", piece.len()).as_bytes());
                r.extend(piece);
                r.extend(b"\r\n");
            }
            r.extend(b"0\r\nX-Trailer: yes\r\n\r\n");
            r
        }
        Up::Body(body) => ok_response(&body),
        Up::Status(code) => format!(
            "HTTP/1.1 {code} Oops\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{{\"error\":\"no\"}}"
        )
        .into_bytes(),
        Up::Hang => {
            let mut buf = [0u8; 64];
            while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
            closed.fetch_add(1, SeqCst);
            return;
        }
    };
    let _ = s.write_all(&reply);
}

/// A URL where nothing listens.
fn refused_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}/v1", listener.local_addr().unwrap())
}

// ---------------------------------------------------------------- server under test

fn limits() -> Limits {
    Limits {
        max_jobs: 64,
        queue_len: 8,
        ttl: Duration::from_secs(60),
        deadline: Duration::from_secs(5),
    }
}

const fn test_npc(id: u32) -> Npc {
    Npc {
        id,
        name: "Tester",
        identity: "a test NPC",
        personality: "plain",
        style: "plain",
    }
}

/// Several NPCs, so the queue can hold more than one PENDING job.
static TEST_NPCS: [Npc; 6] = [
    test_npc(1),
    test_npc(2),
    test_npc(3),
    test_npc(4),
    test_npc(5),
    test_npc(6),
];

fn server(url: &str, limits: Limits, npcs: &'static [Npc]) -> SocketAddr {
    let backend = Backend::Upstream {
        endpoint: parse_endpoint(url).unwrap(),
        model: "test-model".into(),
        connect_timeout: Duration::from_secs(2),
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || start(listener, limits, backend, None, npcs));
    addr
}

fn server_for(f: &Fake) -> SocketAddr {
    server(&f.url, limits(), npc::NPCS)
}

// ---------------------------------------------------------------- client

#[derive(Debug, PartialEq)]
struct Frame {
    rid: u32,
    code: u8,
    text: String,
}

fn fr(rid: u32, code: u8, text: &str) -> Frame {
    Frame {
        rid,
        code,
        text: text.to_string(),
    }
}

/// Sends raw bytes and returns the body of a 200 response.
fn exchange(addr: SocketAddr, request: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(request).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").expect("no response head");
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(head.contains(&format!("Content-Length: {}\r\n", body.len())));
    body.to_string()
}

/// A GET like the engine's: only Host and Accept.
fn get(addr: SocketAddr, target: &str) -> String {
    let req = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:8766\r\nAccept: */*\r\n\r\n");
    exchange(addr, req.as_bytes())
}

/// Parses a body as `R|C|T|R`, asserting every guarantee of the frame format.
fn parse_frame(body: &str) -> Frame {
    assert!(body.len() <= 522, "frame of {} bytes", body.len());
    let parts: Vec<&str> = body.split('|').collect();
    assert_eq!(parts.len(), 4, "{body:?}");
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    assert!(
        digits(parts[0]) && parts[0] == parts[3],
        "R fields {body:?}"
    );
    assert!(
        parts[1].len() == 1 && (b'0'..=b'6').contains(&parts[1].as_bytes()[0]),
        "C field {body:?}"
    );
    let t = parts[2];
    assert!(
        t.bytes().any(|b| b.is_ascii_alphabetic()),
        "T without a letter {body:?}"
    );
    assert!(
        t.bytes()
            .all(|b| (0x20..0x7f).contains(&b) && !b"|{}^".contains(&b)),
        "T with a forbidden byte {body:?}"
    );
    assert!(t.len() <= MAX_TEXT && t.trim() == t, "T {body:?}");
    Frame {
        rid: parts[0].parse().unwrap(),
        code: parts[1].parse().unwrap(),
        text: t.to_string(),
    }
}

/// Percent-encodes like the engine with encode_url = 1: every byte outside [0-9A-Za-z].
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn talk_url(rid: u32, job: u32, npc: u32, day: u32, pname: &str, msg: &str) -> String {
    format!(
        "/v1/talk?v=1&rid={rid}&job={job}&npc={npc}&day={day}&pname={}&msg={}&end=1",
        enc(pname),
        enc(msg)
    )
}

fn talk_npc(addr: SocketAddr, job: u32, npc: u32, msg: &str) -> Frame {
    let rid = job + 1000;
    let f = parse_frame(&get(addr, &talk_url(rid, job, npc, 12, "Ragnar", msg)));
    assert_eq!(f.rid, rid);
    f
}

fn talk(addr: SocketAddr, job: u32, msg: &str) -> Frame {
    talk_npc(addr, job, 1, msg)
}

fn result(addr: SocketAddr, job: u32) -> Frame {
    let f = parse_frame(&get(
        addr,
        &format!("/v1/result?v=1&rid=77&job={job}&end=1"),
    ));
    assert_eq!(f.rid, 77);
    f
}

fn cancel(addr: SocketAddr, job: u32) -> Frame {
    let f = parse_frame(&get(
        addr,
        &format!("/v1/cancel?v=1&rid=78&job={job}&end=1"),
    ));
    assert_eq!(f.rid, 78);
    f
}

/// Polls /v1/result until the job leaves PENDING, checking every poll answers within 1 s.
fn wait_done(addr: SocketAddr, job: u32) -> Frame {
    let start = Instant::now();
    loop {
        let t0 = Instant::now();
        let f = result(addr, job);
        let took = t0.elapsed();
        assert!(took < Duration::from_secs(1), "/v1/result took {took:?}");
        if f.code != CODE_PENDING {
            return f;
        }
        assert!(start.elapsed() < Duration::from_secs(8), "job {job} stuck");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "timed out: {what}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Talks and waits for the outcome; used where jobs run one after another.
fn talk_and_wait(addr: SocketAddr, job: u32, msg: &str) -> Frame {
    assert_eq!(
        talk(addr, job, msg),
        fr(job + 1000, CODE_PENDING, "pending")
    );
    wait_done(addr, job)
}

// ---------------------------------------------------------------- outcomes

#[test]
fn success_round_trip_and_upstream_request_shape() {
    let up = fake(vec![Up::Reply("Aye, captain. The road to Praven is mud.")]);
    let addr = server_for(&up);
    let ready = fr(77, CODE_READY, "Aye, captain. The road to Praven is mud.");
    assert_eq!(talk_and_wait(addr, 21, "How is the road?"), ready);
    // READY answers repeatedly.
    assert_eq!(result(addr, 21), ready);
    assert_eq!(up.hits(), 1);

    let (path, body) = up.request(0);
    assert_eq!(path, "/v1/chat/completions");
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 220);
    assert_eq!(body["temperature"], 0.8);
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"enable_thinking": false})
    );
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(
        body["messages"][1],
        json!({"role": "user", "content": "How is the road?"})
    );
    let system = up.system_prompt(0);
    for part in [
        "You are Hrodvar",
        "Calradia",
        "your captain, Ragnar.",
        "day 12",
        "AI",
    ] {
        assert!(system.contains(part), "{part:?} missing from {system:?}");
    }
}

#[test]
fn upstream_unavailable_fails() {
    let addr = server(&refused_url(), limits(), npc::NPCS);
    assert_eq!(
        talk_and_wait(addr, 1, "Hello"),
        fr(77, CODE_FAILED, "upstream_unavailable")
    );
}

#[test]
fn hung_upstream_times_out_while_handlers_answer_fast() {
    let up = fake(vec![Up::Hang]);
    let short = Limits {
        deadline: Duration::from_millis(1000),
        ..limits()
    };
    let addr = server(&up.url, short, npc::NPCS);
    let start = Instant::now();
    talk(addr, 1, "Hello");
    wait_until("upstream reached", || up.hits() == 1);
    // wait_done asserts that each poll answers within 1 s while the upstream hangs.
    assert_eq!(wait_done(addr, 1), fr(77, CODE_FAILED, "timeout"));
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(950) && took < Duration::from_secs(3),
        "{took:?}"
    );
    wait_until("upstream socket closed", || up.closed() == 1);
}

#[test]
fn malformed_and_error_responses_fail() {
    let up = fake(vec![
        Up::Body("{not json".into()),
        Up::Status(500),
        Up::Body("{\"choices\":[]}".into()),
        Up::Body("{\"choices\":[{\"message\":{\"content\":7}}]}".into()),
    ]);
    let addr = server_for(&up);
    for job in 1..=4 {
        assert_eq!(
            talk_and_wait(addr, job, "Hello"),
            fr(77, CODE_FAILED, "upstream_error"),
            "job {job}"
        );
    }
}

#[test]
fn empty_and_letterless_replies_fail_as_empty_reply() {
    let up = fake(vec![
        Up::Reply(""),
        Up::Reply("12345 678"),
        Up::Body("{\"choices\":[{\"message\":{\"content\":null}}]}".into()),
        Up::Reply("<think>Only thinking, no answer.</think>"),
        Up::Reply("\u{1F37A} \u{4F60}\u{597D} ... 42"),
    ]);
    let addr = server_for(&up);
    for job in 1..=5 {
        assert_eq!(
            talk_and_wait(addr, job, "Hello"),
            fr(77, CODE_FAILED, "empty_reply"),
            "job {job}"
        );
    }
}

#[test]
fn think_blocks_are_stripped_and_chunked_bodies_decoded() {
    let up = fake(vec![
        Up::Reply("<think>He wants news.</think>Aye. Rain by nightfall."),
        Up::Chunked(format!("Coin is thin, captain. {}", "Aye. ".repeat(2000))),
    ]);
    let addr = server_for(&up);
    assert_eq!(
        talk_and_wait(addr, 1, "Weather?"),
        fr(77, CODE_READY, "Aye. Rain by nightfall.")
    );
    let f = talk_and_wait(addr, 2, "Coin?");
    assert_eq!(f.code, CODE_READY);
    assert!(
        f.text.starts_with("Coin is thin, captain. Aye. Aye."),
        "{f:?}"
    );
}

#[test]
fn non_ascii_reply_is_transliterated() {
    let up = fake(vec![Up::Reply(
        "\u{201C}Aye,\u{201D} said he \u{2014} it\u{2019}s a long road\u{2026} to the \
         caf\u{E9} in Sargoth \u{1F37A} \u{4F60}\u{597D}, Stra\u{DF}e \u{C6}sir",
    )]);
    let addr = server_for(&up);
    assert_eq!(
        talk_and_wait(addr, 1, "Hello"),
        fr(
            77,
            CODE_READY,
            "\"Aye,\" said he - it's a long road... to the cafe in Sargoth , Strasse AEsir"
        )
    );
}

#[test]
fn oversized_reply_is_cut_at_a_word_boundary() {
    let long = "Hrodvar talks at great length about the road. ".repeat(220);
    assert!(long.len() > 10_000);
    let up = fake(vec![Up::ReplyOwned(long.clone())]);
    let addr = server_for(&up);
    talk(addr, 1, "Tell me everything.");
    wait_done(addr, 1);
    let body = get(addr, "/v1/result?v=1&rid=999999999&job=1&end=1");
    // The trailing R survives the cut.
    assert!(body.ends_with("...|999999999"), "{body}");
    let f = parse_frame(&body);
    assert_eq!(f.code, CODE_READY);
    assert!(f.text.len() <= MAX_TEXT && f.text.len() > 400);
    let kept = f.text.strip_suffix("...").unwrap();
    assert!(long.starts_with(kept));
    assert_eq!(long.as_bytes()[kept.len()], b' ', "cut inside a word");
}

#[test]
fn oversized_upstream_body_fails() {
    let big = "a".repeat(300 * 1024);
    let up = fake(vec![
        Up::Body(format!(
            "{{\"choices\":[{{\"message\":{{\"content\":\"{big}\"}}}}]}}"
        )),
        Up::Chunked(big),
    ]);
    let addr = server_for(&up);
    for job in 1..=2 {
        assert_eq!(
            talk_and_wait(addr, job, "Hello"),
            fr(77, CODE_FAILED, "upstream_error"),
            "job {job}"
        );
    }
}

// ---------------------------------------------------------------- job semantics

#[test]
fn duplicate_job_id_is_idempotent() {
    let up = fake(vec![Up::Slow(300, "Hello, captain.")]);
    let addr = server_for(&up);
    let pending = fr(1005, CODE_PENDING, "pending");
    assert_eq!(talk(addr, 5, "Hi"), pending);
    assert_eq!(talk(addr, 5, "Hi"), pending);
    assert_eq!(wait_done(addr, 5), fr(77, CODE_READY, "Hello, captain."));
    assert_eq!(talk(addr, 5, "Hi"), fr(1005, CODE_READY, "Hello, captain."));
    assert_eq!(up.hits(), 1);
}

#[test]
fn same_job_id_with_other_parameters_conflicts() {
    let up = fake(vec![Up::Slow(300, "Hello, captain.")]);
    let addr = server_for(&up);
    assert_eq!(talk(addr, 5, "Hi").code, CODE_PENDING);
    let conflict = fr(1005, CODE_BAD_REQUEST, "conflict");
    assert_eq!(talk(addr, 5, "Hi there"), conflict);
    let other_day = parse_frame(&get(addr, &talk_url(1005, 5, 1, 13, "Ragnar", "Hi")));
    assert_eq!(other_day, conflict);
    let other_name = parse_frame(&get(addr, &talk_url(1005, 5, 1, 12, "Rolf", "Hi")));
    assert_eq!(other_name, conflict);
    // The original job is untouched.
    assert_eq!(wait_done(addr, 5), fr(77, CODE_READY, "Hello, captain."));
    assert_eq!(up.hits(), 1);
}

#[test]
fn new_job_supersedes_pending_job_of_same_npc() {
    let up = fake(vec![Up::Hang, Up::Reply("Second answer.")]);
    let addr = server_for(&up);
    talk(addr, 1, "First");
    wait_until("first job running", || up.hits() == 1);
    assert_eq!(talk(addr, 2, "Second").code, CODE_PENDING);
    assert_eq!(result(addr, 1), fr(77, CODE_CANCELED, "superseded"));
    wait_until("first upstream socket closed", || up.closed() == 1);
    assert_eq!(wait_done(addr, 2), fr(77, CODE_READY, "Second answer."));
    assert_eq!(up.user_text(1), "Second");
}

#[test]
fn cancel_running_job_shuts_down_its_upstream_socket() {
    let up = fake(vec![Up::Hang, Up::Reply("Next answer.")]);
    let addr = server_for(&up);
    talk(addr, 1, "First");
    wait_until("job running", || up.hits() == 1);
    let t0 = Instant::now();
    assert_eq!(cancel(addr, 1), fr(78, CODE_CANCELED, "canceled"));
    assert!(t0.elapsed() < Duration::from_secs(1));
    wait_until("upstream socket closed", || up.closed() == 1);
    assert_eq!(result(addr, 1), fr(77, CODE_CANCELED, "canceled"));
    // The worker is free again.
    assert_eq!(
        talk_and_wait(addr, 2, "Next"),
        fr(77, CODE_READY, "Next answer.")
    );
}

#[test]
fn cancel_queued_job_never_reaches_upstream() {
    let up = fake(vec![Up::Hang, Up::Reply("Third answer.")]);
    let addr = server(&up.url, limits(), &TEST_NPCS);
    talk_npc(addr, 1, 1, "First");
    wait_until("first job running", || up.hits() == 1);
    assert_eq!(talk_npc(addr, 2, 2, "Queued").code, CODE_PENDING);
    assert_eq!(cancel(addr, 2), fr(78, CODE_CANCELED, "canceled"));
    assert_eq!(cancel(addr, 1), fr(78, CODE_CANCELED, "canceled"));
    wait_until("first upstream socket closed", || up.closed() == 1);
    assert_eq!(talk_npc(addr, 3, 2, "Third").code, CODE_PENDING);
    assert_eq!(wait_done(addr, 3), fr(77, CODE_READY, "Third answer."));
    assert_eq!(up.hits(), 2);
    assert_eq!(up.user_text(1), "Third");
}

#[test]
fn cancel_terminal_and_unknown_jobs() {
    let up = fake(vec![Up::Reply("Done."), Up::Body("{bad".into()), Up::Hang]);
    let addr = server_for(&up);
    // READY stays READY; the answer carries code 0 and, like every C = 0 frame, the reply.
    assert_eq!(talk_and_wait(addr, 1, "One").code, CODE_READY);
    assert_eq!(cancel(addr, 1), fr(78, CODE_READY, "Done."));
    assert_eq!(result(addr, 1), fr(77, CODE_READY, "Done."));
    // FAILED stays FAILED.
    assert_eq!(talk_and_wait(addr, 2, "Two").code, CODE_FAILED);
    assert_eq!(cancel(addr, 2), fr(78, CODE_FAILED, "upstream_error"));
    // CANCELED stays CANCELED.
    talk(addr, 3, "Three");
    assert_eq!(cancel(addr, 3), fr(78, CODE_CANCELED, "canceled"));
    assert_eq!(cancel(addr, 3), fr(78, CODE_CANCELED, "canceled"));
    // Unknown.
    assert_eq!(cancel(addr, 999), fr(78, CODE_UNKNOWN_JOB, "unknown_job"));
    assert_eq!(result(addr, 999), fr(77, CODE_UNKNOWN_JOB, "unknown_job"));
}

#[test]
fn full_queue_answers_busy_without_side_effects() {
    let up = fake(vec![Up::Hang]);
    let small = Limits {
        queue_len: 2,
        ..limits()
    };
    let addr = server(&up.url, small, &TEST_NPCS);
    talk_npc(addr, 1, 1, "Running");
    wait_until("job 1 running", || up.hits() == 1);
    assert_eq!(talk_npc(addr, 2, 2, "Queued").code, CODE_PENDING);
    assert_eq!(talk_npc(addr, 3, 3, "Queued").code, CODE_PENDING);
    assert_eq!(
        talk_npc(addr, 4, 4, "Too many"),
        fr(1004, CODE_BUSY, "busy")
    );
    assert_eq!(result(addr, 4).code, CODE_UNKNOWN_JOB);
    assert_eq!(result(addr, 2).code, CODE_PENDING);
    // A new job for NPC 2 supersedes job 2, which frees its queue slot.
    assert_eq!(talk_npc(addr, 5, 2, "Replacement").code, CODE_PENDING);
    assert_eq!(result(addr, 2), fr(77, CODE_CANCELED, "superseded"));
}

#[test]
fn full_store_answers_busy_then_evicts_oldest_terminal_job() {
    let up = fake(vec![Up::Hang]);
    let small = Limits {
        max_jobs: 3,
        ..limits()
    };
    let addr = server(&up.url, small, &TEST_NPCS);
    talk_npc(addr, 1, 1, "Running");
    wait_until("job 1 running", || up.hits() == 1);
    talk_npc(addr, 2, 2, "Queued");
    talk_npc(addr, 3, 3, "Queued");
    assert_eq!(
        talk_npc(addr, 4, 4, "Too many"),
        fr(1004, CODE_BUSY, "busy")
    );
    assert_eq!(cancel(addr, 3).code, CODE_CANCELED);
    assert_eq!(cancel(addr, 2).code, CODE_CANCELED);
    // Jobs 3 then 2 are terminal: job 3 finished first, so it is evicted.
    assert_eq!(talk_npc(addr, 4, 4, "Now it fits").code, CODE_PENDING);
    assert_eq!(result(addr, 3).code, CODE_UNKNOWN_JOB);
    assert_eq!(result(addr, 2).code, CODE_CANCELED);
    assert_eq!(result(addr, 1).code, CODE_PENDING);
}

#[test]
fn finished_jobs_expire_after_ttl() {
    let up = fake(vec![Up::Reply("Hello.")]);
    let short = Limits {
        ttl: Duration::from_millis(300),
        ..limits()
    };
    let addr = server(&up.url, short, npc::NPCS);
    assert_eq!(talk_and_wait(addr, 1, "Hi").code, CODE_READY);
    assert_eq!(result(addr, 1).code, CODE_READY);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(result(addr, 1), fr(77, CODE_UNKNOWN_JOB, "unknown_job"));
    assert_eq!(cancel(addr, 1), fr(78, CODE_UNKNOWN_JOB, "unknown_job"));
}

#[test]
fn restarted_server_knows_no_old_job() {
    let up = fake(vec![Up::Reply("Hello.")]);
    let first = server_for(&up);
    assert_eq!(talk_and_wait(first, 42, "Hi").code, CODE_READY);
    let second = server_for(&up);
    assert_eq!(result(second, 42), fr(77, CODE_UNKNOWN_JOB, "unknown_job"));
    assert_eq!(cancel(second, 42), fr(78, CODE_UNKNOWN_JOB, "unknown_job"));
}

// ---------------------------------------------------------------- request validation

#[test]
fn request_validation() {
    let up = fake(vec![Up::Reply("Fine.")]);
    let addr = server_for(&up);
    let ask = |target: &str| parse_frame(&get(addr, target));
    let bad = |rid: u32, reason: &str| fr(rid, CODE_BAD_REQUEST, reason);

    // end=1 missing (URL cut short): truncated, rid still echoed.
    assert_eq!(
        ask("/v1/talk?v=1&rid=5&job=6&npc=1&day=1&pname=A&msg=hel"),
        bad(5, "truncated")
    );
    assert_eq!(ask("/v1/result?v=1&rid=5&job=6&end=0"), bad(5, "truncated"));
    assert_eq!(ask("/v1/cancel?v=1&rid=5&job=6"), bad(5, "truncated"));
    // Version.
    assert_eq!(
        ask("/v1/result?v=2&rid=5&job=6&end=1"),
        bad(5, "bad_version")
    );
    assert_eq!(ask("/v1/result?rid=5&job=6&end=1"), bad(5, "bad_version"));
    // Bad rid: R = 0.
    for rid in ["0", "abc", "1000000000", "-1", "+5", "", "99999999999"] {
        assert_eq!(
            ask(&format!("/v1/result?v=1&rid={rid}&job=6&end=1")),
            bad(0, "bad_param"),
            "rid={rid}"
        );
    }
    assert_eq!(ask("/v1/result?v=1&job=6&end=1"), bad(0, "bad_param"));
    // Bad job, npc, day.
    assert_eq!(ask("/v1/result?v=1&rid=5&job=0&end=1"), bad(5, "bad_param"));
    assert_eq!(ask("/v1/cancel?v=1&rid=5&end=1"), bad(5, "bad_param"));
    assert_eq!(ask(&talk_url(5, 6, 2, 1, "A", "Hi")), bad(5, "bad_param"));
    assert_eq!(
        ask(&talk_url(5, 6, 1, 100001, "A", "Hi")),
        bad(5, "bad_param")
    );
    assert_eq!(
        ask("/v1/talk?v=1&rid=5&job=6&npc=1&pname=A&msg=Hi&end=1"),
        bad(5, "bad_param")
    );
    // Unknown routes.
    assert_eq!(ask("/v1/nope?v=1&rid=5&job=6&end=1"), bad(5, "bad_param"));
    assert_eq!(ask("/"), bad(0, "bad_param"));
    // msg length is checked after sanitizing: rejected, never truncated.
    assert_eq!(
        ask(&talk_url(5, 6, 1, 1, "A", &"a".repeat(301))),
        bad(5, "too_long")
    );
    assert_eq!(ask(&talk_url(5, 6, 1, 1, "A", "")), bad(5, "empty_msg"));
    assert_eq!(
        ask(&talk_url(5, 6, 1, 1, "A", " \t\r\n\u{1}")),
        bad(5, "empty_msg")
    );
    assert_eq!(
        ask("/v1/talk?v=1&rid=5&job=6&npc=1&day=1&pname=A&end=1"),
        bad(5, "empty_msg")
    );
    // Raw target over 4096 bytes.
    let long = format!("{}&x={}", talk_url(5, 6, 1, 1, "A", "Hi"), "a".repeat(4096));
    assert_eq!(ask(&long), bad(5, "too_long"));
    // Not HTTP at all: still a frame, and the server keeps serving.
    assert_eq!(
        parse_frame(&exchange(addr, b"garbage\r\n\r\n")),
        bad(0, "bad_param")
    );
    assert_eq!(up.hits(), 0);

    // Limits that are accepted: 300 characters, day 100000.
    let msg300 = "b".repeat(300);
    let f = ask(&talk_url(5, 7, 1, MAX_DAY, "A", &msg300));
    assert_eq!(f, fr(5, CODE_PENDING, "pending"));
    wait_done(addr, 7);
    assert_eq!(up.user_text(0), msg300);
    assert!(up.system_prompt(0).contains("day 100000"));
}

#[test]
fn engine_encoded_text_round_trips_through_input_sanitizing() {
    let up = fake(vec![Up::Reply("Fine.")]);
    let addr = server_for(&up);
    let cases = [
        (
            "Hello, Hrodvar! How's the road to Praven? (50% mud)",
            "Hello, Hrodvar! How's the road to Praven? (50% mud)",
        ),
        ("{s0}|&%^+", "{s0}/&% +"),
        ("a=b&c=d?e#f/g\\h~_", "a=b&c=d?e#f/g\\h~_"),
        ("\u{E9}t\u{E9} \u{201C}ok\u{201D} \u{1F37A}", "ete \"ok\" ?"),
        ("  tabs\tand\nnewlines  ", "tabs and newlines"),
    ];
    for (i, (sent, seen)) in cases.iter().enumerate() {
        let job = i as u32 + 1;
        let target = talk_url(
            job + 1000,
            job,
            1,
            3,
            "Sir Reginald Fitzwilliam the Third",
            sent,
        );
        assert!(target
            .split('?')
            .nth(1)
            .unwrap()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"=&%".contains(&b)));
        assert_eq!(parse_frame(&get(addr, &target)).code, CODE_PENDING);
        assert_eq!(wait_done(addr, job).code, CODE_READY);
        assert_eq!(up.user_text(i), *seen, "sent {sent:?}");
    }
    // pname is cut to 32 characters.
    assert!(up
        .system_prompt(0)
        .contains("your captain, Sir Reginald Fitzwilliam the Thi."));
    // A literal '+' (not sent by the engine, which encodes it) decodes as a space.
    get(
        addr,
        "/v1/talk?v=1&rid=9&job=9&npc=1&day=3&pname=A&msg=Hail+there&end=1",
    );
    wait_done(addr, 9);
    assert_eq!(up.user_text(cases.len()), "Hail there");
}

// ---------------------------------------------------------------- frame property

/// xorshift64*, enough to generate varied inputs without a dependency.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64) as usize
    }
}

#[test]
fn every_frame_is_well_formed() {
    const PIECES: &[&str] = &[
        "a",
        "Z",
        "word ",
        "Hrodvar ",
        " ",
        "  ",
        "\t",
        "\n",
        "\r\n",
        "|",
        "{",
        "}",
        "^",
        "{s0}",
        "<think>",
        "</think>",
        "<think>x</think>",
        "\u{E9}",
        "\u{DF}",
        "\u{C6}",
        "\u{201C}",
        "\u{2019}",
        "\u{2014}",
        "\u{2026}",
        "\u{A0}",
        "\u{2028}",
        "\u{1F37A}",
        "\u{4F60}\u{597D}",
        "\u{0}",
        "\u{7F}",
        "\u{85}",
        "\u{FFFD}",
        "\u{A1}",
        "0",
        "123",
        "-",
        "...",
        "%",
        "&",
        "=",
        "+",
        "_",
        "?",
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..5000 {
        let pieces = if rng.below(4) == 0 { 400 } else { 30 };
        let n = rng.below(pieces);
        let text: String = (0..n).map(|_| PIECES[rng.below(PIECES.len())]).collect();
        let rid = match rng.below(4) {
            0 => 0,
            1 => RID_MAX,
            2 => rng.below(u32::MAX as usize) as u32,
            _ => rng.below(1000) as u32 + 1,
        };
        let code = rng.below(7) as u8;
        let f = parse_frame(&frame(rid, code, &text));
        assert_eq!(f.rid, if rid <= RID_MAX { rid } else { 0 });
        assert!(
            f.code == code || (f.code == CODE_FAILED && f.text == REASON_EMPTY_REPLY),
            "{text:?}"
        );
    }
    let tokens = [
        REASON_PENDING,
        REASON_TIMEOUT,
        REASON_UPSTREAM_UNAVAILABLE,
        REASON_UPSTREAM_ERROR,
        REASON_EMPTY_REPLY,
        REASON_SUPERSEDED,
        REASON_CANCELED,
        REASON_CONFLICT,
        REASON_TOO_LONG,
        REASON_EMPTY_MSG,
        REASON_TRUNCATED,
        REASON_BAD_VERSION,
        REASON_BAD_PARAM,
        REASON_UNKNOWN_JOB,
        REASON_BUSY,
    ];
    for token in tokens {
        for code in CODE_READY..=CODE_BUSY {
            assert_eq!(
                parse_frame(&frame(RID_MAX, code, token)),
                fr(RID_MAX, code, token)
            );
        }
    }
}

// ---------------------------------------------------------------- command line

#[test]
fn parses_args() {
    let no_env = |_: &str| None;
    let a = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()), no_env);
    let d = a(&[]).unwrap().unwrap();
    assert_eq!(d.bind, DEFAULT_BIND);
    assert_eq!(d.upstream, "http://172.17.0.1:8080/v1");
    assert_eq!(d.model, "calradia-qwen3.5-9b");
    assert_eq!((d.fake_llm, d.fault), (false, None));
    assert_eq!(d.limits, Limits::default());
    assert_eq!(d.limits.deadline, Duration::from_secs(90));

    let env = |k: &str| match k {
        "CALRADIA_UPSTREAM" => Some("http://10.0.0.2:9000/v1".to_string()),
        "CALRADIA_MODEL" => Some("m-env".to_string()),
        _ => None,
    };
    let e = parse_args(std::iter::empty(), env).unwrap().unwrap();
    assert_eq!(
        (e.upstream.as_str(), e.model.as_str()),
        ("http://10.0.0.2:9000/v1", "m-env")
    );
    let args = ["--model", "m-flag", "--upstream", "http://h/v1"].map(String::from);
    let f = parse_args(args.into_iter(), env).unwrap().unwrap();
    assert_eq!(
        (f.upstream.as_str(), f.model.as_str()),
        ("http://h/v1", "m-flag")
    );

    let o = a(&[
        "--bind",
        "0.0.0.0:9",
        "--fake-llm",
        "--fault",
        "oversize-2000",
    ])
    .unwrap()
    .unwrap();
    assert_eq!(o.bind, "0.0.0.0:9");
    assert_eq!((o.fake_llm, o.fault), (true, Some(Fault::Oversize(2000))));
    for (name, fault) in [
        ("hang", Fault::Hang),
        ("close", Fault::Close),
        ("empty", Fault::Empty),
        ("wrong-rid", Fault::WrongRid),
        ("malformed", Fault::Malformed),
        ("delay", Fault::Delay),
        ("nonascii", Fault::NonAscii),
    ] {
        assert_eq!(a(&["--fault", name]).unwrap().unwrap().fault, Some(fault));
    }
    let l = a(&[
        "--max-jobs",
        "3",
        "--queue-len",
        "2",
        "--ttl-secs",
        "5",
        "--deadline-secs",
        "10",
    ])
    .unwrap()
    .unwrap()
    .limits;
    assert_eq!((l.max_jobs, l.queue_len), (3, 2));
    assert_eq!(
        (l.ttl, l.deadline),
        (Duration::from_secs(5), Duration::from_secs(10))
    );

    let bad_args: [&[&str]; 6] = [
        &["--bind"],
        &["--nope"],
        &["--fault", "bogus"],
        &["--fault", "oversize-0"],
        &["--deadline-secs", "120"],
        &["--queue-len", "0"],
    ];
    for bad in bad_args {
        assert!(a(bad).is_err(), "{bad:?}");
    }
    assert!(a(&["--help"]).unwrap().is_none());
    let https = a(&["--upstream", "https://h/v1"]).unwrap().unwrap();
    assert!(backend_for(&https).is_err());
}
