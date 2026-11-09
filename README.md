# Log_reader

Parse a log file and view it in a dashboard. Levels are read from the
**level field** (`2026-10-25 09:00:01 INFO message`), not from mentions
inside the message — so `INFO User received an ERROR notification`
counts as INFO, not ERROR.

Log format: `YYYY-MM-DD HH:MM:SS LEVEL message`, where LEVEL is one of
`TRACE DEBUG INFO WARNING ERROR FATAL` (`WARN` is accepted as WARNING).

## Quick start

```sh
# Print summary + entries
cargo run -- data/info.log

# Generate a standalone dashboard (single HTML file, no server needed)
cargo run -- data/info.log --dashboard
# → open dashboard.html in a browser

# Serve the dashboard live (re-reads the file on each refresh)
cargo run -- data/info.log --serve        # http://127.0.0.1:3000
cargo run -- data/info.log --serve 8080   # custom port
```

## Options

| Option           | Description                                          |
|------------------|------------------------------------------------------|
| `--level LEVEL`  | Only show this level (INFO, WARNING, ERROR, …)       |
| `--search TEXT`  | Only show lines containing TEXT                      |
| `--tail N`       | Only show the last N matching lines                  |
| `--json`         | Print stats as JSON                                  |
| `--dashboard [OUT]` | Generate HTML dashboard (default: `dashboard.html`) |
| `--serve [PORT]` | Serve dashboard over HTTP (default port: 3000)       |

Examples:

```sh
cargo run -- data/info.log --level ERROR
cargo run -- data/info.log --search database
cargo run -- data/info.log --level ERROR --tail 5
cargo run -- data/info.log --json
```

## Dashboard

The HTML dashboard shows:

- Stat cards: total, INFO, WARNING, ERROR+FATAL (with error rate), unparsed
- Level distribution (stacked bar + per-level bars)
- Entries-per-minute chart
- Recent errors and top repeated messages
- Full entry table with live search, level filter, and "errors only" toggle

The HTTP server also exposes:

- `GET /api/stats` — `{"total":…,"levels":{…},…}`
- `GET /api/entries?level=ERROR&q=database` — filtered entries as JSON
- `GET /health` — `ok`

## Tests

```sh
cargo test
```
