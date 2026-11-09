//! Tiny std-only HTTP server: dashboard at `/`, JSON at `/api/*`.
//!
//! No web framework — just `std::net::TcpListener`. That is enough for a
//! local, single-user dashboard and keeps the zero-dependency promise.
//! Requests are handled one at a time (blocking accept loop); that is a
//! deliberate simplification, acceptable for a personal tool but not for
//! a public service.
//!
//! Routes:
//!   GET /                          dashboard HTML (re-reads the log file)
//!   GET /api/stats                 aggregate counts as JSON
//!   GET /api/entries?level=ERROR&q=text
//!                                  filtered entries as JSON
//!   GET /health                    `ok` (liveness check)
//!   anything else                  404

use crate::dashboard::render_dashboard;
use crate::log::{compute_stats, Level, LogEntry};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

/// JSON string escaping — same rules as `dashboard::json_str`
/// (`"`, `\`, control characters). Duplicated rather than shared so
/// this module stays independent of the dashboard module's internals.
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `GET /api/stats` body: one JSON object with the aggregate counts.
/// `by_level` is a BTreeMap, so level keys come out in sorted order.
fn stats_json(entries: &[LogEntry]) -> String {
    let s = compute_stats(entries);
    let mut levels = String::new();
    for (k, v) in &s.by_level {
        if !levels.is_empty() {
            levels.push(',');
        }
        levels.push_str(&format!("{}:{}", json_str(k), v));
    }
    format!(
        "{{\"total\":{},\"parsed\":{},\"unparsed\":{},\"levels\":{{{}}},\"first\":{},\"last\":{}}}",
        s.total,
        s.parsed,
        s.unparsed,
        levels,
        s.first_ts.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
        s.last_ts.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
    )
}

/// `GET /api/entries` body: a JSON array with the same field names and
/// the same level/search filtering as the CLI (via `LogEntry::matches`).
fn entries_json(entries: &[LogEntry], level: Option<&Level>, q: Option<&str>) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for e in entries.iter().filter(|e| e.matches(level, q)) {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&format!(
            "{{\"n\":{},\"ts\":{},\"lvl\":{},\"msg\":{}}}",
            e.line_no,
            e.timestamp.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
            e.level.as_ref().map(|l| json_str(l.as_str())).unwrap_or_else(|| "null".into()),
            json_str(&e.message),
        ));
    }
    out.push(']');
    out
}

/// Extract one query parameter (`?level=ERROR&q=x` → `level` → `"ERROR"`).
/// Only the two encodings a browser search box produces are decoded
/// (`+` and `%20` for spaces); full percent-decoding is unnecessary here
/// and would need more code than the feature justifies.
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            Some(
                v.replace('+', " ")
                    .replace("%20", " ")
                    .to_string(),
            )
        } else {
            None
        }
    })
}

/// Write a complete HTTP/1.1 response: status line, headers, blank line,
/// body. `Connection: close` because we handle one request per socket —
/// the client (browser/curl) opens a fresh connection per request.
fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// Start the server on 127.0.0.1:`port` and serve until killed (Ctrl-C).
/// Blocks forever in the accept loop; returns only on a bind failure.
/// Loopback-only binding: the dashboard is meant for local use, so it
/// is never exposed to the network by default.
pub fn serve(path: PathBuf, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!(
        "Log dashboard: http://127.0.0.1:{port}  (reading {}, Ctrl-C to stop)",
        path.display()
    );
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            // A failed accept (e.g. client disconnected early) should
            // never take the server down — just wait for the next one.
            Err(_) => continue,
        };
        // Read only the head of the request: we need just the first
        // line (`GET /path?query HTTP/1.1`), never a request body.
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        // First line → middle token is the route. `unwrap_or("/")` covers
        // empty/garbage requests, which then fall through to the 404 arm.
        let request_line = req.lines().next().unwrap_or("");
        let route = request_line.split_whitespace().nth(1).unwrap_or("/");
        // Split off the query string: `/api/entries?level=ERROR`.
        let (route, query) = route.split_once('?').unwrap_or((route, ""));

        // Re-read the file on every request so the dashboard stays live
        // while the log is being appended to. A read error (file moved,
        // permissions) serves an empty dashboard instead of crashing.
        let entries = crate::log::read_log_file(&path)
            .unwrap_or_else(|_| Vec::new());
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());

        match route {
            "/" => respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                &render_dashboard(&file_name, &entries),
            ),
            "/api/stats" => respond(
                &mut stream,
                "200 OK",
                "application/json",
                &stats_json(&entries),
            ),
            "/api/entries" => {
                // An unrecognised `level` value parses to None, which
                // `matches` treats as "no level filter" — graceful.
                let level = query_param(query, "level").and_then(|v| Level::parse(&v));
                let q = query_param(query, "q");
                respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    &entries_json(&entries, level.as_ref(), q.as_deref()),
                )
            }
            "/health" => respond(&mut stream, "200 OK", "text/plain", "ok"),
            _ => respond(&mut stream, "404 Not Found", "text/plain", "not found"),
        }
    }
    Ok(())
}
