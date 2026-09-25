//! Persistent conversation memory in SQLite (docs/protocol-v1.md, "Memory").
//!
//! A *turn* is one player message and the NPC's reply. Turns are keyed by (campaign, job),
//! the game's own ids, which makes a retried talk idempotent across server restarts. Each
//! turn records the game's memory head at the time it was sent (`parent_job`): the job id
//! of the last reply that save had shown the player. The turns reachable from a head
//! through `parent_job` are that savegame's history, and nothing else is. Reloading an
//! older save sends an older head, so replies from an abandoned branch are never recalled,
//! and a new campaign starts from head 0 with an empty history.
//!
//! Turns are written when the reply is ready, before the game can fetch it. A turn becomes
//! *delivered* when a later talk names it as its head, which the game does only after
//! showing the reply. Undelivered turns (canceled, lost, never shown) stay in the database
//! for inspection but are not on any chain, so they are never recalled.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE campaigns (
    id INTEGER PRIMARY KEY,           -- the game's $cai_campaign
    created_at INTEGER NOT NULL,      -- unix seconds
    first_day INTEGER NOT NULL,       -- campaign day of the first stored turn
    player_name TEXT NOT NULL
);
CREATE TABLE conversations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    campaign_id INTEGER NOT NULL REFERENCES campaigns(id),
    conv_key INTEGER NOT NULL,        -- the game's $cai_conv_id: one per talk window
    character TEXT NOT NULL,          -- troop identifier, e.g. trp_npc1
    started_day INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    UNIQUE (campaign_id, conv_key)
);
CREATE TABLE turns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    campaign_id INTEGER NOT NULL REFERENCES campaigns(id),
    job INTEGER NOT NULL,             -- the game's job id: idempotency key and chain node
    parent_job INTEGER NOT NULL,      -- the game's memory head when it sent this turn; 0 = none
    conversation_id INTEGER NOT NULL REFERENCES conversations(id),
    character TEXT NOT NULL,
    day INTEGER NOT NULL,
    player_name TEXT NOT NULL,
    player_text TEXT NOT NULL,
    npc_text TEXT NOT NULL,           -- exactly the text the game was sent
    context TEXT NOT NULL,            -- JSON: the live game state sent with the talk
    fingerprint TEXT NOT NULL,        -- all talk parameters, for idempotency checks
    created_at INTEGER NOT NULL,
    delivered_at INTEGER,             -- set when a later talk names this turn as its head
    UNIQUE (campaign_id, job)
);
";

/// Schema 2 (Milestones 4-6): actions on turns, the world chain, derived world events,
/// characters' plans and their initiatives. Applied on top of schema 1.
const MIGRATE_V2: &str = "
ALTER TABLE turns ADD COLUMN action_kind INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turns ADD COLUMN action_amount INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turns ADD COLUMN parent_outcome INTEGER NOT NULL DEFAULT 0;
CREATE TABLE world_nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    campaign_id INTEGER NOT NULL,
    node INTEGER NOT NULL,            -- the game's id for the node
    parent INTEGER NOT NULL,          -- the game's world head when it sent the node; 0 = none
    kind TEXT NOT NULL,               -- log, snapshot or tick
    day INTEGER NOT NULL,
    data TEXT NOT NULL,               -- JSON: the log entry, snapshot or tick
    fingerprint TEXT NOT NULL,
    initiative INTEGER,               -- tick: the initiative delivered with it
    created_at INTEGER NOT NULL,
    UNIQUE (campaign_id, node)
);
CREATE TABLE world_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    campaign_id INTEGER NOT NULL,
    node INTEGER NOT NULL,            -- the world node it was derived from
    day INTEGER NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    troops TEXT NOT NULL,             -- ,trp_a,trp_b,
    factions TEXT NOT NULL,           -- ,fac_a,fac_b,
    importance INTEGER NOT NULL
);
CREATE INDEX world_events_node ON world_events (campaign_id, node);
CREATE TABLE plans (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    campaign_id INTEGER NOT NULL,
    character TEXT NOT NULL,
    base_node INTEGER NOT NULL,       -- the world head it was made on; 0 = none
    day INTEGER NOT NULL,
    goal TEXT NOT NULL,
    plan TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX plans_base ON plans (campaign_id, base_node);
CREATE TABLE initiatives (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id INTEGER NOT NULL REFERENCES plans(id),
    campaign_id INTEGER NOT NULL,
    character TEXT NOT NULL,
    base_node INTEGER NOT NULL,
    day INTEGER NOT NULL,
    kind INTEGER NOT NULL,            -- INIT_*
    amount INTEGER NOT NULL,
    target TEXT NOT NULL,             -- rivalry: the other lord's troop id
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX initiatives_base ON initiatives (campaign_id, base_node);
";

/// How far back a chain is followed. Far more turns than a campaign is likely to have;
/// it also stops a (theoretical) cycle of parent links.
pub const MAX_CHAIN: u32 = 5000;
/// The same for world chains, which get a node per log entry, snapshot and tick.
pub const MAX_WORLD_CHAIN: u32 = 100000;

/// The recursive walk of a world chain: `chain(node, parent, depth)` from node ?2 of
/// campaign ?1, at most ?3 deep. Depth 0 is the head.
const WORLD_CHAIN: &str = "WITH RECURSIVE chain(node, parent, depth) AS (
        SELECT node, parent, 0 FROM world_nodes WHERE campaign_id = ?1 AND node = ?2
        UNION ALL
        SELECT w.node, w.parent, chain.depth + 1
        FROM world_nodes w JOIN chain ON w.campaign_id = ?1 AND w.node = chain.parent
        WHERE chain.parent != 0 AND chain.depth + 1 < ?3
    )";

/// Why memory could not be used; maps to the FAILED reasons `memory_unavailable` and
/// `memory_error`.
#[derive(Debug, PartialEq)]
pub enum MemoryFailure {
    Unavailable(String),
    Error(String),
}

impl std::fmt::Display for MemoryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            MemoryFailure::Unavailable(e) => write!(f, "memory unavailable: {e}"),
            MemoryFailure::Error(e) => write!(f, "memory error: {e}"),
        }
    }
}

impl From<rusqlite::Error> for MemoryFailure {
    fn from(e: rusqlite::Error) -> Self {
        MemoryFailure::Error(e.to_string())
    }
}

/// A stored turn, as recalled.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub job: u32,
    pub conversation: u32,
    pub day: u32,
    pub player_name: String,
    pub player_text: String,
    pub npc_text: String,
    /// The action proposed with the reply (`ACT_*`, 0 = none) and its amount.
    pub action_kind: u32,
    pub action_amount: i32,
    /// What became of it (`OUT_*`), as the next turn on the chain reported; 0 = unknown.
    pub outcome: u32,
}

/// A turn to store.
#[derive(Debug)]
pub struct NewTurn<'a> {
    pub campaign: u32,
    pub job: u32,
    pub parent: u32,
    pub conversation: u32,
    pub character: &'a str,
    pub day: u32,
    pub player_name: &'a str,
    pub player_text: &'a str,
    pub npc_text: &'a str,
    pub context: &'a str,
    pub fingerprint: &'a str,
    pub action_kind: u32,
    pub action_amount: i32,
    /// The outcome of the parent turn's proposal, as this talk reported it.
    pub parent_outcome: u32,
}

/// A turn that already exists under the same (campaign, job).
#[derive(Debug, PartialEq)]
pub struct Existing {
    pub fingerprint: String,
    pub npc_text: String,
    pub action_kind: u32,
    pub action_amount: i32,
}

/// All of one character's turns on a savegame's chain, oldest first.
#[derive(Debug, Default, PartialEq)]
pub struct History {
    /// False if the head was not 0 but no stored turn has it: the chain is lost (another
    /// database, or a deleted one), so the history is empty.
    pub head_found: bool,
    /// Turns on the chain, all characters.
    pub chain_len: u32,
    pub turns: Vec<Turn>,
}

pub struct Memory {
    conn: Mutex<Result<Connection, String>>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Memory {
    /// Opens (creating if needed) the database at `path`. Never fails: a database that
    /// cannot be used makes every call answer `Unavailable` with the reason, which
    /// `status` also returns.
    pub fn open(path: &Path) -> Memory {
        let conn = (|| {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
            let conn = Connection::open(path).map_err(|e| e.to_string())?;
            prepare(&conn).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(conn)
        })();
        Memory {
            conn: Mutex::new(conn),
        }
    }

    /// A private in-memory database, for tests.
    #[cfg(test)]
    pub fn in_memory() -> Memory {
        let conn = Connection::open_in_memory().unwrap();
        prepare(&conn).unwrap();
        Memory {
            conn: Mutex::new(Ok(conn)),
        }
    }

    pub fn status(&self) -> Result<(), String> {
        self.lock().as_ref().map(|_| ()).map_err(Clone::clone)
    }

    fn lock(&self) -> MutexGuard<'_, Result<Connection, String>> {
        // SQLite rolls back an unfinished transaction, so a poisoned lock is safe to reuse.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, MemoryFailure>,
    ) -> Result<T, MemoryFailure> {
        match &mut *self.lock() {
            Ok(conn) => f(conn),
            Err(e) => Err(MemoryFailure::Unavailable(e.clone())),
        }
    }

    /// The turn stored under (campaign, job), if any.
    pub fn find(&self, campaign: u32, job: u32) -> Result<Option<Existing>, MemoryFailure> {
        self.with(|c| {
            Ok(c.query_row(EXISTING_TURN, params![campaign, job], existing)
                .optional()?)
        })
    }

    /// Marks the head turn delivered (the game shows a reply before making it its head).
    /// Returns whether the head exists; head 0 always does.
    pub fn deliver(&self, campaign: u32, head: u32) -> Result<bool, MemoryFailure> {
        if head == 0 {
            return Ok(true);
        }
        self.with(|c| {
            c.execute(
                "UPDATE turns SET delivered_at = ?3 \
                 WHERE campaign_id = ?1 AND job = ?2 AND delivered_at IS NULL",
                params![campaign, head, now()],
            )?;
            let found = c
                .query_row(
                    "SELECT 1 FROM turns WHERE campaign_id = ?1 AND job = ?2",
                    params![campaign, head],
                    |_| Ok(()),
                )
                .optional()?;
            Ok(found.is_some())
        })
    }

    /// `character`'s turns on the chain that ends at `head`, oldest first.
    pub fn history(
        &self,
        campaign: u32,
        head: u32,
        character: &str,
    ) -> Result<History, MemoryFailure> {
        if head == 0 {
            return Ok(History {
                head_found: true,
                ..History::default()
            });
        }
        self.with(|c| {
            let mut stmt = c.prepare_cached(
                "WITH RECURSIVE chain(job, parent_job, depth) AS (
                     SELECT job, parent_job, 0 FROM turns WHERE campaign_id = ?1 AND job = ?2
                     UNION ALL
                     SELECT t.job, t.parent_job, chain.depth + 1
                     FROM turns t JOIN chain
                       ON t.campaign_id = ?1 AND t.job = chain.parent_job
                     WHERE chain.parent_job != 0 AND chain.depth + 1 < ?3
                 )
                 SELECT t.job, conv.conv_key, t.day, t.player_name, t.player_text, t.npc_text,
                        t.character = ?4, (SELECT count(*) FROM chain),
                        t.action_kind, t.action_amount, t.parent_outcome
                 FROM chain
                 JOIN turns t ON t.campaign_id = ?1 AND t.job = chain.job
                 JOIN conversations conv ON conv.id = t.conversation_id
                 ORDER BY chain.depth DESC",
            )?;
            let mut history = History::default();
            let rows = stmt.query_map(params![campaign, head, MAX_CHAIN, character], |r| {
                let turn = Turn {
                    job: r.get(0)?,
                    conversation: r.get(1)?,
                    day: r.get(2)?,
                    player_name: r.get(3)?,
                    player_text: r.get(4)?,
                    npc_text: r.get(5)?,
                    action_kind: r.get(8)?,
                    action_amount: r.get(9)?,
                    outcome: 0,
                };
                Ok((
                    turn,
                    r.get::<_, bool>(6)?,
                    r.get::<_, u32>(7)?,
                    r.get::<_, u32>(10)?,
                ))
            })?;
            // Each turn reports the outcome of its parent's proposal.
            let mut last_mine = false;
            for row in rows {
                let (turn, mine, chain_len, parent_outcome) = row?;
                history.head_found = true;
                history.chain_len = chain_len;
                if last_mine {
                    if let Some(parent) = history.turns.last_mut() {
                        parent.outcome = parent_outcome;
                    }
                }
                last_mine = mine;
                if mine {
                    history.turns.push(turn);
                }
            }
            Ok(history)
        })
    }

    /// Stores a turn in one transaction, creating its campaign and conversation records.
    /// If (campaign, job) is already stored, nothing changes and the stored turn is
    /// returned instead.
    pub fn store(&self, t: &NewTurn) -> Result<Option<Existing>, MemoryFailure> {
        self.with(|c| {
            let tx = c.transaction()?;
            let at = now();
            tx.execute(
                "INSERT INTO campaigns (id, created_at, first_day, player_name) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT (id) DO NOTHING",
                params![t.campaign, at, t.day, t.player_name],
            )?;
            tx.execute(
                "INSERT INTO conversations \
                 (campaign_id, conv_key, character, started_day, started_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (campaign_id, conv_key) DO NOTHING",
                params![t.campaign, t.conversation, t.character, t.day, at],
            )?;
            let conversation: i64 = tx.query_row(
                "SELECT id FROM conversations WHERE campaign_id = ?1 AND conv_key = ?2",
                params![t.campaign, t.conversation],
                |r| r.get(0),
            )?;
            let inserted = tx.execute(
                "INSERT INTO turns (campaign_id, job, parent_job, conversation_id, character, \
                 day, player_name, player_text, npc_text, context, fingerprint, created_at, \
                 action_kind, action_amount, parent_outcome) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
                 ON CONFLICT (campaign_id, job) DO NOTHING",
                params![
                    t.campaign,
                    t.job,
                    t.parent,
                    conversation,
                    t.character,
                    t.day,
                    t.player_name,
                    t.player_text,
                    t.npc_text,
                    t.context,
                    t.fingerprint,
                    at,
                    t.action_kind,
                    t.action_amount,
                    t.parent_outcome
                ],
            )?;
            let existing = if inserted == 0 {
                Some(tx.query_row(EXISTING_TURN, params![t.campaign, t.job], existing)?)
            } else {
                None
            };
            tx.commit()?;
            Ok(existing)
        })
    }

    /// A plain-text summary of every campaign, for `--memory-report`.
    pub fn report(&self) -> Result<String, MemoryFailure> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT cp.id, cp.player_name, cp.first_day,
                        (SELECT count(*) FROM conversations v WHERE v.campaign_id = cp.id),
                        (SELECT count(*) FROM turns t WHERE t.campaign_id = cp.id),
                        (SELECT count(*) FROM turns t
                         WHERE t.campaign_id = cp.id AND t.delivered_at IS NOT NULL),
                        (SELECT group_concat(DISTINCT t.character) FROM turns t
                         WHERE t.campaign_id = cp.id)
                 FROM campaigns cp ORDER BY cp.created_at, cp.id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(format!(
                    "campaign {} (player {:?}, from day {}): {} conversations, {} turns \
                     ({} delivered), characters {}",
                    r.get::<_, u32>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, u32>(4)?,
                    r.get::<_, u32>(5)?,
                    r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                ))
            })?;
            let lines: Vec<String> = rows.collect::<Result<_, _>>()?;
            Ok(lines)
        })
        .and_then(|lines| {
            let mut out = String::new();
            for line in lines {
                let campaign: u32 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let _ = writeln!(out, "{line}{}", self.world_report(campaign)?);
            }
            if out.is_empty() {
                out = "no campaigns stored\n".into();
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------- world chain (M4-M6)

/// A world node already stored under the same (campaign, node).
#[derive(Debug, PartialEq)]
pub struct ExistingNode {
    pub fingerprint: String,
    pub initiative: Option<i64>,
}

/// A character's latest plan on a chain.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub day: u32,
    pub goal: String,
    pub plan: String,
}

/// An initiative a character decided on (M6).
#[derive(Clone, Debug, PartialEq)]
pub struct Initiative {
    pub id: i64,
    pub character: String,
    pub day: u32,
    pub kind: u32,
    pub amount: i32,
    /// Rivalry: the other lord's troop id; otherwise empty.
    pub target: String,
    pub text: String,
}

fn list(items: &[String]) -> String {
    format!(",{},", items.join(","))
}

fn unlist(s: &str) -> Vec<String> {
    s.split(',')
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

impl Memory {
    /// The node stored under (campaign, node), if any.
    pub fn find_node(
        &self,
        campaign: u32,
        node: u32,
    ) -> Result<Option<ExistingNode>, MemoryFailure> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT fingerprint, initiative FROM world_nodes WHERE campaign_id = ?1 AND node = ?2",
                params![campaign, node],
                |r| Ok(ExistingNode { fingerprint: r.get(0)?, initiative: r.get(1)? }),
            )
            .optional()?)
        })
    }

    /// The latest snapshot on the world chain that ends at `head`.
    pub fn last_snapshot(
        &self,
        campaign: u32,
        head: u32,
    ) -> Result<Option<crate::protocol::Snapshot>, MemoryFailure> {
        if head == 0 {
            return Ok(None);
        }
        self.with(|c| {
            let sql = format!(
                "{WORLD_CHAIN} SELECT w.data FROM chain JOIN world_nodes w \
                 ON w.campaign_id = ?1 AND w.node = chain.node \
                 WHERE w.kind = 'snapshot' ORDER BY chain.depth LIMIT 1"
            );
            let data: Option<String> = c
                .query_row(&sql, params![campaign, head, MAX_WORLD_CHAIN], |r| r.get(0))
                .optional()?;
            Ok(data.and_then(|d| match serde_json::from_str(&d) {
                Ok(crate::protocol::Payload::Snapshot(s)) => Some(s),
                _ => None,
            }))
        })
    }

    /// Stores a world node and the events derived from it, in one transaction. If the node
    /// is already stored, nothing changes and the stored node is returned instead.
    pub fn store_node(
        &self,
        n: &crate::protocol::WorldNode,
        events: &[crate::world::WorldEvent],
        initiative: Option<i64>,
    ) -> Result<Option<ExistingNode>, MemoryFailure> {
        use crate::protocol::Payload;
        let kind = match n.payload {
            Payload::Log(_) => "log",
            Payload::Snapshot(_) => "snapshot",
            Payload::Tick { .. } => "tick",
        };
        let data = serde_json::to_string(&n.payload).expect("serializable");
        let fingerprint = n.fingerprint();
        self.with(|c| {
            let tx = c.transaction()?;
            let at = now();
            tx.execute(
                "INSERT INTO campaigns (id, created_at, first_day, player_name) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT (id) DO NOTHING",
                params![n.campaign, at, n.day, n.player_name],
            )?;
            let inserted = tx.execute(
                "INSERT INTO world_nodes (campaign_id, node, parent, kind, day, data, \
                 fingerprint, initiative, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT (campaign_id, node) DO NOTHING",
                params![
                    n.campaign,
                    n.node,
                    n.parent,
                    kind,
                    n.day,
                    data,
                    fingerprint,
                    initiative,
                    at
                ],
            )?;
            if inserted == 0 {
                let existing = tx.query_row(
                    "SELECT fingerprint, initiative FROM world_nodes \
                     WHERE campaign_id = ?1 AND node = ?2",
                    params![n.campaign, n.node],
                    |r| {
                        Ok(ExistingNode {
                            fingerprint: r.get(0)?,
                            initiative: r.get(1)?,
                        })
                    },
                )?;
                return Ok(Some(existing));
            }
            for e in events {
                tx.execute(
                    "INSERT INTO world_events (campaign_id, node, day, kind, text, troops, \
                     factions, importance) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        n.campaign,
                        n.node,
                        e.day,
                        e.kind,
                        e.text,
                        list(&e.troops),
                        list(&e.factions),
                        e.importance
                    ],
                )?;
            }
            tx.commit()?;
            Ok(None)
        })
    }

    /// The world events on the chain that ends at `head` that may matter to `character` of
    /// `faction`: those involving them or the player, news of their realm, and major events.
    /// Oldest first. The bool is false if `head` is not stored (a lost chain).
    pub fn world_events(
        &self,
        campaign: u32,
        head: u32,
        character: &str,
        faction: &str,
    ) -> Result<(bool, Vec<crate::world::WorldEvent>), MemoryFailure> {
        if head == 0 {
            return Ok((true, Vec::new()));
        }
        let found = self.find_node(campaign, head)?.is_some();
        self.with(|c| {
            let sql = format!(
                "{WORLD_CHAIN} SELECT e.day, e.kind, e.text, e.troops, e.factions, e.importance \
                 FROM chain JOIN world_events e ON e.campaign_id = ?1 AND e.node = chain.node \
                 WHERE instr(e.troops, ?4) > 0 OR instr(e.troops, ',trp_player,') > 0 \
                    OR (?5 != '' AND instr(e.factions, ?5) > 0) OR e.importance >= 3 \
                 ORDER BY chain.depth DESC, e.id"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let character = format!(",{character},");
            let faction = if faction.is_empty() {
                String::new()
            } else {
                format!(",{faction},")
            };
            let rows = stmt.query_map(
                params![campaign, head, MAX_WORLD_CHAIN, character, faction],
                |r| {
                    Ok(crate::world::WorldEvent {
                        day: r.get(0)?,
                        kind: r.get(1)?,
                        text: r.get(2)?,
                        troops: unlist(&r.get::<_, String>(3)?),
                        factions: unlist(&r.get::<_, String>(4)?),
                        importance: r.get(5)?,
                    })
                },
            )?;
            Ok((found, rows.collect::<Result<Vec<_>, _>>()?))
        })
    }

    /// `character`'s latest plan made on the chain that ends at `head` (or before any node).
    pub fn latest_plan(
        &self,
        campaign: u32,
        head: u32,
        character: &str,
    ) -> Result<Option<Plan>, MemoryFailure> {
        self.with(|c| {
            let sql = format!(
                "{WORLD_CHAIN} SELECT day, goal, plan FROM plans \
                 WHERE campaign_id = ?1 AND character = ?4 \
                   AND (base_node = 0 OR base_node IN (SELECT node FROM chain)) \
                 ORDER BY day DESC, id DESC LIMIT 1"
            );
            Ok(c.query_row(
                &sql,
                params![campaign, head, MAX_WORLD_CHAIN, character],
                |r| {
                    Ok(Plan {
                        day: r.get(0)?,
                        goal: r.get(1)?,
                        plan: r.get(2)?,
                    })
                },
            )
            .optional()?)
        })
    }

    /// Stores a plan and, if any, the initiative that came with it.
    pub fn store_plan(
        &self,
        campaign: u32,
        character: &str,
        base: u32,
        plan: &Plan,
        initiative: Option<&Initiative>,
    ) -> Result<(), MemoryFailure> {
        self.with(|c| {
            let tx = c.transaction()?;
            let at = now();
            tx.execute(
                "INSERT INTO plans (campaign_id, character, base_node, day, goal, plan, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![campaign, character, base, plan.day, plan.goal, plan.plan, at],
            )?;
            let plan_id = tx.last_insert_rowid();
            if let Some(i) = initiative {
                tx.execute(
                    "INSERT INTO initiatives (plan_id, campaign_id, character, base_node, day, \
                     kind, amount, target, text, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![plan_id, campaign, character, base, i.day, i.kind, i.amount, i.target, i.text, at],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Initiatives delivered on the chain that ends at `head`, oldest first.
    pub fn delivered_initiatives(
        &self,
        campaign: u32,
        head: u32,
    ) -> Result<Vec<Initiative>, MemoryFailure> {
        if head == 0 {
            return Ok(Vec::new());
        }
        self.with(|c| {
            let sql = format!(
                "{WORLD_CHAIN} SELECT i.id, i.character, w.day, i.kind, i.amount, i.target, i.text \
                 FROM chain JOIN world_nodes w ON w.campaign_id = ?1 AND w.node = chain.node \
                 JOIN initiatives i ON i.id = w.initiative ORDER BY chain.depth DESC"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![campaign, head, MAX_WORLD_CHAIN], initiative_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Initiatives decided on the chain that ends at `head` (or before any node) from day
    /// `since` on, oldest first; delivered ones included.
    pub fn open_initiatives(
        &self,
        campaign: u32,
        head: u32,
        since: u32,
    ) -> Result<Vec<Initiative>, MemoryFailure> {
        self.with(|c| {
            let sql = format!(
                "{WORLD_CHAIN} SELECT id, character, day, kind, amount, target, text \
                 FROM initiatives WHERE campaign_id = ?1 AND day >= ?4 \
                   AND (base_node = 0 OR base_node IN (SELECT node FROM chain)) \
                 ORDER BY day, id"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(
                params![campaign, head, MAX_WORLD_CHAIN, since],
                initiative_row,
            )?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// For each character with a plan on the chain that ends at `head`, the day of its
    /// latest plan.
    pub fn plan_days(
        &self,
        campaign: u32,
        head: u32,
    ) -> Result<std::collections::HashMap<String, u32>, MemoryFailure> {
        self.with(|c| {
            let sql = format!(
                "{WORLD_CHAIN} SELECT character, max(day) FROM plans WHERE campaign_id = ?1 \
                   AND (base_node = 0 OR base_node IN (SELECT node FROM chain)) GROUP BY character"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![campaign, head, MAX_WORLD_CHAIN], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// The characters the player has spoken with on the conversation chain that ends at
    /// `head`.
    pub fn spoken_with(&self, campaign: u32, head: u32) -> Result<Vec<String>, MemoryFailure> {
        if head == 0 {
            return Ok(Vec::new());
        }
        self.with(|c| {
            let mut stmt = c.prepare_cached(
                "WITH RECURSIVE chain(job, parent_job, depth) AS (
                     SELECT job, parent_job, 0 FROM turns WHERE campaign_id = ?1 AND job = ?2
                     UNION ALL
                     SELECT t.job, t.parent_job, chain.depth + 1
                     FROM turns t JOIN chain ON t.campaign_id = ?1 AND t.job = chain.parent_job
                     WHERE chain.parent_job != 0 AND chain.depth + 1 < ?3
                 )
                 SELECT DISTINCT t.character FROM chain
                 JOIN turns t ON t.campaign_id = ?1 AND t.job = chain.job ORDER BY t.character",
            )?;
            let rows = stmt.query_map(params![campaign, head, MAX_CHAIN], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<String>, _>>()?)
        })
    }

    /// The live context last sent in a talk with `character` on the conversation chain that
    /// ends at `head` (JSON of `GameContext`), if any.
    pub fn last_context(
        &self,
        campaign: u32,
        head: u32,
        character: &str,
    ) -> Result<Option<String>, MemoryFailure> {
        if head == 0 {
            return Ok(None);
        }
        self.with(|c| {
            let mut stmt = c.prepare_cached(
                "WITH RECURSIVE chain(job, parent_job, depth) AS (
                     SELECT job, parent_job, 0 FROM turns WHERE campaign_id = ?1 AND job = ?2
                     UNION ALL
                     SELECT t.job, t.parent_job, chain.depth + 1
                     FROM turns t JOIN chain ON t.campaign_id = ?1 AND t.job = chain.parent_job
                     WHERE chain.parent_job != 0 AND chain.depth + 1 < ?3
                 )
                 SELECT t.context FROM chain JOIN turns t ON t.campaign_id = ?1 AND t.job = chain.job
                 WHERE t.character = ?4 ORDER BY chain.depth LIMIT 1",
            )?;
            Ok(stmt
                .query_row(params![campaign, head, MAX_CHAIN, character], |r| r.get(0))
                .optional()?)
        })
    }

    /// World nodes and derived events per campaign, for `--memory-report`.
    fn world_report(&self, campaign: u32) -> Result<String, MemoryFailure> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT (SELECT count(*) FROM world_nodes WHERE campaign_id = ?1),
                        (SELECT count(*) FROM world_events WHERE campaign_id = ?1),
                        (SELECT count(*) FROM plans WHERE campaign_id = ?1),
                        (SELECT count(*) FROM initiatives WHERE campaign_id = ?1)",
                params![campaign],
                |r| {
                    Ok(format!(
                        "; world {} nodes, {} events; {} plans, {} initiatives",
                        r.get::<_, u32>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, u32>(2)?,
                        r.get::<_, u32>(3)?
                    ))
                },
            )?)
        })
    }
}

fn initiative_row(r: &rusqlite::Row) -> rusqlite::Result<Initiative> {
    Ok(Initiative {
        id: r.get(0)?,
        character: r.get(1)?,
        day: r.get(2)?,
        kind: r.get(3)?,
        amount: r.get(4)?,
        target: r.get(5)?,
        text: r.get(6)?,
    })
}

const EXISTING_TURN: &str = "SELECT fingerprint, npc_text, action_kind, action_amount \
    FROM turns WHERE campaign_id = ?1 AND job = ?2";

fn existing(r: &rusqlite::Row) -> rusqlite::Result<Existing> {
    Ok(Existing {
        fingerprint: r.get(0)?,
        npc_text: r.get(1)?,
        action_kind: r.get(2)?,
        action_amount: r.get(3)?,
    })
}

/// Configures a fresh or existing database and checks it: creates the schema in an empty
/// database, refuses one written by a newer schema, and runs SQLite's quick check.
fn prepare(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(format!("integrity check failed: {check}")),
        ));
    }
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    match version {
        0 => conn.execute_batch(&format!(
            "BEGIN; {SCHEMA} {MIGRATE_V2} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
        )),
        1 => conn.execute_batch(&format!(
            "BEGIN; {MIGRATE_V2} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
        )),
        SCHEMA_VERSION => Ok(()),
        v => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
            Some(format!(
                "memory schema version {v} is newer than this server's {SCHEMA_VERSION}"
            )),
        )),
    }
}

// ---------------------------------------------------------------- recall

/// How many turns of each kind a prompt may use.
pub const CURRENT_TURNS: usize = 8;
pub const RECENT_TURNS: usize = 4;
pub const RELEVANT_TURNS: usize = 3;

/// What one talk recalls: a bounded, deterministic selection from a character's history.
#[derive(Debug, Default, PartialEq)]
pub struct Recall {
    pub head_found: bool,
    pub chain_len: u32,
    /// Every turn with this character on the chain, and in how many conversations.
    pub total_turns: usize,
    pub conversations: usize,
    pub first_day: Option<u32>,
    /// The latest turns of the conversation in progress, oldest first.
    pub current: Vec<Turn>,
    /// The latest turns of earlier conversations, oldest first.
    pub recent: Vec<Turn>,
    /// Older turns that share words with the player's message, oldest first.
    pub relevant: Vec<Turn>,
}

/// Selects from `history` for a talk in `conversation` whose message is `msg`: the current
/// conversation's last `CURRENT_TURNS`, the last `RECENT_TURNS` of earlier conversations,
/// and up to `RELEVANT_TURNS` older ones that share the most keywords with `msg` (newer
/// first among equals).
pub fn recall(history: History, conversation: u32, msg: &str) -> Recall {
    let total_turns = history.turns.len();
    let conversations = history
        .turns
        .iter()
        .map(|t| t.conversation)
        .collect::<BTreeSet<_>>()
        .len();
    let first_day = history.turns.first().map(|t| t.day);
    let (mut current, mut earlier): (Vec<Turn>, Vec<Turn>) = history
        .turns
        .into_iter()
        .partition(|t| t.conversation == conversation);
    current.drain(..current.len().saturating_sub(CURRENT_TURNS));
    let recent = earlier.split_off(earlier.len().saturating_sub(RECENT_TURNS));
    let wanted = keywords(msg);
    let mut scored: Vec<(usize, usize)> = earlier
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let words = keywords(&format!("{} {}", t.player_text, t.npc_text));
            (wanted.intersection(&words).count(), i)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut picked: Vec<usize> = scored.iter().take(RELEVANT_TURNS).map(|s| s.1).collect();
    picked.sort_unstable();
    let relevant = picked.into_iter().map(|i| earlier[i].clone()).collect();
    Recall {
        head_found: history.head_found,
        chain_len: history.chain_len,
        total_turns,
        conversations,
        first_day,
        current,
        recent,
        relevant,
    }
}

/// Lowercase words of four or more letters, minus common ones.
fn keywords(text: &str) -> BTreeSet<String> {
    const COMMON: &[&str] = &[
        "about", "after", "again", "also", "been", "before", "being", "come", "could", "does",
        "done", "down", "each", "even", "from", "give", "good", "have", "here", "into", "just",
        "know", "like", "made", "make", "many", "more", "most", "much", "must", "never", "once",
        "only", "other", "over", "said", "same", "shall", "should", "some", "such", "take", "tell",
        "than", "that", "their", "them", "then", "there", "these", "they", "thing", "think",
        "this", "those", "through", "time", "told", "very", "want", "well", "were", "what", "when",
        "where", "which", "while", "will", "with", "would", "your", "yours", "remember", "captain",
    ];
    text.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 4)
        .map(str::to_ascii_lowercase)
        .filter(|w| !COMMON.contains(&w.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn<'a>(
        campaign: u32,
        job: u32,
        parent: u32,
        conv: u32,
        who: &'a str,
        text: &'a str,
    ) -> NewTurn<'a> {
        NewTurn {
            campaign,
            job,
            parent,
            conversation: conv,
            character: who,
            day: job,
            player_name: "Ragnar",
            player_text: text,
            npc_text: "Aye.",
            context: "{}",
            fingerprint: text,
            action_kind: 0,
            action_amount: 0,
            parent_outcome: 0,
        }
    }

    fn texts(h: &History) -> Vec<&str> {
        h.turns.iter().map(|t| t.player_text.as_str()).collect()
    }

    #[test]
    fn chains_follow_the_savegame_branch() {
        let m = Memory::in_memory();
        // 1 <- 2 <- 3 is one branch; reloading the save made at 1 led to 1 <- 4.
        m.store(&turn(7, 1, 0, 100, "trp_npc1", "one")).unwrap();
        m.store(&turn(7, 2, 1, 100, "trp_npc1", "two")).unwrap();
        m.store(&turn(7, 3, 2, 101, "trp_npc2", "three")).unwrap();
        m.store(&turn(7, 4, 1, 102, "trp_npc1", "four")).unwrap();
        let h = m.history(7, 3, "trp_npc1").unwrap();
        assert!(h.head_found);
        assert_eq!((texts(&h), h.chain_len), (vec!["one", "two"], 3));
        assert_eq!(
            texts(&m.history(7, 4, "trp_npc1").unwrap()),
            ["one", "four"]
        );
        assert_eq!(texts(&m.history(7, 3, "trp_npc2").unwrap()), ["three"]);
        // Head 0: a fresh campaign. Another campaign's head: not found, empty.
        assert_eq!(m.history(7, 0, "trp_npc1").unwrap().turns, []);
        let other = m.history(8, 3, "trp_npc1").unwrap();
        assert!(!other.head_found && other.turns.is_empty());
        assert!(m.history(7, 0, "x").unwrap().head_found);
    }

    #[test]
    fn store_is_idempotent_and_reports_existing_turns() {
        let m = Memory::in_memory();
        assert_eq!(
            m.store(&turn(7, 1, 0, 100, "trp_npc1", "one")).unwrap(),
            None
        );
        let again = m.store(&turn(7, 1, 0, 100, "trp_npc1", "changed")).unwrap();
        assert_eq!(
            again,
            Some(Existing {
                fingerprint: "one".into(),
                npc_text: "Aye.".into(),
                action_kind: 0,
                action_amount: 0,
            })
        );
        assert_eq!(m.find(7, 1).unwrap().unwrap().fingerprint, "one");
        assert_eq!(m.find(7, 2).unwrap(), None);
        assert_eq!(m.find(8, 1).unwrap(), None);
        // The same job id in another campaign is a different turn.
        assert_eq!(
            m.store(&turn(8, 1, 0, 100, "trp_npc1", "other")).unwrap(),
            None
        );
        let report = m.report().unwrap();
        assert!(report.contains("campaign 7 (player \"Ragnar\", from day 1): 1 conversations, 1 turns (0 delivered)"), "{report}");
        assert!(m.deliver(7, 1).unwrap());
        assert!(m.deliver(7, 0).unwrap());
        assert!(!m.deliver(7, 99).unwrap());
        assert!(m.report().unwrap().contains("(1 delivered)"));
    }

    #[test]
    fn cycles_are_bounded() {
        let m = Memory::in_memory();
        m.store(&turn(7, 1, 2, 100, "trp_npc1", "one")).unwrap();
        m.store(&turn(7, 2, 1, 100, "trp_npc1", "two")).unwrap();
        let h = m.history(7, 2, "trp_npc1").unwrap();
        assert_eq!(h.chain_len, MAX_CHAIN);
    }

    #[test]
    fn unusable_databases_are_reported_not_fatal() {
        let dir = std::env::temp_dir().join(format!("calradia-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let garbage = dir.join("garbage.sqlite3");
        std::fs::write(&garbage, vec![0x5A; 8192]).unwrap();
        let m = Memory::open(&garbage);
        assert!(
            m.status().unwrap_err().contains("not a database"),
            "{:?}",
            m.status()
        );
        assert!(matches!(m.find(1, 1), Err(MemoryFailure::Unavailable(_))));
        // A path below a regular file cannot be created.
        let m = Memory::open(&garbage.join("x/memory.sqlite3"));
        assert!(m.status().is_err());
        // A database from a newer schema is refused.
        let newer = dir.join("newer.sqlite3");
        Connection::open(&newer)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99;")
            .unwrap();
        assert!(Memory::open(&newer).status().unwrap_err().contains("newer"));
        // A fresh file works and survives reopening.
        let good = dir.join("sub/memory.sqlite3");
        Memory::open(&good)
            .store(&turn(7, 1, 0, 100, "trp_npc1", "one"))
            .unwrap();
        let reopened = Memory::open(&good);
        assert_eq!(reopened.status(), Ok(()));
        assert_eq!(texts(&reopened.history(7, 1, "trp_npc1").unwrap()), ["one"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn history(items: &[(u32, &str)]) -> History {
        History {
            head_found: true,
            chain_len: items.len() as u32,
            turns: items
                .iter()
                .enumerate()
                .map(|(i, (conv, text))| Turn {
                    job: i as u32 + 1,
                    conversation: *conv,
                    day: i as u32,
                    player_name: "P".into(),
                    player_text: text.to_string(),
                    npc_text: "Aye.".into(),
                    action_kind: 0,
                    action_amount: 0,
                    outcome: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn recall_is_bounded_and_deterministic() {
        let mut items: Vec<(u32, String)> = (0..20)
            .map(|i| (1 + i / 5, format!("filler message {i}")))
            .collect();
        items[2].1 = "My sister Ylva lives in Sargoth.".into();
        items[6].1 = "Sargoth is cold this winter.".into();
        items.extend((0..10).map(|i| (9, format!("current {i}"))));
        let refs: Vec<(u32, &str)> = items.iter().map(|(c, t)| (*c, t.as_str())).collect();
        let r = recall(history(&refs), 9, "Do you remember my sister in Sargoth?");
        let t = |v: &Vec<Turn>| v.iter().map(|t| t.player_text.clone()).collect::<Vec<_>>();
        assert_eq!(
            (r.total_turns, r.conversations, r.first_day),
            (30, 5, Some(0))
        );
        assert_eq!(
            t(&r.current),
            (2..10).map(|i| format!("current {i}")).collect::<Vec<_>>()
        );
        assert_eq!(
            t(&r.recent),
            (16..20)
                .map(|i| format!("filler message {i}"))
                .collect::<Vec<_>>()
        );
        // "sister" + "sargoth" beats "sargoth" alone; both are older than the recent ones.
        assert_eq!(
            t(&r.relevant),
            [
                "My sister Ylva lives in Sargoth.",
                "Sargoth is cold this winter."
            ]
        );
        assert_eq!(
            r,
            recall(history(&refs), 9, "Do you remember my sister in Sargoth?")
        );
        // No shared keywords: nothing relevant. Common words do not count.
        assert!(
            recall(history(&refs), 9, "Tell me what you think about that")
                .relevant
                .is_empty()
        );
        assert_eq!(recall(History::default(), 1, "hi"), Recall::default());
    }
}
