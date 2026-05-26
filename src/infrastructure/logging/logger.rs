use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use chrono::Utc;

use super::log_metadata::{LogLevel, LogMetadata};

/// JSONL batched logger. Writes one JSON object per line to
/// `logs/migration-YYYY-MM-DD.jsonl`. Thread-safe (Mutex-guarded buffer).
pub struct Logger {
    inner: Mutex<Inner>,
}

struct Inner {
    logs_dir: PathBuf,
    batch_size: usize,
    min_level: LogLevel,
    buffer: Vec<LogMetadata>,
}

impl Logger {
    pub fn new(logs_dir: impl Into<PathBuf>, batch_size: usize) -> Self {
        Self::with_level(logs_dir, batch_size, LogLevel::Debug)
    }

    pub fn with_level(logs_dir: impl Into<PathBuf>, batch_size: usize, min_level: LogLevel) -> Self {
        let logs_dir = logs_dir.into();
        let _ = fs::create_dir_all(&logs_dir);
        Self {
            inner: Mutex::new(Inner {
                logs_dir,
                batch_size,
                min_level,
                buffer: Vec::new(),
            }),
        }
    }

    fn current_log_path(dir: &PathBuf) -> PathBuf {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        dir.join(format!("migration-{}.jsonl", date))
    }

    pub fn log(&self, mut meta: LogMetadata) {
        let lvl = meta.level.unwrap_or(LogLevel::Info);
        let min = self.inner.lock().unwrap().min_level;
        if lvl < min {
            return;
        }
        meta.ensure_timestamp();

        // Also mirror to tracing for stderr visibility.
        match lvl {
            LogLevel::Debug => tracing::debug!(level = lvl.as_str(), json = %meta.to_json()),
            LogLevel::Info => tracing::info!(level = lvl.as_str(), json = %meta.to_json()),
            LogLevel::Warn => tracing::warn!(level = lvl.as_str(), json = %meta.to_json()),
            LogLevel::Error | LogLevel::Fatal => tracing::error!(level = lvl.as_str(), json = %meta.to_json()),
        }

        let mut inner = self.inner.lock().unwrap();
        inner.buffer.push(meta);
        let should_flush = inner.buffer.len() >= inner.batch_size;
        drop(inner);
        if should_flush {
            let _ = self.flush();
        }
    }

    pub fn flush(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if inner.buffer.is_empty() {
            return Ok(());
        }
        let path = Self::current_log_path(&inner.logs_dir);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        for entry in inner.buffer.drain(..) {
            let line = entry.to_json().to_string();
            writeln!(file, "{}", line)?;
        }
        Ok(())
    }

    pub fn info(&self, component: &str, message: &str) {
        self.log(LogMetadata::new(LogLevel::Info, component).with_message(message));
    }
    pub fn warn(&self, component: &str, message: &str) {
        self.log(LogMetadata::new(LogLevel::Warn, component).with_message(message));
    }
    pub fn error(&self, component: &str, message: &str) {
        self.log(LogMetadata::new(LogLevel::Error, component).with_message(message));
    }
    pub fn fatal(&self, component: &str, message: &str) {
        self.log(LogMetadata::new(LogLevel::Fatal, component).with_message(message));
    }
    pub fn debug(&self, component: &str, message: &str) {
        self.log(LogMetadata::new(LogLevel::Debug, component).with_message(message));
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
