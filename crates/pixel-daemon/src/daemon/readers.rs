// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Read replicas that answer read-plane requests while the writer is busy
//! (#1150).
//!
//! The loop sends a [`crate::plane::Plane::Read`] request to a reader only
//! when the writer has a job running or queued and none of those jobs is a
//! watcher batch: a read never overtakes an `Apply`, so a request that
//! follows an edit still sees it. Overtaking any other job is safe, because
//! that job has not answered anyone yet: the read is served from the last
//! published generation, as if it had arrived just before. A replica that
//! cannot answer (no graph to read, the generation moved under it) gives the
//! request back and the writer serves it in order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;

use super::{Msg, panic_message};
use crate::api::{Request, Response, failure_response};

/// Readers a daemon runs beside its writer.
pub(super) const READERS: usize = 2;

/// A copy of the corpus that answers read-plane requests from what the
/// writer last published, on its own thread.
pub trait ReadReplica: Send {
    /// The answer, or `None` when only the writer can give it.
    fn read(&mut self, req: &Request) -> Option<Response>;
}

/// How much the writer holds: every job queued or running, and among them
/// the watcher batches.
#[derive(Debug, Default)]
pub(super) struct Busy {
    jobs: AtomicUsize,
    applies: AtomicUsize,
}

impl Busy {
    /// A job was queued; `apply` when it is a watcher batch.
    pub(super) fn queued(&self, apply: bool) {
        self.jobs.fetch_add(1, Ordering::AcqRel);
        if apply {
            self.applies.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The writer finished a job; `apply` when it was a watcher batch.
    pub(super) fn done(&self, apply: bool) {
        if apply {
            self.applies.fetch_sub(1, Ordering::AcqRel);
        }
        self.jobs.fetch_sub(1, Ordering::AcqRel);
    }

    /// A read may skip the writer's queue: the writer is busy, and nothing
    /// ahead of the read is a watcher batch it must see.
    pub(super) fn read_may_bypass(&self) -> bool {
        self.applies.load(Ordering::Acquire) == 0 && self.jobs.load(Ordering::Acquire) > 0
    }
}

/// A read no reader took, handed back for the writer.
pub(super) type Returned = (Request, mpsc::Sender<Response>);

/// One read handed to a reader, and where its answer goes.
struct ReadJob {
    req: Request,
    reply: mpsc::Sender<Response>,
}

/// The reader threads and the number of them waiting for work.
pub(super) struct Readers {
    jobs: Option<mpsc::Sender<ReadJob>>,
    idle: Arc<AtomicUsize>,
    threads: Vec<JoinHandle<()>>,
}

impl Readers {
    /// One thread per replica. A request a replica gives back goes to the
    /// loop as [`Msg::Requeue`], for the writer.
    pub(super) fn spawn(replicas: Vec<Box<dyn ReadReplica>>, requeue: &mpsc::Sender<Msg>) -> Self {
        let (jobs, queue) = mpsc::channel::<ReadJob>();
        let queue = Arc::new(Mutex::new(queue));
        let idle = Arc::new(AtomicUsize::new(replicas.len()));
        let threads = replicas
            .into_iter()
            .map(|mut replica| {
                let queue = Arc::clone(&queue);
                let idle = Arc::clone(&idle);
                let requeue = requeue.clone();
                std::thread::spawn(move || {
                    loop {
                        let next = queue.lock().unwrap_or_else(PoisonError::into_inner).recv();
                        let Ok(job) = next else {
                            return;
                        };
                        serve_read(replica.as_mut(), job, &requeue);
                        idle.fetch_add(1, Ordering::AcqRel);
                    }
                })
            })
            .collect();
        Readers {
            jobs: Some(jobs),
            idle,
            threads,
        }
    }

    /// No readers: every request goes to the writer.
    pub(super) fn none() -> Self {
        Readers {
            jobs: None,
            idle: Arc::new(AtomicUsize::new(0)),
            threads: Vec::new(),
        }
    }

    /// Hand `req` to an idle reader; `Some` gives it back when none is idle.
    pub(super) fn offer(&self, req: Request, reply: mpsc::Sender<Response>) -> Option<Returned> {
        let Some(jobs) = &self.jobs else {
            return Some((req, reply));
        };
        if !self.take_idle() {
            return Some((req, reply));
        }
        jobs.send(ReadJob { req, reply })
            .err()
            .map(|mpsc::SendError(job)| {
                self.idle.fetch_add(1, Ordering::AcqRel);
                (job.req, job.reply)
            })
    }

    /// Claim an idle reader, `false` when none is. Only the loop thread
    /// claims and readers only give back, so the count read here can only
    /// grow before the decrement.
    fn take_idle(&self) -> bool {
        if self.idle.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.idle.fetch_sub(1, Ordering::AcqRel);
        true
    }
}

impl Drop for Readers {
    /// Close the queue and wait for each reader to finish the read it holds.
    fn drop(&mut self) {
        self.jobs = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Answer one read. A panic fails that request alone; a replica that cannot
/// answer hands the request back to the loop.
fn serve_read(replica: &mut dyn ReadReplica, job: ReadJob, requeue: &mpsc::Sender<Msg>) {
    let op = job.req.op_name();
    let answered =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| replica.read(&job.req)));
    match answered {
        Ok(Some(response)) => {
            let _ = job.reply.send(response);
        }
        Ok(None) => {
            let _ = requeue.send(Msg::Requeue(job.req, job.reply));
        }
        Err(payload) => {
            let _ = job.reply.send(failure_response(
                op,
                format!("request panicked: {}", panic_message(payload.as_ref())),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Answers `search`, gives back `status`, panics on `resolve`, and holds
    /// `targets` until released.
    struct Scripted {
        release: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    impl ReadReplica for Scripted {
        fn read(&mut self, req: &Request) -> Option<Response> {
            match req {
                Request::Search { .. } => Some(Response::success(
                    "search",
                    serde_json::json!({"from": "replica"}),
                )),
                Request::Resolve { .. } => panic!("replica panicked"),
                Request::Targets { .. } => {
                    let _ = self
                        .release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                    Some(Response::success("targets", serde_json::json!({})))
                }
                _ => None,
            }
        }
    }

    fn search() -> Request {
        Request::Search {
            pattern: "x".into(),
            json: false,
            limit: None,
            offset: None,
            paths: None,
            scope: None,
            globs: Vec::new(),
            types: Vec::new(),
        }
    }

    fn targets() -> Request {
        Request::Targets {
            task: "t".into(),
            limit: None,
            max_tier: None,
            precision: false,
            regions: false,
        }
    }

    fn pool(release: &Arc<Mutex<mpsc::Receiver<()>>>) -> (Readers, mpsc::Receiver<Msg>) {
        let (loop_tx, loop_rx) = mpsc::channel();
        let replicas: Vec<Box<dyn ReadReplica>> = (0..READERS)
            .map(|_| {
                Box::new(Scripted {
                    release: Arc::clone(release),
                }) as Box<dyn ReadReplica>
            })
            .collect();
        (Readers::spawn(replicas, &loop_tx), loop_rx)
    }

    fn wait_idle(readers: &Readers, expected: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while readers.idle.load(Ordering::Acquire) != expected {
            assert!(std::time::Instant::now() < deadline, "readers never idled");
            std::thread::yield_now();
        }
    }

    /// A reader answers; a panic fails only its own request and the reader
    /// keeps serving; a request it cannot answer goes back to the loop.
    #[test]
    fn a_reader_should_answer_fail_alone_or_give_back() {
        let (_release_tx, release_rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        let (readers, loop_rx) = pool(&release);

        let (reply, answer) = mpsc::channel();
        assert!(
            readers
                .offer(
                    Request::Resolve {
                        phrase: "p".into(),
                        limit: None
                    },
                    reply
                )
                .is_none()
        );
        let failed = answer.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!failed.ok);
        assert_eq!(failed.op, "resolve");
        assert_eq!(
            failed.error.map(|e| e.message),
            Some("request panicked: replica panicked".to_string())
        );
        wait_idle(&readers, READERS);

        for _ in 0..READERS + 1 {
            let (reply, answer) = mpsc::channel();
            assert!(readers.offer(search(), reply).is_none());
            let answered = answer.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(answered.result.unwrap()["from"], "replica");
            wait_idle(&readers, READERS);
        }

        let (reply, _answer) = mpsc::channel();
        assert!(readers.offer(Request::Status {}, reply).is_none());
        let Ok(Msg::Requeue(req, _reply)) = loop_rx.recv_timeout(Duration::from_secs(5)) else {
            panic!("a request the replica cannot answer goes back to the loop");
        };
        assert_eq!(req, Request::Status {});
    }

    /// Past `READERS` reads in progress, `offer` gives the request back for
    /// the writer instead of queueing it behind a reader.
    #[test]
    fn offer_should_give_back_a_read_when_every_reader_is_busy() {
        let (release_tx, release_rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        let (readers, _loop_rx) = pool(&release);
        let mut held = Vec::new();
        for _ in 0..READERS {
            let (reply, answer) = mpsc::channel();
            assert!(readers.offer(targets(), reply).is_none());
            held.push(answer);
        }
        let (reply, _answer) = mpsc::channel();
        let Some((req, _reply)) = readers.offer(search(), reply) else {
            panic!("a third read reached a reader with both busy");
        };
        assert_eq!(req, search());
        for _ in 0..READERS {
            release_tx.send(()).unwrap();
        }
        for answer in held {
            assert!(answer.recv_timeout(Duration::from_secs(5)).unwrap().ok);
        }
        wait_idle(&readers, READERS);
        let (reply, _answer) = mpsc::channel();
        assert!(
            readers.offer(search(), reply).is_none(),
            "a freed reader takes work again"
        );
    }

    #[test]
    fn no_readers_give_every_read_back() {
        let (reply, _answer) = mpsc::channel();
        assert!(Readers::none().offer(search(), reply).is_some());
    }

    /// Reads skip the queue only behind a busy writer with no watcher batch
    /// ahead of them.
    #[test]
    fn busy_should_let_a_read_bypass_only_a_busy_writer_with_no_batch_ahead() {
        let busy = Busy::default();
        assert!(!busy.read_may_bypass(), "an idle writer answers at once");
        busy.queued(false);
        assert!(busy.read_may_bypass(), "a running write is overtaken");
        busy.queued(true);
        assert!(!busy.read_may_bypass(), "a queued batch is not");
        busy.done(false);
        assert!(!busy.read_may_bypass(), "the batch still runs");
        busy.done(true);
        assert!(!busy.read_may_bypass(), "idle again");
        assert_eq!(busy.jobs.load(Ordering::Acquire), 0);
        assert_eq!(busy.applies.load(Ordering::Acquire), 0);
    }
}
