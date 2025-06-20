# f_log

**F Log** is a simple, fast, and efficient Rust logger

## Features

- Logging with background flush
- Configurable logger parameters for latency and throughput
- Convenient logging macros (`trace!`, `debug!`, `info!`, `warn!`, `error!`)
- Customizable logger configuration through `LoggerConfig`
- Automatically flushes logs on panic

## Getting Started

Add `f_log` to your `Cargo.toml` dependencies:

```toml
[dependencies]
f_log = { version = "0.1", features = ["global"] }
```

## Basic Usage

Initialize the logger with your preferred configuration and use the logging macros as shown below:

```rust
use std::time::Instant;
use f_log::{LoggerConfig, GlobalLogHelper, init_global_flog, info};

fn main() {
    // Initialize global flog with default config
    // Only initialize global log once in main without being wrapped in function or scope
    init_global_flog!(LoggerConfig::default());

    let iterations = 20;
    let start = Instant::now();

    for i in 0..iterations {
        info!(g, "Benchmarking log perf: {}", i + 1);
    }

    let elapsed = start.elapsed();
    let per_log_ns = elapsed.as_nanos() / iterations as u128;

    println!(
        "Macro logger: Logged {} entries in {:?} ({} ns/log)",
        iterations, elapsed, per_log_ns
    );

    /*
        If there is a function that blocks indefinitely, such as one that starts a server
        you should manually flush the logs before calling that function if you want to see the logs immediately. For example:

        fn main() {
            // Initialize flog and perform some logging

            flush_global(); // or info!(gf, "Flushed: gf will flush all batched logs");
            server.run("0.0.0.0:8080");
        }

        Logs will still be flushed eventually, even if you don't flush manually and keep writing more logs,
        such as inside an HTTP handler, they will be flushed automatically when
        the accumulated logs reach certain thresholds defined in the configuration
    */


    // Logs are always automatically flushed before the main function exits.
}
```

## Logger Configuration

You can configure `f_log` using the `LoggerConfig` struct:

- `LoggerConfig::default()` – Balanced defaults
- `LoggerConfig::low_latency()` – Prioritizes low latency with smaller buffers and frequent flushes
- `LoggerConfig::high_throughput()` – Prioritizes throughput with larger buffers and batching

Example:

```rust
use f_log::{LoggerConfig, GlobalLogHelper, init_global_flog, flush_global};

fn main() {
    let config = LoggerConfig::low_latency();
    init_global_flog!(config);

    // Logging code here
}
```

## Logging Scope: Global vs Local

`f_log` supports two logging scopes:

- **Global logger** — Shared across all threads and modules. Activate it with "global" feature
- **Local logger** — Thread-local logger, independent for each thread. Activate it with "local" feature

Each log macro takes a selector as its first argument:

- **g** for global logger (batched and flushed automatically)
- **gf** for global logger and force flush all the current global logs

- **l** for local (thread-local) logger (batched and flushed automatically)
- **lf** for local (thread-local) logger and force flush all the current local logs

Choose based on your use case. Here are examples using threads.

---

## Example: Global Logger with Threads

```rust
use std::thread;
use f_log::{LoggerConfig, GlobalLogHelper, init_global_flog, flush_global, info};

fn main() {
    // Initialize global logger (shared across all threads)
    init_global_flog!(LoggerConfig::high_throughput());

    let handles: Vec<_> = (0..3).map(|i| {
        thread::spawn(move || {
            for j in 0..5 {
                info!(g, "Global thread {} - iteration {}", i, j);
            }
        })
    }).collect();

    for handle in handles {
        handle.join().unwrap();
    }
}
```

---

## Example: Local Logger with Threads

```rust
use std::thread;
use f_log::{LoggerConfig, init_local_flog, flush_local, warn};

fn main() {
    let handles: Vec<_> = (0..3).map(|i| {
        thread::spawn(move || {
            // Initialize local logger inside each thread
            init_local_flog!(LoggerConfig::low_latency());

            for j in 0..5 {
                warn!(l, "Local thread {} - warning number {}", i, j);
            }
        })
    }).collect();

    for handle in handles {
        handle.join().unwrap();
    }
}
```

---

## License

Licensed under MIT OR Apache-2.0
