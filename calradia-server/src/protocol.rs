//! Protocol v1: constants, the response frame and `/v1` request validation
//! (docs/protocol-v1.md).
//!
//! This file is the source of truth for the protocol's numbers. The mod mirrors them and
//! tests/test_calradia_ai.py reads the `pub const` lines below with a regex, so keep each
//! one a plain literal on a line of its own.

use crate::http::Request;
use crate::ids::{self, troop_kind, Kind};
use crate::npc::{self, Npc};
use crate::sanitize::{sanitize_input, sanitize_text};
use serde::Serialize;

pub const PROTOCOL_VERSION: u32 = 1;
/// `/v2/talk` carries it; `/v1/result` and `/v1/cancel` serve jobs of both talk versions.
pub const PROTOCOL_V2: u32 = 2;

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
/// v2 names (NPC, factions, location) are cut to this many characters.
pub const MAX_NAME: usize = 40;
/// v2 integer fields must lie within -MAX_STAT..=MAX_STAT.
pub const MAX_STAT: i32 = 1000000;

// v2 status bits (the `st=` field).
pub const ST_IN_PARTY: u32 = 1;
pub const ST_PLAYER_PRISONER: u32 = 2;
pub const ST_OTHER_PRISONER: u32 = 4;
pub const ST_FACTION_LEADER: u32 = 8;
pub const ST_MARSHAL: u32 = 16;
pub const ST_SPOUSE: u32 = 32;
pub const ST_BETROTHED: u32 = 64;
pub const ST_PLAYER_VASSAL: u32 = 128;
pub const ST_PLAYER_RULER: u32 = 256;
/// The `wars=` mask has one bit per realm, from `fac_player_supporters_faction` (bit 0)
/// through `fac_kingdom_6` (bit 6), as in module_constants.py kingdoms_begin..kingdoms_end.
pub const WARS_MAX: u32 = 127;

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
pub const ROUTE_TALK_V2: &str = "/v2/talk";

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
pub const REASON_MEMORY_UNAVAILABLE: &str = "memory_unavailable";
pub const REASON_MEMORY_ERROR: &str = "memory_error";

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
#[derive(Clone, Debug)]
pub struct TalkParams {
    pub npc: &'static Npc,
    pub day: u32,
    /// Sanitized, at most `MAX_PNAME` characters; may be empty.
    pub pname: String,
    /// Sanitized, 1..=`MAX_MSG` characters.
    pub msg: String,
}

/// The live game state a `/v2/talk` carries (docs/protocol-v1.md, "Protocol v2"). Every
/// value is what the game read at send time; nothing here is inferred by the server.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GameContext {
    pub day: u32,
    /// Faction indices (ID_factions.py). `player_faction` is `$players_kingdom`, 0 if none.
    pub faction: u32,
    pub player_faction: u32,
    /// `store_relation` of the NPC's faction and `fac_player_faction`: below 0 is hostile.
    pub faction_relation: i32,
    /// The NPC's `slot_troop_player_relation`, -100..100.
    pub relation: i32,
    /// `slot_lord_reputation_type` (lrep_*) and `slot_troop_occupation` (slto_*).
    pub reputation: u32,
    pub occupation: u32,
    /// `ST_*` bits.
    pub status: u32,
    pub renown: i32,
    pub honor: i32,
    /// The settlement nearest to the player's party (ID_parties.py) and its map distance.
    pub location: u32,
    pub location_distance: u32,
    pub player_female: bool,
    /// Names as the game shows them, sanitized, at most `MAX_PNAME` / `MAX_NAME` characters.
    pub player_name: String,
    pub npc_name: String,
    pub faction_name: String,
    pub player_faction_name: String,
    pub location_name: String,
    /// Optional fields (added after the first v2 build, so absent from older mods):
    /// the realms the NPC's faction is at war with (`WARS_MAX` bits), and the names of the
    /// NPC's liege, spouse and father as the game shows them (empty if none or not sent).
    pub wars: Option<u32>,
    pub ruler_name: String,
    pub spouse_name: String,
    pub father_name: String,
}

/// The validated parameters of a `/v2/talk`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TalkV2 {
    /// The game's campaign id (`$cai_campaign`).
    pub campaign: u32,
    /// The game's conversation id: one per opening of the talk window.
    pub conversation: u32,
    /// The job id of the last reply the player saw in this save (`$cai_mem_head`), or 0.
    pub head: u32,
    pub troop: u32,
    /// The troop's stable Module System identifier, e.g. `trp_npc1`.
    pub character: String,
    pub context: GameContext,
    pub msg: String,
}

impl TalkV2 {
    /// Everything that defines the talk; two talks with the same job id must agree on it.
    pub fn fingerprint(&self) -> String {
        serde_json::to_string(self).expect("serializable")
    }
}

#[derive(Clone, Debug)]
pub enum Talk {
    V1(TalkParams),
    V2(Box<TalkV2>),
}

#[derive(Debug)]
pub enum Op {
    Talk(Box<Talk>),
    Result,
    Cancel,
}

/// A valid request.
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

/// Validates a request against the server input rules of protocols v1 and v2.
///
/// Checks run in this order and the first failure wins: target length (`too_long`),
/// route (`bad_param`), `end=1` (`truncated`, checked early because a truncated URL
/// explains any later failure), `v` (`bad_version`: 1 for `/v1/*`, 2 for `/v2/talk`), then
/// rid, job and the talk's metadata (`bad_param`), then msg (`empty_msg` / `too_long`).
/// The rid is echoed whenever it is valid, so the game can match even a rejection to its
/// request.
pub fn parse(req: &Request, npcs: &'static [Npc]) -> Result<V1Request, Rejection> {
    let rid = parse_id(req.param("rid")).unwrap_or(0);
    let reject = |reason| Err(Rejection { rid, reason });
    if req.target.len() > MAX_TARGET {
        return reject(REASON_TOO_LONG);
    }
    let version = match req.path.as_str() {
        ROUTE_TALK | ROUTE_RESULT | ROUTE_CANCEL => PROTOCOL_VERSION,
        ROUTE_TALK_V2 => PROTOCOL_V2,
        _ => return reject(REASON_BAD_PARAM),
    };
    if req.param("end") != Some("1") {
        return reject(REASON_TRUNCATED);
    }
    if parse_uint(req.param("v")) != Some(version) {
        return reject(REASON_BAD_VERSION);
    }
    if rid == 0 {
        return reject(REASON_BAD_PARAM);
    }
    let Some(job) = parse_id(req.param("job")) else {
        return reject(REASON_BAD_PARAM);
    };
    let op = match req.path.as_str() {
        ROUTE_TALK => {
            let Some(npc) = parse_uint(req.param("npc")).and_then(|id| npc::find(npcs, id)) else {
                return reject(REASON_BAD_PARAM);
            };
            let Some(day) = parse_day(req) else {
                return reject(REASON_BAD_PARAM);
            };
            let msg = match parse_msg(req) {
                Ok(m) => m,
                Err(reason) => return reject(reason),
            };
            Op::Talk(Box::new(Talk::V1(TalkParams {
                npc,
                day,
                pname: name(req, "pname", MAX_PNAME),
                msg,
            })))
        }
        ROUTE_TALK_V2 => {
            let Some(mut talk) = parse_v2_metadata(req) else {
                return reject(REASON_BAD_PARAM);
            };
            talk.msg = match parse_msg(req) {
                Ok(m) => m,
                Err(reason) => return reject(reason),
            };
            Op::Talk(Box::new(Talk::V2(Box::new(talk))))
        }
        ROUTE_RESULT => Op::Result,
        _ => Op::Cancel,
    };
    Ok(V1Request { rid, job, op })
}

/// The metadata of a `/v2/talk`; None if any field is missing or out of range. `msg` is
/// left empty for the caller.
fn parse_v2_metadata(req: &Request) -> Option<TalkV2> {
    let ids = ids::ids();
    let int = |k: &str| parse_int(req.param(k)).filter(|n| n.abs() <= MAX_STAT);
    let uint = |k: &str| int(k).and_then(|n| u32::try_from(n).ok());
    let index = |k: &str, table: &ids::Table| uint(k).filter(|&i| (i as usize) < table.len());
    let troop = uint("troop")?;
    let character = ids
        .troops
        .name(troop)
        .filter(|c| troop_kind(c) != Kind::Other)?;
    let context = GameContext {
        day: parse_day(req)?,
        faction: index("fac", &ids.factions)?,
        player_faction: index("pfac", &ids.factions)?,
        faction_relation: int("frel")?,
        relation: int("rel")?,
        reputation: uint("rep")?,
        occupation: uint("occ")?,
        status: uint("st")?,
        renown: int("ren")?,
        honor: int("hon")?,
        location: index("loc", &ids.parties)?,
        location_distance: uint("ldist")?,
        player_female: match uint("pg")? {
            0 => false,
            1 => true,
            _ => return None,
        },
        player_name: name(req, "pname", MAX_PNAME),
        npc_name: name(req, "nname", MAX_NAME),
        faction_name: name(req, "fname", MAX_NAME),
        player_faction_name: name(req, "pfname", MAX_NAME),
        location_name: name(req, "lname", MAX_NAME),
        wars: match req.param("wars") {
            None => None,
            Some(_) => Some(uint("wars").filter(|&w| w <= WARS_MAX)?),
        },
        ruler_name: name(req, "ruler", MAX_NAME),
        spouse_name: name(req, "spouse", MAX_NAME),
        father_name: name(req, "father", MAX_NAME),
    };
    Some(TalkV2 {
        campaign: parse_id(req.param("camp"))?,
        conversation: parse_id(req.param("conv"))?,
        head: parse_uint(req.param("head")).filter(|&h| h <= RID_MAX)?,
        troop,
        character: character.to_string(),
        context,
        msg: String::new(),
    })
}

fn parse_day(req: &Request) -> Option<u32> {
    parse_uint(req.param("day")).filter(|d| *d <= MAX_DAY)
}

/// The sanitized message: 1..=`MAX_MSG` characters, rejected (never truncated) otherwise.
fn parse_msg(req: &Request) -> Result<String, &'static str> {
    // Input sanitizing leaves only ASCII, so byte lengths are character counts.
    let msg = sanitize_input(req.param("msg").unwrap_or(""));
    if msg.is_empty() {
        Err(REASON_EMPTY_MSG)
    } else if msg.len() > MAX_MSG {
        Err(REASON_TOO_LONG)
    } else {
        Ok(msg)
    }
}

/// A sanitized free-text name, cut to `max` characters; may be empty.
fn name(req: &Request, key: &str, max: usize) -> String {
    let mut s = sanitize_input(req.param(key).unwrap_or(""));
    s.truncate(max);
    s.truncate(s.trim_end().len());
    s
}

/// Parses a plain decimal integer (ASCII digits only: no sign, no spaces).
fn parse_uint(s: Option<&str>) -> Option<u32> {
    s.filter(|s| !s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
}

/// Parses a decimal integer with an optional leading `-` (the engine sends negative
/// register values that way).
fn parse_int(s: Option<&str>) -> Option<i32> {
    let s = s?;
    let (negative, digits) = s.strip_prefix('-').map_or((false, s), |d| (true, d));
    let n = parse_uint(Some(digits)).and_then(|n| i32::try_from(n).ok())?;
    Some(if negative { -n } else { n })
}

/// Parses a rid or job id: an integer in 1..=`RID_MAX`.
fn parse_id(s: Option<&str>) -> Option<u32> {
    parse_uint(s).filter(|n| (1..=RID_MAX).contains(n))
}
