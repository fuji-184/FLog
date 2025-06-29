use chrono::{DateTime, Local};
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossbeam_queue::SegQueue;
use smallvec::SmallVec;
use std::fmt::Write as FmtWrite;
use std::io::{self, BufWriter, Write};
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
            LogLevel::Debug => "\x1b[34mDEBUG\x1b[0m",
            LogLevel::Info => "\x1b[32mINFO\x1b[0m",
            LogLevel::Warn => "	\x1b[33mWARN\x1b[0m",
            LogLevel::Error => "\x1b[31mERROR\x1b[0m",
        }
    }
}

struct LogMsg {
    level: LogLevel,
    msg: String,
}

enum LoggerCommand {
    Msg(LogMsg),
    Flush,
    Shutdown,
}

pub struct FLog {
    sender: Option<Sender<LoggerCommand>>,
    thread_handle: Option<JoinHandle<()>>,
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
        panic::set_hook(Box::new(|info| {
            #[cfg(feature = "global")]
            flush_global();

            #[cfg(feature = "local")]
            flush_local();

            let msg = match info.payload().downcast_ref::<&str>() {
                Some(isi) => isi,
                None => "No panic message was set",
            };

            if let Some(location) = info.location() {
                #[cfg(feature = "global")]
                error!(
                    g,
                    "\x1b[31mPanic\x1b[0m occurred in file \x1b[31m'{}'\x1b[0m at line \x1b[31m{}\x1b[0m, at character \x1b[31m{}\x1b[0m, message : {}",
                    location.file(),
                    location.line(),
                    location.column(),
                    msg
                );

                #[cfg(feature = "local")]
                error!(
                    l,
                    "\x1b[31mPanic\x1b[0m occurred in file \x1b[31m'{}'\x1b[0m at line \x1b[31m{}\x1b[0m, at character \x1b[31m{}\x1b[0m, message : {}",
                    location.file(),
                    location.line(),
                    location.column(),
                    msg
                );
            } else {
                #[cfg(feature = "global")]
                error!(
                    g,
                    "\x1b[31mPanic\x1b[0m occurred but location info is unavailable, message : {}",
                    msg
                );

                #[cfg(feature = "local")]
                error!(
                    l,
                    "\x1b[31mPanic\x1b[0m occurred but location info is unavailable, message : {}",
                    msg
                );
            }
        }));
        let (tx, rx) = unbounded::<LoggerCommand>();

        let pool = Arc::clone(&GLOBAL_STRING_POOL);

        let handle = thread::spawn(move || {
            writer_loop(rx, config, pool);
        });

        Self {
            sender: Some(tx),
            thread_handle: Some(handle),
        }
    }

    #[inline(always)]
    pub fn log(&self, level: LogLevel, msg: String) {
        if let Some(ref sender) = self.sender {
            let log_msg = LogMsg { level, msg };
            let _ = sender.try_send(LoggerCommand::Msg(log_msg));
        }
    }

    #[inline(always)]
    pub fn trace(&self, msg: String) {
        self.log(LogLevel::Trace, msg);
    }

    #[inline(always)]
    pub fn debug(&self, msg: String) {
        self.log(LogLevel::Debug, msg);
    }

    #[inline(always)]
    pub fn info(&self, msg: String) {
        self.log(LogLevel::Info, msg);
    }

    #[inline(always)]
    pub fn warn(&self, msg: String) {
        self.log(LogLevel::Warn, msg);
    }

    #[inline(always)]
    pub fn error(&self, msg: String) {
        self.log(LogLevel::Error, msg);
    }

    #[inline(always)]
    pub fn flush(&self) {
        if let Some(ref sender) = self.sender {
            let _ = sender.send(LoggerCommand::Flush);
        }
    }

    #[inline(always)]
    pub fn shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(LoggerCommand::Shutdown);
        }
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

pub struct StringPool {
    queue: SegQueue<String>,
    count: AtomicUsize,
}

impl StringPool {
    pub fn new() -> Self {
        let pool = Self {
            queue: SegQueue::new(),
            count: AtomicUsize::new(0),
        };

        for _ in 0..1000 {
            pool.queue.push(String::with_capacity(256));
            pool.count.fetch_add(1, Ordering::Relaxed);
        }

        pool
    }

    pub fn get(&self) -> String {
        match self.queue.pop() {
            Some(s) => {
                self.count.fetch_sub(1, Ordering::Relaxed);
                s
            }
            None => String::with_capacity(256),
        }
    }

    pub fn put(&self, mut s: String) {
        s.clear();
        if self.count.load(Ordering::Relaxed) < 1000 {
            self.queue.push(s);
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

lazy_static::lazy_static! {
    pub static ref GLOBAL_STRING_POOL: Arc<StringPool> = Arc::new(StringPool::new());
}

fn writer_loop(rx: Receiver<LoggerCommand>, config: LoggerConfig, pool: Arc<StringPool>) {
    use LoggerCommand::*;

    let stderr = io::stderr();
    let mut writer = BufWriter::with_capacity(config.buffer_size, stderr);
    let mut batch: SmallVec<[LogMsg; 512]> = SmallVec::with_capacity(config.batch_size);
    let mut last_flush = Instant::now();
    let mut bytes_written_since_flush = 0usize;
    let mut buf = String::new();
    let mut time = Local::now();
    let mut last_time_update = Instant::now();
    let mut ts = format!("{}", time.format("%Y-%m-%d %H:%M:%S"));

    #[inline]
    fn write_batch(
        batch: &mut SmallVec<[LogMsg; 512]>,
        writer: &mut BufWriter<io::Stderr>,
        bytes_written: &mut usize,
        buf: &mut String,
        time: &mut DateTime<Local>,
        last_time_update: &mut Instant,
        ts: &mut String,
        pool: &Arc<StringPool>,
    ) {
        if batch.is_empty() {
            return;
        }

        buf.clear();

        for log in batch.drain(..) {
            if last_time_update.elapsed() >= Duration::from_secs(1) {
                *time = Local::now();
                *last_time_update = Instant::now();
                ts.clear();
                _ = write!(ts, "{}", time.format("%Y-%m-%d %H:%M:%S"));
            }
            let _ = write!(buf, "{} [{}] {}\n", ts, log.level.as_str(), log.msg);
            pool.put(log.msg);
        }

        let _ = writer.write_all(buf.as_bytes());
        *bytes_written += buf.len();
    }

    #[inline]
    fn flush_writer(
        writer: &mut BufWriter<io::Stderr>,
        last_flush: &mut Instant,
        bytes_written: &mut usize,
    ) {
        let _ = writer.flush();
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
                                &mut bytes_written_since_flush,
                                &mut buf,
                                &mut time,
                                &mut last_time_update,
                                &mut ts,
                                &pool,
                            );
                            flush_writer(
                                &mut writer,
                                &mut last_flush,
                                &mut bytes_written_since_flush,
                            );
                        }
                        Ok(Shutdown) => {
                            write_batch(
                                &mut batch,
                                &mut writer,
                                &mut bytes_written_since_flush,
                                &mut buf,
                                &mut time,
                                &mut last_time_update,
                                &mut ts,
                                &pool,
                            );
                            flush_writer(
                                &mut writer,
                                &mut last_flush,
                                &mut bytes_written_since_flush,
                            );

                            buf.clear();

                            while let Ok(cmd) = rx.try_recv() {
                                if let LoggerCommand::Msg(log) = cmd {
                                    if last_time_update.elapsed() >= Duration::from_secs(1) {
                                        time = Local::now();
                                        last_time_update = Instant::now();
                                        ts.clear();
                                        _ = write!(&mut ts, "{}", time.format("%Y-%m-%d %H:%M:%S"));
                                    }
                                    let _ = write!(
                                        buf,
                                        "{} [{}] {}\n",
                                        ts,
                                        log.level.as_str(),
                                        log.msg
                                    );
                                    pool.put(log.msg);
                                }
                            }
                            let _ = writer.write_all(buf.as_bytes());

                            let _ = writer.flush();
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
                        &mut bytes_written_since_flush,
                        &mut buf,
                        &mut time,
                        &mut last_time_update,
                        &mut ts,
                        &pool,
                    );
                }

                if should_flush {
                    flush_writer(&mut writer, &mut last_flush, &mut bytes_written_since_flush);
                }
            }

            Ok(Flush) => {
                write_batch(
                    &mut batch,
                    &mut writer,
                    &mut bytes_written_since_flush,
                    &mut buf,
                    &mut time,
                    &mut last_time_update,
                    &mut ts,
                    &pool,
                );
                flush_writer(&mut writer, &mut last_flush, &mut bytes_written_since_flush);
            }

            Ok(Shutdown) => {
                write_batch(
                    &mut batch,
                    &mut writer,
                    &mut bytes_written_since_flush,
                    &mut buf,
                    &mut time,
                    &mut last_time_update,
                    &mut ts,
                    &pool,
                );
                flush_writer(&mut writer, &mut last_flush, &mut bytes_written_since_flush);

                buf.clear();
                while let Ok(cmd) = rx.try_recv() {
                    if let LoggerCommand::Msg(log) = cmd {
                        if last_time_update.elapsed() >= Duration::from_secs(1) {
                            time = Local::now();
                            last_time_update = Instant::now();
                            ts.clear();
                            _ = write!(&mut ts, "{}", time.format("%Y-%m-%d %H:%M:%S"));
                        }
                        let _ = write!(buf, "{} [{}] {}\n", ts, log.level.as_str(), log.msg);
                        pool.put(log.msg);
                    }
                }

                let _ = writer.write_all(buf.as_bytes());

                let _ = writer.flush();
                return;
            }

            Err(_) => {
                write_batch(
                    &mut batch,
                    &mut writer,
                    &mut bytes_written_since_flush,
                    &mut buf,
                    &mut time,
                    &mut last_time_update,
                    &mut ts,
                    &pool,
                );
                let _ = writer.flush();
                return;
            }
        }
    }
}

#[cfg(feature = "global")]
#[inline(always)]
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
#[inline(always)]
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
#[inline(always)]
pub fn init_local() {
    init_local_with_config(LoggerConfig::default());
}

#[cfg(feature = "local")]
#[inline(always)]
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
#[inline(always)]
pub fn init_global_with_config(config: LoggerConfig) {
    unsafe { GLOBAL_LOGGER = Some(FLog::with_config(config)) };
}

#[cfg(feature = "global")]
#[inline(always)]
pub fn init_global() {
    init_global_with_config(LoggerConfig::default());
}

#[cfg(feature = "local")]
#[inline(always)]
pub fn local_log_with_level(level: LogLevel, msg: String) {
    LOGGER.with(|logger| {
        if let Some(ref logger) = *logger.borrow() {
            logger.log(level, msg);
        }
    });
}

#[cfg(feature = "global")]
#[inline(always)]
pub fn global_log_with_level(level: LogLevel, msg: String) {
    unsafe {
        let ptr = &raw const GLOBAL_LOGGER;
        match *ptr {
            Some(ref logger) => logger.log(level, msg),
            None => panic!("Logger has not been initialized"),
        }
    };
}

#[macro_export]
macro_rules! log {
    (g, $level:expr, $($arg:tt)*) => {{
        let mut msg = $crate::GLOBAL_STRING_POOL.get();
        let _ = ::std::fmt::Write::write_fmt(&mut msg, format_args!($($arg)*));
        $crate::global_log_with_level($level, msg);
    }};
    (gf, $level:expr, $($arg:tt)*) => {{
        let mut msg = $crate::GLOBAL_STRING_POOL.get();
        let _ = ::std::fmt::Write::write_fmt(&mut msg, format_args!($($arg)*));
        $crate::global_log_with_level($level, msg);
        $crate::flush_global();
    }};
    (l, $level:expr, $($arg:tt)*) => {{
        let mut msg = $crate::GLOBAL_STRING_POOL.get();
        let _ = ::std::fmt::Write::write_fmt(&mut msg, format_args!($($arg)*));
        $crate::local_log_with_level($level, msg);
    }};
    (lf, $level:expr, $($arg:tt)*) => {{
        let mut msg = $crate::GLOBAL_STRING_POOL.get();
        let _ = ::std::fmt::Write::write_fmt(&mut msg, format_args!($($arg)*));
        $crate::local_log_with_level($level, msg);
        $crate::flush_local();
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
