//! Log file parsing and statistics.
//!
//! Supported line format:
//!   `2026-10-25 09:00:01 INFO Application started`
//!    ^^^^^^^^^^ ^^^^^^^^ ^^^^ ^^^^^^^^^^^^^^^^^
//!    date       time     level message
//!
//! Design rule (why this module exists): a level is identified by the
//! level *field* only — the third whitespace-separated token. The original
//! version counted substring matches (`text.matches("ERROR")`), which
//! also counted levels merely mentioned inside a message, e.g.
//! `INFO User received an ERROR notification` was counted as an ERROR.
//! Parsing by field avoids that false positive.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Severity of a log entry, ordered from least to most severe.
///
/// The variants are declared in severity order so the derived
/// `PartialOrd`/`Ord` reflects severity: `lvl >= Level::Error`
/// means "ERROR or worse" (see `compute_stats`).
/// `Copy` is derived because a level is a tiny value passed around
/// by value (e.g. `e.level.map(...)` in `main.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warning,
    Error,
    Fatal,
}

impl Level {
    /// Recognise a level token, accepting common aliases (`WARN`, `ERR`, …)
    /// and any casing. Returns `None` for tokens that are not levels,
    /// which is how the caller tells "no level field" apart from a level.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "TRACE" => Some(Level::Trace),
            "DEBUG" => Some(Level::Debug),
            "INFO" | "INFORMATION" => Some(Level::Info),
            "WARN" | "WARNING" => Some(Level::Warning),
            "ERROR" | "ERR" => Some(Level::Error),
            "FATAL" | "CRITICAL" | "CRIT" => Some(Level::Fatal),
            _ => None,
        }
    }

    /// Canonical display name. Aliases are normalised here, so `WARN`
    /// in a file is always reported as `WARNING`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
            Level::Fatal => "FATAL",
        }
    }

    /// Dashboard colour for this level (GitHub-dark palette).
    pub fn color(&self) -> &'static str {
        match self {
            Level::Trace => "#8b949e",
            Level::Debug => "#a371f7",
            Level::Info => "#58a6ff",
            Level::Warning => "#d29922",
            Level::Error => "#f85149",
            Level::Fatal => "#ff2d55",
        }
    }
}

/// `Display` delegates to `as_str`, so `format!("{lvl}")` prints `INFO`, etc.
impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One parsed line of a log file.
///
/// `level`/`timestamp` are `Option` because not every line is a
/// well-formed log entry (see the fallbacks in `parse`). `raw` always
/// holds the untouched line so nothing is lost and search can match
/// against the original text.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// 1-based line number in the file, for pointing back at the source.
    pub line_no: usize,
    /// `"2026-10-25 09:00:01"` when the line starts with a timestamp.
    pub timestamp: Option<String>,
    /// Severity, or `None` when the line has no recognisable level field.
    pub level: Option<Level>,
    /// Text after the timestamp/level prefix (or the whole line if unparsed).
    pub message: String,
    /// The full original line, unmodified (except trailing newline).
    pub raw: String,
}

impl LogEntry {
    /// Parse one line. Never fails: any line yields an entry, with
    /// `level: None` if it does not look like a log line.
    ///
    /// Parsing order:
    /// 1. `DATE TIME LEVEL message` — the expected format.
    /// 2. `LEVEL message` — fallback for files without timestamps.
    /// 3. Anything else — kept verbatim as an unparsed entry.
    pub fn parse(line_no: usize, raw: &str) -> Self {
        // `str::lines()` already strips `\n`, but strip `\r` too so
        // Windows-generated files do not leave a stray carriage return.
        let raw = raw.trim_end_matches(['\r', '\n']).to_string();
        let trimmed = raw.trim();

        // --- 1. Expected shape: `2026-10-25 09:00:01 INFO message...` ---
        // The level is the *third token only*. Counting substring matches
        // (the old behaviour) also counted levels mentioned inside the
        // message, e.g. `INFO User received an ERROR notification`.
        let mut iter = trimmed.split_whitespace();
        let date = iter.next();
        let time = iter.next();
        let level_tok = iter.next();

        // A cheap shape check (length + separator positions) instead of a
        // full date parser: good enough to decide "is this a timestamp?",
        // and it cannot fail on unusual-but-valid dates.
        let looks_timestamped = date
            .map(|d| d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-')
            .unwrap_or(false)
            && time.map(|t| t.contains(':')).unwrap_or(false);

        if looks_timestamped {
            if let Some(lvl) = level_tok.and_then(Level::parse) {
                // `splitn(4, …)` splits only at the first three whitespace
                // runs, so the message keeps its own internal spacing.
                let message = trimmed
                    .splitn(4, char::is_whitespace)
                    .nth(3)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                return LogEntry {
                    line_no,
                    timestamp: Some(format!("{} {}", date.unwrap(), time.unwrap())),
                    level: Some(lvl),
                    message,
                    raw,
                };
            }
        }

        // --- 2. Fallback: first token is the level, e.g. `INFO hello`. ---
        if let Some(lvl) = trimmed.split_whitespace().next().and_then(Level::parse) {
            let message = trimmed
                .splitn(2, char::is_whitespace)
                .nth(1)
                .unwrap_or("")
                .trim()
                .to_string();
            return LogEntry {
                line_no,
                timestamp: None,
                level: Some(lvl),
                message,
                raw,
            };
        }

        // --- 3. Not a log line: keep it, marked unparsed. ---
        // Dropping noise lines would make line numbers drift from the
        // file, so they are preserved and reported separately.
        LogEntry {
            line_no,
            timestamp: None,
            level: None,
            message: trimmed.to_string(),
            raw,
        }
    }

    /// Whether this entry passes the CLI filters.
    /// `level`: only entries with exactly this level pass.
    /// `search`: case-insensitive substring match against the raw line.
    /// `None` for either means "no filtering on that dimension".
    pub fn matches(&self, level: Option<&Level>, search: Option<&str>) -> bool {
        if let Some(want) = level {
            if self.level.as_ref() != Some(want) {
                return false;
            }
        }
        if let Some(q) = search {
            // Lower-casing both sides gives a case-insensitive search
            // without pulling in a regex crate.
            let q = q.to_ascii_lowercase();
            if !self.raw.to_ascii_lowercase().contains(&q) {
                return false;
            }
        }
        true
    }
}

/// Aggregated counts over a whole file, used by the CLI summary,
/// the `--json` output, and the dashboard cards.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Total lines read, including unparsed ones.
    pub total: usize,
    /// Lines that carried a recognisable level.
    pub parsed: usize,
    /// Lines with no recognisable level.
    pub unparsed: usize,
    /// Count per canonical level name (`"INFO" -> 7`, …).
    /// `BTreeMap` keeps the keys sorted for stable output.
    pub by_level: BTreeMap<String, usize>,
    /// Timestamp of the first timestamped entry, if any.
    pub first_ts: Option<String>,
    /// Timestamp of the last timestamped entry, if any.
    pub last_ts: Option<String>,
    /// All ERROR/FATAL entries, in file order (for "recent errors").
    pub errors: Vec<LogEntry>,
}

impl Stats {
    /// Count for one level, 0 when that level never appears.
    pub fn get(&self, level: &Level) -> usize {
        *self.by_level.get(level.as_str()).unwrap_or(&0)
    }
}

/// Parse every line of an in-memory log text.
pub fn parse_log_text(text: &str) -> Vec<LogEntry> {
    text.lines()
        .enumerate() // 0-based index…
        .map(|(i, line)| LogEntry::parse(i + 1, line)) // …reported 1-based
        .collect()
}

/// Read a log file from disk and parse it. I/O errors are returned
/// to the caller, which decides how to report them (see `main`).
pub fn read_log_file(path: &Path) -> std::io::Result<Vec<LogEntry>> {
    let text = fs::read_to_string(path)?;
    Ok(parse_log_text(&text))
}

/// Single pass over all entries computing every aggregate at once.
pub fn compute_stats(entries: &[LogEntry]) -> Stats {
    let mut stats = Stats {
        total: entries.len(),
        ..Default::default()
    };
    for e in entries {
        match &e.level {
            Some(lvl) => {
                stats.parsed += 1;
                // `entry(...).or_insert(0)` is the standard "count in a
                // map" idiom: insert 0 on first sight, then increment.
                *stats.by_level.entry(lvl.as_str().to_string()).or_insert(0) += 1;
                // Severity ordering: ERROR and FATAL both count as errors.
                if lvl >= &Level::Error {
                    stats.errors.push(e.clone());
                }
            }
            None => stats.unparsed += 1,
        }
        if let Some(ts) = &e.timestamp {
            // Lines are in file order, so the first timestamp seen is the
            // start of the range and every later one moves the end forward.
            if stats.first_ts.is_none() {
                stats.first_ts = Some(ts.clone());
            }
            stats.last_ts = Some(ts.clone());
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression test for the original substring-counting bug:
    // an ERROR mentioned inside an INFO message must not count as ERROR.
    #[test]
    fn counts_level_field_not_message_mentions() {
        let entries = parse_log_text(
            "2026-10-25 09:00:09 INFO User received an ERROR notification\n\
             2026-10-25 09:00:04 ERROR Failed\n",
        );
        let stats = compute_stats(&entries);
        assert_eq!(stats.get(&Level::Info), 1);
        assert_eq!(stats.get(&Level::Error), 1);
    }

    // `WARN` is an alias, not a separate level: it normalises to WARNING.
    #[test]
    fn warn_alias_normalizes_to_warning() {
        let e = LogEntry::parse(1, "2026-10-25 09:00:03 WARN disk at 80%");
        assert_eq!(e.level, Some(Level::Warning));
        assert_eq!(e.message, "disk at 80%");
    }

    // Noise lines are kept (as unparsed) instead of being dropped,
    // so line numbers still match the file.
    #[test]
    fn unparsed_lines_are_kept() {
        let e = LogEntry::parse(1, "just some noise");
        assert_eq!(e.level, None);
        assert!(!e.message.is_empty());
    }
}
