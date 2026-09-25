//! Server tests: protocol v1 end to end over TCP, against a scripted fake upstream.
//! Every response body is checked against the frame guarantees by `parse_frame`.

use super::*;
use crate::protocol::*;
use crate::upstream::parse_endpoint;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
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

fn upstream_backend(url: &str) -> Backend {
    Backend::Upstream {
        endpoint: parse_endpoint(url).unwrap(),
        model: "test-model".into(),
        connect_timeout: Duration::from_secs(2),
    }
}

/// The shipped character profiles.
fn shipped_characters() -> Registry {
    Registry::load(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/characters"
    )))
    .unwrap()
}

/// The shipped log entry sentences.
fn shipped_templates() -> world::LogTemplates {
    world::LogTemplates::load(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/world/log_entries.toml"
    )))
    .unwrap()
}

/// The shipped kingdom lore.
fn shipped_realms() -> Realms {
    Realms::load(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/factions"
    )))
    .unwrap()
}

fn serve(runner: Runner, limits: Limits, npcs: &'static [Npc]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || start(listener, limits, runner, None, npcs));
    addr
}

fn server(url: &str, limits: Limits, npcs: &'static [Npc]) -> SocketAddr {
    let runner = Runner {
        backend: upstream_backend(url),
        characters: shipped_characters(),
        realms: shipped_realms(),
        templates: shipped_templates(),
        actions: true,
        autonomy: true,
        memory: Arc::new(Memory::in_memory()),
        log_prompts: false,
    };
    serve(runner, limits, npcs)
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
    assert!(body.get("response_format").is_none());
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

// ---------------------------------------------------------------- protocol v2: characters and memory

use crate::ids::ids;
use crate::memory::MAX_CHAIN;
use crate::prompt::MAX_PROMPT_CHARS;

/// A `/v2/talk`, with the values a new Swadian campaign might send to Borcha.
#[derive(Clone)]
struct V2 {
    camp: u32,
    conv: u32,
    head: u32,
    troop: &'static str,
    day: u32,
    fac: &'static str,
    pfac: &'static str,
    frel: i32,
    rel: i32,
    rep: u32,
    occ: u32,
    st: u32,
    ren: i32,
    hon: i32,
    loc: &'static str,
    ldist: u32,
    pg: u32,
    pname: &'static str,
    nname: &'static str,
    fname: &'static str,
    pfname: &'static str,
    lname: &'static str,
    /// Sent only when set, as by the current mod; None mimics an older build.
    wars: Option<u32>,
    ruler: &'static str,
    spouse: &'static str,
    father: &'static str,
    msg: String,
}

fn borcha(msg: &str) -> V2 {
    V2 {
        camp: 7,
        conv: 100,
        head: 0,
        troop: "trp_npc1",
        day: 12,
        fac: "fac_commoners",
        pfac: "fac_no_faction",
        frel: 0,
        rel: 4,
        rep: 8,
        occ: 5,
        st: ST_IN_PARTY,
        ren: 60,
        hon: -3,
        loc: "p_town_6",
        ldist: 5,
        pg: 1,
        pname: "Ylva",
        nname: "Borcha",
        fname: "Commoners",
        pfname: "",
        lname: "Praven",
        wars: Some(0),
        ruler: "",
        spouse: "",
        father: "",
        msg: msg.to_string(),
    }
}

fn harlaus(msg: &str) -> V2 {
    V2 {
        troop: "trp_kingdom_1_lord",
        nname: "King Harlaus",
        fac: "fac_kingdom_1",
        fname: "Kingdom of Swadia",
        rep: 0,
        occ: 2,
        st: ST_FACTION_LEADER,
        rel: -15,
        frel: -30,
        ldist: 0,
        // Bits: fac_kingdom_2 and fac_kingdom_3 (supporters faction is bit 0).
        wars: Some(1 << 2 | 1 << 3),
        spouse: "Lady Esmerelda",
        ..borcha(msg)
    }
}

fn v2_url(rid: u32, job: u32, v: &V2) -> String {
    let ids = ids();
    format!(
        "/v2/talk?v=2&rid={rid}&job={job}&camp={}&conv={}&head={}&troop={}&day={}&fac={}\
         &pfac={}&frel={}&rel={}&rep={}&occ={}&st={}&ren={}&hon={}&loc={}&ldist={}&pg={}\
         {}&pname={}&nname={}&fname={}&pfname={}&lname={}{}&msg={}&end=1",
        v.camp,
        v.conv,
        v.head,
        ids.troops.index(v.troop).unwrap(),
        v.day,
        ids.factions.index(v.fac).unwrap(),
        ids.factions.index(v.pfac).unwrap(),
        enc(&v.frel.to_string()),
        enc(&v.rel.to_string()),
        v.rep,
        v.occ,
        v.st,
        enc(&v.ren.to_string()),
        enc(&v.hon.to_string()),
        ids.parties.index(v.loc).unwrap(),
        v.ldist,
        v.pg,
        v.wars.map_or(String::new(), |w| format!("&wars={w}")),
        enc(v.pname),
        enc(v.nname),
        enc(v.fname),
        enc(v.pfname),
        enc(v.lname),
        if v.wars.is_some() {
            format!(
                "&ruler={}&spouse={}&father={}",
                enc(v.ruler),
                enc(v.spouse),
                enc(v.father)
            )
        } else {
            String::new()
        },
        enc(&v.msg)
    )
}

fn talk_v2(addr: SocketAddr, job: u32, v: &V2) -> Frame {
    let rid = job + 2000;
    let f = parse_frame(&get(addr, &v2_url(rid, job, v)));
    assert_eq!(f.rid, rid);
    f
}

/// Talks, waits and returns the outcome.
fn say(addr: SocketAddr, job: u32, v: &V2) -> Frame {
    let f = talk_v2(addr, job, v);
    assert!(f.code == CODE_PENDING || f.code == CODE_READY, "{f:?}");
    wait_done(addr, job)
}

/// A server with the shipped profiles and the given memory; returns the memory too.
fn memory_server(url: &str, memory: Arc<Memory>) -> SocketAddr {
    let runner = Runner {
        backend: upstream_backend(url),
        characters: shipped_characters(),
        realms: shipped_realms(),
        templates: shipped_templates(),
        actions: true,
        autonomy: true,
        memory,
        log_prompts: false,
    };
    serve(runner, limits(), npc::NPCS)
}

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("calradia-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("memory.sqlite3")
}

impl Fake {
    fn messages(&self, i: usize) -> Vec<(String, String)> {
        self.request(i).1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["role"].as_str().unwrap().to_string(),
                    m["content"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

fn contains_all(text: &str, parts: &[&str]) {
    for part in parts {
        assert!(text.contains(part), "{part:?} missing from:\n{text}");
    }
}

#[test]
fn v2_identifies_the_character_and_states_the_live_context() {
    let up = fake(vec![Up::Reply("Heh. Evening, boss.")]);
    let memory = Arc::new(Memory::in_memory());
    let addr = memory_server(&up.url, memory.clone());
    assert_eq!(
        say(addr, 1, &borcha("Hello, Borcha.")),
        fr(77, CODE_READY, "Heh. Evening, boss.")
    );
    let messages = up.messages(0);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1], ("user".into(), "Hello, Borcha.".into()));
    contains_all(
        &messages[0].1,
        &[
            "You are Borcha.\nBackground: Born in the high steppe near the village of Dashbigha",
            "Speaking style: Calls the player 'boss'",
            "It is day 12 of the campaign.",
            "You and Ylva are near Praven, a town.",
            "You ride in Ylva's company.",
            "Ylva is a woman. Renown 60 (somewhat known), honour -3 (thought a little untrustworthy).",
            "Ylva has sworn allegiance to no kingdom.",
            "relation with Ylva is 4 on a scale from -100 to 100: you are on good terms.",
            "you remember no earlier conversation with Ylva",
            "Speak only as Borcha",
            "you need not agree with, like, trust or help Ylva",
        ],
    );
    // A commoner companion has no realm to speak of; a king does.
    assert!(!messages[0].1.contains("You belong to"));
    // The turn is stored under the game's ids, not yet delivered.
    assert!(memory.find(7, 1).unwrap().is_some());
    assert!(memory
        .report()
        .unwrap()
        .contains("1 turns (0 delivered), characters trp_npc1"));
}

#[test]
fn different_characters_get_different_profiles_and_faction_context() {
    let up = fake(vec![Up::Reply("Speak.")]);
    let addr = memory_server(&up.url, Arc::new(Memory::in_memory()));
    say(addr, 1, &borcha("Who are you?"));
    say(
        addr,
        2,
        &V2 {
            conv: 101,
            ..harlaus("Who are you?")
        },
    );
    let (b, h) = (up.messages(0)[0].1.clone(), up.messages(1)[0].1.clone());
    contains_all(
        &h,
        &[
            "You are King Harlaus.",
            "cousin of the late King Esterich",
            "You belong to Kingdom of Swadia, and you are its ruler.",
            "You and Ylva are at Praven, a town.",
            "relation with Ylva is -15 on a scale from -100 to 100: you dislike them.",
            "Your realm is hostile to Ylva and their side (relation -30).",
            "Your realm is at war with Kingdom of Vaegirs and Khergit Khanate.",
            "You are married to Lady Esmerelda.",
            "What you know as one of the Swadian (Kingdom of Swadia):\nLand:",
        ],
    );
    assert!(!h.contains("Borcha") && !b.contains("Harlaus"));
    assert!(!h.contains("You ride in"));
    // The part before the shared situation differs entirely.
    let profile = |s: &str| s.split("\n\nSetting:").next().unwrap().to_string();
    assert_ne!(profile(&b), profile(&h));
}

#[test]
fn missing_profiles_fall_back_to_live_data() {
    let up = fake(vec![Up::Reply("Hmph.")]);
    let addr = memory_server(&up.url, Arc::new(Memory::in_memory()));
    let lord = V2 {
        troop: "trp_knight_1_3",
        nname: "Count Plais",
        fac: "fac_kingdom_1",
        fname: "Kingdom of Swadia",
        rep: 4,
        occ: 2,
        st: 0,
        ruler: "King Harlaus",
        father: "Count Ancestor",
        ..borcha("Good day, my lord.")
    };
    assert_eq!(say(addr, 1, &lord).code, CODE_READY);
    let system = up.messages(0)[0].1.clone();
    contains_all(&system, &[
        "You are Count Plais, a lord of Calradia.\nPersonality: cunning: cold-blooded, pragmatic and amoral.",
        "No personal history has been written for you",
        // The realm's lore stands in for a personal history.
        "What you know as one of the Swadian (Kingdom of Swadia):\nLand:",
        "You belong to Kingdom of Swadia.\n- Your liege is King Harlaus.",
        "Your father: Count Ancestor.",
        "Your realm is at war with no other realm.",
    ]);
    // An older mod build sends none of the optional fields: nothing about them is stated.
    let old = V2 {
        wars: None,
        ..lord.clone()
    };
    assert!(!v2_url(1, 2, &old).contains("wars="));
    assert_eq!(say(addr, 2, &V2 { conv: 101, ..old }).code, CODE_READY);
    let system = up.messages(1)[0].1.clone();
    assert!(
        !system.contains("liege") && !system.contains("Your realm is at war with"),
        "{system}"
    );
    assert!(system.contains("Kingdom of Swadia):\nLand:"));
}

#[test]
fn memory_follows_turns_conversations_and_server_restarts() {
    let up = fake(vec![Up::Reply("I will not forget it, boss.")]);
    let db = temp_db("restart");
    let first = memory_server(&up.url, Arc::new(Memory::open(&db)));
    // Conversation 100: two turns; the second names the first as its head.
    say(
        first,
        11,
        &borcha("My sister Ylfa keeps an inn in Sargoth."),
    );
    say(
        first,
        12,
        &V2 {
            head: 11,
            ..borcha("Her ale is the best in Calradia.")
        },
    );
    let m = up.messages(1);
    assert_eq!(m.len(), 4, "{m:?}");
    assert_eq!(m[1].1, "My sister Ylfa keeps an inn in Sargoth.");
    assert_eq!(
        m[2],
        ("assistant".into(), "I will not forget it, boss.".into())
    );
    // Conversation 101, later: the earlier conversation is a memory now.
    say(
        first,
        13,
        &V2 {
            conv: 101,
            head: 12,
            day: 15,
            ..borcha("Good morning.")
        },
    );
    let m = up.messages(2);
    assert_eq!(m.len(), 2);
    contains_all(&m[0].1, &[
        "You first spoke with Ylva on day 12; you have talked 2 times before this conversation.",
        "Your most recent earlier conversations:\n- Day 12: Ylva said \"My sister Ylfa keeps an inn in Sargoth.\" and you answered \"I will not forget it, boss.\"",
    ]);

    // Restart: a new server on the same database remembers, after many other turns.
    let second = memory_server(&up.url, Arc::new(Memory::open(&db)));
    let mut head = 13;
    for job in 14..24 {
        say(
            second,
            job,
            &V2 {
                conv: 102,
                head,
                day: 20,
                ..borcha(&format!("Filler {job}."))
            },
        );
        head = job;
    }
    say(
        second,
        30,
        &V2 {
            conv: 103,
            head,
            day: 25,
            ..borcha("Do you remember my sister?")
        },
    );
    let system = &up.messages(up.hits() - 1)[0].1;
    contains_all(system, &[
        "have talked 13 times before",
        "Older words that bear on what is being said now:\n- Day 12: Ylva said \"My sister Ylfa keeps an inn in Sargoth.\"",
    ]);
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn savegame_branches_and_campaigns_are_isolated() {
    let up = fake(vec![Up::Reply("Aye.")]);
    let memory = Arc::new(Memory::in_memory());
    let addr = memory_server(&up.url, memory.clone());
    say(addr, 1, &borcha("I buried the gold under the old oak."));
    say(
        addr,
        2,
        &V2 {
            conv: 101,
            head: 1,
            ..borcha("I moved the gold to the chapel.")
        },
    );
    // The player reloads the save made after job 1: its head is 1, so job 2 never happened.
    say(
        addr,
        3,
        &V2 {
            conv: 102,
            head: 1,
            ..borcha("Where is the gold?")
        },
    );
    let branch = &up.messages(2)[0].1;
    contains_all(branch, &["old oak", "talked 1 times"]);
    assert!(!branch.contains("chapel"), "{branch}");
    // A new campaign: nothing, even with a head that names a turn of another campaign.
    // (Its own job ids are separate in the database; memory.rs tests reuse across campaigns.)
    say(
        addr,
        6,
        &V2 {
            camp: 8,
            ..borcha("Where is the gold?")
        },
    );
    say(
        addr,
        4,
        &V2 {
            camp: 8,
            head: 2,
            conv: 5,
            ..borcha("Where is the gold?")
        },
    );
    for i in [3, 4] {
        let system = &up.messages(i)[0].1;
        assert!(
            system.contains("you remember no earlier conversation"),
            "{system}"
        );
        assert!(!system.contains("oak") && !system.contains("chapel"));
    }
    // Another character in the same campaign does not share Borcha's memories.
    say(
        addr,
        5,
        &V2 {
            conv: 103,
            head: 2,
            ..harlaus("Where is the gold?")
        },
    );
    assert!(up.messages(5)[0]
        .1
        .contains("you remember no earlier conversation"));
    // Heads that the game named are delivered; job 2 was named by job 5 (Harlaus).
    let report = memory.report().unwrap();
    assert!(
        report.contains(
            "campaign 7 (player \"Ylva\", from day 12): 4 conversations, 4 turns (2 delivered)"
        ),
        "{report}"
    );
}

#[test]
fn canceled_and_undelivered_replies_are_not_remembered() {
    let up = fake(vec![
        Up::Hang,
        Up::Reply("A secret reply."),
        Up::Reply("Fine."),
    ]);
    let memory = Arc::new(Memory::in_memory());
    let addr = memory_server(&up.url, memory.clone());
    // Canceled while the model runs: nothing is stored.
    talk_v2(addr, 1, &borcha("First words."));
    wait_until("job 1 running", || up.hits() == 1);
    assert_eq!(cancel(addr, 1).code, CODE_CANCELED);
    assert_eq!(memory.find(7, 1).unwrap(), None);
    // READY but never shown (the player canceled before the poll, or closed the window):
    // stored, but the game never makes it its head, so the next talk does not recall it.
    assert_eq!(say(addr, 2, &borcha("Second words.")).code, CODE_READY);
    assert!(memory.find(7, 2).unwrap().is_some());
    say(
        addr,
        3,
        &V2 {
            conv: 101,
            head: 0,
            ..borcha("Third words.")
        },
    );
    let system = &up.messages(2)[0].1;
    assert!(
        system.contains("you remember no earlier conversation"),
        "{system}"
    );
    assert!(!system.contains("secret"));
    assert!(memory.report().unwrap().contains("(0 delivered)"));
}

#[test]
fn duplicate_v2_talks_run_once_even_across_restarts() {
    let up = fake(vec![Up::Slow(300, "Once only.")]);
    let db = temp_db("dup");
    let first = memory_server(&up.url, Arc::new(Memory::open(&db)));
    let v = borcha("Hello.");
    assert_eq!(talk_v2(first, 5, &v), fr(2005, CODE_PENDING, "pending"));
    assert_eq!(talk_v2(first, 5, &v), fr(2005, CODE_PENDING, "pending"));
    assert_eq!(wait_done(first, 5), fr(77, CODE_READY, "Once only."));
    assert_eq!(
        talk_v2(
            first,
            5,
            &V2 {
                msg: "Other.".into(),
                ..v.clone()
            }
        )
        .code,
        CODE_BAD_REQUEST
    );
    assert_eq!(up.hits(), 1);
    // After a restart the job is unknown to /v1/result, but the same talk is answered
    // from memory without a new generation or a second record.
    let second = memory_server(&up.url, Arc::new(Memory::open(&db)));
    assert_eq!(result(second, 5).code, CODE_UNKNOWN_JOB);
    assert_eq!(say(second, 5, &v), fr(77, CODE_READY, "Once only."));
    assert_eq!(up.hits(), 1);
    // The same ids with a different talk conflict: at once while the job is in memory, and
    // after the job has expired (here: a third server) when the worker finds the turn.
    let other = V2 {
        msg: "Something else.".into(),
        ..v.clone()
    };
    assert_eq!(
        talk_v2(second, 5, &other),
        fr(2005, CODE_BAD_REQUEST, "conflict")
    );
    let third = memory_server(&up.url, Arc::new(Memory::open(&db)));
    assert_eq!(say(third, 5, &other), fr(77, CODE_FAILED, "conflict"));
    assert_eq!(up.hits(), 1);
    let memory = Memory::open(&db);
    assert!(memory.report().unwrap().contains("1 turns"));
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn unavailable_memory_fails_v2_talks_but_not_v1() {
    let up = fake(vec![Up::Reply("Fine.")]);
    let db = temp_db("corrupt");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, vec![0x42; 4096]).unwrap();
    let addr = memory_server(&up.url, Arc::new(Memory::open(&db)));
    assert_eq!(
        say(addr, 1, &borcha("Hello.")),
        fr(77, CODE_FAILED, "memory_unavailable")
    );
    assert_eq!(up.hits(), 0);
    assert_eq!(
        talk_and_wait(addr, 2, "Hello, Hrodvar."),
        fr(77, CODE_READY, "Fine.")
    );
    let _ = std::fs::remove_dir_all(db.parent().unwrap());
}

#[test]
fn model_failures_store_nothing() {
    let memory = Arc::new(Memory::in_memory());
    let addr = memory_server(&refused_url(), memory.clone());
    assert_eq!(
        say(addr, 1, &borcha("Hello.")),
        fr(77, CODE_FAILED, "upstream_unavailable")
    );
    let up = fake(vec![
        Up::Reply("<think>only thoughts</think>"),
        Up::Status(500),
    ]);
    let addr = memory_server(&up.url, memory.clone());
    assert_eq!(
        say(addr, 2, &borcha("Hello.")),
        fr(77, CODE_FAILED, "empty_reply")
    );
    assert_eq!(
        say(addr, 3, &borcha("Hello.")),
        fr(77, CODE_FAILED, "upstream_error")
    );
    for job in 1..=3 {
        assert_eq!(memory.find(7, job).unwrap(), None);
    }
    assert!(memory.report().unwrap().contains("no campaigns"));
}

#[test]
fn oversized_context_is_bounded() {
    let long = |i: usize| format!("{i} {}", "x".repeat(290));
    let up = fake(vec![Up::ReplyOwned("y ".repeat(250))]);
    let memory = Arc::new(Memory::in_memory());
    let addr = memory_server(&up.url, memory.clone());
    // 30 long turns in one conversation and 30 in earlier ones, all on one chain.
    let mut head = 0;
    for job in 1..=60u32 {
        let conv = if job > 30 { 200 } else { 100 + job };
        say(
            addr,
            job,
            &V2 {
                conv,
                head,
                ..borcha(&long(job as usize))
            },
        );
        head = job;
    }
    let messages = up.messages(59);
    let total: usize = messages.iter().map(|(_, c)| c.len()).sum();
    assert!(total <= MAX_PROMPT_CHARS, "{total}");
    assert_eq!(messages.last().unwrap().1, long(60));
    // The conversation in progress comes first; its latest turns are kept.
    assert!(messages.iter().any(|(r, c)| r == "user" && *c == long(59)));
    assert!(MAX_CHAIN as usize > 60);
}

#[test]
fn v2_request_validation_and_v1_compatibility() {
    let up = fake(vec![Up::Reply("Fine.")]);
    let addr = memory_server(&up.url, Arc::new(Memory::in_memory()));
    let ask = |target: &str| parse_frame(&get(addr, target));
    let bad = |reason: &str| fr(9, CODE_BAD_REQUEST, reason);
    let url = |v: &V2| v2_url(9, 9, v);
    let base = url(&borcha("Hi"));
    // Each talk version needs its own v=.
    assert_eq!(ask(&base.replace("v=2&", "v=1&")), bad("bad_version"));
    assert_eq!(
        ask(&talk_url(9, 9, 1, 1, "A", "Hi").replace("v=1&", "v=2&")),
        bad("bad_version")
    );
    assert_eq!(ask(&base.replace("&end=1", "")), bad("truncated"));
    let ids = ids();
    let troop = |t: &str| format!("troop={}&", ids.troops.index(t).unwrap());
    let swap = |from: String, to: String| ask(&base.replacen(&from, &to, 1));
    let npc1 = troop("trp_npc1");
    for (from, to) in [
        (npc1.clone(), "troop=0&".to_string()),     // the player
        (npc1.clone(), troop("trp_temp_troop")),    // not a character
        (npc1.clone(), "troop=99999&".to_string()), // no such troop
        ("camp=7&".into(), "camp=0&".into()),
        ("conv=100&".into(), "conv=x&".into()),
        ("head=0&".into(), "head=%2D1&".into()),
        ("rel=4&".into(), "rel=9999999&".into()),
        ("pg=1&".into(), "pg=2&".into()),
        ("day=12&".into(), "day=100001&".into()),
        ("fac=1&".into(), "fac=999&".into()),
        ("st=1&".into(), "".into()),
        ("&wars=0&".into(), "&wars=128&".into()),
        ("&wars=0&".into(), "&wars=x&".into()),
    ] {
        assert_eq!(
            swap(from.clone(), to.clone()),
            bad("bad_param"),
            "{from} -> {to}"
        );
    }
    assert_eq!(ask(&url(&borcha(&"a".repeat(301)))), bad("too_long"));
    // The largest talk the mod can send fits in MAX_TARGET: every name and the message at
    // their caps, all in characters the engine percent-encodes, and 9-digit ids.
    let wide = |n: usize| -> &'static str { Box::leak(",".repeat(n).into_boxed_str()) };
    let widest = V2 {
        camp: RID_MAX,
        conv: RID_MAX,
        head: RID_MAX,
        pname: wide(MAX_PNAME),
        nname: wide(MAX_NAME),
        fname: wide(MAX_NAME),
        pfname: wide(MAX_NAME),
        lname: wide(MAX_NAME),
        ruler: wide(MAX_NAME),
        spouse: wide(MAX_NAME),
        father: wide(MAX_NAME),
        ..borcha(&"?".repeat(MAX_MSG))
    };
    let target = v2_url(RID_MAX, RID_MAX, &widest);
    assert!(target.len() < MAX_TARGET, "{}", target.len());
    assert_eq!(ask(&url(&borcha(" "))), bad("empty_msg"));
    assert_eq!(up.hits(), 0);
    // Negative values arrive percent-encoded, as the engine sends them; long names are cut.
    let v = V2 {
        rel: -100,
        hon: -45,
        nname: "Borcha the Unreasonably Long-Named Tracker of the Steppe",
        ..borcha("Hi")
    };
    assert!(url(&v).contains("&rel=%2D100&"));
    assert_eq!(say(addr, 9, &v).code, CODE_READY);
    contains_all(
        &up.messages(0)[0].1,
        &[
            "is -100 on a scale",
            "honour -45 (known to be dishonourable)",
            "Speak only as Borcha the Unreasonably Long-Named Track, ",
        ],
    );
    // v1 talks work as before, next to v2.
    assert_eq!(
        talk_and_wait(addr, 10, "Hello"),
        fr(77, CODE_READY, "Fine.")
    );
    assert!(up.system_prompt(1).starts_with("You are Hrodvar"));
}

#[test]
fn fake_llm_summarizes_what_the_prompt_contains() {
    let runner = Runner {
        backend: Backend::Canned {
            delay: Duration::from_millis(10),
            kind: Canned::Reply,
        },
        characters: shipped_characters(),
        realms: shipped_realms(),
        templates: shipped_templates(),
        actions: true,
        autonomy: true,
        memory: Arc::new(Memory::in_memory()),
        log_prompts: true,
    };
    let addr = serve(runner, limits(), npc::NPCS);
    let f = say(addr, 1, &borcha("My horse is called Swiftfoot."));
    assert_eq!(
        f.text,
        "[fake] I am Borcha of Commoners, day 12, relation 4. I remember 0 earlier talks."
    );
    let f = say(
        addr,
        2,
        &V2 {
            conv: 101,
            head: 1,
            ..harlaus("My horse is called Swiftfoot.")
        },
    );
    assert!(
        f.text
            .starts_with("[fake] I am King Harlaus of Kingdom of Swadia"),
        "{f:?}"
    );
    let f = say(
        addr,
        3,
        &V2 {
            conv: 102,
            head: 2,
            ..borcha("Is Swiftfoot well?")
        },
    );
    assert_eq!(
        f.text,
        "[fake] I am Borcha of Commoners, day 12, relation 4. I remember 1 earlier talks; \
         you last said: My horse is called Swiftfoot.."
    );
    // v1 keeps its canned in-character replies.
    assert_eq!(talk(addr, 4, "Hi").code, CODE_PENDING);
    assert!(!wait_done(addr, 4).text.starts_with("[fake]"));
}

// ---------------------------------------------------------------- Milestones 4-6

use crate::memory::Plan;
use crate::protocol::{LogEntry, Snapshot};

/// Parses a v2 body `R|C|T|K|N|W|X|R`, asserting the frame's guarantees.
fn parse_frame2(body: &str) -> (Frame, [i32; 4]) {
    assert!(body.len() <= 580, "frame of {} bytes", body.len());
    let parts: Vec<&str> = body.split('|').collect();
    assert_eq!(parts.len(), 8, "{body:?}");
    assert_eq!(parts[0], parts[7], "{body:?}");
    let extra = [3, 4, 5, 6].map(|i| parts[i].parse::<i32>().expect("integer"));
    let v1 = format!("{}|{}|{}|{}", parts[0], parts[1], parts[2], parts[7]);
    (parse_frame(&v1), extra)
}

fn list(values: &[u32]) -> String {
    values.iter().map(|v| format!(".{v}")).collect()
}

fn event_url(rid: u32, node: u32, whead: u32, day: u32, e: &LogEntry) -> String {
    format!(
        "/v2/event?v=2&rid={rid}&job={node}&camp=7&whead={whead}&day={day}&idx={}&type={}\
         &time={}&actor={}&center={}&clord={}&cfac={}&troop={}&tfac={}&fac={}&pname=Ylva&pfname=&end=1",
        e.index,
        e.kind,
        e.hours,
        enc(&e.actor.to_string()),
        enc(&e.center.to_string()),
        enc(&e.center_lord.to_string()),
        enc(&e.center_faction.to_string()),
        enc(&e.troop.to_string()),
        enc(&e.troop_faction.to_string()),
        enc(&e.faction.to_string()),
    )
}

fn world_url(rid: u32, node: u32, whead: u32, day: u32, s: &Snapshot) -> String {
    let half = s.lords.len() / 2;
    format!(
        "/v2/world?v=2&rid={rid}&job={node}&camp=7&whead={whead}&day={day}&alive={}&pname=Ylva\
         &pfname=&wars={}&owners={}&lords={}&lords2={}&end=1",
        s.alive,
        enc(&list(&s.wars)),
        enc(&list(&s.owners)),
        enc(&list(&s.lords[..half])),
        enc(&list(&s.lords[half..])),
    )
}

fn tick_url(rid: u32, node: u32, whead: u32, day: u32, head: u32) -> String {
    format!(
        "/v2/tick?v=2&rid={rid}&job={node}&camp=7&whead={whead}&day={day}&head={head}\
         &pname=Ylva&pfname=&end=1"
    )
}

fn send2(addr: SocketAddr, target: &str) -> (Frame, [i32; 4]) {
    parse_frame2(&get(addr, target))
}

/// `wait_done` for a job whose results come in v2 frames (`f=2`).
fn wait_done2(addr: SocketAddr, job: u32) -> (Frame, [i32; 4]) {
    let start = Instant::now();
    loop {
        let (f, extra) = send2(addr, &format!("/v1/result?v=1&rid=77&job={job}&end=1"));
        if f.code != CODE_PENDING {
            return (f, extra);
        }
        assert!(start.elapsed() < Duration::from_secs(8), "job {job} stuck");
        thread::sleep(Duration::from_millis(20));
    }
}

fn fac(id: &str) -> i32 {
    ids().factions.index(id).unwrap() as i32
}

fn troop(id: &str) -> i32 {
    ids().troops.index(id).unwrap() as i32
}

/// Ylva (the player) defeated Count Klargus of Swadia on day 3.
fn defeat() -> LogEntry {
    LogEntry {
        index: 1,
        kind: 11,
        hours: 3 * 24 + 2,
        actor: 0,
        center: -1,
        center_lord: -1,
        center_faction: -1,
        troop: troop("trp_knight_1_1"),
        troop_faction: fac("fac_kingdom_1"),
        faction: -1,
    }
}

fn baseline() -> Snapshot {
    let (realms, centers, lords) = protocol::snapshot_shape();
    let swadia = fac("fac_kingdom_1") as u32;
    Snapshot {
        alive: 0b111_1110,
        wars: vec![0; realms * (realms - 1) / 2],
        owners: vec![swadia; centers],
        lords: vec![swadia * 2; lords],
    }
}

fn server_with(url: &str, memory: Arc<Memory>, actions: bool, autonomy: bool) -> SocketAddr {
    let runner = Runner {
        backend: upstream_backend(url),
        characters: shipped_characters(),
        realms: shipped_realms(),
        templates: shipped_templates(),
        memory,
        log_prompts: false,
        actions,
        autonomy,
    };
    serve(runner, limits(), npc::NPCS)
}

fn stored() -> Frame {
    fr(3001, CODE_READY, "stored")
}

#[test]
fn world_events_are_chained_per_save_and_recalled_in_talks() {
    let up = fake(vec![Up::Reply("I remember.")]);
    let memory = Arc::new(Memory::in_memory());
    let addr = server_with(&up.url, memory.clone(), true, false);
    // Node 11: the log entry. Node 12: a first snapshot (a baseline, no events). Node 13:
    // the next day's snapshot, in which Sargoth fell to the Vaegirs.
    assert_eq!(
        send2(addr, &event_url(3001, 11, 0, 3, &defeat())),
        (stored(), [0; 4])
    );
    assert_eq!(
        send2(addr, &world_url(3001, 12, 11, 3, &baseline())).0,
        stored()
    );
    let mut next = baseline();
    next.owners[0] = fac("fac_kingdom_2") as u32;
    assert_eq!(send2(addr, &world_url(3001, 13, 12, 4, &next)).0, stored());
    // A lord of Swadia, talked to with the world head 13, knows both.
    let lord = V2 {
        troop: "trp_knight_1_1",
        nname: "Count Klargus",
        fac: "fac_kingdom_1",
        fname: "Kingdom of Swadia",
        occ: 2,
        st: 0,
        ..borcha("Do you know me?")
    };
    let url = |v: &V2, whead: u32| {
        v2_url(2001, 1, v).replace("&pname=", &format!("&whead={whead}&pname="))
    };
    let f = parse_frame(&get(addr, &url(&lord, 13)));
    assert_eq!(f.code, CODE_PENDING);
    wait_done(addr, 1);
    contains_all(&up.messages(0)[0].1, &[
        "Events in Calradia that you know of (from the game's record; these happened):",
        "- Day 3: Ylva defeated Count Klargus in battle.",
        "- Day 4: The town of Sargoth passed from the Kingdom of Swadia to the Kingdom of Vaegirs.",
    ]);
    // Another save that reloaded before node 13 only knows the defeat.
    get(
        addr,
        &url(
            &V2 {
                conv: 101,
                ..lord.clone()
            },
            12,
        )
        .replace("job=1&", "job=2&"),
    );
    wait_done(addr, 2);
    let system = up.messages(1)[0].1.clone();
    assert!(
        system.contains("defeated Count Klargus") && !system.contains("Sargoth passed"),
        "{system}"
    );
    // Retrying a node is harmless; reusing its id for something else is a conflict.
    assert_eq!(
        send2(addr, &event_url(3001, 11, 0, 3, &defeat())).0,
        stored()
    );
    let other = LogEntry {
        index: 2,
        ..defeat()
    };
    assert_eq!(
        send2(addr, &event_url(3001, 11, 0, 3, &other)).0,
        fr(3001, CODE_BAD_REQUEST, "conflict")
    );
    assert!(
        memory.report().unwrap().contains("world 3 nodes, 2 events"),
        "{}",
        memory.report().unwrap()
    );
}

#[test]
fn world_nodes_are_validated() {
    let up = fake(vec![Up::Reply("x")]);
    let addr = server_with(&up.url, Arc::new(Memory::in_memory()), true, true);
    let bad = |target: String| {
        let (f, extra) = send2(addr, &target);
        assert_eq!((f.code, extra), (CODE_BAD_REQUEST, [0; 4]), "{target}");
    };
    let good = world_url(3001, 12, 0, 3, &baseline());
    bad(good.replace("v=2", "v=1"));
    bad(good.replace("&owners=%2E15", "&owners=%2E99999999999"));
    bad(good.replace("&lords2=", "&lords3="));
    let mut short = baseline();
    short.owners.pop();
    bad(world_url(3001, 12, 0, 3, &short));
    let mut wars = baseline();
    wars.wars[0] = 2;
    bad(world_url(3001, 12, 0, 3, &wars));
    bad(event_url(
        3001,
        11,
        0,
        3,
        &LogEntry {
            index: 0,
            ..defeat()
        },
    ));
    bad(event_url(
        3001,
        11,
        0,
        3,
        &LogEntry {
            actor: -2,
            ..defeat()
        },
    ));
    bad(tick_url(3001, 11, 0, 3, 0).replace("&head=0", ""));
    bad(tick_url(3001, 11, 0, 3, 0).replace("camp=7", "camp=0"));
    // A v1 route never answers with a v2 frame.
    assert_eq!(
        parse_frame(&get(addr, "/v1/result?v=1&rid=5&job=6&end=1")).code,
        CODE_UNKNOWN_JOB
    );
}

#[test]
fn actions_are_validated_delivered_and_remembered() {
    let up = fake(vec![
        Up::Reply("Take this purse, and my thanks.\nACTION: give 100"),
        Up::Reply("A fair question."),
        Up::Reply("Absurd. [ACTION: give 900]"),
        Up::Reply("Hm. ACTION: relation -2"),
    ]);
    let memory = Arc::new(Memory::in_memory());
    let addr = server_with(&up.url, memory.clone(), true, false);
    let lord = V2 {
        troop: "trp_knight_1_1",
        nname: "Count Klargus",
        fac: "fac_kingdom_1",
        fname: "Kingdom of Swadia",
        occ: 2,
        st: 0,
        ..borcha("You fought well.")
    };
    // f=2, with gold: the reply carries the action in the v2 frame.
    let url = |v: &V2, job: u32, extra: &str| {
        v2_url(2000 + job, job, v).replace(
            "&pname=",
            &format!("&f=2&gold=1000&pgold=300{extra}&pname="),
        )
    };
    let (f, extra) = send2(addr, &url(&lord, 1, ""));
    assert_eq!((f.code, extra), (CODE_PENDING, [0; 4]));
    wait_done2(addr, 1);
    let (f, extra) = send2(addr, "/v1/result?v=1&rid=77&job=1&end=1");
    assert_eq!(f, fr(77, CODE_READY, "Take this purse, and my thanks."));
    assert_eq!(extra, [ACT_GIVE as i32, 100, troop("trp_knight_1_1"), 0]);
    // The prompt offered the deeds, with the purses.
    contains_all(
        &up.messages(0)[0].1,
        &[
            "ACTION: <deed> <number>",
            "you have about 1000 denars",
            "Ylva carries 300 denars",
        ],
    );
    // The player accepted (hres=1): the next talk says so, and memory keeps it.
    let next = V2 {
        conv: 101,
        head: 1,
        ..lord.clone()
    };
    send2(addr, &url(&next, 2, "&hres=1"));
    wait_done2(addr, 2);
    contains_all(
        &up.messages(1)[0].1,
        &["(you offered Ylva 100 denars; accepted)"],
    );
    // Too much for the purse, or a second gold action within three days: stripped, not sent.
    send2(
        addr,
        &url(
            &V2 {
                conv: 102,
                head: 2,
                ..lord.clone()
            },
            3,
            "",
        ),
    );
    wait_done2(addr, 3);
    let (f, extra) = send2(addr, "/v1/result?v=1&rid=77&job=3&end=1");
    assert_eq!((f.text.as_str(), extra), ("Absurd.", [0; 4]));
    // A game without f=2 gets v1 frames and no action, though the text is still clean.
    get(
        addr,
        &v2_url(
            2004,
            4,
            &V2 {
                conv: 103,
                head: 3,
                ..lord.clone()
            },
        ),
    );
    wait_done(addr, 4);
    let f = parse_frame(&get(addr, "/v1/result?v=1&rid=77&job=4&end=1"));
    assert_eq!(f.text, "Hm.");
    assert_eq!(memory.find(7, 4).unwrap().unwrap().action_kind, 0);
    assert_eq!(memory.find(7, 1).unwrap().unwrap().action_kind, ACT_GIVE);
}

#[test]
fn actions_can_be_turned_off() {
    let up = fake(vec![Up::Reply("Hm.\nACTION: relation 2")]);
    let addr = server_with(&up.url, Arc::new(Memory::in_memory()), false, false);
    let url = v2_url(2001, 1, &borcha("Hello")).replace("&pname=", "&f=2&gold=10&pgold=10&pname=");
    send2(addr, &url);
    wait_done2(addr, 1);
    let (f, extra) = send2(addr, "/v1/result?v=1&rid=77&job=1&end=1");
    assert_eq!((f.text.as_str(), extra), ("Hm.", [0; 4]));
    assert!(!up.messages(0)[0].1.contains("ACTION:"));
}

/// Waits until `character` has a plan on the chain that ends at `head`.
fn wait_plan(memory: &Memory, head: u32, character: &str) -> Plan {
    let start = Instant::now();
    loop {
        if let Some(p) = memory.latest_plan(7, head, character).unwrap() {
            return p;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "no plan for {character}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

const PLAN: &str =
    "{\"goal\": \"Keep Swadia whole.\", \"plan\": \"Watch Isolla and her friends.\", \
    \"act\": {\"kind\": \"letter\", \"text\": \"Ylva, come to Praven. We should talk.\"}}";

#[test]
fn ticks_plan_in_the_background_and_deliver_initiatives() {
    let up = fake(vec![
        Up::ReplyOwned(PLAN.to_string()),
        Up::Reply("Well met."),
    ]);
    let memory = Arc::new(Memory::in_memory());
    let addr = server_with(&up.url, memory.clone(), true, true);
    // Day 1: no initiative yet; planning is queued for the first unplanned ruler.
    assert_eq!(
        send2(addr, &tick_url(3001, 21, 0, 1, 0)),
        (stored(), [0; 4])
    );
    let plan = wait_plan(&memory, 21, "trp_kingdom_1_lord");
    assert_eq!(plan.goal, "Keep Swadia whole.");
    let system = up.messages(0)[0].1.clone();
    contains_all(
        &system,
        &[
            "You are King Harlaus.",
            "private mind of King Harlaus",
            "\"goal\"",
        ],
    );
    // Day 2: the letter is delivered with the tick.
    let (f, extra) = send2(addr, &tick_url(3001, 22, 21, 2, 0));
    assert_eq!(
        f,
        fr(3001, CODE_READY, "Ylva, come to Praven. We should talk.")
    );
    assert_eq!(
        extra,
        [INIT_LETTER as i32, 0, troop("trp_kingdom_1_lord"), 0]
    );
    // Retrying the same tick gives the same answer; the next tick does not repeat it.
    assert_eq!(send2(addr, &tick_url(3001, 22, 21, 2, 0)).1, extra);
    assert_eq!(send2(addr, &tick_url(3001, 23, 22, 3, 0)).1[0], 0);
    // A save that reloaded before the plan (world head 0) never gets the letter.
    assert_eq!(send2(addr, &tick_url(3001, 24, 0, 3, 0)).1[0], 0);
    // Talking to Harlaus afterwards: he knows his aims and what he wrote.
    let king = harlaus("You wrote to me?");
    let url = v2_url(2001, 1, &king).replace("&pname=", "&whead=23&pname=");
    get(addr, &url);
    wait_done(addr, 1);
    let talk = up.messages(up.hits() - 1)[0].1.clone();
    contains_all(&talk, &[
        "Your private aims (decided on day 1; reveal them only if it serves you):\n- Goal: Keep Swadia whole.",
        "- Day 2: you wrote to Ylva: \"Ylva, come to Praven. We should talk.\"",
    ]);
}

#[test]
fn a_talk_preempts_background_planning() {
    let up = fake(vec![Up::Hang, Up::Reply("Speak.")]);
    let memory = Arc::new(Memory::in_memory());
    let addr = server_with(&up.url, memory.clone(), true, true);
    send2(addr, &tick_url(3001, 21, 0, 1, 0));
    wait_until("planning reached the model", || up.hits() == 1);
    let t0 = Instant::now();
    assert_eq!(talk_v2(addr, 1, &borcha("Hello")).code, CODE_PENDING);
    wait_until("planning socket closed", || up.closed() == 1);
    assert_eq!(wait_done(addr, 1), fr(77, CODE_READY, "Speak."));
    assert!(t0.elapsed() < Duration::from_secs(3));
    assert_eq!(
        memory.latest_plan(7, 21, "trp_kingdom_1_lord").unwrap(),
        None
    );
}

#[test]
fn autonomy_can_be_turned_off_and_bad_plans_are_dropped() {
    let up = fake(vec![Up::Reply("I refuse to answer in JSON.")]);
    let memory = Arc::new(Memory::in_memory());
    let off = server_with(&up.url, memory.clone(), true, false);
    send2(off, &tick_url(3001, 21, 0, 1, 0));
    thread::sleep(Duration::from_millis(200));
    assert_eq!(up.hits(), 0);
    let on = server_with(&up.url, memory.clone(), true, true);
    send2(on, &tick_url(3001, 31, 0, 1, 0));
    wait_until("planning ran", || up.hits() == 1);
    // Planning asks for a JSON object, with room for a whole letter.
    let (_, body) = up.request(0);
    assert_eq!(body["response_format"], json!({"type": "json_object"}));
    assert_eq!(body["max_tokens"], 400);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        memory.latest_plan(7, 31, "trp_kingdom_1_lord").unwrap(),
        None
    );
}

#[test]
fn fake_llm_plans_and_writes_letters() {
    let runner = Runner {
        backend: Backend::Canned {
            delay: Duration::from_millis(10),
            kind: Canned::Reply,
        },
        characters: shipped_characters(),
        realms: shipped_realms(),
        templates: shipped_templates(),
        memory: Arc::new(Memory::in_memory()),
        log_prompts: false,
        actions: true,
        autonomy: true,
    };
    let memory = runner.memory.clone();
    let addr = serve(runner, limits(), npc::NPCS);
    send2(addr, &tick_url(3001, 21, 0, 1, 0));
    wait_plan(&memory, 21, "trp_kingdom_1_lord");
    let (f, extra) = send2(addr, &tick_url(3001, 22, 21, 2, 0));
    assert!(
        f.text
            .starts_with("[fake] King Harlaus writes to Ylva on day 1"),
        "{f:?}"
    );
    assert_eq!(extra[0], INIT_LETTER as i32);
    // The fake echoes an ACTION the player types, which is validated like any other.
    let url = v2_url(2001, 1, &borcha("Here is my offer. ACTION: ask 50"))
        .replace("&pname=", "&f=2&pgold=100&pname=");
    send2(addr, &url);
    wait_done2(addr, 1);
    let (_, extra) = send2(addr, "/v1/result?v=1&rid=77&job=1&end=1");
    assert_eq!(extra, [ACT_ASK as i32, 50, troop("trp_npc1"), 0]);
}
