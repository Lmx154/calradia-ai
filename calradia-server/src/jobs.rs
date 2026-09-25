//! The job store and its single worker (protocol-v1.md, "Jobs (server)").
//!
//! All state lives in one `Mutex<Inner>`, and every transition out of PENDING happens
//! under it, so a job leaves PENDING exactly once and terminal states never change.
//! Handlers take the lock only briefly and never wait on the worker. Nobody holds the lock
//! during I/O: an upstream socket that must be aborted is taken out of the state under the
//! lock and shut down after the guard is dropped.

use crate::npc::Npc;
use crate::protocol::{
    TalkParams, CODE_BAD_REQUEST, CODE_BUSY, CODE_CANCELED, CODE_FAILED, CODE_PENDING, CODE_READY,
    CODE_UNKNOWN_JOB, JOB_DEADLINE_SECS, JOB_TTL_SECS, MAX_JOBS, QUEUE_LEN, REASON_BUSY,
    REASON_CANCELED, REASON_CONFLICT, REASON_PENDING, REASON_SUPERSEDED, REASON_TIMEOUT,
    REASON_UNKNOWN_JOB,
};
use crate::sanitize::sanitize_text;
use crate::upstream::{Backend, Failure};
use crate::{log, preview};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{Shutdown, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub max_jobs: usize,
    /// Jobs waiting for the worker, not counting the one it is running.
    pub queue_len: usize,
    /// How long a terminal job is kept.
    pub ttl: Duration,
    /// How long a job may stay PENDING, from creation.
    pub deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_jobs: MAX_JOBS,
            queue_len: QUEUE_LEN,
            ttl: Duration::from_secs(JOB_TTL_SECS),
            deadline: Duration::from_secs(JOB_DEADLINE_SECS),
        }
    }
}

#[derive(Debug)]
enum State {
    Pending,
    /// The model's raw reply. `frame` sanitizes it on every answer; the worker only lets a
    /// job become READY if that yields a text with a letter.
    Ready(String),
    Failed(&'static str),
    Canceled(&'static str),
}

struct Job {
    npc: &'static Npc,
    day: u32,
    pname: String,
    msg: String,
    prompt: Option<String>,
    scope: String,
    state: State,
    created: Instant,
    deadline: Instant,
    /// When the job left PENDING; the TTL counts from here.
    finished: Option<Instant>,
}

impl Job {
    fn is_pending(&self) -> bool {
        matches!(self.state, State::Pending)
    }

    fn same_talk(&self, t: &TalkParams) -> bool {
        self.npc.id == t.npc.id && self.day == t.day && self.pname == t.pname && self.msg == t.msg
    }

    fn answer(&self) -> Answer {
        match &self.state {
            State::Pending => Answer::token(CODE_PENDING, REASON_PENDING),
            State::Ready(text) => Answer {
                code: CODE_READY,
                text: text.clone(),
            },
            State::Failed(reason) => Answer::token(CODE_FAILED, reason),
            State::Canceled(reason) => Answer::token(CODE_CANCELED, reason),
        }
    }
}

/// A handler's answer: the frame's C and T (a token, or the raw reply for READY).
#[derive(Debug)]
pub struct Answer {
    pub code: u8,
    pub text: String,
}

impl Answer {
    fn token(code: u8, token: &str) -> Self {
        Answer {
            code,
            text: token.to_string(),
        }
    }
}

/// Everything the worker needs to run one job, copied out of the store.
pub struct WorkItem {
    pub id: u32,
    pub npc: &'static Npc,
    pub day: u32,
    pub pname: String,
    pub msg: String,
    pub deadline: Instant,
    pub prompt: Option<String>,
}

struct Running {
    id: u32,
    /// A clone of the upstream socket, for cancel-by-shutdown. None until connected.
    conn: Option<TcpStream>,
}

struct Inner {
    jobs: HashMap<u32, Job>,
    queue: VecDeque<u32>,
    running: Option<Running>,
}

pub struct Store {
    inner: Mutex<Inner>,
    work: Condvar,
    limits: Limits,
}

impl Store {
    pub fn new(limits: Limits) -> Self {
        Store {
            inner: Mutex::new(Inner {
                jobs: HashMap::new(),
                queue: VecDeque::new(),
                running: None,
            }),
            work: Condvar::new(),
            limits,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock cannot leave a half-made transition (each is a
        // single assignment), so a poisoned lock is safe to keep using.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `/v1/talk`: returns the state of an existing job with the same parameters, rejects
    /// the same id with other parameters, or queues a new job.
    pub fn talk(&self, id: u32, talk: TalkParams) -> Answer {
        self.talk_with_prompt(id, talk, None, String::new())
    }

    pub fn talk_with_prompt(
        &self,
        id: u32,
        talk: TalkParams,
        prompt: Option<String>,
        scope: String,
    ) -> Answer {
        let now = Instant::now();
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut kill = self.sweep(inner, now);
        let answer = match inner.jobs.get(&id) {
            Some(job) if job.same_talk(&talk) => job.answer(),
            Some(_) => {
                log(format!("job {id} conflict: same id, different parameters"));
                Answer::token(CODE_BAD_REQUEST, REASON_CONFLICT)
            }
            None => self.admit(inner, id, talk, (prompt, scope), now, &mut kill),
        };
        drop(guard);
        shut_down(kill);
        answer
    }

    fn admit(
        &self,
        inner: &mut Inner,
        id: u32,
        talk: TalkParams,
        context: (Option<String>, String),
        now: Instant,
        kill: &mut Vec<TcpStream>,
    ) -> Answer {
        // The new job supersedes the PENDING jobs of its NPC (by construction at most
        // one). Capacity is checked as if they were already canceled, and before anything
        // changes, so a busy answer has no side effects.
        let victims: Vec<u32> = inner
            .jobs
            .iter()
            .filter(|(_, j)| j.npc.id == talk.npc.id && j.scope == context.1 && j.is_pending())
            .map(|(v, _)| *v)
            .collect();
        let waiting = inner.queue.iter().filter(|q| !victims.contains(q)).count();
        let evictable = !victims.is_empty() || inner.jobs.values().any(|j| !j.is_pending());
        if waiting >= self.limits.queue_len
            || (inner.jobs.len() >= self.limits.max_jobs && !evictable)
        {
            log(format!(
                "job {id} rejected busy (queue {waiting}/{}, store {}/{})",
                self.limits.queue_len,
                inner.jobs.len(),
                self.limits.max_jobs
            ));
            return Answer::token(CODE_BUSY, REASON_BUSY);
        }
        for victim in victims {
            log(format!("job {victim} superseded by job {id}"));
            kill.extend(settle(
                inner,
                victim,
                State::Canceled(REASON_SUPERSEDED),
                now,
            ));
        }
        // Expired jobs were purged by the sweep; if still full, evict the terminal job
        // that finished first, which is the one closest to expiry.
        while inner.jobs.len() >= self.limits.max_jobs {
            let oldest = inner
                .jobs
                .iter()
                .filter_map(|(j, job)| job.finished.map(|f| (f, *j)))
                .min();
            let Some((_, old)) = oldest else { break };
            inner.jobs.remove(&old);
            log(format!("job {old} evicted: store full"));
        }
        log(format!(
            "job {id} queued npc={} day={} msg={:?} (queue {}, store {})",
            talk.npc.id,
            talk.day,
            preview(&talk.msg),
            inner.queue.len() + 1,
            inner.jobs.len() + 1
        ));
        inner.jobs.insert(
            id,
            Job {
                npc: talk.npc,
                day: talk.day,
                pname: talk.pname,
                msg: talk.msg,
                prompt: context.0,
                scope: context.1,
                state: State::Pending,
                created: now,
                deadline: now + self.limits.deadline,
                finished: None,
            },
        );
        inner.queue.push_back(id);
        self.work.notify_one();
        Answer::token(CODE_PENDING, REASON_PENDING)
    }

    /// `/v1/result`.
    pub fn result(&self, id: u32) -> Answer {
        let now = Instant::now();
        let mut guard = self.lock();
        let kill = self.sweep(&mut guard, now);
        let answer = guard.jobs.get(&id).map_or_else(unknown, Job::answer);
        drop(guard);
        shut_down(kill);
        answer
    }

    /// `/v1/cancel`: a PENDING job becomes CANCELED and its upstream socket is shut down;
    /// a terminal job is left unchanged and answers with its own code.
    pub fn cancel(&self, id: u32) -> Answer {
        let now = Instant::now();
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut kill = self.sweep(inner, now);
        let answer = match inner.jobs.get(&id).map(Job::is_pending) {
            None => unknown(),
            Some(true) => {
                log(format!("job {id} cancel requested"));
                kill.extend(settle(inner, id, State::Canceled(REASON_CANCELED), now));
                Answer::token(CODE_CANCELED, REASON_CANCELED)
            }
            Some(false) => inner.jobs[&id].answer(),
        };
        drop(guard);
        shut_down(kill);
        answer
    }

    /// Lazy enforcement, run on every access: PENDING jobs past their deadline fail with
    /// `timeout`, and terminal jobs past their TTL are removed. Returns the upstream
    /// socket to shut down if the running job timed out.
    fn sweep(&self, inner: &mut Inner, now: Instant) -> Vec<TcpStream> {
        let overdue: Vec<u32> = inner
            .jobs
            .iter()
            .filter(|(_, j)| j.is_pending() && now >= j.deadline)
            .map(|(id, _)| *id)
            .collect();
        let mut kill = Vec::new();
        for id in overdue {
            kill.extend(settle(inner, id, State::Failed(REASON_TIMEOUT), now));
        }
        let ttl = self.limits.ttl;
        inner.jobs.retain(|id, job| match job.finished {
            Some(f) if now.duration_since(f) >= ttl => {
                log(format!("job {id} expired"));
                false
            }
            _ => true,
        });
        kill
    }

    /// Worker: blocks until a job is queued, marks it running and returns a copy of it.
    fn next_job(&self) -> WorkItem {
        let mut guard = self.lock();
        loop {
            let now = Instant::now();
            while let Some(id) = guard.queue.pop_front() {
                let inner = &mut *guard;
                let Some(job) = inner.jobs.get(&id).filter(|j| j.is_pending()) else {
                    continue;
                };
                if now >= job.deadline {
                    settle(inner, id, State::Failed(REASON_TIMEOUT), now);
                    continue;
                }
                log(format!(
                    "job {id} started after {} ms in queue",
                    now.duration_since(job.created).as_millis()
                ));
                let item = WorkItem {
                    id,
                    npc: job.npc,
                    day: job.day,
                    pname: job.pname.clone(),
                    msg: job.msg.clone(),
                    deadline: job.deadline,
                    prompt: job.prompt.clone(),
                };
                inner.running = Some(Running { id, conn: None });
                return item;
            }
            guard = self
                .work
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Worker: registers the connected upstream socket of the running job, so a cancel
    /// can shut it down. Returns false if the job already left PENDING.
    fn attach(&self, id: u32, conn: &TcpStream) -> bool {
        // dup(2) the socket before taking the lock.
        let clone = conn.try_clone();
        let mut guard = self.lock();
        let inner = &mut *guard;
        let pending = inner.jobs.get(&id).is_some_and(Job::is_pending);
        match &mut inner.running {
            Some(running) if running.id == id && pending => {
                match clone {
                    Ok(c) => running.conn = Some(c),
                    Err(e) => log(format!("job {id} cannot clone upstream socket: {e}")),
                }
                true
            }
            _ => false,
        }
    }

    /// Worker: records the outcome of the running job. A result for a job that has
    /// already left PENDING (canceled, superseded, timed out) is discarded.
    fn finish(&self, id: u32, outcome: Result<String, Failure>) {
        // Decide READY or FAILED before taking the lock; sanitizing is pure CPU.
        let state = match outcome {
            Ok(raw) => match sanitize_text(&raw) {
                Ok(_) => State::Ready(raw),
                Err(reason) => State::Failed(reason),
            },
            Err(failure) => {
                log(format!("job {id} upstream failure: {failure}"));
                State::Failed(failure.reason)
            }
        };
        let now = Instant::now();
        let mut guard = self.lock();
        let inner = &mut *guard;
        inner.running = None;
        // The deadline wins over a result that arrives after it.
        let kill = self.sweep(inner, now);
        match inner.jobs.get(&id) {
            Some(job) if job.is_pending() => {
                settle(inner, id, state, now);
            }
            Some(job) => log(format!(
                "job {id} late result discarded: job is already {}",
                state_name(&job.state)
            )),
            None => log(format!("job {id} late result discarded: job is gone")),
        }
        drop(guard);
        shut_down(kill);
    }
}

/// Moves a PENDING job to a terminal state, takes it off the queue and, if it is running,
/// returns its upstream socket for the caller to shut down after unlocking.
fn settle(inner: &mut Inner, id: u32, state: State, now: Instant) -> Option<TcpStream> {
    let job = inner.jobs.get_mut(&id)?;
    if !job.is_pending() {
        return None;
    }
    let ms = now.duration_since(job.created).as_millis();
    match &state {
        State::Ready(raw) => {
            let shown = sanitize_text(raw).unwrap_or_default();
            log(format!(
                "job {id} READY after {ms} ms: {:?}",
                preview(&shown)
            ));
        }
        other => log(format!("job {id} {} after {ms} ms", state_name(other))),
    }
    job.state = state;
    job.finished = Some(now);
    inner.queue.retain(|q| *q != id);
    match &mut inner.running {
        Some(running) if running.id == id => running.conn.take(),
        _ => None,
    }
}

fn state_name(state: &State) -> String {
    match state {
        State::Pending => "PENDING".into(),
        State::Ready(_) => "READY".into(),
        State::Failed(reason) => format!("FAILED {reason}"),
        State::Canceled(reason) => format!("CANCELED {reason}"),
    }
}

fn unknown() -> Answer {
    Answer::token(CODE_UNKNOWN_JOB, REASON_UNKNOWN_JOB)
}

fn shut_down(conns: Vec<TcpStream>) {
    for conn in conns {
        // Wakes the worker's blocked read or write with EOF or an error.
        let _ = conn.shutdown(Shutdown::Both);
    }
}

/// Starts the single worker thread: it runs queued jobs one at a time.
pub fn spawn_worker(store: Arc<Store>, backend: Backend) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("worker".into())
        .spawn(move || loop {
            let item = store.next_job();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                backend.generate(&item, &mut |conn| store.attach(item.id, conn))
            }))
            .unwrap_or_else(|_| Err(Failure::error("worker panicked")));
            store.finish(item.id, outcome);
        })
}
