//! Background jobs for a single-threaded UI: `spawn` runs a closure on a small worker pool and hands back a `Pending` mailbox the UI thread
//! polls; the worker wakes the UI thread through a `Waker` (a posted window message) when the job finished. The pool (`pool`) keeps the
//! thread count bounded and lets a blocked job (a hung download) not starve the ones queued behind it.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Mailbox for the result of a background job started with `spawn`. Poll it with `take()` from the UI thread.
pub struct Pending<T>(Arc<PendingInner<T>>);
struct PendingInner<T> {
    val: Mutex<Option<T>>,
    busy: AtomicBool,
}
impl<T> Clone for Pending<T> {
    fn clone(&self) -> Self {
        Pending(self.0.clone())
    }
}
impl<T> Default for Pending<T> {
    fn default() -> Self {
        Pending(Arc::new(PendingInner { val: Mutex::new(None), busy: AtomicBool::new(false) }))
    }
}
impl<T> Pending<T> {
    /// the finished result (once), or None while running / not started / the job panicked
    pub fn take(&self) -> Option<T> {
        self.0.val.lock().ok().and_then(|mut g| g.take())
    }
    /// a job is running right now
    pub fn busy(&self) -> bool {
        self.0.busy.load(Ordering::Relaxed)
    }
}

/// Wakes the UI thread from any thread by posting window message `msg` to `hwnd` with `idx` (which card / view) as `wparam`. Cheap to clone.
/// A zero `hwnd` is a no-op (previews and tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct Waker {
    pub hwnd: isize,
    pub idx: usize,
    pub msg: u32,
}
impl Waker {
    pub fn new(hwnd: isize, idx: usize, msg: u32) -> Waker {
        Waker { hwnd, idx, msg }
    }
    pub fn wake(&self) {
        if self.hwnd != 0 {
            unsafe {
                let _ = PostMessageW(HWND(self.hwnd as *mut _), self.msg, WPARAM(self.idx), LPARAM(0));
            }
        }
    }
}

/// Run `job` on a pool worker; when it finishes the result lands in the returned `Pending` and `waker` is woken. A panicking job leaves the
/// mailbox empty. The job must not touch Direct2D (UI thread only). A full queue drops the job (`busy()` is false then).
pub fn spawn<T: Send + 'static>(waker: Waker, job: impl FnOnce() -> T + Send + 'static) -> Pending<T> {
    let p = Pending::<T>::default();
    let inner = p.0.clone();
    inner.busy.store(true, Ordering::Relaxed);
    // a pool worker runs it (see `pool`): the same body as a per-job thread, in submission order
    let queued = pool::global().submit(Box::new(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        if let Ok(v) = r {
            if let Ok(mut g) = inner.val.lock() {
                *g = Some(v);
            }
        }
        inner.busy.store(false, Ordering::Relaxed);
        waker.wake();
    }));
    if !queued {
        p.0.busy.store(false, Ordering::Relaxed);
    }
    p
}
/// The worker threads behind `spawn`. A thread per job (the old way) meant a few dozen thread creations per second on a busy desk, and the
/// NVIDIA user-mode driver keeps ~192 B per thread it has ever seen (the 0.5.10 memory growth). Now: up to `core` workers, created on demand and
/// kept; jobs run in submission order; a job that blocks (curl, a prompt, a sleeping disk) is noticed after `slow` and then an extra worker is
/// added so the jobs queued behind it never wait for it (the extras leave after a quiet spell, `max` bounds them).
pub mod pool {
    use std::collections::{HashMap, VecDeque};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    pub type Job = Box<dyn FnOnce() + Send + 'static>;

    struct State {
        queue: VecDeque<Job>,
        /// live worker threads / of them waiting for a job
        workers: usize,
        idle: usize,
        /// worker id -> when its current job started
        running: HashMap<u64, Instant>,
        next_id: u64,
        supervisor: bool,
    }
    struct Inner {
        st: Mutex<State>,
        /// workers wait here for jobs, the supervisor here for a non-empty queue
        work: Condvar,
        watch: Condvar,
        core: usize,
        max: usize,
        slow: Duration,
        cap: usize,
    }
    pub struct Pool(Arc<Inner>);

    fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The pool `spawn` uses: 4 permanent workers, up to 16 while jobs block, 256 queued jobs.
    pub fn global() -> &'static Pool {
        static P: OnceLock<Pool> = OnceLock::new();
        P.get_or_init(|| Pool::new(4, 16, Duration::from_millis(250), 256))
    }

    impl Pool {
        pub fn new(core: usize, max: usize, slow: Duration, cap: usize) -> Pool {
            let st = State { queue: VecDeque::new(), workers: 0, idle: 0, running: HashMap::new(), next_id: 0, supervisor: false };
            Pool(Arc::new(Inner { st: Mutex::new(st), work: Condvar::new(), watch: Condvar::new(), core: core.max(1), max: max.max(core.max(1)), slow, cap }))
        }

        /// Queue `job`; false = not accepted (queue full or no thread could be created), the job is dropped.
        pub fn submit(&self, job: Job) -> bool {
            let inner = &self.0;
            let mut st = lock(&inner.st);
            if st.queue.len() >= inner.cap {
                return false;
            }
            st.queue.push_back(job);
            if st.queue.len() > st.idle && st.workers < inner.core && !spawn_worker(inner, &mut st) && st.workers == 0 {
                st.queue.pop_back();
                return false;
            }
            if st.idle > 0 {
                inner.work.notify_one();
            } else if st.workers >= inner.core {
                if !st.supervisor {
                    let i2 = inner.clone();
                    st.supervisor = std::thread::Builder::new().name("widget-job-watch".into()).spawn(move || supervise(i2)).is_ok();
                }
                inner.watch.notify_one();
            }
            true
        }

        #[cfg(test)]
        pub fn workers(&self) -> usize {
            lock(&self.0.st).workers
        }
    }

    fn spawn_worker(inner: &Arc<Inner>, st: &mut State) -> bool {
        let (i2, id) = (inner.clone(), st.next_id);
        st.next_id += 1;
        let ok = std::thread::Builder::new().name("widget-job".into()).spawn(move || worker(i2, id)).is_ok();
        if ok {
            st.workers += 1;
        }
        ok
    }

    /// A thread that ends takes its COM apartment with it; a pooled one must not carry the state of one job (STA / MTA, init counts) into the next.
    fn reset_com() {
        use windows::Win32::System::Com::{CoGetApartmentType, CoUninitialize, APTTYPE, APTTYPEQUALIFIER, APTTYPEQUALIFIER_IMPLICIT_MTA};
        unsafe {
            for _ in 0..16 {
                let (mut t, mut q) = (APTTYPE::default(), APTTYPEQUALIFIER::default());
                if CoGetApartmentType(&mut t, &mut q).is_err() || q == APTTYPEQUALIFIER_IMPLICIT_MTA {
                    break;
                }
                CoUninitialize();
            }
        }
    }

    fn worker(inner: Arc<Inner>, id: u64) {
        let mut st = lock(&inner.st);
        loop {
            if let Some(job) = st.queue.pop_front() {
                st.running.insert(id, Instant::now());
                drop(st);
                let _ = catch_unwind(AssertUnwindSafe(job)); // a panicking job must not take the worker down
                reset_com();
                st = lock(&inner.st);
                st.running.remove(&id);
                continue;
            }
            st.idle += 1;
            let extra = st.workers > inner.core;
            if extra {
                let (g, t) = inner.work.wait_timeout(st, Duration::from_secs(3)).unwrap_or_else(|e| e.into_inner());
                st = g;
                st.idle -= 1;
                if t.timed_out() && st.queue.is_empty() && st.workers > inner.core {
                    st.workers -= 1;
                    return;
                }
            } else {
                st = inner.work.wait(st).unwrap_or_else(|e| e.into_inner());
                st.idle -= 1;
            }
        }
    }

    /// While jobs wait and every worker is busy, one of them for longer than `slow`: add a worker (up to `max`).
    fn supervise(inner: Arc<Inner>) {
        let mut st = lock(&inner.st);
        loop {
            while st.queue.is_empty() {
                st = inner.watch.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            let stuck = st.running.values().any(|t| t.elapsed() >= inner.slow);
            if st.idle == 0 && stuck && st.workers < inner.max {
                spawn_worker(&inner, &mut st);
            }
            st = inner.watch.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        fn pool(core: usize, max: usize, slow_ms: u64, cap: usize) -> Pool {
            Pool::new(core, max, Duration::from_millis(slow_ms), cap)
        }

        #[test]
        fn jobs_run_in_order_and_results_arrive() {
            let p = pool(1, 1, 250, 256);
            let (tx, rx) = mpsc::channel();
            for i in 0..200 {
                let tx = tx.clone();
                assert!(p.submit(Box::new(move || tx.send(i).unwrap())));
            }
            let got: Vec<i32> = (0..200).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
            assert_eq!(got, (0..200).collect::<Vec<_>>(), "one worker = strict submission order");
        }

        #[test]
        fn a_panicking_job_does_not_kill_the_pool() {
            let p = pool(2, 2, 250, 256);
            for _ in 0..10 {
                assert!(p.submit(Box::new(|| panic!("job panic (expected in this test)"))));
            }
            let (tx, rx) = mpsc::channel();
            for i in 0..20 {
                let tx = tx.clone();
                assert!(p.submit(Box::new(move || tx.send(i).unwrap())));
            }
            let mut got: Vec<i32> = (0..20).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
            got.sort();
            assert_eq!(got, (0..20).collect::<Vec<_>>());
            assert_eq!(p.workers(), 2, "both workers survived ten panics");
        }

        fn process_threads() -> usize {
            use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
            use windows::Win32::System::Threading::GetCurrentProcessId;
            unsafe {
                let pid = GetCurrentProcessId();
                let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return 0 };
                let mut e = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
                let (mut n, mut ok) = (0, Thread32First(snap, &mut e).is_ok());
                while ok {
                    n += (e.th32OwnerProcessID == pid) as usize;
                    ok = Thread32Next(snap, &mut e).is_ok();
                }
                let _ = windows::Win32::Foundation::CloseHandle(snap);
                n
            }
        }

        #[test]
        fn many_jobs_do_not_create_many_threads() {
            let p = pool(4, 16, 250, 4096);
            let before = process_threads();
            let ids = Arc::new(Mutex::new(std::collections::HashSet::new()));
            let done = Arc::new(AtomicUsize::new(0));
            for _ in 0..3000 {
                while !p.submit(Box::new({
                    let (ids, done) = (ids.clone(), done.clone());
                    move || {
                        ids.lock().unwrap().insert(std::thread::current().id());
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                })) {
                    std::thread::sleep(Duration::from_millis(1)); // queue full: wait for the workers
                }
            }
            let t0 = Instant::now();
            while done.load(Ordering::SeqCst) < 3000 && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(done.load(Ordering::SeqCst), 3000);
            let distinct = ids.lock().unwrap().len();
            let after = process_threads();
            println!("3000 jobs ran on {distinct} threads (workers {}); process threads {before} -> {after} (other tests run in parallel); a thread per job would have been 3000", p.workers());
            assert!(distinct <= 4, "{distinct} threads ran 3000 quick jobs");
            assert_eq!(p.workers(), 4);
        }

        #[test]
        fn a_blocking_job_does_not_starve_the_others() {
            let p = pool(2, 8, 80, 256);
            let (tx, rx) = mpsc::channel();
            // both permanent workers block for 700 ms (a hung download) ...
            for _ in 0..2 {
                let tx = tx.clone();
                assert!(p.submit(Box::new(move || {
                    std::thread::sleep(Duration::from_millis(700));
                    tx.send("slow").unwrap();
                })));
            }
            std::thread::sleep(Duration::from_millis(20));
            // ... a quick job queued behind them still runs long before they end
            let t0 = Instant::now();
            let tx2 = tx.clone();
            assert!(p.submit(Box::new(move || tx2.send("quick").unwrap())));
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "quick");
            assert!(t0.elapsed() < Duration::from_millis(500), "the quick job waited {:?}", t0.elapsed());
            assert!(p.workers() >= 3 && p.workers() <= 8);
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "slow");
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "slow");
        }

        #[test]
        fn the_queue_and_the_extra_workers_are_bounded() {
            let p = pool(2, 4, 30, 5);
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let mut accepted = 0;
            for _ in 0..40 {
                let gate = gate.clone();
                if p.submit(Box::new(move || {
                    let (m, c) = &*gate;
                    let mut open = m.lock().unwrap();
                    while !*open {
                        open = c.wait(open).unwrap();
                    }
                })) {
                    accepted += 1;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(300));
            assert!(p.workers() <= 4, "workers {}", p.workers());
            assert!(accepted <= 4 + 5, "accepted {accepted}: 4 running + a queue of 5 at most");
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn spawn_delivers_the_result_once_and_survives_a_panic() {
        let p = spawn(Waker::default(), || 21 * 2);
        let t0 = Instant::now();
        while p.busy() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(p.take(), Some(42));
        assert_eq!(p.take(), None, "the result is handed out once");
        let bad = spawn(Waker::default(), || -> u8 { panic!("job panic (expected in this test)") });
        let t0 = Instant::now();
        while bad.busy() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(bad.take(), None);
    }

    #[test]
    fn a_waker_without_a_window_is_a_no_op() {
        Waker::new(0, 3, 0x8002).wake();
        assert_eq!(Waker::default().msg, 0);
    }
}