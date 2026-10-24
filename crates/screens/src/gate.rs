//! At most one native capture request at a time in this process (#2737).
//!
//! ScreenCaptureKit's window list and single-image calls answer through a completion handler and
//! return no cancellation token: a request that has not answered within its budget has not been
//! cancelled, it is still outstanding. In QA's recordings of a dev Electron window (#2737) a
//! stall was followed by request after request that also timed out (299 of 498 attempts). So the
//! permit belongs to the native request, not to the wait for it:
//!
//! ```text
//! Idle -> Submitted -> Idle                  answered within the budget
//! Submitted -> TimedOutOutstanding           no answer within the budget: the caller gets TimedOut
//! TimedOutOutstanding -> Idle                the late answer arrives (and is dropped)
//! ```
//!
//! Only the request's own completion releases the permit (or dropping it unanswered, which means
//! it was never submitted); a stale completion never releases a later request's permit. A caller
//! that finds the permit held by a request past its budget gets [`Busy`] at once and submits
//! nothing; one that finds it held by a request still within its budget waits for it, so normal
//! concurrent use (Remote Control's screenshot and live view) is never refused.
//!
//! A request that never answers (a lost callback) holds the permit for the life of the process.
//! [`Gate::stalled`] reports one held long past its budget, so a long-lived process can end and
//! be started again rather than refuse every request from then on.
//!
//! Platform-independent, so the lifecycle is tested everywhere with a fake native call. The gate
//! covers the requests made through this package (listing and still images), not continuous
//! stream frames, and only this process: other processes still compete in the OS.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often a wait looks at the stop flag.
const SLICE: Duration = Duration::from_millis(100);

/// Nothing was submitted: a previous request still holds the permit, past its budget (or the
/// caller's own budget ran out waiting for one within it).
#[derive(Debug, Clone)]
pub struct Busy {
    /// The outstanding request's operation.
    pub op: &'static str,
    /// How long it has been outstanding.
    pub outstanding: Duration,
}

impl fmt::Display for Busy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a previous {} request has not answered for {:.0} s",
            self.op,
            self.outstanding.as_secs_f64()
        )
    }
}

impl std::error::Error for Busy {}

/// The request did not answer within the budget; it is still outstanding.
#[derive(Debug, Clone)]
pub struct TimedOut {
    /// What timed out, e.g. "listing windows" or "screenshot of window 52505".
    pub what: String,
}

impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} timed out", self.what)
    }
}

impl std::error::Error for TimedOut {}

/// The caller stopped waiting because it is stopping; a submitted request stays outstanding.
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stopped while waiting for a capture request")
    }
}

impl std::error::Error for Cancelled {}

/// Counts per operation, for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OpStats {
    pub submitted: u64,
    /// Answered, in time or late.
    pub completed: u64,
    pub timed_out: u64,
    /// Answered after the caller's budget ran out.
    pub late: u64,
    /// Refused without submitting (see [`Busy`]).
    pub busy: u64,
    pub cancelled: u64,
    /// Submission to answer, for answered requests.
    pub total_ms: u64,
    pub max_ms: u64,
}

struct Request {
    id: u64,
    op: &'static str,
    submitted: Instant,
    /// Its caller stopped waiting (timed out or cancelled).
    abandoned: bool,
}

struct State {
    current: Option<Request>,
    next_id: u64,
    stats: Vec<(&'static str, OpStats)>,
}

impl State {
    fn stats(&mut self, op: &'static str) -> &mut OpStats {
        let i = match self.stats.iter().position(|(o, _)| *o == op) {
            Some(i) => i,
            None => {
                self.stats.push((op, OpStats::default()));
                self.stats.len() - 1
            }
        };
        &mut self.stats[i].1
    }
}

pub struct Gate {
    state: Mutex<State>,
    freed: Condvar,
    budget: Duration,
}

/// Hands the native answer back; give it to the native completion handler. Completing twice, or
/// after another completion, does nothing.
pub struct Completion<T> {
    gate: &'static Gate,
    id: u64,
    tx: Mutex<Option<mpsc::Sender<T>>>,
}

impl<T> Completion<T> {
    /// Deliver the answer (dropped if the caller stopped waiting) and release the permit.
    pub fn complete(&self, value: T) {
        let Some(tx) = self.tx.lock().unwrap().take() else {
            return;
        };
        // Sent before the release: a caller that finds the request released finds its answer.
        let _ = tx.send(value);
        self.gate.release(self.id);
    }
}

impl<T> Drop for Completion<T> {
    /// Dropped unanswered: the request was never submitted (or the native side let go of it
    /// without answering), so nothing is outstanding.
    fn drop(&mut self) {
        if self.tx.get_mut().unwrap().take().is_some() {
            self.gate.release(self.id);
        }
    }
}

impl Gate {
    /// Each request may take `budget` to answer.
    pub const fn new(budget: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                current: None,
                next_id: 1,
                stats: Vec::new(),
            }),
            freed: Condvar::new(),
            budget,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make one native request: `submit` gets the [`Completion`] to call from the native
    /// completion handler, and returns an error only if it did not submit. Waits up to the
    /// budget for the answer, looking at `stop` every 100 ms.
    ///
    /// Errors: [`Busy`] (nothing submitted), [`TimedOut`] (still outstanding), [`Cancelled`],
    /// or `submit`'s own error.
    pub fn run<T: 'static>(
        &'static self,
        op: &'static str,
        stop: Option<&AtomicBool>,
        submit: impl FnOnce(Completion<T>) -> anyhow::Result<()>,
    ) -> anyhow::Result<T> {
        let (id, rx) = self.begin(op, stop, submit)?;
        let deadline = Instant::now() + self.budget;
        loop {
            if stop.is_some_and(|s| s.load(Ordering::SeqCst)) {
                self.abandon(id, op, false);
                return Err(Cancelled.into());
            }
            let now = Instant::now();
            if now >= deadline {
                return self.expire(id, op, &rx);
            }
            match rx.recv_timeout(SLICE.min(deadline - now)) {
                Ok(value) => return Ok(value),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // The completion was dropped unanswered: released, nothing outstanding.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("{op} ended without an answer")
                }
            }
        }
    }

    /// Acquire the permit and submit.
    fn begin<T: 'static>(
        &'static self,
        op: &'static str,
        stop: Option<&AtomicBool>,
        submit: impl FnOnce(Completion<T>) -> anyhow::Result<()>,
    ) -> anyhow::Result<(u64, mpsc::Receiver<T>)> {
        let id = self.acquire(op, stop)?;
        let (tx, rx) = mpsc::channel();
        submit(Completion {
            gate: self,
            id,
            tx: Mutex::new(Some(tx)),
        })?;
        Ok((id, rx))
    }

    fn acquire(&self, op: &'static str, stop: Option<&AtomicBool>) -> anyhow::Result<u64> {
        let wait_until = Instant::now() + self.budget;
        let mut s = self.lock();
        loop {
            let busy = match &s.current {
                None => None,
                Some(r) if r.abandoned => Some(Busy {
                    op: r.op,
                    outstanding: r.submitted.elapsed(),
                }),
                Some(r) => (Instant::now() >= wait_until).then(|| Busy {
                    op: r.op,
                    outstanding: r.submitted.elapsed(),
                }),
            };
            if let Some(busy) = busy {
                s.stats(op).busy += 1;
                return Err(busy.into());
            }
            if s.current.is_none() {
                let id = s.next_id;
                s.next_id += 1;
                s.current = Some(Request {
                    id,
                    op,
                    submitted: Instant::now(),
                    abandoned: false,
                });
                s.stats(op).submitted += 1;
                return Ok(id);
            }
            if stop.is_some_and(|f| f.load(Ordering::SeqCst)) {
                s.stats(op).cancelled += 1;
                return Err(Cancelled.into());
            }
            let left = wait_until.saturating_duration_since(Instant::now());
            s = self
                .freed
                .wait_timeout(s, SLICE.min(left).max(Duration::from_millis(1)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The budget ran out: the request is abandoned, unless it was answered meanwhile.
    ///
    /// The answer is sent before the permit is released, so it can already be in `rx` while the
    /// request still holds the permit: it is looked for first, and an answer that arrived is
    /// returned rather than thrown away as a timeout (and counted late). If the request is no
    /// longer outstanding when it is abandoned, its answer arrived in between.
    fn expire<T>(&self, id: u64, op: &'static str, rx: &mpsc::Receiver<T>) -> anyhow::Result<T> {
        if let Ok(value) = rx.try_recv() {
            return Ok(value);
        }
        if self.abandon(id, op, true) {
            return Err(TimedOut {
                what: op.to_string(),
            }
            .into());
        }
        rx.try_recv()
            .map_err(|_| anyhow::anyhow!("{op} ended without an answer"))
    }

    /// Mark request `id` abandoned if it is still outstanding; false if it was already answered.
    fn abandon(&self, id: u64, op: &'static str, timed_out: bool) -> bool {
        let mut s = self.lock();
        let Some(r) = s.current.as_mut().filter(|r| r.id == id) else {
            return false;
        };
        r.abandoned = true;
        let st = s.stats(op);
        if timed_out {
            st.timed_out += 1;
        } else {
            st.cancelled += 1;
        }
        drop(s);
        // Waiters for this permit now get Busy instead of waiting out their budget.
        self.freed.notify_all();
        true
    }

    fn release(&self, id: u64) {
        let mut s = self.lock();
        let Some(r) = s.current.take_if(|r| r.id == id) else {
            return;
        };
        let ms = r.submitted.elapsed().as_millis() as u64;
        let st = s.stats(r.op);
        st.completed += 1;
        st.total_ms += ms;
        st.max_ms = st.max_ms.max(ms);
        if r.abandoned {
            st.late += 1;
        }
        drop(s);
        self.freed.notify_all();
    }

    /// The request holding the permit, and how long it has been outstanding.
    pub fn outstanding(&self) -> Option<(&'static str, Duration)> {
        self.lock()
            .current
            .as_ref()
            .map(|r| (r.op, r.submitted.elapsed()))
    }

    /// The request holding the permit if its caller gave up on it and it has been outstanding
    /// for longer than `after`, and how long it has been outstanding.
    ///
    /// Nothing cancels a native request, so such a request may never answer (a lost
    /// ScreenCaptureKit callback), and every later request in this process is then refused as
    /// [`Busy`]. Only the process ending frees the permit; the caller decides when to do that.
    /// A request still within its budget is never reported, however long `after` is.
    pub fn stalled(&self, after: Duration) -> Option<(&'static str, Duration)> {
        let s = self.lock();
        let r = s.current.as_ref().filter(|r| r.abandoned)?;
        let held = r.submitted.elapsed();
        (held > after).then_some((r.op, held))
    }

    /// Counts per operation since the start.
    pub fn stats(&self) -> Vec<(&'static str, OpStats)> {
        self.lock().stats.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    fn gate(budget_ms: u64) -> &'static Gate {
        Box::leak(Box::new(Gate::new(Duration::from_millis(budget_ms))))
    }

    /// What the fake native call answers: a value or a native error.
    type Answer = Result<u32, String>;

    /// A fake native call that keeps its completion handler for the test to answer later.
    #[derive(Clone, Default)]
    struct Native {
        pending: Arc<Mutex<Vec<Completion<Answer>>>>,
        submits: Arc<AtomicUsize>,
    }

    impl Native {
        fn submit(&self) -> impl FnOnce(Completion<Answer>) -> anyhow::Result<()> {
            let me = self.clone();
            move |done| {
                me.submits.fetch_add(1, Ordering::SeqCst);
                me.pending.lock().unwrap().push(done);
                Ok(())
            }
        }

        fn answer(&self, value: Result<u32, String>) {
            let done = self.pending.lock().unwrap().remove(0);
            done.complete(value);
        }

        fn submits(&self) -> usize {
            self.submits.load(Ordering::SeqCst)
        }
    }

    fn answered(value: u32) -> impl FnOnce(Completion<Answer>) -> anyhow::Result<()> {
        move |done| {
            done.complete(Ok(value));
            Ok(())
        }
    }

    #[test]
    fn an_answer_within_the_budget_is_returned_and_frees_the_permit() {
        let g = gate(1000);
        assert_eq!(g.run("capture", None, answered(7)).unwrap(), Ok(7));
        assert!(g.outstanding().is_none());
        assert_eq!(g.run("list", None, answered(8)).unwrap(), Ok(8));
        let stats = g.stats();
        assert_eq!(stats[0].1.submitted, 1);
        assert_eq!(stats[0].1.completed, 1);
        assert_eq!(stats[0].1.busy, 0);
    }

    #[test]
    fn a_stalled_request_makes_later_ones_busy_until_it_answers() {
        let g = gate(40);
        let native = Native::default();
        let e = g.run("capture", None, native.submit()).unwrap_err();
        assert!(e.downcast_ref::<TimedOut>().is_some(), "{e}");
        assert!(
            g.outstanding().is_some(),
            "still outstanding after the timeout"
        );

        // Several later ticks: refused at once, nothing more submitted, no wait for the budget.
        for _ in 0..3 {
            let t = Instant::now();
            let e = g.run("list", None, native.submit()).unwrap_err();
            assert!(e.downcast_ref::<Busy>().is_some(), "{e}");
            assert!(t.elapsed() < Duration::from_millis(30));
        }
        assert_eq!(native.submits(), 1);

        // The late answer releases the permit and is not handed to anyone.
        native.answer(Ok(1));
        assert!(g.outstanding().is_none());
        let fresh = Native::default();
        let next = fresh.submit();
        let caller = std::thread::spawn(move || g.run("capture", None, next));
        while fresh.pending.lock().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(1));
        }
        fresh.answer(Ok(2));
        assert_eq!(
            caller.join().unwrap().unwrap(),
            Ok(2),
            "its own answer, not the late one"
        );

        let capture = g.stats().into_iter().find(|s| s.0 == "capture").unwrap().1;
        assert_eq!(
            (capture.timed_out, capture.late, capture.completed),
            (1, 1, 2)
        );
        let list = g.stats().into_iter().find(|s| s.0 == "list").unwrap().1;
        assert_eq!((list.busy, list.submitted), (3, 0));
    }

    #[test]
    fn a_native_error_answer_frees_the_permit() {
        let g = gate(1000);
        let r = g.run("capture", None, |done: Completion<Answer>| {
            done.complete(Err("the window is gone".into()));
            Ok(())
        });
        assert_eq!(r.unwrap(), Err("the window is gone".into()));
        assert!(g.outstanding().is_none());
    }

    #[test]
    fn a_failure_before_submission_frees_the_permit() {
        let g = gate(1000);
        let r = g.run("capture", None, |_done: Completion<Answer>| {
            anyhow::bail!("the window has no visible size")
        });
        assert!(r.unwrap_err().to_string().contains("no visible size"));
        assert!(g.outstanding().is_none());
        assert_eq!(g.run("capture", None, answered(3)).unwrap(), Ok(3));
    }

    #[test]
    fn an_answer_racing_the_deadline_settles_once() {
        // Answered before the caller looks at the deadline: the answer counts.
        let g = gate(1000);
        let native = Native::default();
        let (id, rx) = g.begin("capture", None, native.submit()).unwrap();
        native.answer(Ok(5));
        assert_eq!(g.expire(id, "capture", &rx).unwrap(), Ok(5));
        assert!(g.outstanding().is_none());

        // The deadline first: a timeout, and the answer after it only frees the permit.
        let native = Native::default();
        let (id, rx) = g.begin("capture", None, native.submit()).unwrap();
        let e = g.expire(id, "capture", &rx).unwrap_err();
        assert!(e.downcast_ref::<TimedOut>().is_some());
        native.answer(Ok(6));
        drop(rx);
        assert!(g.outstanding().is_none());
        let s = g.stats()[0].1;
        assert_eq!((s.completed, s.timed_out, s.late), (2, 1, 1));
    }

    #[test]
    fn an_answer_sent_before_the_deadline_is_returned_even_while_it_holds_the_permit() {
        // The completion has sent its answer but not yet released the permit (it is between the
        // two) when the caller's budget runs out: the answer counts, nothing is abandoned.
        let g = gate(1000);
        let native = Native::default();
        let (id, rx) = g.begin("capture", None, native.submit()).unwrap();
        let done = native.pending.lock().unwrap().remove(0);
        let tx = done.tx.lock().unwrap().take().unwrap();
        tx.send(Ok(9)).unwrap();
        assert_eq!(g.expire(id, "capture", &rx).unwrap(), Ok(9));
        let s = g.stats()[0].1;
        assert_eq!((s.timed_out, s.late), (0, 0), "not counted as a timeout");
        // The release that follows the send frees the permit as usual.
        g.release(id);
        drop(done);
        assert!(g.outstanding().is_none());
        assert!(g.stalled(Duration::ZERO).is_none());
    }

    #[test]
    fn only_an_abandoned_request_held_past_the_limit_is_stalled() {
        let g = gate(30);
        let native = Native::default();

        // Within its budget: outstanding, not stalled, however short the limit.
        let (id, rx) = g.begin("capture", None, native.submit()).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(g.stalled(Duration::ZERO).is_none());

        // Abandoned, but not held for longer than the limit yet.
        assert!(g.expire(id, "capture", &rx).is_err());
        assert!(g.stalled(Duration::from_secs(60)).is_none());

        // Held past the limit: reported with its operation and age.
        std::thread::sleep(Duration::from_millis(60));
        let (op, held) = g.stalled(Duration::from_millis(50)).expect("stalled");
        assert_eq!(op, "capture");
        assert!(held > Duration::from_millis(50), "{held:?}");

        // The late answer frees the permit: nothing is stalled any more.
        native.answer(Ok(1));
        assert!(g.stalled(Duration::ZERO).is_none());
        assert_eq!(g.run("list", None, answered(2)).unwrap(), Ok(2));
    }

    #[test]
    fn a_cancelled_request_that_never_answers_is_stalled_too() {
        let g = gate(5000);
        let native = Native::default();
        let stop = AtomicBool::new(true);
        let e = g.run("capture", Some(&stop), native.submit()).unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some(), "{e}");
        std::thread::sleep(Duration::from_millis(20));
        assert!(g.stalled(Duration::from_millis(10)).is_some());
    }

    #[test]
    fn a_stale_completion_never_frees_a_later_request() {
        let g = gate(1000);
        let native = Native::default();
        let (_, _rx) = g.begin("capture", None, native.submit()).unwrap();
        let first = native.pending.lock().unwrap().remove(0);
        first.complete(Ok(1));
        let (_, _rx2) = g.begin("capture", None, native.submit()).unwrap();
        // The first completion again (and dropping it) leaves the second request outstanding.
        first.complete(Ok(1));
        drop(first);
        assert!(g.outstanding().is_some());
        native.answer(Ok(2));
        assert!(g.outstanding().is_none());
    }

    #[test]
    fn stopping_ends_the_wait_without_freeing_an_unanswered_request() {
        let g = gate(5000);
        let native = Native::default();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            s.store(true, Ordering::SeqCst);
        });
        let t = Instant::now();
        let e = g.run("capture", Some(&stop), native.submit()).unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some(), "{e}");
        assert!(
            t.elapsed() < Duration::from_millis(1000),
            "bounded by the slice"
        );
        assert!(g.outstanding().is_some(), "the callback never came");
        let e = g.run("capture", None, native.submit()).unwrap_err();
        assert!(e.downcast_ref::<Busy>().is_some());
    }

    #[test]
    fn concurrent_callers_never_have_two_requests_outstanding() {
        let g = gate(2000);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let callers: Vec<_> = (0..8)
            .map(|i| {
                let (in_flight, max) = (in_flight.clone(), max.clone());
                std::thread::spawn(move || {
                    let op = if i % 2 == 0 { "list" } else { "capture" };
                    for _ in 0..5 {
                        let (in_flight, max) = (in_flight.clone(), max.clone());
                        let answer = g
                            .run(op, None, move |done: Completion<Answer>| {
                                let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                                max.fetch_max(n, Ordering::SeqCst);
                                std::thread::spawn(move || {
                                    std::thread::sleep(Duration::from_millis(2));
                                    in_flight.fetch_sub(1, Ordering::SeqCst);
                                    done.complete(Ok(i));
                                });
                                Ok(())
                            })
                            .expect("no spurious Busy while requests answer in time");
                        assert_eq!(answer, Ok(i), "each caller gets its own answer");
                    }
                })
            })
            .collect();
        for c in callers {
            c.join().unwrap();
        }
        assert_eq!(max.load(Ordering::SeqCst), 1);
        let busy: u64 = g.stats().iter().map(|s| s.1.busy).sum();
        assert_eq!(busy, 0);
    }

    #[test]
    fn a_waiter_gets_busy_once_the_request_it_waits_for_times_out() {
        let g = gate(60);
        let native = Native::default();
        let n = native.clone();
        let holder = std::thread::spawn(move || g.run("capture", None, n.submit()));
        while g.outstanding().is_none() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let e = g.run("list", None, native.submit()).unwrap_err();
        assert!(e.downcast_ref::<Busy>().is_some(), "{e}");
        assert!(holder
            .join()
            .unwrap()
            .unwrap_err()
            .downcast_ref::<TimedOut>()
            .is_some());
        assert_eq!(native.submits(), 1);
    }
}
