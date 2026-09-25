//! What the server does for one piece of work: talks and planning tasks (run by the worker),
//! and world nodes (run in the handler; no model involved).
//!
//! A v2 talk, in order:
//! 1. If (campaign, job) is already stored (a retried talk, even across a server restart),
//!    answer the stored reply without generating again; a different talk under the same
//!    ids fails with `conflict`.
//! 2. Mark the game's memory head delivered, and recall this character's history on the
//!    head's chain; with a world head, also the world events it knows of, its aims and its
//!    recent deeds (Milestones 4 and 6).
//! 3. Build the prompt and generate. Split off and validate a proposed action (Milestone 5).
//! 4. Store the turn (with its action), then return the text and the action. The job
//!    becomes READY only after the turn is stored, so the game never shows a reply that
//!    memory does not have. A failed or empty generation stores nothing. A job that left
//!    PENDING meanwhile (canceled, superseded, timed out) stores nothing either.

use crate::actions;
use crate::characters::Registry;
use crate::ids;
use crate::jobs::{Answer, Reply, Store, Task, WorkItem};
use crate::log;
use crate::memory::{self, Memory, MemoryFailure, NewTurn};
use crate::planner::{self, PlanTask};
use crate::prompt::{self, Chat, Extras};
use crate::protocol::{
    Payload, Talk, TalkV2, WorldNode, CODE_BAD_REQUEST, CODE_READY, REASON_CANCELED,
    REASON_CONFLICT, REASON_MEMORY_ERROR, REASON_MEMORY_UNAVAILABLE, TEXT_STORED,
};
use crate::realms::Realms;
use crate::sanitize::sanitize_text;
use crate::upstream::{Backend, Failure};
use crate::world::{self, LogTemplates, Namer};
use std::net::TcpStream;
use std::sync::Arc;

pub struct Runner {
    pub backend: Backend,
    pub characters: Registry,
    pub realms: Realms,
    pub templates: LogTemplates,
    pub memory: Arc<Memory>,
    /// Log every prompt in full (`--log-prompts`).
    pub log_prompts: bool,
    /// Characters may propose actions (Milestone 5; `--no-actions` turns it off).
    pub actions: bool,
    /// Characters plan and act on their own (Milestone 6; `--no-autonomy` turns it off).
    pub autonomy: bool,
}

fn memory_failure(e: MemoryFailure) -> Failure {
    match e {
        MemoryFailure::Unavailable(d) => Failure::new(REASON_MEMORY_UNAVAILABLE, d),
        MemoryFailure::Error(d) => Failure::new(REASON_MEMORY_ERROR, d),
    }
}

/// `sanitize_text` repeated until it no longer changes the text, so that the stored text is
/// exactly what `frame` will send (one pass can, rarely, join a new `<think>`).
fn sanitized(raw: &str) -> Result<String, &'static str> {
    let mut text = sanitize_text(raw)?;
    loop {
        let again = sanitize_text(&text)?;
        if again == text {
            return Ok(text);
        }
        text = again;
    }
}

fn troop_index(id: &str) -> i32 {
    ids::ids().troops.index(id).map_or(0, |i| i as i32)
}

impl Runner {
    /// Runs one work item: the reply of a talk (raw for v1, final for v2), or a planning
    /// task (whose reply is empty).
    pub fn run(
        &self,
        item: &WorkItem,
        attach: &mut dyn FnMut(&TcpStream) -> bool,
        still_pending: &dyn Fn() -> bool,
    ) -> Result<Reply, Failure> {
        match &item.task {
            Task::Talk(Talk::V1(t)) => {
                let chat = prompt::v1_chat(t.npc, &t.pname, t.day, &t.msg);
                self.log_chat(item.id, &chat);
                self.backend
                    .generate(&chat, item.id, item.deadline, attach)
                    .map(Reply::text)
            }
            Task::Talk(Talk::V2(t)) => self.run_v2(item, t, attach, still_pending),
            Task::Plan(task) => {
                self.plan(task, item, attach, still_pending)?;
                Ok(Reply::text(String::new()))
            }
        }
    }

    fn run_v2(
        &self,
        item: &WorkItem,
        t: &TalkV2,
        attach: &mut dyn FnMut(&TcpStream) -> bool,
        still_pending: &dyn Fn() -> bool,
    ) -> Result<Reply, Failure> {
        let id = item.id;
        let troop = t.troop as i32;
        let fingerprint = t.fingerprint();
        let stored = |existing: memory::Existing| {
            if existing.fingerprint == fingerprint {
                log(format!(
                    "job {id} already stored: answering the stored reply"
                ));
                Ok(Reply {
                    text: existing.npc_text,
                    extra: match existing.action_kind {
                        0 => [0; 4],
                        k => [k as i32, existing.action_amount, troop, 0],
                    },
                })
            } else {
                Err(Failure::new(
                    REASON_CONFLICT,
                    format!(
                        "campaign {} job {id} is stored with other parameters",
                        t.campaign
                    ),
                ))
            }
        };
        if let Some(existing) = self.memory.find(t.campaign, id).map_err(memory_failure)? {
            return stored(existing);
        }
        let head_found = self
            .memory
            .deliver(t.campaign, t.head)
            .map_err(memory_failure)?;
        if !head_found {
            log(format!(
                "job {id} WARNING: memory head {} of campaign {} is not stored (another or a \
                 deleted database?); talking without memories",
                t.head, t.campaign
            ));
        }
        let mut history = self
            .memory
            .history(t.campaign, t.head, &t.character)
            .map_err(memory_failure)?;
        // The head turn's outcome is reported by this talk.
        if let Some(last) = history.turns.last_mut().filter(|l| l.job == t.head) {
            last.outcome = t.head_outcome;
        }
        let turns = history.turns.clone();
        let recall = memory::recall(history, t.conversation, &t.msg);
        let extras = self.extras(t).map_err(memory_failure)?;
        let profile = self.characters.get(&t.character);
        if profile.is_none() {
            log(format!(
                "job {id} no profile for {}: using a generic one",
                t.character
            ));
        }
        let chat = prompt::v2_chat(t, profile, &self.realms, &recall, &extras);
        log(format!(
            "job {id} memory: chain {} turns, {} with {} in {} conversations; using {} of this \
             talk, {} recent, {} relevant; {} world events; aims {}; prompt {} characters",
            recall.chain_len,
            recall.total_turns,
            t.character,
            recall.conversations,
            recall.current.len(),
            recall.recent.len(),
            recall.relevant.len(),
            extras.world.len(),
            if extras.plan.is_some() { "yes" } else { "none" },
            chat.len()
        ));
        self.log_chat(id, &chat);
        let raw = self.backend.generate(&chat, id, item.deadline, attach)?;
        let (spoken, proposal) = actions::split(&raw);
        let text = sanitized(&spoken).map_err(|reason| Failure::new(reason, "nothing left"))?;
        let action = match (&proposal, extras.actions) {
            (Some(p), true) => match actions::validate(p, t, &turns) {
                Ok(a) => {
                    log(format!("job {id} action {} {}", a.kind, a.amount));
                    Some(a)
                }
                Err(why) => {
                    log(format!("job {id} action {p:?} refused: {why}"));
                    None
                }
            },
            (Some(p), false) => {
                log(format!("job {id} action {p:?} ignored: actions are off"));
                None
            }
            (None, _) => None,
        };
        if !still_pending() {
            return Err(Failure::new(
                REASON_CANCELED,
                "job left PENDING; reply not stored",
            ));
        }
        let context = serde_json::to_string(&t.context).expect("serializable");
        let turn = NewTurn {
            campaign: t.campaign,
            job: id,
            parent: t.head,
            conversation: t.conversation,
            character: &t.character,
            day: t.context.day,
            player_name: &t.context.player_name,
            player_text: &t.msg,
            npc_text: &text,
            context: &context,
            fingerprint: &fingerprint,
            action_kind: action.map_or(0, |a| a.kind),
            action_amount: action.map_or(0, |a| a.amount),
            parent_outcome: t.head_outcome,
        };
        match self.memory.store(&turn).map_err(memory_failure)? {
            None => Ok(Reply {
                text,
                extra: match action {
                    Some(a) => [a.kind as i32, a.amount, troop, 0],
                    None => [0; 4],
                },
            }),
            Some(existing) => stored(existing),
        }
    }

    /// World events, aims and deeds for a talk, from its world head.
    fn extras(&self, t: &TalkV2) -> Result<Extras, MemoryFailure> {
        let mut extras = Extras {
            actions: self.actions && t.frame == crate::protocol::FRAME_V2,
            ..Extras::default()
        };
        let Some(whead) = t.world_head else {
            return Ok(extras);
        };
        let faction = ids::ids().factions.name(t.context.faction).unwrap_or("");
        let (found, events) = self
            .memory
            .world_events(t.campaign, whead, &t.character, faction)?;
        if !found {
            log(format!(
                "WARNING: world head {whead} of campaign {} is not stored; no world events",
                t.campaign
            ));
        }
        extras.world = world::recall(&events, &t.character, faction, t.context.day);
        if self.autonomy {
            extras.plan = self.memory.latest_plan(t.campaign, whead, &t.character)?;
            let mut deeds = self.memory.delivered_initiatives(t.campaign, whead)?;
            deeds.retain(|d| d.character == t.character);
            extras.deeds = deeds.split_off(deeds.len().saturating_sub(3));
        }
        Ok(extras)
    }

    /// `/v2/event`, `/v2/world` and `/v2/tick`: stores the node (idempotently) with what it
    /// tells, and answers READY. A tick may carry an initiative (frame K..X) and queues
    /// planning. Runs in the handler: no model is called.
    pub fn world(&self, n: &WorldNode, store: &Store) -> Answer {
        match self.world_inner(n, store) {
            Ok(answer) => answer,
            Err(e) => {
                log(format!(
                    "world node {} of campaign {}: {e}",
                    n.node, n.campaign
                ));
                let reason = match e {
                    MemoryFailure::Unavailable(_) => REASON_MEMORY_UNAVAILABLE,
                    MemoryFailure::Error(_) => REASON_MEMORY_ERROR,
                };
                let mut answer = Answer::token(crate::protocol::CODE_FAILED, reason);
                answer.extra = Some([0; 4]);
                answer
            }
        }
    }

    fn world_inner(&self, n: &WorldNode, store: &Store) -> Result<Answer, MemoryFailure> {
        let ready = |text: String, extra: [i32; 4]| Answer {
            code: CODE_READY,
            text,
            extra: Some(extra),
        };
        let conflict = || {
            let mut a = Answer::token(CODE_BAD_REQUEST, REASON_CONFLICT);
            a.extra = Some([0; 4]);
            a
        };
        let answer_for = |initiative: Option<i64>| -> Result<Answer, MemoryFailure> {
            let Some(id) = initiative else {
                return Ok(ready(TEXT_STORED.into(), [0; 4]));
            };
            let open = self.memory.open_initiatives(n.campaign, n.parent, 0)?;
            Ok(match open.into_iter().find(|i| i.id == id) {
                Some(i) => ready(
                    i.text.clone(),
                    [
                        i.kind as i32,
                        i.amount,
                        troop_index(&i.character),
                        if i.target.is_empty() {
                            0
                        } else {
                            troop_index(&i.target)
                        },
                    ],
                ),
                None => ready(TEXT_STORED.into(), [0; 4]),
            })
        };
        if let Some(existing) = self.memory.find_node(n.campaign, n.node)? {
            return if existing.fingerprint == n.fingerprint() {
                answer_for(existing.initiative)
            } else {
                log(format!(
                    "world node {} conflict: same id, other content",
                    n.node
                ));
                Ok(conflict())
            };
        }
        let namer = Namer {
            player: &n.player_name,
            player_realm: &n.player_faction_name,
            realms: &self.realms,
        };
        let mut initiative = None;
        let events = match &n.payload {
            Payload::Log(e) => world::render_log(e, &self.templates, &namer)
                .into_iter()
                .collect(),
            Payload::Snapshot(s) => {
                let prev = self.memory.last_snapshot(n.campaign, n.parent)?;
                world::diff(prev.as_ref(), s, n.day, &namer)
            }
            Payload::Tick { head } => {
                if self.autonomy {
                    let since = n.day.saturating_sub(planner::INITIATIVE_LIFETIME_DAYS);
                    let open = self.memory.open_initiatives(n.campaign, n.parent, since)?;
                    let delivered = self.memory.delivered_initiatives(n.campaign, n.parent)?;
                    initiative = planner::deliverable(&open, &delivered, n.day).map(|i| i.id);
                    let task = PlanTask {
                        campaign: n.campaign,
                        base: n.node,
                        head: *head,
                        day: n.day,
                        player_name: n.player_name.clone(),
                    };
                    if store.plan(task) {
                        log(format!(
                            "tick {} of campaign {}: planning queued",
                            n.node, n.campaign
                        ));
                    }
                }
                Vec::new()
            }
        };
        for e in &events {
            log(format!(
                "world campaign {} day {}: {}",
                n.campaign, e.day, e.text
            ));
        }
        match self.memory.store_node(n, &events, initiative)? {
            None => answer_for(initiative),
            Some(existing) if existing.fingerprint == n.fingerprint() => {
                answer_for(existing.initiative)
            }
            Some(_) => Ok(conflict()),
        }
    }

    /// A planning task: picks a character, asks the model for its aims, stores them.
    fn plan(
        &self,
        task: &PlanTask,
        item: &WorkItem,
        attach: &mut dyn FnMut(&TcpStream) -> bool,
        still_pending: &dyn Fn() -> bool,
    ) -> Result<(), Failure> {
        let m = &self.memory;
        let spoken = m
            .spoken_with(task.campaign, task.head)
            .map_err(memory_failure)?;
        let candidates = planner::important(&self.characters, &spoken);
        let days = m
            .plan_days(task.campaign, task.base)
            .map_err(memory_failure)?;
        let Some(character) = planner::pick(&candidates, &days, task.day) else {
            return Ok(());
        };
        let profile = self
            .characters
            .get(&character)
            .expect("candidates have profiles");
        // The realm: a ruler's or claimant's own, or the one last seen in a talk.
        let last = m
            .last_context(task.campaign, task.head, &character)
            .map_err(memory_failure)?;
        let faction = match last
            .as_deref()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        {
            Some(v) => v["faction"]
                .as_u64()
                .and_then(|f| ids::ids().factions.name(f as u32))
                .unwrap_or("")
                .to_string(),
            None => character
                .strip_prefix("trp_kingdom_")
                .and_then(|r| r.split('_').next())
                .map(|n| format!("fac_kingdom_{n}"))
                .unwrap_or_default(),
        };
        let (_, events) = m
            .world_events(task.campaign, task.base, &character, &faction)
            .map_err(memory_failure)?;
        let world = world::recall(&events, &character, &faction, task.day);
        let history = m
            .history(task.campaign, task.head, &character)
            .map_err(memory_failure)?;
        let previous = m
            .latest_plan(task.campaign, task.base, &character)
            .map_err(memory_failure)?;
        let chat = prompt::plan_chat(
            &character,
            profile,
            self.realms.get(&faction),
            &world,
            &history.turns,
            previous.as_ref(),
            &task.player_name,
            task.day,
        );
        log(format!(
            "planning for {character} on day {} (prompt {} characters)",
            task.day,
            chat.len()
        ));
        self.log_chat(0, &chat);
        let raw = self.backend.generate(&chat, 0, item.deadline, attach)?;
        if !still_pending() {
            return Err(Failure::new(REASON_CANCELED, "planning preempted"));
        }
        match planner::parse(&raw, &character, task.day) {
            Ok((plan, act)) => {
                log(format!(
                    "plan for {character}: goal {:?}; act {}",
                    plan.goal,
                    act.as_ref()
                        .map_or("none".to_string(), |a| format!("{} {:?}", a.kind, a.text))
                ));
                m.store_plan(task.campaign, &character, task.base, &plan, act.as_ref())
                    .map_err(memory_failure)
            }
            Err(why) => {
                log(format!("plan for {character} rejected: {why}"));
                Ok(())
            }
        }
    }

    fn log_chat(&self, id: u32, chat: &Chat) {
        if self.log_prompts {
            for (role, content) in &chat.messages {
                log(format!("job {id} prompt [{role}]\n{content}"));
            }
        }
    }
}
