//! calradia-server: the bridge between the Calradia AI Warband mod and a language model.
//!
//! It implements protocol v1 (docs/protocol-v1.md). The game sends
//! `GET /v1/{talk,result,cancel}?...` through the engine's `send_message_to_url`, and
//! every answer is HTTP 200 with the body `R|C|T|R`. Replies come from an
//! OpenAI-compatible upstream (`POST {upstream}/chat/completions`), run one at a time by
//! a single worker thread, so no `/v1` handler ever waits on the model.
//!
//! Protocol v2 adds `/v2/talk`: a real Warband character, the live game state, and memory
//! of earlier conversations, kept in SQLite per campaign and savegame branch (memory.rs).

mod characters;
mod http;
mod ids;
mod jobs;
mod memory;
mod npc;
mod prompt;
mod protocol;
mod runner;
mod sanitize;
mod upstream;

use characters::Registry;
use http::{format_response, read_request, Request};
use jobs::{Limits, Store};
use memory::Memory;
use npc::Npc;
use protocol::{
    frame, Op, CODE_BAD_REQUEST, DEFAULT_BIND, GAME_GIVE_UP_SECS, REASON_BAD_PARAM, RID_MAX,
    UPSTREAM_CONNECT_TIMEOUT_SECS, UPSTREAM_MAX_BODY,
};
use runner::Runner;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};
use upstream::{Backend, Canned, DEFAULT_MODEL, DEFAULT_UPSTREAM};

const USAGE: &str = "\
usage: calradia-server [options]
  --bind ADDR:PORT      listen address (default 127.0.0.1:8766)
  --upstream URL        OpenAI-compatible base URL, http only
                        (env CALRADIA_UPSTREAM, default http://172.17.0.1:8080/v1)
  --model NAME          model name (env CALRADIA_MODEL, default calradia-qwen3.5-9b)
  --characters DIR      character profiles, one TOML file per character
                        (env CALRADIA_CHARACTERS, default calradia-server/characters)
  --memory-db PATH      conversation memory, SQLite (env CALRADIA_MEMORY_DB, default
                        $XDG_DATA_HOME/calradia-ai/memory.sqlite3)
  --memory-report       print what the memory database holds, then exit
  --log-prompts         log every prompt sent to the model in full
  --fake-llm            skip the upstream; answer a canned reply after 1.5 s (for
                        /v2/talk, a summary of the character, context and memories)
  --fault MODE          misbehave on purpose on /v1/*, for testing the game:
                        hang, close, empty, wrong-rid, malformed, delay,
                        oversize-N, nonascii
  --max-jobs N          stored jobs (default 64)
  --queue-len N         queued jobs (default 8)
  --ttl-secs N          lifetime of finished jobs (default 600)
  --deadline-secs N     job deadline, below 120 (default 90)";

/// How long a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `--fake-llm` and the text faults take to "think".
const FAKE_LLM_DELAY: Duration = Duration::from_millis(1500);
/// How long `--fault delay` holds each /v1 answer.
const FAULT_DELAY: Duration = Duration::from_secs(3);

/// A deliberate misbehaviour for testing the game (`--fault`). Requests are still
/// processed normally; the fault only changes what goes back on the wire, except for
/// `Oversize` and `NonAscii`, which replace the model's reply.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fault {
    /// Accept, then never reply.
    Hang,
    /// Close without replying.
    Close,
    /// 200 with an empty body.
    Empty,
    /// Echo rid+1 in both R positions.
    WrongRid,
    /// Reply `garbage|x`.
    Malformed,
    /// Reply after 3 s.
    Delay,
    /// READY text of N characters before sanitizing.
    Oversize(usize),
    /// READY text full of non-ASCII.
    NonAscii,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Fault::Hang => write!(f, "hang: /v1 requests are accepted and never answered"),
            Fault::Close => write!(f, "close: /v1 connections are closed without a reply"),
            Fault::Empty => write!(f, "empty: /v1 answers are 200 with an empty body"),
            Fault::WrongRid => write!(f, "wrong-rid: /v1 answers carry rid+1"),
            Fault::Malformed => write!(f, "malformed: /v1 answers are `garbage|x`"),
            Fault::Delay => write!(f, "delay: /v1 answers are held for 3 s"),
            Fault::Oversize(n) => write!(f, "oversize-{n}: replies are {n} characters"),
            Fault::NonAscii => write!(f, "nonascii: replies are full of non-ASCII"),
        }
    }
}

fn parse_fault(s: &str) -> Result<Fault, String> {
    Ok(match s {
        "hang" => Fault::Hang,
        "close" => Fault::Close,
        "empty" => Fault::Empty,
        "wrong-rid" => Fault::WrongRid,
        "malformed" => Fault::Malformed,
        "delay" => Fault::Delay,
        "nonascii" => Fault::NonAscii,
        _ => match s.strip_prefix("oversize-").map(str::parse::<usize>) {
            Some(Ok(n)) if (1..=UPSTREAM_MAX_BODY).contains(&n) => Fault::Oversize(n),
            _ => return Err(format!("unknown fault {s:?}")),
        },
    })
}

#[derive(Debug, PartialEq)]
struct Options {
    bind: String,
    upstream: String,
    model: String,
    fake_llm: bool,
    fault: Option<Fault>,
    limits: Limits,
    characters: PathBuf,
    memory_db: PathBuf,
    memory_report: bool,
    log_prompts: bool,
}

/// `$XDG_DATA_HOME/calradia-ai/memory.sqlite3`, or under `~/.local/share`.
fn default_memory_db(env: &impl Fn(&str) -> Option<String>) -> PathBuf {
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    let data = set("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    data.join("calradia-ai").join("memory.sqlite3")
}

/// Parses the command line; `env` looks up environment variables. `Ok(None)` means help
/// was requested.
fn parse_args(
    mut args: impl Iterator<Item = String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Options>, String> {
    let from_env = |name: &str, default: &str| {
        env(name)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    let mut o = Options {
        bind: DEFAULT_BIND.to_string(),
        upstream: from_env("CALRADIA_UPSTREAM", DEFAULT_UPSTREAM),
        model: from_env("CALRADIA_MODEL", DEFAULT_MODEL),
        fake_llm: false,
        fault: None,
        limits: Limits::default(),
        characters: PathBuf::from(from_env(
            "CALRADIA_CHARACTERS",
            concat!(env!("CARGO_MANIFEST_DIR"), "/characters"),
        )),
        memory_db: env("CALRADIA_MEMORY_DB")
            .filter(|v| !v.is_empty())
            .map_or_else(|| default_memory_db(&env), PathBuf::from),
        memory_report: false,
        log_prompts: false,
    };
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        let count = |v: String| match v.parse::<u64>() {
            Ok(n) if n >= 1 => Ok(n),
            _ => Err(format!("{flag} needs a positive integer, got {v:?}")),
        };
        match flag.as_str() {
            "--bind" => o.bind = value()?,
            "--upstream" => o.upstream = value()?,
            "--model" => o.model = value()?,
            "--fake-llm" => o.fake_llm = true,
            "--characters" => o.characters = PathBuf::from(value()?),
            "--memory-db" => o.memory_db = PathBuf::from(value()?),
            "--memory-report" => o.memory_report = true,
            "--log-prompts" => o.log_prompts = true,
            "--fault" => o.fault = Some(parse_fault(&value()?)?),
            "--max-jobs" => o.limits.max_jobs = count(value()?)? as usize,
            "--queue-len" => o.limits.queue_len = count(value()?)? as usize,
            "--ttl-secs" => o.limits.ttl = Duration::from_secs(count(value()?)?),
            "--deadline-secs" => {
                let secs = count(value()?)?;
                if secs >= GAME_GIVE_UP_SECS {
                    return Err(format!(
                        "--deadline-secs must be below the game's {GAME_GIVE_UP_SECS} s give-up"
                    ));
                }
                o.limits.deadline = Duration::from_secs(secs);
            }
            "-h" | "--help" => return Ok(None),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Some(o))
}

/// Chooses where replies come from: a canned text for the text faults and `--fake-llm`,
/// otherwise the upstream.
fn backend_for(o: &Options) -> Result<Backend, String> {
    let canned = match o.fault {
        Some(Fault::Oversize(n)) => Some(Canned::Oversize(n)),
        Some(Fault::NonAscii) => Some(Canned::NonAscii),
        _ if o.fake_llm => Some(Canned::Reply),
        _ => None,
    };
    Ok(match canned {
        Some(kind) => Backend::Canned {
            delay: FAKE_LLM_DELAY,
            kind,
        },
        None => Backend::Upstream {
            endpoint: upstream::parse_endpoint(&o.upstream)?,
            model: o.model.clone(),
            connect_timeout: Duration::from_secs(UPSTREAM_CONNECT_TIMEOUT_SECS),
        },
    })
}

fn main() {
    let opts = match parse_args(env::args().skip(1), |k| env::var(k).ok()) {
        Ok(Some(o)) => o,
        Ok(None) => {
            println!("{USAGE}");
            return;
        }
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            process::exit(2);
        }
    };
    let memory = Arc::new(Memory::open(&opts.memory_db));
    if opts.memory_report {
        match memory.report() {
            Ok(r) => print!("memory {}:\n{r}", opts.memory_db.display()),
            Err(e) => {
                eprintln!("error: {e}");
                process::exit(1);
            }
        }
        return;
    }
    let characters = match Registry::load(&opts.characters) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: character profiles: {e}");
            process::exit(2);
        }
    };
    let backend = match backend_for(&opts) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(2);
        }
    };
    let listener = match TcpListener::bind(&opts.bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot listen on {}: {e}", opts.bind);
            process::exit(1);
        }
    };
    if let Some(fault) = opts.fault {
        let bar = "!".repeat(72);
        eprintln!(
            "{bar}\n!!! FAULT INJECTION IS ON: --fault {fault}\n!!! This server misbehaves \
             on purpose. Do not use it for normal play.\n{bar}"
        );
    }
    let source = match &backend {
        Backend::Upstream {
            endpoint, model, ..
        } => format!("upstream {endpoint} model {model}"),
        Backend::Canned { kind, .. } => format!("canned replies ({kind:?}), no upstream"),
    };
    if let Err(e) = memory.status() {
        let bar = "!".repeat(72);
        eprintln!(
            "{bar}\n!!! MEMORY IS UNAVAILABLE: {e}\n!!! Every /v2/talk will fail with \
             memory_unavailable. Fix the file or pass --memory-db.\n{bar}"
        );
    }
    log(format!(
        "calradia-server listening on {}; {source}; limits {:?}",
        opts.bind, opts.limits
    ));
    log(format!(
        "memory {}; {} character profiles from {}: {}",
        opts.memory_db.display(),
        characters.ids().count(),
        opts.characters.display(),
        characters.ids().collect::<Vec<_>>().join(", ")
    ));
    let runner = Runner {
        backend,
        characters,
        memory,
        log_prompts: opts.log_prompts,
    };
    if let Err(e) = start(listener, opts.limits, runner, opts.fault, npc::NPCS) {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

struct App {
    store: Arc<Store>,
    npcs: &'static [Npc],
    fault: Option<Fault>,
}

/// Starts the worker, then accepts connections forever, one thread per connection.
fn start(
    listener: TcpListener,
    limits: Limits,
    runner: Runner,
    fault: Option<Fault>,
    npcs: &'static [Npc],
) -> std::io::Result<()> {
    let store = Arc::new(Store::new(limits));
    jobs::spawn_worker(store.clone(), runner)?;
    let app = Arc::new(App { store, npcs, fault });
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let app = app.clone();
                thread::spawn(move || handle(stream, &app));
            }
            Err(e) => log(format!("accept error: {e}")),
        }
    }
    Ok(())
}

/// The frame's parts, before the frame is built.
struct Outcome {
    rid: u32,
    code: u8,
    text: String,
}

impl App {
    fn respond(&self, req: &Request) -> Outcome {
        match protocol::parse(req, self.npcs) {
            Err(rejection) => Outcome {
                rid: rejection.rid,
                code: CODE_BAD_REQUEST,
                text: rejection.reason.to_string(),
            },
            Ok(v1) => {
                let answer = match v1.op {
                    Op::Talk(talk) => self.store.talk(v1.job, *talk),
                    Op::Result => self.store.result(v1.job),
                    Op::Cancel => self.store.cancel(v1.job),
                };
                Outcome {
                    rid: v1.rid,
                    code: answer.code,
                    text: answer.text,
                }
            }
        }
    }
}

fn handle(mut stream: TcpStream, app: &App) {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            log(format!("bad HTTP request: {e}"));
            let body = frame(0, CODE_BAD_REQUEST, REASON_BAD_PARAM);
            let _ = stream.write_all(&format_response(200, &body));
            return;
        }
    };
    let started = Instant::now();
    let out = app.respond(&req);
    let mut body = frame(out.rid, out.code, &out.text);
    let fault = app.fault.filter(|_| req.path.starts_with("/v1/"));
    match fault {
        Some(Fault::Hang) => {
            log_request(&req, &body, started, "fault hang: holding the connection");
            // Hold the connection until the client gives up.
            let _ = stream.set_read_timeout(None);
            let mut sink = [0u8; 256];
            while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
            return;
        }
        Some(Fault::Close) => {
            return log_request(&req, &body, started, "fault close: closed without reply");
        }
        Some(Fault::Empty) => body.clear(),
        Some(Fault::WrongRid) => body = frame(out.rid % RID_MAX + 1, out.code, &out.text),
        Some(Fault::Malformed) => body = "garbage|x".to_string(),
        Some(Fault::Delay) => thread::sleep(FAULT_DELAY),
        Some(Fault::Oversize(_) | Fault::NonAscii) | None => {}
    }
    match stream.write_all(&format_response(200, &body)) {
        Ok(()) => log_request(&req, &body, started, ""),
        Err(e) => log_request(&req, &body, started, &format!("write error: {e}")),
    }
}

/// One line per request: route, rid, job, the answer and the time taken. Player text is
/// shown only as a preview.
fn log_request(req: &Request, body: &str, started: Instant, note: &str) {
    let param = |k: &str| req.param(k).map_or_else(|| "-".to_string(), preview);
    let mut line = format!(
        "{} {} rid={} job={}",
        req.method,
        req.path,
        param("rid"),
        param("job")
    );
    if req.path == protocol::ROUTE_TALK {
        line += &format!(
            " npc={} day={} msg={:?}",
            param("npc"),
            param("day"),
            param("msg")
        );
    } else if req.path == protocol::ROUTE_TALK_V2 {
        line += &format!(
            " camp={} conv={} head={} troop={} day={} msg={:?}",
            param("camp"),
            param("conv"),
            param("head"),
            param("troop"),
            param("day"),
            param("msg")
        );
    }
    if !req.body.is_empty() {
        line += &format!(" (request body of {} bytes ignored)", req.body.len());
    }
    line += &format!(
        " -> {:?} in {} ms",
        preview(body),
        started.elapsed().as_millis()
    );
    if !note.is_empty() {
        line += &format!(" [{note}]");
    }
    log(line);
}

/// At most 80 characters of `s`, marked with "..." if cut.
fn preview(s: &str) -> String {
    const MAX: usize = 80;
    match s.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

/// Prints one timestamped line to stdout.
fn log(msg: impl fmt::Display) {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    println!("[{}.{:03}] {msg}", t.as_secs(), t.subsec_millis());
}

#[cfg(test)]
mod tests;
