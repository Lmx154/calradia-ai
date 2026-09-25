//! The job store and its single worker (protocol-v1.md, "Jobs (server)").
//!
//! All state lives in one `Mutex<Inner>`, and every transition out of PENDING happens
//! under it, so a job leaves PENDING exactly once and terminal states never change.
//! Handlers take the lock only briefly and never wait on the worker. Nobody holds the lock
//! during I/O: an upstream socket that must be aborted is taken out of the state under the
//! lock and shut down after the guard is dropped.

use crate::planner::PlanTask;
use crate::protocol::{
    Talk, CODE_BAD_REQUEST, CODE_BUSY, CODE_CANCELED, CODE_FAILED, CODE_PENDING, CODE_READY,
    CODE_UNKNOWN_JOB, FRAME_V2, JOB_DEADLINE_SECS, JOB_TTL_SECS, MAX_JOBS, QUEUE_LEN, REASON_BUSY,
    REASON_CANCELED, REASON_CONFLICT, REASON_PENDING, REASON_SUPERSEDED, REASON_TIMEOUT,
    REASON_UNKNOWN_JOB,
};
use crate::runner::Runner;
use crate::sanitize::sanitize_text;
use crate::upstream::Failure;
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
    Ready(Reply),
    Failed(&'static str),
    Canceled(&'static str),
}

/// A finished talk: the reply, and for a v2 frame the four integers `K|N|W|X` (an action:
/// kind, amount, the NPC's troop, 0).
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub text: String,
    pub extra: [i32; 4],
}

impl Reply {
    pub fn text(text: String) -> Reply {
        Reply {
            text,
            extra: [0; 4],
        }
    }
}

struct Job {
    talk: Talk,
    /// The game reads v2 frames for this job (`/v2/talk` with `f=2`).
    frame2: bool,
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

    fn same_talk(&self, t: &Talk) -> bool {
        match (&self.talk, t) {
            (Talk::V1(a), Talk::V1(b)) => {
                a.npc.id == b.npc.id && a.day == b.day && a.pname == b.pname && a.msg == b.msg
            }
            (Talk::V2(a), Talk::V2(b)) => a == b,
            _ => false,
        }
    }

    fn answer(&self) -> Answer {
        let mut answer = match &self.state {
            State::Pending => Answer::token(CODE_PENDING, REASON_PENDING),
            State::Ready(reply) => Answer {
                code: CODE_READY,
                text: reply.text.clone(),
                extra: Some(reply.extra),
            },
            State::Failed(reason) => Answer::token(CODE_FAILED, reason),
            State::Canceled(reason) => Answer::token(CODE_CANCELED, reason),
        };
        answer.extra = match self.frame2 {
            true => Some(answer.extra.unwrap_or([0; 4])),
            false => None,
        };
        answer
    }
}

/// A handler's answer: the frame's C and T (a token, or the raw reply for READY).
#[derive(Debug)]
pub struct Answer {
    pub code: u8,
    pub text: String,
    /// `K|N|W|X` for a v2 frame; None for a v1 frame.
    pub extra: Option<[i32; 4]>,
}

impl Answer {
    pub fn token(code: u8, token: &str) -> Self {
        Answer {
            code,
            text: token.to_string(),
            extra: None,
        }
    }
}

/// How long a planning task may take.
pub const PLAN_DEADLINE: Duration = Duration::from_secs(120);

/// What the worker runs: a talk, or (in the background) a planning task.
#[derive(Clone, Debug)]
pub enum Task {
    Talk(Talk),
    Plan(PlanTask),
}

/// Everything the worker needs to run one job, copied out of the store. Background tasks
/// have id 0.
pub struct WorkItem {
    pub id: u32,
    pub task: Task,
    pub deadline: Instant,
}

/// Who a talk is addressed to: a new talk supersedes the PENDING jobs of the same speaker.
/// For v2 that is the character within the campaign.
fn speaker(talk: &Talk) -> (u32, u32, u32) {
    match talk {
        Talk::V1(t) => (1, t.npc.id, 0),
        Talk::V2(t) => (2, t.campaign, t.troop),
    }
}

/// A short description of a talk for the log.
fn describe(talk: &Talk) -> String {
    match talk {
        Talk::V1(t) => format!("npc={} day={} msg={:?}", t.npc.id, t.day, preview(&t.msg)),
        Talk::V2(t) => format!(
            "v2 camp={} conv={} head={} {} day={} msg={:?}",
            t.campaign,
            t.conversation,
            t.head,
            t.character,
            t.context.day,
            preview(&t.msg)
        ),
    }
}

struct Running {
    id: u32,
    /// A clone of the upstream socket, for cancel-by-shutdown. None until connected.
    conn: Option<TcpStream>,
    /// A background task, which yields to any talk.
    background: bool,
    /// The background task was stopped because a talk arrived.
    preempted: bool,
}

struct Inner {
    jobs: HashMap<u32, Job>,
    queue: VecDeque<u32>,
    /// Planning tasks: run only when no talk waits, at most one queued.
    background: VecDeque<PlanTask>,
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
                background: VecDeque::new(),
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
    pub fn talk(&self, id: u32, talk: Talk) -> Answer {
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
            None => self.admit(inner, id, talk, now, &mut kill),
        };
        drop(guard);
        shut_down(kill);
        answer
    }

    fn admit(
        &self,
        inner: &mut Inner,
        id: u32,
        talk: Talk,
        now: Instant,
        kill: &mut Vec<TcpStream>,
    ) -> Answer {
        // The new job supersedes the PENDING jobs of its NPC (by construction at most
        // one). Capacity is checked as if they were already canceled, and before anything
        // changes, so a busy answer has no side effects.
        let victims: Vec<u32> = inner
            .jobs
            .iter()
            .filter(|(_, j)| speaker(&j.talk) == speaker(&talk) && j.is_pending())
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
            "job {id} queued {} (queue {}, store {})",
            describe(&talk),
            inner.queue.len() + 1,
            inner.jobs.len() + 1
        ));
        let frame2 = matches!(&talk, Talk::V2(t) if t.frame == FRAME_V2);
        inner.jobs.insert(
            id,
            Job {
                talk,
                frame2,
                state: State::Pending,
                created: now,
                deadline: now + self.limits.deadline,
                finished: None,
            },
        );
        inner.queue.push_back(id);
        // A talk never waits for a plan: stop the running background task.
        if let Some(running) = inner.running.as_mut().filter(|r| r.background) {
            log("background task preempted by a talk");
            running.preempted = true;
            kill.extend(running.conn.take());
        }
        self.work.notify_one();
        let mut answer = Answer::token(CODE_PENDING, REASON_PENDING);
        if frame2 {
            answer.extra = Some([0; 4]);
        }
        answer
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
                    task: Task::Talk(job.talk.clone()),
                    deadline: job.deadline,
                };
                inner.running = Some(Running {
                    id,
                    conn: None,
                    background: false,
                    preempted: false,
                });
                return item;
            }
            if let Some(task) = guard.background.pop_front() {
                guard.running = Some(Running {
                    id: 0,
                    conn: None,
                    background: true,
                    preempted: false,
                });
                return WorkItem {
                    id: 0,
                    task: Task::Plan(task),
                    deadline: now + PLAN_DEADLINE,
                };
            }
            guard = self
                .work
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Whether job `id` is still waiting for its outcome (id 0: the background task was
    /// not preempted).
    pub fn is_pending(&self, id: u32) -> bool {
        let inner = self.lock();
        if id == 0 {
            return inner
                .running
                .as_ref()
                .is_some_and(|r| r.background && !r.preempted);
        }
        inner.jobs.get(&id).is_some_and(Job::is_pending)
    }

    /// Queues a planning task, unless one is already queued or running. Returns whether it
    /// was queued.
    pub fn plan(&self, task: PlanTask) -> bool {
        let mut inner = self.lock();
        let busy =
            !inner.background.is_empty() || inner.running.as_ref().is_some_and(|r| r.background);
        if busy {
            return false;
        }
        inner.background.push_back(task);
        self.work.notify_one();
        true
    }

    /// Worker: registers the connected upstream socket of the running job, so a cancel
    /// can shut it down. Returns false if the job already left PENDING.
    fn attach(&self, id: u32, conn: &TcpStream) -> bool {
        // dup(2) the socket before taking the lock.
        let clone = conn.try_clone();
        let mut guard = self.lock();
        let inner = &mut *guard;
        let pending = id == 0 || inner.jobs.get(&id).is_some_and(Job::is_pending);
        match &mut inner.running {
            Some(running) if running.id == id && pending && !running.preempted => {
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
    fn finish(&self, id: u32, outcome: Result<Reply, Failure>) {
        if id == 0 {
            if let Err(failure) = &outcome {
                log(format!("background task failed: {failure}"));
            }
            self.lock().running = None;
            return;
        }
        // Decide READY or FAILED before taking the lock; sanitizing is pure CPU.
        let state = match outcome {
            Ok(reply) => match sanitize_text(&reply.text) {
                Ok(_) => State::Ready(reply),
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
        State::Ready(reply) => {
            let shown = sanitize_text(&reply.text).unwrap_or_default();
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
pub fn spawn_worker(store: Arc<Store>, runner: Arc<Runner>) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("worker".into())
        .spawn(move || loop {
            let item = store.next_job();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                runner.run(&item, &mut |conn| store.attach(item.id, conn), &|| {
                    store.is_pending(item.id)
                })
            }))
            .unwrap_or_else(|_| Err(Failure::error("worker panicked")));
            store.finish(item.id, outcome);
        })
}
