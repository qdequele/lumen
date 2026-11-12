//! Structured logging setup.
//!
//! Prompts and request bodies are NEVER logged: the gateway treats user data
//! as radioactive. Only metadata (request id, model, status, latency) is
//! emitted, and that is the responsibility of the request span, not this
//! module - which only wires up the subscriber.
//!
//! Log lines never reach stdout from a runtime worker: the subscriber pushes
//! each formatted line into a bounded in-memory queue and a dedicated thread
//! does the `write(2)`, so a slow log pipe cannot stall request handling. See
//! [`init_logging`] and [`LogGuard`].

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::IntCounter;
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;

use crate::metrics::Metrics;

/// Maximum number of formatted log lines waiting for the writer thread.
///
/// The queue is allocated up front (a few dozen bytes per slot, so roughly
/// 0.5 MB), which is why this is far below `tracing-appender`'s default of
/// 128 000: that default alone would cost several MB of idle memory. The
/// writer thread drains a healthy pipe much faster than requests produce
/// lines, so the queue only fills when the pipe itself is stalled.
const LOG_BUFFER_LINES: usize = 16_384;

/// Output format for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, coloured output for local development.
    Pretty,
    /// One JSON object per line, for production log pipelines.
    Json,
}

/// Name of the log writer thread, which [`LogHandle::flush`] recognises so it
/// never waits on itself.
const WRITER_THREAD_NAME: &str = "lumen-log-writer";

/// Longest a flush waits for one that another thread already started.
///
/// Slightly above the bound `tracing-appender` puts on stopping its worker
/// (100 ms to queue the stop message, then 1 s for the worker to drain).
const FLUSH_WAIT: Duration = Duration::from_millis(1_200);

/// How often a waiting flush checks whether the other one is done.
const FLUSH_POLL: Duration = Duration::from_millis(5);

/// Keeps the log writer thread alive; dropping it flushes the queued lines.
///
/// Hold it for the whole life of the process (a named binding in `main`, not
/// `let _ =`, which would drop it immediately): once it is gone the writer
/// thread has exited and later log lines are never written. Dropping it at the
/// end of `main` writes every line still queued, then returns. The wait is
/// bounded (about one second), so a stalled log pipe cannot hang shutdown.
///
/// It is not dropped when the process ends through `std::process::exit` or an
/// aborting panic. For the panic case, pass [`LogGuard::handle`] to
/// [`install_panic_flush_hook`]; lines still queued at a
/// `std::process::exit` are lost.
#[must_use = "dropping the guard stops the log writer thread; hold it until the process exits"]
#[derive(Debug)]
pub struct LogGuard {
    handle: LogHandle,
}

impl LogGuard {
    /// Number of log lines discarded so far because the queue was full.
    pub fn dropped_lines(&self) -> usize {
        self.handle.dropped_lines()
    }

    /// A handle on the same writer, for the places that outlive a borrow of
    /// the guard: the panic hook (see [`install_panic_flush_hook`]).
    pub fn handle(&self) -> LogHandle {
        self.handle.clone()
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        // Blocks (bounded) until the writer thread has written everything
        // queued before this point.
        self.handle.flush();
        let dropped = self.handle.dropped_lines();
        if dropped > 0 {
            // The log queue is closed by now, so this goes straight to stderr.
            // A failed write is ignored: there is nowhere left to report it.
            let _ = writeln!(
                std::io::stderr(),
                "lumen: {dropped} log line(s) were dropped because the log output could not keep up"
            );
        }
    }
}

/// A cloneable handle on the log writer a [`LogGuard`] owns.
///
/// It does not keep the writer alive: the guard does. It lets code that
/// cannot own the guard (a panic hook) flush the queue before the process
/// dies.
#[derive(Debug, Clone)]
pub struct LogHandle {
    dropped: ErrorCounter,
    writer: Arc<WriterSlot>,
}

/// The writer thread's guard, shared so that whichever comes first (the
/// [`LogGuard`] dropping or a panic hook) stops the writer, exactly once.
#[derive(Debug)]
struct WriterSlot {
    /// `tracing-appender` can only flush by dropping this guard, so it sits
    /// in an `Option` that the first flush empties.
    worker: Mutex<Option<WorkerGuard>>,
    /// Set once the flush that emptied `worker` has returned.
    stopped: AtomicBool,
}

impl LogHandle {
    /// Number of log lines discarded so far because the queue was full.
    pub fn dropped_lines(&self) -> usize {
        self.dropped.dropped_lines()
    }

    /// Write every queued line and stop the writer thread. Lines logged
    /// afterwards are never written, so this is for the end of the process
    /// only.
    ///
    /// The wait is bounded (a little over one second) and the call can be
    /// made from any thread, any number of times, concurrently:
    ///
    /// - the first caller stops the writer and waits for it to drain;
    /// - a caller arriving while that is in progress waits for it to finish
    ///   (bounded by the same limit) and stops nothing itself;
    /// - a later caller returns at once;
    /// - on the writer thread itself (a panic while writing) it does
    ///   nothing: that thread cannot drain its own queue while it waits.
    pub fn flush(&self) {
        if std::thread::current().name() == Some(WRITER_THREAD_NAME) {
            return;
        }
        // `try_lock`, never `lock`: this runs in a panic hook, which must not
        // park behind another thread. The lock is only ever held for the
        // `take` below, so a busy lock means another flush is starting.
        let worker = match self.writer.worker.try_lock() {
            Ok(mut slot) => slot.take(),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().take(),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(worker) = worker {
            // Dropping the worker guard is the only flush `tracing-appender`
            // offers. It blocks, bounded, until the writer has drained.
            drop(worker);
            self.writer.stopped.store(true, Ordering::Release);
            return;
        }
        // Another thread took the worker. Give its flush time to finish, so
        // a panic that aborts during shutdown does not cut it short. The
        // sleep is deliberate: this branch only runs while the process is
        // going down (the guard dropping in `main`, or a panic hook).
        let deadline = Instant::now() + FLUSH_WAIT;
        while !self.writer.stopped.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(FLUSH_POLL);
        }
    }

    /// Register `lumen_log_lines_dropped_total` on `metrics`: the log lines
    /// discarded because the queue was full, as [`dropped_lines`] counts
    /// them.
    ///
    /// The value is read when `/metrics` is scraped, from the atomic the
    /// writer already keeps: emitting a log line touches nothing new (no
    /// lock, no extra counter).
    ///
    /// # Errors
    ///
    /// [`prometheus::Error`] if the collector is registered twice.
    ///
    /// [`dropped_lines`]: Self::dropped_lines
    pub fn register_dropped_lines(&self, metrics: &Metrics) -> Result<(), prometheus::Error> {
        let counter = IntCounter::new(
            "lumen_log_lines_dropped_total",
            "Log lines dropped because the log output could not keep up (the queue was full).",
        )?;
        metrics.registry().register(Box::new(DroppedLinesCollector {
            dropped: self.dropped.clone(),
            counter,
            scrape: Mutex::new(()),
        }))
    }

    /// Whether a flush has stopped the writer.
    #[cfg(test)]
    fn is_stopped(&self) -> bool {
        self.writer.stopped.load(Ordering::Acquire)
    }

    /// Whether a flush has taken the writer (it may still be draining).
    #[cfg(test)]
    fn is_claimed(&self) -> bool {
        self.writer.worker.lock().unwrap().is_none()
    }
}

/// Exposes the writer's dropped-line count as a Prometheus counter.
///
/// `tracing-appender` owns the count (an atomic), so there is nothing to
/// increment from here: each scrape raises the Prometheus counter to the
/// current value.
struct DroppedLinesCollector {
    dropped: ErrorCounter,
    counter: IntCounter,
    /// Held for the length of a scrape only, so that two concurrent scrapes
    /// cannot both add the same difference. Nothing else ever takes it.
    scrape: Mutex<()>,
}

impl Collector for DroppedLinesCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.counter.desc()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let scrape = match self.scrape.lock() {
            Ok(scrape) => scrape,
            Err(poisoned) => poisoned.into_inner(),
        };
        let dropped = u64::try_from(self.dropped.dropped_lines()).unwrap_or(u64::MAX);
        self.counter
            .inc_by(dropped.saturating_sub(self.counter.get()));
        drop(scrape);
        self.counter.collect()
    }
}

/// Make a panic flush the log queue before the process aborts.
///
/// The release profile builds with `panic = "abort"`, so a panic ends the
/// process without unwinding: the [`LogGuard`] in `main` is never dropped,
/// and without this hook the lines queued just before the crash (the ones
/// that explain it) would be lost. The hook first runs the hook that was
/// installed before it (the default one prints the panic message to stderr),
/// then calls [`LogHandle::flush`], whose wait is bounded.
///
/// Call it once, from `main`, right after [`init_logging`]. It does nothing
/// in a build where panics unwind (dev and test profiles): there a panic
/// inside a task is caught and the process keeps running, so stopping the
/// log writer would silence every later line.
pub fn install_panic_flush_hook(handle: LogHandle) {
    if cfg!(panic = "abort") {
        set_panic_flush_hook(handle);
    }
}

/// Chain a hook that flushes `handle` after the current panic hook.
fn set_panic_flush_hook(handle: LogHandle) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        handle.flush();
    }));
}

/// Wrap `writer` in a queue drained by a dedicated thread.
///
/// Lossy on purpose. `tracing-appender` offers two behaviours when the queue
/// is full: lossless, where the thread emitting the log line blocks until
/// there is room, and lossy, where the line is discarded and counted. Lossless
/// would bring back exactly the stall this writer exists to remove (a runtime
/// worker parked on a slow log pipe), so the request path uses lossy: emitting
/// a line is always a non-blocking queue push.
///
/// Lossy does not put shutdown-time error lines at risk: lines are discarded
/// only while the queue is full, never because the process is ending. On a
/// healthy pipe the queue is near empty, and dropping the [`LogGuard`] writes
/// every queued line, in order, before `main` returns. Lines are lost only
/// when the pipe is stalled for long enough to fill the queue, in which case
/// blocking would not have delivered them either; the count is reported on
/// stderr at exit.
fn non_blocking<W>(writer: W, buffered_lines: usize) -> (NonBlocking, LogGuard)
where
    W: Write + Send + 'static,
{
    let (non_blocking, worker) = NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(buffered_lines)
        .thread_name(WRITER_THREAD_NAME)
        .finish(writer);
    let guard = LogGuard {
        handle: LogHandle {
            dropped: non_blocking.error_counter(),
            writer: Arc::new(WriterSlot {
                worker: Mutex::new(Some(worker)),
                stopped: AtomicBool::new(false),
            }),
        },
    };
    (non_blocking, guard)
}

/// Initialise the global tracing subscriber.
///
/// Reads `RUST_LOG` for filtering, falling back to `default_directive`
/// (e.g. `"info"`). Lines go to stdout through a non-blocking writer (see
/// [`LogGuard`]); the caller must keep the returned guard until the process
/// exits. Safe to call more than once: a second call is a no-op (returns
/// `None`) rather than a panic, which keeps tests independent.
pub fn init_logging(format: LogFormat, default_directive: &str) -> Option<LogGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));

    let (writer, guard) = non_blocking(std::io::stdout(), LOG_BUFFER_LINES);

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(writer);

    let installed = match format {
        LogFormat::Pretty => builder.try_init().is_ok(),
        LogFormat::Json => builder.json().flatten_event(true).try_init().is_ok(),
    };
    installed.then_some(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;

    /// A writer that parks the writer thread until `open` is called, standing
    /// in for a stalled log pipe.
    #[derive(Clone, Default)]
    struct GatedSink {
        gate: Arc<(Mutex<bool>, Condvar)>,
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl GatedSink {
        fn open(&self) {
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
        }

        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.bytes.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Write for GatedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut open = self.gate.0.lock().unwrap();
            while !*open {
                open = self.gate.1.wait(open).unwrap();
            }
            drop(open);
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Emit `count` info lines then one error line through `writer`.
    fn emit(writer: NonBlocking, count: usize) {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..count {
                tracing::info!("line {i}");
            }
            tracing::error!("fatal at shutdown");
        });
    }

    #[test]
    fn dropping_the_guard_flushes_buffered_lines() {
        let sink = GatedSink::default();
        let (writer, guard) = non_blocking(sink.clone(), 64);

        emit(writer, 10);
        // The writer thread is parked on the gate: everything is still queued.
        assert_eq!(sink.lines(), Vec::<String>::new());

        sink.open();
        drop(guard);

        let lines = sink.lines();
        assert_eq!(lines.len(), 11, "{lines:?}");
        for (i, line) in lines.iter().take(10).enumerate() {
            assert!(line.ends_with(&format!("line {i}")), "{line}");
        }
        assert!(lines[10].contains("ERROR"), "{}", lines[10]);
        assert!(lines[10].ends_with("fatal at shutdown"), "{}", lines[10]);
    }

    #[test]
    fn flushing_through_a_handle_writes_buffered_lines_once() {
        let sink = GatedSink::default();
        let (writer, guard) = non_blocking(sink.clone(), 64);
        let handle = guard.handle();

        emit(writer, 10);
        assert_eq!(sink.lines(), Vec::<String>::new());

        sink.open();
        handle.flush();
        let lines = sink.lines();
        assert_eq!(lines.len(), 11, "{lines:?}");
        assert!(lines[10].ends_with("fatal at shutdown"), "{}", lines[10]);

        // The writer is gone: a second flush and the guard's own drop find
        // nothing left to stop, and return at once.
        handle.flush();
        drop(guard);
        handle.flush();
        assert_eq!(sink.lines().len(), 11);
    }

    #[test]
    fn a_flush_from_the_writer_thread_does_nothing() {
        let sink = GatedSink::default();
        let (writer, guard) = non_blocking(sink.clone(), 64);
        let handle = guard.handle();
        emit(writer, 1);

        // Stopping the writer from its own thread would wait for an answer
        // only that thread can give.
        let on_writer = handle.clone();
        std::thread::Builder::new()
            .name(WRITER_THREAD_NAME.to_owned())
            .spawn(move || on_writer.flush())
            .unwrap()
            .join()
            .unwrap();
        assert!(!handle.is_stopped());
        assert_eq!(sink.lines(), Vec::<String>::new());

        sink.open();
        drop(guard);
        assert_eq!(sink.lines().len(), 2);
    }

    #[test]
    fn a_flush_during_shutdown_waits_for_the_flush_already_running() {
        let sink = GatedSink::default();
        let (writer, guard) = non_blocking(sink.clone(), 64);
        let handle = guard.handle();
        emit(writer, 10);

        // "Shutdown": the guard is dropped while the sink is still stalled.
        let shutdown = std::thread::spawn(move || drop(guard));
        while !handle.is_claimed() {
            std::thread::yield_now();
        }
        // A panic hook firing now has no writer left to stop, but must not
        // return (and let the process abort) before the lines are out.
        let hook = {
            let (handle, sink) = (handle.clone(), sink.clone());
            std::thread::spawn(move || {
                handle.flush();
                sink.lines().len()
            })
        };
        sink.open();
        assert_eq!(hook.join().unwrap(), 11);
        shutdown.join().unwrap();
    }

    #[test]
    fn the_panic_hook_flushes_buffered_lines() {
        let sink = GatedSink::default();
        let (writer, guard) = non_blocking(sink.clone(), 64);
        emit(writer, 10);
        sink.open();

        // Installed unconditionally here: the public entry point only does
        // so when a panic aborts, which a test build does not.
        set_panic_flush_hook(guard.handle());
        let panicked = std::thread::spawn(|| panic!("boom")).join();
        assert!(panicked.is_err());

        // The guard is still alive: only the hook can have stopped the writer.
        assert!(guard.handle().is_stopped());
        assert_eq!(sink.lines().len(), 11);
    }

    #[test]
    fn dropped_lines_are_exposed_as_a_prometheus_counter() {
        let sink = GatedSink::default();
        let (writer, guard) = non_blocking(sink.clone(), 4);
        let metrics = Metrics::new();
        guard.handle().register_dropped_lines(&metrics).unwrap();

        // Present from the first scrape, at zero.
        let out = metrics.encode_text();
        assert!(out.contains("# TYPE lumen_log_lines_dropped_total counter"));
        assert!(out.contains("lumen_log_lines_dropped_total 0\n"), "{out}");

        emit(writer, 49);
        let dropped = guard.dropped_lines();
        assert!(dropped >= 44, "dropped {dropped}");
        // Read at scrape time, and stable from one scrape to the next.
        for _ in 0..2 {
            let out = metrics.encode_text();
            assert!(
                out.contains(&format!("lumen_log_lines_dropped_total {dropped}\n")),
                "{out}"
            );
        }

        // Once per registry, like every other collector.
        assert!(guard.handle().register_dropped_lines(&metrics).is_err());

        sink.open();
        drop(guard);
    }

    #[test]
    fn a_stalled_writer_drops_lines_instead_of_blocking_the_caller() {
        let sink = GatedSink::default();
        let (writer, guard) = non_blocking(sink.clone(), 4);

        // With the gate closed a lossless writer would block here forever.
        emit(writer, 49);
        let dropped = guard.dropped_lines();
        assert!(dropped >= 44, "dropped {dropped}");

        sink.open();
        drop(guard);
        assert_eq!(sink.lines().len() + dropped, 50);
    }
}
