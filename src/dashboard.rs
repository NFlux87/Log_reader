//! Standalone HTML dashboard generation (no external assets).
//!
//! Strategy: build one self-contained `.html` file — CSS in a `<style>`
//! block, behaviour in a `<script>` block, and all log data embedded as
//! a JSON array inside that script. No CDN links, no chart library, no
//! web server required, so the generated file works offline by simply
//! opening it in a browser. Serving the same page live is `server.rs`'s
//! job; it reuses `render_dashboard` below.
//!
//! Rust-side work here is deliberately limited to pre-computing the
//! aggregates (cards, bars, recent errors, top messages) that are cheap
//! to compute once. Interactive filtering and the per-minute chart happen
//! in the browser over the embedded JSON, so no round-trip is needed.

use crate::log::{compute_stats, Level, LogEntry};
use std::collections::BTreeMap;

/// Escape a string for safe insertion into HTML text/attribute context.
/// Log messages are untrusted input: without this, a message containing
/// `<script>` would execute in whoever opens the dashboard (XSS).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Escape a string as a JSON string literal (including the quotes).
/// Hand-rolled to keep the zero-dependency promise; handles the
/// characters JSON requires escaping: `"`, `\`, and control characters
/// below U+0020 (which JSON forbids raw). Non-ASCII text passes through.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
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

/// Serialise all entries to a compact JSON array `[{n,ts,lvl,msg,raw}, …]`
/// for the page's `<script>` block. Short key names keep the file small;
/// missing timestamp/level are emitted as JSON `null`, not omitted.
fn entries_json(entries: &[LogEntry]) -> String {
    let mut out = String::from("[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"n\":{},\"ts\":{},\"lvl\":{},\"msg\":{},\"raw\":{}}}",
            e.line_no,
            e.timestamp
                .as_deref()
                .map(json_str)
                .unwrap_or_else(|| "null".into()),
            e.level
                .as_ref()
                .map(|l| json_str(l.as_str()))
                .unwrap_or_else(|| "null".into()),
            json_str(&e.message),
            json_str(&e.raw),
        ));
    }
    out.push(']');
    out
}

/// Render the complete dashboard page as a single HTML string.
/// `file_name` is only used for display (title + header), never opened.
pub fn render_dashboard(file_name: &str, entries: &[LogEntry]) -> String {
    let stats = compute_stats(entries);
    // `max(1)` guards the percentage maths against division by zero
    // on an empty file; every rate below is computed against `total`.
    let total = stats.total.max(1);
    let info = stats.get(&Level::Info);
    let warn = stats.get(&Level::Warning);
    let error = stats.get(&Level::Error) + stats.get(&Level::Fatal);
    let error_rate = error as f64 / total as f64 * 100.0;

    // --- Level distribution: a stacked strip (share of total) plus one
    // bar per level (width relative to the largest level, so the biggest
    // bar always fills its track). Levels with zero entries are skipped
    // to keep the panel short; the loop order is fixed severity order. ---
    let order = [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warning,
        Level::Error,
        Level::Fatal,
    ];
    let mut bars = String::new();
    let mut stack = String::new();
    let max_count = order.iter().map(|l| stats.get(l)).max().unwrap_or(0).max(1);
    for lvl in &order {
        let n = stats.get(lvl);
        if n == 0 {
            continue;
        }
        let pct = n as f64 / total as f64 * 100.0;
        let bar_pct = n as f64 / max_count as f64 * 100.0;
        bars.push_str(&format!(
            r#"<div class="bar-row"><span class="bar-label"><i style="background:{}"></i>{}</span><span class="bar-track"><span class="bar-fill" style="width:{:.1}%;background:{}"></span></span><span class="bar-num">{} · {:.1}%</span></div>"#,
            lvl.color(),
            lvl.as_str(),
            bar_pct,
            lvl.color(),
            n,
            pct
        ));
        stack.push_str(&format!(
            r#"<span title="{}: {}" style="width:{:.2}%;background:{}"></span>"#,
            lvl.as_str(),
            n,
            pct,
            lvl.color()
        ));
    }
    // Empty-file fallback: the panel explains itself instead of showing
    // a blank box.
    if bars.is_empty() {
        bars.push_str(r#"<p class="muted">No parsed log levels found.</p>"#);
    }

    // --- Top repeated messages: count identical messages, show the 8 most
    // frequent. Sorting is by count descending, then message ascending so
    // ties are deterministic from run to run. ---
    let mut msg_counts: BTreeMap<String, usize> = BTreeMap::new();
    for e in entries {
        if e.level.is_some() {
            *msg_counts.entry(e.message.clone()).or_insert(0) += 1;
        }
    }
    let mut top: Vec<_> = msg_counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(8);
    let mut top_rows = String::new();
    for (msg, n) in &top {
        top_rows.push_str(&format!(
            r#"<tr><td class="mono msg-cell">{}</td><td class="num">{}</td></tr>"#,
            esc(msg),
            n
        ));
    }
    if top_rows.is_empty() {
        top_rows.push_str(r#"<tr><td colspan="2" class="muted">No messages.</td></tr>"#);
    }

    // --- Recent errors: `stats.errors` is in file order, so `.rev().take(10)`
    // yields the 10 most recent ERROR/FATAL entries, newest first.
    // The `unwrap()`s are safe: `stats.errors` only ever contains entries
    // with `level: Some(..)` (see `compute_stats`). ---
    let mut err_rows = String::new();
    for e in stats.errors.iter().rev().take(10) {
        err_rows.push_str(&format!(
            r#"<li><span class="badge" style="--c:{}">{}</span><span class="mono ts">{}</span><span class="msg-cell">{}</span></li>"#,
            e.level.as_ref().unwrap().color(),
            e.level.as_ref().unwrap(),
            esc(e.timestamp.as_deref().unwrap_or("—")),
            esc(&e.message)
        ));
    }
    if err_rows.is_empty() {
        err_rows.push_str(r#"<li class="muted">No errors or fatal entries. 🎉</li>"#);
    }

    let range = match (&stats.first_ts, &stats.last_ts) {
        (Some(a), Some(b)) if a == b => esc(a),
        (Some(a), Some(b)) => format!("{} → {}", esc(a), esc(b)),
        _ => "—".to_string(),
    };

    // All entries as JSON for the page's client-side table/chart.
    let entries_data = entries_json(entries);

    // The page template. Note: `{{`/`}}` are escaped braces for Rust's
    // `format!` (they render as single `{`/`}` in the CSS/JS), while
    // `{name}` are the placeholders filled by the arguments below.
    // Log-derived values are inserted via `esc()`; the JSON blob is
    // intentionally *not* HTML-escaped — it lives inside `<script>`,
    // where HTML escaping would corrupt the data.
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Log Dashboard — {file}</title>
<style>
:root{{--bg:#0b0e13;--panel:#12161d;--panel2:#161c26;--line:#232b38;--text:#e6edf3;--muted:#8b949e;--info:#58a6ff;--warn:#d29922;--err:#f85149}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--text);font:14px/1.5 -apple-system,"Segoe UI",Roboto,sans-serif}}
header{{padding:22px 28px;border-bottom:1px solid var(--line);display:flex;flex-wrap:wrap;gap:10px;align-items:baseline;justify-content:space-between}}
h1{{font-size:20px;margin:0;letter-spacing:-.02em}}h1 .dot{{color:var(--info)}}
.sub{{color:var(--muted);font-size:13px}}.mono{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}}
main{{padding:20px 28px 40px;max-width:1200px;margin:0 auto}}
.cards{{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin-bottom:16px}}
.card{{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:14px 16px}}
.card .k{{color:var(--muted);font-size:12px;text-transform:uppercase;letter-spacing:.08em}}
.card .v{{font-size:28px;font-weight:700;margin-top:4px}}.card .v small{{font-size:13px;color:var(--muted);font-weight:400}}
.grid{{display:grid;grid-template-columns:1.2fr .8fr;gap:12px;margin-bottom:16px}}
@media(max-width:820px){{.grid{{grid-template-columns:1fr}}}}
.panel{{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:16px}}
.panel h2{{font-size:14px;margin:0 0 12px;letter-spacing:-.01em}}
.stack{{display:flex;height:10px;border-radius:6px;overflow:hidden;background:#1c2330;margin-bottom:14px}}
.bar-row{{display:grid;grid-template-columns:110px 1fr 90px;align-items:center;gap:10px;margin:7px 0;font-size:13px}}
.bar-label i{{display:inline-block;width:9px;height:9px;border-radius:3px;margin-right:7px}}
.bar-track{{background:#1c2330;border-radius:5px;height:12px;overflow:hidden}}
.bar-fill{{display:block;height:100%;border-radius:5px}}
.bar-num{{color:var(--muted);text-align:right;font-variant-numeric:tabular-nums}}
table{{width:100%;border-collapse:collapse;font-size:13px}}
th,td{{text-align:left;padding:6px 8px;border-bottom:1px solid var(--line)}}
th{{color:var(--muted);font-weight:600;font-size:12px;text-transform:uppercase;letter-spacing:.06em}}
.num{{text-align:right;font-variant-numeric:tabular-nums}}.msg-cell{{word-break:break-word}}
ul.errs{{list-style:none;margin:0;padding:0}}ul.errs li{{display:flex;gap:10px;padding:7px 0;border-bottom:1px solid var(--line);font-size:13px;align-items:baseline}}
ul.errs li:last-child{{border-bottom:0}}.ts{{color:var(--muted);white-space:nowrap;font-size:12px}}
.badge{{--c:var(--info);color:var(--c);border:1px solid var(--c);border-radius:20px;padding:1px 8px;font-size:11px;white-space:nowrap;font-weight:600}}
.toolbar{{display:flex;flex-wrap:wrap;gap:8px;margin-bottom:12px}}
.toolbar input,.toolbar select{{background:var(--panel2);color:var(--text);border:1px solid var(--line);border-radius:8px;padding:8px 10px;font-size:13px}}
.toolbar input{{flex:1;min-width:200px}}.chip{{cursor:pointer;border-radius:20px;padding:6px 12px;border:1px solid var(--line);background:var(--panel);font-size:12px}}
.chip.on{{border-color:var(--info);color:var(--info)}}
#chart{{width:100%;height:140px;display:block}}
.logtable td.raw{{color:var(--muted);font-size:12px}}
footer{{color:var(--muted);text-align:center;padding:18px;font-size:12px}}
</style>
</head>
<body>
<header>
  <div><h1><span class="dot">●</span> Log Dashboard</h1><div class="sub mono">{file} · {total_lines} lines</div></div>
  <div class="sub">{range}</div>
</header>
<main>
  <section class="cards">
    <div class="card"><div class="k">Total</div><div class="v">{total}</div></div>
    <div class="card"><div class="k">INFO</div><div class="v" style="color:var(--info)">{info}</div></div>
    <div class="card"><div class="k">WARNING</div><div class="v" style="color:var(--warn)">{warn}</div></div>
    <div class="card"><div class="k">ERROR + FATAL</div><div class="v" style="color:var(--err)">{error} <small>{error_rate:.1}%</small></div></div>
    <div class="card"><div class="k">Unparsed</div><div class="v">{unparsed}</div></div>
  </section>

  <section class="grid">
    <div class="panel"><h2>Level distribution</h2><div class="stack">{stack}</div>{bars}</div>
    <div class="panel"><h2>Entries per minute</h2><canvas id="chart"></canvas></div>
  </section>

  <section class="grid">
    <div class="panel"><h2>Recent errors</h2><ul class="errs">{err_rows}</ul></div>
    <div class="panel"><h2>Top messages</h2><table><thead><tr><th>Message</th><th style="text-align:right">Count</th></tr></thead><tbody>{top_rows}</tbody></table></div>
  </section>

  <section class="panel">
    <h2>All entries</h2>
    <div class="toolbar">
      <input id="q" type="search" placeholder="Search messages… (e.g. database)">
      <select id="lvl"><option value="">All levels</option><option>TRACE</option><option>DEBUG</option><option>INFO</option><option>WARNING</option><option>ERROR</option><option>FATAL</option><option value="__none">Unparsed</option></select>
      <button class="chip" id="onlyErr">Errors only</button>
      <span class="sub" id="shown"></span>
    </div>
    <table class="logtable"><thead><tr><th>#</th><th>Time</th><th>Level</th><th>Message</th></tr></thead><tbody id="rows"></tbody></table>
  </section>
</main>
<footer>Generated by Log_reader · open this file in any browser · levels come from the level field, not from mentions inside messages</footer>
<!-- Client-side behaviour: filtering/rendering runs in the browser over the JSON below.
     No server calls are made, so this file works fully offline. -->
<script>
const DATA={entries_data};
const COLORS={{TRACE:'#8b949e',DEBUG:'#a371f7',INFO:'#58a6ff',WARNING:'#d29922',ERROR:'#f85149',FATAL:'#ff2d55'}};
const rows=document.getElementById('rows'),q=document.getElementById('q'),lvl=document.getElementById('lvl'),shown=document.getElementById('shown');
let onlyErr=false;
function render(){{
  const needle=q.value.trim().toLowerCase();
  let shownN=0;
  rows.innerHTML=DATA.filter(e=>{{
    if(onlyErr&&!(e.lvl==='ERROR'||e.lvl==='FATAL'))return false;
    if(lvl.value==='__none'){{if(e.lvl)return false}}else if(lvl.value&&e.lvl!==lvl.value)return false;
    if(needle&&!e.raw.toLowerCase().includes(needle))return false;
    return true;
  }}).map(e=>{{
    shownN++;
    const c=COLORS[e.lvl]||'#8b949e';
    return `<tr><td class="mono">${{e.n}}</td><td class="mono ts">${{e.ts||'—'}}</td><td><span class="badge" style="--c:${{c}}">${{e.lvl||'—'}}</span></td><td class="msg-cell">${{escapeHtml(e.msg)}}</td></tr>`;
  }}).join('');
  shown.textContent=shownN+' / '+DATA.length+' shown';
}}
function escapeHtml(s){{return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')}}
q.addEventListener('input',render);lvl.addEventListener('change',render);
document.getElementById('onlyErr').addEventListener('click',e=>{{onlyErr=!onlyErr;e.target.classList.toggle('on',onlyErr);render()}});
render();
// Per-minute bar chart
(function(){{
  const c=document.getElementById('chart'),ctx=c.getContext('2d');
  const buckets={{}};
  DATA.forEach(e=>{{if(e.ts){{const k=e.ts.slice(0,16);buckets[k]=(buckets[k]||0)+1}}}});
  const keys=Object.keys(buckets).sort();if(!keys.length)return;
  const max=Math.max(...keys.map(k=>buckets[k]));
  const W=c.width=c.offsetWidth*devicePixelRatio,H=c.height=140*devicePixelRatio;
  const bw=W/keys.length;
  keys.forEach((k,i)=>{{const h=(buckets[k]/max)*(H-30*devicePixelRatio);ctx.fillStyle='#58a6ff';ctx.fillRect(i*bw+1,H-h,bw-2,h);}});
  ctx.fillStyle='#8b949e';ctx.font=`${{11*devicePixelRatio}}px ui-monospace,monospace`;
  ctx.fillText(keys[0],0,H-6*devicePixelRatio);
  const last=keys[keys.length-1];ctx.fillText(last,W-ctx.measureText(last).width,H-6*devicePixelRatio);
}})();
</script>
</body>
</html>"#,
        file = esc(file_name),
        total_lines = entries.len(),
        total = stats.total,
        info = info,
        warn = warn,
        error = error,
        error_rate = error_rate,
        unparsed = stats.unparsed,
        stack = stack,
        bars = bars,
        range = range,
        err_rows = err_rows,
        top_rows = top_rows,
        entries_data = entries_data,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_and_json() {
        assert_eq!(esc("<a>"), "&lt;a&gt;");
        assert_eq!(json_str("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn dashboard_renders_counts() {
        let entries = crate::log::parse_log_text(
            "2026-10-25 09:00:01 INFO hi\n2026-10-25 09:00:02 ERROR boom\n",
        );
        let html = render_dashboard("info.log", &entries);
        assert!(html.contains("Log Dashboard"));
        assert!(html.contains("ERROR"));
        assert!(html.contains("boom"));
    }
}
