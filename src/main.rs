//! Log_reader — parse a log file and show a dashboard.
//!
//! This file is only the CLI shell: it turns command-line arguments into
//! an `Args` struct, hands the parsed entries to the report generators
//! (terminal summary, `--json`, `--dashboard`, `--serve`), and prints the
//! result. All parsing logic lives in `log.rs`; all HTML lives in
//! `dashboard.rs`; the HTTP server lives in `server.rs`.
//!
//! Usage:
//!   cargo run -- [LOG_FILE] [OPTIONS]
//!
//! Options:
//!   --level <LEVEL>   Only show this level (INFO, WARNING, ERROR, …)
//!   --search <TEXT>   Only show lines containing TEXT
//!   --tail <N>        Only show the last N matching lines
//!   --json            Print stats as JSON
//!   --dashboard [OUT] Generate a standalone HTML dashboard (default: dashboard.html)
//!   --serve [PORT]    Serve the dashboard over HTTP (default port: 3000)

// `mod` (not `extern crate`) because this is a single-binary crate:
// each file becomes a module of the binary, no library target needed.
mod dashboard;
mod log;
mod server;

use log::{compute_stats, Level};
use std::path::PathBuf;

/// The `--help` text, kept as one constant so it is reused for both
/// `-h` output and error messages (see `main`), so the two never drift.
const HELP: &str = "\
Log_reader — parse a log file and show a dashboard

Usage: cargo run -- [LOG_FILE] [OPTIONS]

Arguments:
  [LOG_FILE]          Log file to read (default: data/info.log)

Options:
  --level <LEVEL>     Only show this level (INFO, WARNING, ERROR, ...)
  --search <TEXT>     Only show lines containing TEXT
  --tail <N>          Only show the last N matching lines
  --json              Print stats as JSON
  --dashboard [OUT]   Generate a standalone HTML dashboard (default: dashboard.html)
  --serve [PORT]      Serve the dashboard over HTTP (default port: 3000)
  -h, --help          Show this help
";

/// Everything the command line can express, in one struct.
/// `Option` fields mean "not requested"; `dashboard`/`serve` carry
/// their own defaults (path / port) so `--dashboard` alone is enough.
struct Args {
    file: PathBuf,
    level: Option<Level>,
    search: Option<String>,
    tail: Option<usize>,
    json: bool,
    dashboard: Option<PathBuf>,
    serve: Option<u16>,
}

/// Hand-rolled argument parsing (no `clap` dependency, on purpose:
/// the whole project builds with zero crates).
///
/// Returns `Err(message)` for anything malformed (missing value,
/// unknown level, unknown `--option`) so `main` can print the
/// message plus the help text and exit with code 2.
fn parse_args() -> Result<Args, String> {
    // Defaults first; flags below overwrite only what they mention.
    let mut args = Args {
        file: PathBuf::from("data/info.log"),
        level: None,
        search: None,
        tail: None,
        json: false,
        dashboard: None,
        serve: None,
    };
    // `skip(1)` drops the program name; `peekable()` lets the optional
    // value flags (`--dashboard`, `--serve`) look ahead before deciding
    // whether the next token is their value or another flag.
    let mut iter = std::env::args().skip(1).peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--level" => {
                let v = iter.next().ok_or("--level needs a value")?;
                args.level = Some(
                    Level::parse(&v).ok_or_else(|| format!("unknown level: {v}"))?,
                );
            }
            "--search" => args.search = Some(iter.next().ok_or("--search needs a value")?),
            "--tail" => {
                let v = iter.next().ok_or("--tail needs a value")?;
                args.tail = Some(v.parse().map_err(|_| format!("invalid --tail: {v}"))?);
            }
            "--json" => args.json = true,
            // Optional-value flag: `--dashboard out.html` uses `out.html`,
            // bare `--dashboard` falls back to `dashboard.html`. The peek
            // check stops `--dashboard --json` from swallowing `--json`.
            "--dashboard" => {
                let out = match iter.peek() {
                    Some(next) if !next.starts_with("--") => iter.next().unwrap(),
                    _ => "dashboard.html".to_string(),
                };
                args.dashboard = Some(PathBuf::from(out));
            }
            // Same optional-value pattern, defaulting to port 3000.
            "--serve" => {
                let port = match iter.peek() {
                    Some(next) if !next.starts_with("--") => {
                        iter.next()
                            .unwrap()
                            .parse::<u16>()
                            .map_err(|_| "invalid --serve port".to_string())?
                    }
                    _ => 3000,
                };
                args.serve = Some(port);
            }
            // Unknown `--flag` is an error (typos should not be silently
            // ignored); anything else is treated as the log file path,
            // so the file argument can appear in any position.
            other if other.starts_with("--") => return Err(format!("unknown option: {other}")),
            other => args.file = PathBuf::from(other),
        }
    }
    Ok(args)
}

/// Render an `Option<String>` as a JSON string-or-null, for `--json` output.
fn json_opt(v: &Option<String>) -> String {
    match v {
        Some(s) => format!("{:?}", s),
        None => "null".to_string(),
    }
}

fn main() {
    // --- 1. Parse arguments. Exit codes: 2 = bad usage, 1 = I/O or
    // server failure, 0 = success (conventional Unix distinction). ---
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    // --- 2. Read + parse the log file. ---
    let entries = match log::read_log_file(&args.file) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", args.file.display());
            std::process::exit(1);
        }
    };

    // --- 3. `--serve` takes over the process: the server re-reads the
    // file on every request, so the terminal summary is skipped. ---
    if let Some(port) = args.serve {
        if let Err(e) = server::serve(args.file.clone(), port) {
            eprintln!("error: server failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    // --- 4. `--dashboard` writes a standalone HTML file. This does not
    // stop the terminal summary below — both are produced. ---
    if let Some(out) = &args.dashboard {
        // Show only the file name in the dashboard header, not a full
        // local path (nicer, and avoids leaking directory structure).
        let file_name = args
            .file
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| args.file.to_string_lossy().into_owned());
        let html = dashboard::render_dashboard(&file_name, &entries);
        if let Err(e) = std::fs::write(out, &html) {
            eprintln!("error: cannot write {}: {e}", out.display());
            std::process::exit(1);
        }
        println!("Dashboard written to {} (open it in a browser)", out.display());
    }

    // --- 5. Apply the level/search filters, then the tail window. ---
    let stats = compute_stats(&entries);
    let filtered: Vec<_> = entries
        .iter()
        .filter(|e| e.matches(args.level.as_ref(), args.search.as_deref()))
        .collect();
    // `shown` is a slice of the filtered list — no copying of entries.
    // When `--tail N` exceeds the match count, the guard falls through
    // and everything is shown (tail is a maximum, not a requirement).
    let shown: &[&log::LogEntry] = match args.tail {
        Some(n) if n < filtered.len() => &filtered[filtered.len() - n..],
        _ => &filtered,
    };

    // --- 6. `--json` prints machine-readable stats and exits early,
    // instead of the human-oriented text summary below. ---
    // Built by hand (string concatenation) to keep the zero-dependency
    // promise; `{:?}` on a string produces a JSON-escaped double-quoted
    // string, which is exactly what JSON needs here.
    if args.json {
        let mut out = String::from("{");
        out.push_str(&format!("\"file\":{:?},", args.file.to_string_lossy()));
        out.push_str(&format!("\"total\":{},", stats.total));
        out.push_str(&format!("\"parsed\":{},", stats.parsed));
        out.push_str(&format!("\"unparsed\":{},", stats.unparsed));
        out.push_str("\"levels\":{");
        let mut first = true;
        for (k, v) in &stats.by_level {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&format!("{:?}:{}", k, v));
        }
        out.push_str("},");
        out.push_str(&format!(
            "\"first\":{},\"last\":{},",
            json_opt(&stats.first_ts),
            json_opt(&stats.last_ts)
        ));
        out.push_str(&format!("\"shown\":{}", shown.len()));
        out.push('}');
        println!("{out}");
        return;
    }

    // --- 7. Default: human-readable terminal summary. ---
    println!("File    : {}", args.file.display());
    println!("Total   : {} ({} parsed, {} unparsed)", stats.total, stats.parsed, stats.unparsed);
    // Print levels in severity order, skipping absent ones so the
    // summary stays short for small files.
    for lvl in [Level::Trace, Level::Debug, Level::Info, Level::Warning, Level::Error, Level::Fatal] {
        let n = stats.get(&lvl);
        if n > 0 {
            println!("  {:<7}: {}", lvl, n);
        }
    }
    if let (Some(first), Some(last)) = (&stats.first_ts, &stats.last_ts) {
        println!("Range   : {first} → {last}");
    }
    println!();
    // `shown.len()/entries.len()` shows how the filters narrowed the list.
    println!("Entries ({}/{})", shown.len(), entries.len());
    for e in shown {
        println!("  {:>4}  {:<19} {:<7} {}", e.line_no, e.timestamp.as_deref().unwrap_or("-"), e.level.map(|l| l.to_string()).unwrap_or_else(|| "-".into()), e.message);
    }
}
