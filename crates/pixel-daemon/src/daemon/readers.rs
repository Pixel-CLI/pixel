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
//!
//! A request that sets `stale_ok` (the brief graph reads and `targets_facts`) asks for the
//! opposite trade: it goes to a reader precisely while a batch is queued or
//! running, and the replica answers from the generation before it, saying
//! how long the oldest unpublished batch has waited ([`Busy::behind`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{Msg, panic_message};
use crate::api::{Request, Response, failure_response};
use crate::plane::replica_may_answer;

/// Readers a daemon runs beside its writer.
pub(super) const READERS: usize = 2;

/// A copy of the corpus that answers read-plane requests from what the
/// writer last published, on its own thread.
pub trait ReadReplica: Send {
    /// The answer, or `None` when only the writer can give it. `behind`
    /// says how long the oldest watcher batch the writer has not finished
    /// has been queued, `None` when there is none: a `stale_ok` answer
    /// samples it once computed and states it.
    fn read(&mut self, req: &Request, behind: &dyn Fn() -> Option<Duration>) -> Option<Response>;
}

/// How much the writer holds: every job queued or running, and among them
/// the ones that change what a read answers (a watcher batch, a HEAD move,
/// the watch's catch-up), which no read may overtake, with the instant each
/// was queued.
#[derive(Debug, Default)]
pub(super) struct Busy {
    jobs: AtomicUsize,
    /// Queue instants of the refreshing jobs, oldest first: the writer runs
    /// jobs in order, so the one it finishes is always the front.
    refreshes: Mutex<VecDeque<Instant>>,
}

impl Busy {
    /// A job was queued; `apply` when it is a watcher batch.
    pub(super) fn queued(&self, apply: bool) {
        self.jobs.fetch_add(1, Ordering::AcqRel);
        if apply {
            self.refreshes().push_back(Instant::now());
        }
    }

    /// The writer finished a job; `apply` when it was a watcher batch.
    pub(super) fn done(&self, apply: bool) {
        if apply {
            self.refreshes().pop_front();
        }
        self.jobs.fetch_sub(1, Ordering::AcqRel);
    }

    /// A read may skip the writer's queue: the writer is busy, and nothing
    /// ahead of the read is a watcher batch it must see.
    pub(super) fn read_may_bypass(&self) -> bool {
        self.refreshes().is_empty() && self.jobs.load(Ordering::Acquire) > 0
    }

    /// `req` goes to a reader rather than the writer: a replica may answer
    /// it ([`replica_may_answer`]) and either the writer is busy with no
    /// batch ahead, or the request accepts the generation before the batch.
    pub(super) fn reader_may_take(&self, req: &Request) -> bool {
        replica_may_answer(req)
            && (self.read_may_bypass() || (req.stale_ok() && self.behind().is_some()))
    }

    /// How long the oldest unfinished watcher batch has been queued: what a
    /// generation published before it lacks, at least. It leaves out the
    /// debounce that preceded the queueing (at most `MAX_PENDING_WAIT`), and
    /// a batch already published counts until the writer marks it done.
    pub(super) fn behind(&self) -> Option<Duration> {
        self.refreshes().front().map(Instant::elapsed)
    }

    fn refreshes(&self) -> std::sync::MutexGuard<'_, VecDeque<Instant>> {
        self.refreshes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A read no reader took, handed back for the writer.
pub(super) type Returned = (Request, mpsc::Sender<Response>);

/// One read handed to a reader, and where its answer goes.
struct ReadJob {
    req: Request,
    reply: mpsc::Sender<Response>,
}

/// The reader threads and their shared queue. At most `READERS` reads run
/// at once; more wait in the queue for the first reader to finish, which
/// takes milliseconds, rather than behind the writer's long job.
pub(super) struct Readers {
    jobs: Option<mpsc::Sender<ReadJob>>,
    threads: Vec<JoinHandle<()>>,
}

impl Readers {
    /// One thread per replica. A request a replica gives back goes to the
    /// loop as [`Msg::Requeue`], for the writer. `busy` is the writer's, so
    /// a `stale_ok` answer says how far behind it is.
    pub(super) fn spawn(
        replicas: Vec<Box<dyn ReadReplica>>,
        requeue: &mpsc::Sender<Msg>,
        busy: &Arc<Busy>,
    ) -> Self {
        let (jobs, queue) = mpsc::channel::<ReadJob>();
        let queue = Arc::new(Mutex::new(queue));
        let threads = replicas
            .into_iter()
            .map(|mut replica| {
                let queue = Arc::clone(&queue);
                let requeue = requeue.clone();
                let busy = Arc::clone(busy);
                std::thread::spawn(move || {
                    loop {
                        let next = queue.lock().unwrap_or_else(PoisonError::into_inner).recv();
                        let Ok(job) = next else {
                            return;
                        };
                        serve_read(replica.as_mut(), job, &requeue, &busy);
                    }
                })
            })
            .collect();
        Readers {
            jobs: Some(jobs),
            threads,
        }
    }

    /// No readers: every request goes to the writer.
    pub(super) fn none() -> Self {
        Readers {
            jobs: None,
            threads: Vec::new(),
        }
    }

    /// Queue `req` for the readers; `Some` gives it back when there are none.
    pub(super) fn offer(&self, req: Request, reply: mpsc::Sender<Response>) -> Option<Returned> {
        let Some(jobs) = &self.jobs else {
            return Some((req, reply));
        };
        jobs.send(ReadJob { req, reply })
            .err()
            .map(|mpsc::SendError(job)| (job.req, job.reply))
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
fn serve_read(
    replica: &mut dyn ReadReplica,
    job: ReadJob,
    requeue: &mpsc::Sender<Msg>,
    busy: &Busy,
) {
    let op = job.req.op_name();
    let answered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        replica.read(&job.req, &|| busy.behind())
    }));
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

    /// Answers `search`, gives back `status`, panics on `resolve`, holds
    /// `targets` until released, and answers `symbol` with the lag it was
    /// handed.
    struct Scripted {
        release: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    impl ReadReplica for Scripted {
        fn read(
            &mut self,
            req: &Request,
            behind: &dyn Fn() -> Option<Duration>,
        ) -> Option<Response> {
            match req {
                Request::Symbol { .. } => Some(Response::success(
                    "symbol",
                    serde_json::json!({"behind": behind().is_some()}),
                )),
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

            stale_ok: false,
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

    fn symbol(stale_ok: bool) -> Request {
        Request::Symbol {
            name: "login".into(),
            stale_ok,
        }
    }

    fn targets_facts(stale_ok: bool) -> Request {
        Request::TargetsFacts {
            task: "t".into(),
            limit: None,
            relevance_only: true,
            stale_ok,
        }
    }

    fn pool(release: &Arc<Mutex<mpsc::Receiver<()>>>) -> (Readers, mpsc::Receiver<Msg>) {
        pool_over(release, &Arc::new(Busy::default()))
    }

    fn pool_over(
        release: &Arc<Mutex<mpsc::Receiver<()>>>,
        busy: &Arc<Busy>,
    ) -> (Readers, mpsc::Receiver<Msg>) {
        let (loop_tx, loop_rx) = mpsc::channel();
        let replicas: Vec<Box<dyn ReadReplica>> = (0..READERS)
            .map(|_| {
                Box::new(Scripted {
                    release: Arc::clone(release),
                }) as Box<dyn ReadReplica>
            })
            .collect();
        (Readers::spawn(replicas, &loop_tx, busy), loop_rx)
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
                        limit: None,
                        stale_ok: false,
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

        for _ in 0..READERS + 1 {
            let (reply, answer) = mpsc::channel();
            assert!(readers.offer(search(), reply).is_none());
            let answered = answer.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(answered.result.unwrap()["from"], "replica");
        }

        let (reply, _answer) = mpsc::channel();
        assert!(readers.offer(Request::Status {}, reply).is_none());
        let Ok(Msg::Requeue(req, _reply)) = loop_rx.recv_timeout(Duration::from_secs(5)) else {
            panic!("a request the replica cannot answer goes back to the loop");
        };
        assert_eq!(req, Request::Status {});
    }

    /// At most `READERS` reads run at once: one more waits for a reader to
    /// finish, then is answered by it, never by the busy writer.
    #[test]
    fn a_read_past_the_reader_count_should_wait_for_a_free_reader() {
        let (release_tx, release_rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        let (readers, loop_rx) = pool(&release);
        let mut held = Vec::new();
        for _ in 0..READERS {
            let (reply, answer) = mpsc::channel();
            assert!(readers.offer(targets(), reply).is_none());
            held.push(answer);
        }
        let (reply, answer) = mpsc::channel();
        assert!(readers.offer(search(), reply).is_none());
        assert!(
            answer.recv_timeout(Duration::from_millis(200)).is_err(),
            "a third read ran with both readers busy"
        );
        release_tx.send(()).unwrap();
        let answered = answer.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(answered.result.unwrap()["from"], "replica");
        assert!(
            loop_rx.try_recv().is_err(),
            "nothing went back to the writer"
        );
        release_tx.send(()).unwrap();
        for answer in held {
            assert!(answer.recv_timeout(Duration::from_secs(5)).unwrap().ok);
        }
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
        assert!(busy.refreshes().is_empty());
    }

    /// `behind` dates the oldest batch the writer has not finished: from its
    /// queueing, kept while a later batch waits behind it, and gone with the
    /// last one. A job that is not a batch never dates anything.
    #[test]
    fn busy_behind_should_date_the_oldest_unfinished_batch() {
        let busy = Busy::default();
        busy.queued(false);
        assert_eq!(busy.behind(), None, "a request is not a batch");
        busy.queued(true);
        std::thread::sleep(Duration::from_millis(30));
        busy.queued(true);
        let both = busy.behind().expect("two batches queued");
        assert!(both >= Duration::from_millis(30), "{both:?}");
        busy.done(false);
        busy.done(true);
        let second = busy.behind().expect("the second batch still waits");
        assert!(second < both, "{second:?} after {both:?}");
        busy.done(true);
        assert_eq!(busy.behind(), None);
    }

    /// Who takes a request. A read-plane op overtakes a busy writer but
    /// never a batch; a `stale_ok` read overtakes the batch too; the same
    /// ops without the flag, and every other write-plane op, stay with the
    /// writer whatever it holds.
    #[test]
    fn reader_may_take_should_let_only_stale_ok_reads_overtake_a_batch() {
        let busy = Busy::default();
        let cases = |busy: &Busy| {
            [
                search(),
                symbol(true),
                targets_facts(true),
                symbol(false),
                targets_facts(false),
                Request::Status {},
            ]
            .map(|req| busy.reader_may_take(&req))
        };
        assert_eq!(
            cases(&busy),
            [false; 6],
            "an idle writer answers everything"
        );
        busy.queued(false);
        assert_eq!(
            cases(&busy),
            [true, true, true, false, false, false],
            "a busy writer with no batch"
        );
        busy.queued(true);
        assert_eq!(
            cases(&busy),
            [false, true, true, false, false, false],
            "a batch ahead: only a stale_ok read overtakes it"
        );
        busy.done(false);
        assert_eq!(
            cases(&busy),
            [false, true, true, false, false, false],
            "the batch runs alone"
        );
        busy.done(true);
        assert_eq!(cases(&busy), [false; 6], "idle again");
    }

    #[test]
    fn brief_graph_reads_should_overtake_a_batch_only_with_stale_ok() {
        let busy = Busy::default();
        busy.queued(true);
        for op in [
            "search", "resolve", "meaning", "skeleton", "context", "uses", "impact", "trace",
            "evaluate",
        ] {
            let mut value = serde_json::json!({"op": op, "pattern": "a", "phrase": "a", "query": "a", "file": "a.rs", "uid": "a",
                "uid_or_name": "a", "direction": "upstream", "role": "callers",
                "from": "a", "to": "b"});
            let ordinary = serde_json::from_value(value.clone()).unwrap();
            assert!(
                !busy.reader_may_take(&ordinary),
                "{op} must wait for the edit"
            );
            value["stale_ok"] = serde_json::json!(true);
            let stale = serde_json::from_value(value).unwrap();
            assert!(
                busy.reader_may_take(&stale),
                "{op} accepts the published generation"
            );
        }
    }

    /// A reader hands its replica the writer's lag: present while a batch is
    /// unfinished, absent once it is done.
    #[test]
    fn a_reader_should_hand_the_replica_the_writers_lag() {
        let (_release_tx, release_rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        let busy = Arc::new(Busy::default());
        let (readers, _loop_rx) = pool_over(&release, &busy);
        let ask = || {
            let (reply, answer) = mpsc::channel();
            assert!(readers.offer(symbol(true), reply).is_none());
            answer
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .result
                .unwrap()["behind"]
                .clone()
        };
        busy.queued(true);
        assert_eq!(ask(), serde_json::json!(true));
        busy.done(true);
        assert_eq!(ask(), serde_json::json!(false));
    }
}
