use chrono::Local;
use compact_str::CompactString;
pub use compact_str::format_compact;
use crossbeam_channel::{Receiver, Sender, unbounded};
use smallvec::SmallVec;
use std::fmt::Write as FmtWrite;
use std::io::{self, BufWriter, Write};
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl LogLevel {
    #[inline(always)]
    const fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

struct LogMsg {
    level: LogLevel,
    msg: CompactString,
}

enum LoggerCommand {
    Msg(LogMsg),
    Flush,
    Shutdown,
}

pub struct FLog {
    sender: Option<Sender<LoggerCommand>>,
    thread_handle: Option<JoinHandle<()>>,
    stats: Arc<LoggerStats>,
}

#[derive(Default)]
struct LoggerStats {
    messages_written: AtomicU64,
    flushes_performed: AtomicU64,
}

impl FLog {
    pub fn new() -> Self {
        panic::set_hook(Box::new(|_| {
            #[cfg(feature = "global")]
            flush_global();

            #[cfg(feature = "local")]
            flush_local();
        }));
        Self::with_config(LoggerConfig::default())
    }

    pub fn with_config(config: LoggerConfig) -> Self {
        panic::set_hook(Box::new(|_| {
            #[cfg(feature = "global")]
            flush_global();

            #[cfg(feature = "local")]
            flush_local();
        }));
        let (tx, rx) = unbounded::<LoggerCommand>();
        let stats = Arc::new(LoggerStats::default());
        let stats_clone = Arc::clone(&stats);

        let handle = thread::spawn(move || {
            Self::writer_loop(rx, config, stats_clone);
        });

        Self {
            sender: Some(tx),
            thread_handle: Some(handle),
            stats,
        }
    }

    fn writer_loop(rx: Receiver<LoggerCommand>, config: LoggerConfig, stats: Arc<LoggerStats>) {
        use LoggerCommand::*;

        let stderr = io::stderr();
        let mut writer = BufWriter::with_capacity(config.buffer_size, stderr);
        let mut batch: SmallVec<[LogMsg; 512]> = SmallVec::with_capacity(config.batch_size);
        let mut last_flush = Instant::now();
        let mut bytes_written_since_flush = 0usize;

        thread_local! {
            static TLS_FORMAT_BUF: std::cell::RefCell<CompactString> =
                std::cell::RefCell::new(CompactString::with_capacity(256));
        }

        #[inline]
        fn write_batch(
            batch: &mut SmallVec<[LogMsg; 512]>,
            writer: &mut BufWriter<io::Stderr>,
            stats: &Arc<LoggerStats>,
            bytes_written: &mut usize,
        ) {
            if batch.is_empty() {
                return;
            }

            for log in batch.iter() {
                TLS_FORMAT_BUF.with(|buf_cell| {
                    let mut buf = buf_cell.borrow_mut();
                    buf.clear();
                    let _ = write!(
                        buf,
                        "{} [{}] {}\n",
                        Local::now().format("%Y-%m-%d %H:%M:%S"),
                        log.level.as_str(),
                        log.msg
                    );
                    let _ = writer.write_all(buf.as_bytes());
                    *bytes_written += buf.len();
                });
            }

            stats
                .messages_written
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            batch.clear();
        }

        #[inline]
        fn flush_writer(
            writer: &mut BufWriter<io::Stderr>,
            stats: &Arc<LoggerStats>,
            last_flush: &mut Instant,
            bytes_written: &mut usize,
        ) {
            let _ = writer.flush();
            stats.flushes_performed.fetch_add(1, Ordering::Relaxed);
            *last_flush = Instant::now();
            *bytes_written = 0;
        }

        loop {
            match rx.recv() {
                Ok(Msg(log)) => {
                    batch.push(log);

                    while batch.len() < config.batch_size {
                        match rx.try_recv() {
                            Ok(Msg(log)) => batch.push(log),
                            Ok(Flush) => {
                                write_batch(
                                    &mut batch,
                                    &mut writer,
                                    &stats,
                                    &mut bytes_written_since_flush,
                                );
                                flush_writer(
                                    &mut writer,
                                    &stats,
                                    &mut last_flush,
                                    &mut bytes_written_since_flush,
                                );
                            }
                            Ok(Shutdown) => {
                                write_batch(
                                    &mut batch,
                                    &mut writer,
                                    &stats,
                                    &mut bytes_written_since_flush,
                                );
                                flush_writer(
                                    &mut writer,
                                    &stats,
                                    &mut last_flush,
                                    &mut bytes_written_since_flush,
                                );

                                while let Ok(cmd) = rx.try_recv() {
                                    if let LoggerCommand::Msg(log) = cmd {
                                        TLS_FORMAT_BUF.with(|buf_cell| {
                                            let mut buf = buf_cell.borrow_mut();
                                            buf.clear();
                                            let _ = write!(
                                                buf,
                                                "{} [{}] {}\n",
                                                Local::now().format("%Y-%m-%d %H:%M:%S"),
                                                log.level.as_str(),
                                                log.msg
                                            );
                                            let _ = writer.write_all(buf.as_bytes());
                                        });
                                        stats.messages_written.fetch_add(1, Ordering::Relaxed);
                                    }
                                }

                                let _ = writer.flush();
                                stats.flushes_performed.fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                            _ => break,
                        }
                    }

                    let should_flush = bytes_written_since_flush >= config.flush_threshold
                        || batch.len() >= config.flush_batch_threshold
                        || last_flush.elapsed() >= config.flush_interval;

                    if should_flush || batch.len() >= config.batch_size {
                        write_batch(
                            &mut batch,
                            &mut writer,
                            &stats,
                            &mut bytes_written_since_flush,
                        );
                    }

                    if should_flush {
                        flush_writer(
                            &mut writer,
                            &stats,
                            &mut last_flush,
                            &mut bytes_written_since_flush,
                        );
                    }
                }

                Ok(Flush) => {
                    write_batch(
                        &mut batch,
                        &mut writer,
                        &stats,
                        &mut bytes_written_since_flush,
                    );
                    flush_writer(
                        &mut writer,
                        &stats,
                        &mut last_flush,
                        &mut bytes_written_since_flush,
                    );
                }

                Ok(Shutdown) => {
                    write_batch(
                        &mut batch,
                        &mut writer,
                        &stats,
                        &mut bytes_written_since_flush,
                    );
                    flush_writer(
                        &mut writer,
                        &stats,
                        &mut last_flush,
                        &mut bytes_written_since_flush,
                    );

                    while let Ok(cmd) = rx.try_recv() {
                        if let LoggerCommand::Msg(log) = cmd {
                            TLS_FORMAT_BUF.with(|buf_cell| {
                                let mut buf = buf_cell.borrow_mut();
                                buf.clear();
                                let _ = write!(
                                    buf,
                                    "{} [{}] {}\n",
                                    Local::now().format("%Y-%m-%d %H:%M:%S"),
                                    log.level.as_str(),
                                    log.msg
                                );
                                let _ = writer.write_all(buf.as_bytes());
                            });
                            stats.messages_written.fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    let _ = writer.flush();
                    stats.flushes_performed.fetch_add(1, Ordering::Relaxed);
                    return;
                }

                Err(_) => {
                    write_batch(
                        &mut batch,
                        &mut writer,
                        &stats,
                        &mut bytes_written_since_flush,
                    );
                    let _ = writer.flush();
                    stats.flushes_performed.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }
    }

    #[inline(always)]
    pub fn log(&self, level: LogLevel, msg: CompactString) {
        if let Some(ref sender) = self.sender {
            let log_msg = LogMsg { level, msg };
            let _ = sender.try_send(LoggerCommand::Msg(log_msg));
        }
    }

    #[inline(always)]
    pub fn trace(&self, msg: CompactString) {
        self.log(LogLevel::Trace, msg);
    }

    #[inline(always)]
    pub fn debug(&self, msg: CompactString) {
        self.log(LogLevel::Debug, msg);
    }

    #[inline(always)]
    pub fn info(&self, msg: CompactString) {
        self.log(LogLevel::Info, msg);
    }

    #[inline(always)]
    pub fn warn(&self, msg: CompactString) {
        self.log(LogLevel::Warn, msg);
    }

    #[inline(always)]
    pub fn error(&self, msg: CompactString) {
        self.log(LogLevel::Error, msg);
    }

    pub fn stats(&self) -> (u64, u64) {
        (
            self.stats.messages_written.load(Ordering::Relaxed),
            self.stats.flushes_performed.load(Ordering::Relaxed),
        )
    }

    pub fn flush(&self) {
        if let Some(ref sender) = self.sender {
            let _ = sender.send(LoggerCommand::Flush);
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(LoggerCommand::Shutdown);
        }
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(feature = "global")]
pub fn flush_global() {
    unsafe {
        let ptr = &raw const GLOBAL_LOGGER;
        match *ptr {
            Some(ref logger) => logger.flush(),
            None => panic!("Logger has not been initialized"),
        }
    };
}

#[cfg(feature = "local")]
pub fn flush_local() {
    LOGGER.with(|logger| logger.borrow().as_ref().map(|l| l.flush()));
}

impl Drop for FLog {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
pub struct LoggerConfig {
    pub buffer_size: usize,
    pub batch_size: usize,
    pub flush_threshold: usize,
    pub flush_batch_threshold: usize,
    pub flush_interval: Duration,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            buffer_size: 64 * 1024,
            batch_size: 512,
            flush_threshold: 32 * 1024,
            flush_batch_threshold: 128,
            flush_interval: Duration::from_millis(100),
        }
    }
}

impl LoggerConfig {
    pub fn high_throughput() -> Self {
        Self {
            buffer_size: 256 * 1024,
            batch_size: 1024,
            flush_threshold: 128 * 1024,
            flush_batch_threshold: 256,
            flush_interval: Duration::from_millis(500),
        }
    }

    pub fn low_latency() -> Self {
        Self {
            buffer_size: 16 * 1024,
            batch_size: 64,
            flush_threshold: 8 * 1024,
            flush_batch_threshold: 32,
            flush_interval: Duration::from_millis(10),
        }
    }
}

#[cfg(feature = "local")]
thread_local! {
    static LOGGER: std::cell::RefCell<Option<FLog>> = std::cell::RefCell::new(None);
}

#[cfg(feature = "local")]
pub fn init_local() {
    init_local_with_config(LoggerConfig::default());
}

#[cfg(feature = "local")]
pub fn init_local_with_config(config: LoggerConfig) {
    LOGGER.with(|logger| {
        *logger.borrow_mut() = Some(FLog::with_config(config));
    });
}

#[cfg(feature = "global")]
pub struct GlobalLogHelper {}

#[cfg(feature = "global")]
impl Drop for GlobalLogHelper {
    fn drop(&mut self) {
        unsafe {
            let ptr = &raw const GLOBAL_LOGGER as *const Option<FLog> as *mut Option<FLog>;
            if let Some(mut logger) = (*ptr).take() {
                logger.shutdown();
            }
        }
    }
}

#[cfg(feature = "global")]
static mut GLOBAL_LOGGER: Option<FLog> = None;

#[cfg(feature = "global")]
pub fn init_global_with_config(config: LoggerConfig) {
    unsafe { GLOBAL_LOGGER = Some(FLog::with_config(config)) };
}

#[cfg(feature = "global")]
pub fn init_global() {
    init_global_with_config(LoggerConfig::default());
}

#[cfg(feature = "local")]
#[inline(always)]
pub fn local_log_with_level(level: LogLevel, msg: CompactString) {
    LOGGER.with(|logger| {
        if let Some(ref logger) = *logger.borrow() {
            logger.log(level, msg);
        }
    });
}

#[cfg(feature = "global")]
#[inline(always)]
pub fn global_log_with_level(level: LogLevel, msg: CompactString) {
    unsafe {
        let ptr = &raw const GLOBAL_LOGGER;
        match *ptr {
            Some(ref logger) => logger.log(level, msg),
            None => panic!("Logger has not been initialized"),
        }
    };
}

#[cfg(feature = "local")]
pub fn get_local_logger_stats() -> Option<(u64, u64)> {
    LOGGER.with(|logger| logger.borrow().as_ref().map(|l| l.stats()))
}

#[cfg(feature = "global")]
pub fn get_global_logger_stats() -> Option<(u64, u64)> {
    unsafe {
        let ptr = &raw const GLOBAL_LOGGER;
        match *ptr {
            Some(ref logger) => Some(logger.stats()),
            None => panic!("Logger has not been initialized"),
        }
    }
}

#[macro_export]
macro_rules! log {
    (g, $level:expr, $($arg:tt)*) => {{
        let msg = $crate::format_compact!($($arg)*);
        $crate::global_log_with_level($level, msg);
    }};
    (gf, $level:expr, $($arg:tt)*) => {{
        let msg = $crate::format_compact!($($arg)*);
        $crate::global_log_with_level($level, msg);
        flush_global();
    }};
    (l, $level:expr, $($arg:tt)*) => {{
        let msg = $crate::format_compact!($($arg)*);
        $crate::local_log_with_level($level, msg);
    }};
    (lf, $level:expr, $($arg:tt)*) => {{
        let msg = $crate::format_compact!($($arg)*);
        $crate::local_log_with_level($level, msg);
        flush_local();
    }};
}

#[macro_export]
macro_rules! info {
    (g, $($arg:tt)*) => {
        $crate::log!(g, $crate::LogLevel::Info, $($arg)*)
    };
    (gf, $($arg:tt)*) => {
        $crate::log!(gf, $crate::LogLevel::Info, $($arg)*)
    };
    (l, $($arg:tt)*) => {
        $crate::log!(l, $crate::LogLevel::Info, $($arg)*)
    };
    (lf, $($arg:tt)*) => {
        $crate::log!(lf, $crate::LogLevel::Info, $($arg)*)
    };
}

#[macro_export]
macro_rules! debug {
    (g, $($arg:tt)*) => {
        $crate::log!(g, $crate::LogLevel::Debug, $($arg)*)
    };
    (gf, $($arg:tt)*) => {
        $crate::log!(gf, $crate::LogLevel::Debug, $($arg)*)
    };
    (l, $($arg:tt)*) => {
        $crate::log!(l, $crate::LogLevel::Debug, $($arg)*)
    };
    (lf, $($arg:tt)*) => {
        $crate::log!(lf, $crate::LogLevel::Debug, $($arg)*)
    };
}

#[macro_export]
macro_rules! warn {
    (g, $($arg:tt)*) => {
        $crate::log!(g, $crate::LogLevel::Warn, $($arg)*)
    };
    (gf, $($arg:tt)*) => {
        $crate::log!(gf, $crate::LogLevel::Warn, $($arg)*)
    };
    (l, $($arg:tt)*) => {
        $crate::log!(l, $crate::LogLevel::Warn, $($arg)*)
    };
    (lf, $($arg:tt)*) => {
        $crate::log!(lf, $crate::LogLevel::Warn, $($arg)*)
    };
}

#[macro_export]
macro_rules! error {
    (g, $($arg:tt)*) => {
        $crate::log!(g, $crate::LogLevel::Error, $($arg)*)
    };
    (gf, $($arg:tt)*) => {
        $crate::log!(gf, $crate::LogLevel::Error, $($arg)*)
    };
    (l, $($arg:tt)*) => {
        $crate::log!(l, $crate::LogLevel::Error, $($arg)*)
    };
    (lf, $($arg:tt)*) => {
        $crate::log!(lf, $crate::LogLevel::Error, $($arg)*)
    };
}

#[macro_export]
macro_rules! trace {
    (g, $($arg:tt)*) => {
        $crate::log!(g, $crate::LogLevel::Trace, $($arg)*)
    };
    (gf, $($arg:tt)*) => {
        $crate::log!(gf, $crate::LogLevel::Trace, $($arg)*)
    };
    (l, $($arg:tt)*) => {
        $crate::log!(l, $crate::LogLevel::Trace, $($arg)*)
    };
    (lf, $($arg:tt)*) => {
        $crate::log!(lf, $crate::LogLevel::Trace, $($arg)*)
    };
}

#[macro_export]
macro_rules! init_local_flog {
    () => {
        $crate::init_local();
    };
    ($config:expr) => {
        $crate::init_local_with_config($config);
    };
}

#[macro_export]
macro_rules! init_global_flog {
    () => {
        $crate::init_global();
        let _tes = GlobalLogHelper {};
    };
    ($config:expr) => {
        $crate::init_global_with_config($config);
        let _tes = GlobalLogHelper {};
    };
}
