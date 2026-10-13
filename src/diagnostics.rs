use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn log(scope: &str, message: impl AsRef<str>) {
    let path = LOG_PATH.get_or_init(default_log_path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    rotate_if_needed(path);

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "[{}][{}][{:?}] {}",
            timestamp_ms,
            scope,
            std::thread::current().id(),
            message.as_ref()
        );
    }
}

fn default_log_path() -> PathBuf {
    if let Ok(path) = std::env::var("TERMLET_DIAGNOSTIC_LOG") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }

    config_dir().join("iced_term_diagnostics.log")
}

fn config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".termlet");
        }
    }

    std::env::temp_dir().join("termlet")
}

fn rotate_if_needed(path: &PathBuf) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };

    if metadata.len() <= MAX_LOG_BYTES {
        return;
    }

    let rotated = path.with_extension("log.1");
    let _ = fs::remove_file(&rotated);
    let _ = fs::rename(path, rotated);
}
