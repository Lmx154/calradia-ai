//! What the worker does for one job: build the prompt, call the backend and, for v2 talks,
//! read and write memory.
//!
//! A v2 job, in order:
//! 1. If (campaign, job) is already stored (a retried talk, even across a server restart),
//!    answer the stored reply without generating again; a different talk under the same
//!    ids fails with `conflict`.
//! 2. Mark the game's memory head delivered, and recall this character's history on the
//!    head's chain.
//! 3. Build the prompt and generate.
//! 4. Store the turn, then return the text. The job becomes READY only after the turn is
//!    stored, so the game never shows a reply that memory does not have. A failed or empty
//!    generation stores nothing. A job that left PENDING meanwhile (canceled, superseded,
//!    timed out) stores nothing either.

use crate::characters::Registry;
use crate::jobs::WorkItem;
use crate::log;
use crate::memory::{self, Memory, MemoryFailure, NewTurn};
use crate::prompt::{self, Chat};
use crate::protocol::{
    Talk, TalkV2, REASON_CANCELED, REASON_CONFLICT, REASON_MEMORY_ERROR, REASON_MEMORY_UNAVAILABLE,
};
use crate::sanitize::sanitize_text;
use crate::upstream::{Backend, Failure};
use std::net::TcpStream;
use std::sync::Arc;

pub struct Runner {
    pub backend: Backend,
    pub characters: Registry,
    pub memory: Arc<Memory>,
    /// Log every prompt in full (`--log-prompts`).
    pub log_prompts: bool,
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

impl Runner {
    /// The raw reply for `item` (v1) or the exact text to send (v2).
    pub fn run(
        &self,
        item: &WorkItem,
        attach: &mut dyn FnMut(&TcpStream) -> bool,
        still_pending: &dyn Fn() -> bool,
    ) -> Result<String, Failure> {
        match &item.talk {
            Talk::V1(t) => {
                let chat = prompt::v1_chat(t.npc, &t.pname, t.day, &t.msg);
                self.log_chat(item.id, &chat);
                self.backend.generate(&chat, item.id, item.deadline, attach)
            }
            Talk::V2(t) => self.run_v2(item, t, attach, still_pending),
        }
    }

    fn run_v2(
        &self,
        item: &WorkItem,
        t: &TalkV2,
        attach: &mut dyn FnMut(&TcpStream) -> bool,
        still_pending: &dyn Fn() -> bool,
    ) -> Result<String, Failure> {
        let id = item.id;
        let fingerprint = t.fingerprint();
        let stored = |existing: memory::Existing| {
            if existing.fingerprint == fingerprint {
                log(format!(
                    "job {id} already stored: answering the stored reply"
                ));
                Ok(existing.npc_text)
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
        let history = self
            .memory
            .history(t.campaign, t.head, &t.character)
            .map_err(memory_failure)?;
        let recall = memory::recall(history, t.conversation, &t.msg);
        let profile = self.characters.get(&t.character);
        if profile.is_none() {
            log(format!(
                "job {id} no profile for {}: using a generic one",
                t.character
            ));
        }
        let chat = prompt::v2_chat(t, profile, &recall);
        log(format!(
            "job {id} memory: chain {} turns, {} with {} in {} conversations; using {} of this \
             talk, {} recent, {} relevant; prompt {} characters",
            recall.chain_len,
            recall.total_turns,
            t.character,
            recall.conversations,
            recall.current.len(),
            recall.recent.len(),
            recall.relevant.len(),
            chat.len()
        ));
        self.log_chat(id, &chat);
        let raw = self.backend.generate(&chat, id, item.deadline, attach)?;
        let text = sanitized(&raw).map_err(|reason| Failure::new(reason, "nothing left"))?;
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
        };
        match self.memory.store(&turn).map_err(memory_failure)? {
            None => Ok(text),
            Some(existing) => stored(existing),
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
