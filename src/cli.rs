use indicatif::{ProgressBar, ProgressStyle};
use std::{io, path::Path, time::Duration};

pub struct BuildProgress;

impl BuildProgress {
    pub fn new() -> Self {
        Self
    }

    pub fn step<T, F>(&self, message: &str, operation: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> anyhow::Result<T>,
    {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::with_template("  {spinner:.dim} {msg}")
                .expect("valid MeshScale progress template")
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏  "),
        );
        spinner.set_message(message.to_owned());
        spinner.enable_steady_tick(Duration::from_millis(80));

        match operation() {
            Ok(value) => {
                spinner.finish_with_message(format!("✓ {message}"));
                Ok(value)
            }
            Err(error) => {
                spinner.abandon_with_message(format!("✗ {message}"));
                Err(error)
            }
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        return format!("{millis}ms");
    }

    let seconds = millis as f64 / 1_000.0;
    if seconds < 60.0 {
        return if seconds >= 10.0 {
            format!("{seconds:.1}s")
        } else {
            format!("{seconds:.2}s")
        };
    }

    let minutes = (seconds / 60.0).floor() as u64;
    let remaining_seconds = seconds - minutes as f64 * 60.0;
    if minutes < 60 {
        return if remaining_seconds >= 10.0 {
            format!("{minutes}m {:.0}s", remaining_seconds)
        } else {
            format!("{minutes}m {:.1}s", remaining_seconds)
        };
    }

    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    format!("{hours}h {remaining_minutes}m {:.0}s", remaining_seconds)
}

pub fn output_size(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    if !path.exists() {
        return Ok(0);
    }

    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(150 * 1024 * 1024), "150 MB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_millis(420)), "420ms");
        assert_eq!(format_duration(Duration::from_millis(2_500)), "2.50s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 5s");
    }
}
