//! Protocol v1: constants, the response frame and `/v1` request validation
//! (docs/protocol-v1.md).
//!
//! This file is the source of truth for the protocol's numbers. The mod mirrors them and
//! tests/test_calradia_ai.py reads the `pub const` lines below with a regex, so keep each
//! one a plain literal on a line of its own.

use crate::http::Request;
use crate::npc::{self, Npc};
use crate::sanitize::{sanitize_input, sanitize_text};

pub const PROTOCOL_VERSION: u32 = 1;

pub const CODE_READY: u8 = 0;
pub const CODE_PENDING: u8 = 1;
pub const CODE_FAILED: u8 = 2;
pub const CODE_CANCELED: u8 = 3;
pub const CODE_BAD_REQUEST: u8 = 4;
pub const CODE_UNKNOWN_JOB: u8 = 5;
pub const CODE_BUSY: u8 = 6;

pub const MAX_TEXT: usize = 500;
pub const MAX_MSG: usize = 300;
pub const MAX_PNAME: usize = 32;
pub const MAX_TARGET: usize = 4096;
pub const MAX_DAY: u32 = 100000;
pub const RID_MAX: u32 = 999999999;

pub const JOB_DEADLINE_SECS: u64 = 90;
pub const GAME_GIVE_UP_SECS: u64 = 120;
pub const JOB_TTL_SECS: u64 = 600;
pub const MAX_JOBS: usize = 64;
pub const QUEUE_LEN: usize = 8;
pub const UPSTREAM_CONNECT_TIMEOUT_SECS: u64 = 2;
pub const UPSTREAM_MAX_BODY: usize = 262144;

pub const DEFAULT_BIND: &str = "127.0.0.1:8766";
pub const ROUTE_TALK: &str = "/v1/talk";
pub const ROUTE_RESULT: &str = "/v1/result";
pub const ROUTE_CANCEL: &str = "/v1/cancel";

// Reason tokens: T for every code except READY.
pub const REASON_PENDING: &str = "pending";
pub const REASON_TIMEOUT: &str = "timeout";
pub const REASON_UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";
pub const REASON_UPSTREAM_ERROR: &str = "upstream_error";
pub const REASON_EMPTY_REPLY: &str = "empty_reply";
pub const REASON_SUPERSEDED: &str = "superseded";
pub const REASON_CANCELED: &str = "canceled";
pub const REASON_CONFLICT: &str = "conflict";
pub const REASON_TOO_LONG: &str = "too_long";
pub const REASON_EMPTY_MSG: &str = "empty_msg";
pub const REASON_TRUNCATED: &str = "truncated";
pub const REASON_BAD_VERSION: &str = "bad_version";
pub const REASON_BAD_PARAM: &str = "bad_param";
pub const REASON_UNKNOWN_JOB: &str = "unknown_job";
pub const REASON_BUSY: &str = "busy";

// The game must outwait the server's deadline, or it would give up on jobs that can
// still finish.
const _: () = assert!(GAME_GIVE_UP_SECS > JOB_DEADLINE_SECS);

/// Builds the response body `R|C|T|R`.
///
/// `sanitize_text` is applied here, so it runs exactly once on every T. A READY job stores
/// the model's raw reply, which the worker has already checked sanitizes to a text with a
/// letter; if it somehow did not, the frame degrades to FAILED `empty_reply` instead of
/// breaking the frame's guarantees. A rid above `RID_MAX` is sent as 0, which keeps the
/// frame within 522 bytes.
pub fn frame(rid: u32, code: u8, text: &str) -> String {
    debug_assert!(code <= CODE_BUSY, "unknown code {code}");
    let rid = if rid <= RID_MAX { rid } else { 0 };
    match sanitize_text(text) {
        Ok(t) => format!("{rid}|{code}|{t}|{rid}"),
        Err(reason) => format!("{rid}|{CODE_FAILED}|{reason}|{rid}"),
    }
}

/// The validated parameters of a `/v1/talk`.
#[derive(Debug)]
pub struct TalkParams {
    pub npc: &'static Npc,
    pub day: u32,
    /// Sanitized, at most `MAX_PNAME` characters; may be empty.
    pub pname: String,
    /// Sanitized, 1..=`MAX_MSG` characters.
    pub msg: String,
}

#[derive(Debug)]
pub enum Op {
    Talk(TalkParams),
    Result,
    Cancel,
}

/// A valid `/v1` request.
#[derive(Debug)]
pub struct V1Request {
    pub rid: u32,
    pub job: u32,
    pub op: Op,
}

/// A request answered with code 4. `rid` is the request's rid if it was valid, else 0.
#[derive(Debug, PartialEq)]
pub struct Rejection {
    pub rid: u32,
    pub reason: &'static str,
}

/// Validates a request against the server input rules of protocol v1.
///
/// Checks run in this order and the first failure wins: target length (`too_long`),
/// route (`bad_param`), `end=1` (`truncated`, checked early because a truncated URL
/// explains any later failure), `v` (`bad_version`), then rid, job, npc and day
/// (`bad_param`), then msg (`empty_msg` / `too_long`). The rid is echoed whenever it is
/// valid, so the game can match even a rejection to its request.
pub fn parse_v1(req: &Request, npcs: &'static [Npc]) -> Result<V1Request, Rejection> {
    let rid = parse_id(req.param("rid")).unwrap_or(0);
    let reject = |reason| Err(Rejection { rid, reason });
    if req.target.len() > MAX_TARGET {
        return reject(REASON_TOO_LONG);
    }
    let is_talk = match req.path.as_str() {
        ROUTE_TALK => true,
        ROUTE_RESULT | ROUTE_CANCEL => false,
        _ => return reject(REASON_BAD_PARAM),
    };
    if req.param("end") != Some("1") {
        return reject(REASON_TRUNCATED);
    }
    if parse_uint(req.param("v")) != Some(PROTOCOL_VERSION) {
        return reject(REASON_BAD_VERSION);
    }
    if rid == 0 {
        return reject(REASON_BAD_PARAM);
    }
    let Some(job) = parse_id(req.param("job")) else {
        return reject(REASON_BAD_PARAM);
    };
    let op = if is_talk {
        let Some(npc) = parse_uint(req.param("npc")).and_then(|id| npc::find(npcs, id)) else {
            return reject(REASON_BAD_PARAM);
        };
        let Some(day) = parse_uint(req.param("day")).filter(|d| *d <= MAX_DAY) else {
            return reject(REASON_BAD_PARAM);
        };
        // Input sanitizing leaves only ASCII, so byte lengths are character counts.
        let msg = sanitize_input(req.param("msg").unwrap_or(""));
        if msg.is_empty() {
            return reject(REASON_EMPTY_MSG);
        }
        if msg.len() > MAX_MSG {
            return reject(REASON_TOO_LONG);
        }
        let mut pname = sanitize_input(req.param("pname").unwrap_or(""));
        pname.truncate(MAX_PNAME);
        pname.truncate(pname.trim_end().len());
        Op::Talk(TalkParams {
            npc,
            day,
            pname,
            msg,
        })
    } else if req.path == ROUTE_RESULT {
        Op::Result
    } else {
        Op::Cancel
    };
    Ok(V1Request { rid, job, op })
}

/// Parses a plain decimal integer (ASCII digits only: no sign, no spaces).
fn parse_uint(s: Option<&str>) -> Option<u32> {
    s.filter(|s| !s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
}

/// Parses a rid or job id: an integer in 1..=`RID_MAX`.
fn parse_id(s: Option<&str>) -> Option<u32> {
    parse_uint(s).filter(|n| (1..=RID_MAX).contains(n))
}
