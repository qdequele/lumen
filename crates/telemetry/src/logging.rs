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

use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;

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

/// Keeps the log writer thread alive; dropping it flushes the queued lines.
///
/// Hold it for the whole life of the process (a named binding in `main`, not
/// `let _ =`, which would drop it immediately): once it is gone the writer
/// thread has exited and later log lines are never written. Dropping it at the
/// end of `main` writes every line still queued, then returns. The wait is
/// bounded (about one second), so a stalled log pipe cannot hang shutdown.
///
/// It is not dropped when the process ends through `std::process::exit` or an
/// aborting panic: lines still queued at that point are lost.
#[must_use = "dropping the guard stops the log writer thread; hold it until the process exits"]
#[derive(Debug)]
pub struct LogGuard {
    dropped: ErrorCounter,
    /// `Option` only so `Drop` can flush before reporting dropped lines.
    worker: Option<WorkerGuard>,
}

impl LogGuard {
    /// Number of log lines discarded so far because the queue was full.
    pub fn dropped_lines(&self) -> usize {
        self.dropped.dropped_lines()
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        // Dropping the worker guard blocks (bounded) until the writer thread
        // has written everything queued before this point.
        drop(self.worker.take());
        let dropped = self.dropped.dropped_lines();
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
        .thread_name("lumen-log-writer")
        .finish(writer);
    let guard = LogGuard {
        dropped: non_blocking.error_counter(),
        worker: Some(worker),
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
    use std::sync::{Arc, Condvar, Mutex};

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
