use std::{
    collections::VecDeque,
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use thiserror::Error;
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    EnvFilter, Layer,
    layer::{Context, SubscriberExt as _},
    registry::LookupSpan,
    util::{SubscriberInitExt as _, TryInitError},
};

pub const DEFAULT_GUI_LOG_CAPACITY: usize = 1_000;
const RETAINED_LOG_FILES: usize = 7;

pub struct Logging {
    buffer: LogBuffer,
    _file_guard: Option<WorkerGuard>,
}

impl Logging {
    #[must_use]
    pub fn buffer(&self) -> LogBuffer {
        self.buffer.clone()
    }
}

pub fn initialize(directory: impl AsRef<Path>) -> Result<Logging, InitError> {
    let directory = directory.as_ref();
    fs::create_dir_all(directory).map_err(|source| InitError::CreateDirectory {
        path: directory.to_path_buf(),
        source,
    })?;
    let file = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("tevir")
        .filename_suffix("log")
        .max_log_files(RETAINED_LOG_FILES)
        .build(directory)?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .with_writer(file_writer);
    let stderr_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_target(false)
        .with_writer(io::stderr);
    let buffer = LogBuffer::default();
    tracing_subscriber::registry()
        .with(log_filter()?)
        .with(file_layer)
        .with(stderr_layer)
        .with(BufferLayer {
            buffer: buffer.clone(),
        })
        .try_init()?;

    Ok(Logging {
        buffer,
        _file_guard: Some(file_guard),
    })
}

pub fn initialize_ephemeral() -> Result<Logging, InitError> {
    let buffer = LogBuffer::default();
    let stderr_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_target(false)
        .with_writer(io::stderr);
    tracing_subscriber::registry()
        .with(log_filter()?)
        .with(stderr_layer)
        .with(BufferLayer {
            buffer: buffer.clone(),
        })
        .try_init()?;

    Ok(Logging {
        buffer,
        _file_guard: None,
    })
}

fn log_filter() -> Result<EnvFilter, tracing_subscriber::filter::ParseError> {
    EnvFilter::try_new(env::var("RUST_LOG").unwrap_or_else(|_| String::from("info")))
}

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<LogBufferInner>>,
}

impl LogBuffer {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LogBufferInner {
                started: Instant::now(),
                next_sequence: 1,
                capacity: capacity.max(1),
                entries: VecDeque::with_capacity(capacity.max(1)),
            })),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<LogEntry> {
        let inner = self.lock();
        inner.entries.iter().cloned().collect()
    }

    pub fn clear(&self) {
        self.lock().entries.clear();
    }

    fn push(&self, level: LogLevel, target: String, message: String) {
        let mut inner = self.lock();
        let entry = LogEntry {
            sequence: inner.next_sequence,
            elapsed_millis: inner.started.elapsed().as_millis(),
            level,
            target,
            message,
        };
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        if inner.entries.len() == inner.capacity {
            inner.entries.pop_front();
        }
        inner.entries.push_back(entry);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogBufferInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_GUI_LOG_CAPACITY)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
    pub sequence: u64,
    pub elapsed_millis: u128,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

impl From<&Level> for LogLevel {
    fn from(level: &Level) -> Self {
        match *level {
            Level::TRACE => Self::Trace,
            Level::DEBUG => Self::Debug,
            Level::INFO => Self::Info,
            Level::WARN => Self::Warn,
            Level::ERROR => Self::Error,
        }
    }
}

struct LogBufferInner {
    started: Instant,
    next_sequence: u64,
    capacity: usize,
    entries: VecDeque<LogEntry>,
}

struct BufferLayer {
    buffer: LogBuffer,
}

impl<S> Layer<S> for BufferLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(
            metadata.level().into(),
            String::from(metadata.target()),
            visitor.finish(),
        );
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<String>,
}

impl EventVisitor {
    fn finish(self) -> String {
        match (self.message, self.fields.is_empty()) {
            (Some(message), true) => message,
            (Some(message), false) => format!("{message} {}", self.fields.join(" ")),
            (None, _) => self.fields.join(" "),
        }
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields.push(format!("{}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(String::from(value));
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error("could not create log directory `{}`: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not initialize rotating log files: {0}")]
    File(#[from] tracing_appender::rolling::InitError),
    #[error("RUST_LOG is not a valid filter: {0}")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("the global tracing subscriber is already installed: {0}")]
    Subscriber(#[from] TryInitError),
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::{layer::SubscriberExt as _, registry};

    use super::{BufferLayer, LogBuffer, LogLevel};

    #[test]
    fn retains_only_the_newest_events() {
        let buffer = LogBuffer::new(2);
        let subscriber = registry().with(BufferLayer {
            buffer: buffer.clone(),
        });

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(peer = "left", "connected");
            tracing::warn!(attempt = 2, "reconnecting");
            tracing::error!("connection failed");
        });

        let entries = buffer.snapshot();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, LogLevel::Warn);
        assert!(entries[0].message.contains("reconnecting"));
        assert!(entries[0].message.contains("attempt=2"));
        assert_eq!(entries[1].level, LogLevel::Error);
        assert_eq!(entries[1].sequence, 3);
    }

    #[test]
    fn clearing_does_not_reuse_sequence_numbers() {
        let buffer = LogBuffer::new(2);
        buffer.push(LogLevel::Info, String::from("test"), String::from("first"));
        buffer.clear();
        buffer.push(LogLevel::Info, String::from("test"), String::from("second"));

        assert_eq!(buffer.snapshot()[0].sequence, 2);
    }
}
