//! `journalctl` for SystemL.
//!
//! Reads the per-unit fallback log files that the runtime writes when a
//! unit's stdio target resolves to `journal` / `kmsg` / `syslog` — the
//! only journal mechanism systeml has on macOS today, since there is no
//! systemd-journald. Files live at
//!
//! ```text
//!   $XDG_STATE_HOME/systeml/journal/<unit>.out.log
//!   $XDG_STATE_HOME/systeml/journal/<unit>.err.log
//! ```
//!
//! Each line written by the runtime is prefixed with an RFC3339 wall-
//! clock timestamp captured at line-flush time (see
//! `systeml_runtime::exec::setup_journal_loggers`). We parse the prefix
//! here and render it. Lines from older runs that lack the prefix are
//! tolerated — they show up untimestamped.
//!
//! This `journalctl` only implements the parts of the upstream CLI that
//! make sense for plain-text logs:
//!
//! - `-u UNIT` / `--unit UNIT` (repeatable) — show one or more units
//! - `-f` / `--follow` — tail
//! - `-n N` / `--lines N` — last N lines
//! - `-o short|json` / `--output …` — output format
//! - `--list` — list units that have journal files (non-standard,
//!   handy because we don't have `journalctl --field _SYSTEMD_UNIT`)
//!
//! Time-window flags (`--since`, `--until`) are accepted for
//! compatibility but ignored — our captures are not timestamped per
//! message. They become a no-op rather than an error so existing scripts
//! don't break.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Parser, Debug)]
#[command(
    name = "journalctl",
    about = "Read SystemL per-unit log files (the journal-stub fallback)",
    long_about = None,
    disable_version_flag = false,
    version
)]
struct Cli {
    /// Show entries from this unit. Can be passed multiple times.
    #[arg(short = 'u', long = "unit", value_name = "UNIT")]
    units: Vec<String>,

    /// Follow the log: print new entries as they appear.
    #[arg(short = 'f', long = "follow")]
    follow: bool,

    /// Show the last N lines per stream (out/err) before tailing.
    #[arg(short = 'n', long = "lines", value_name = "N")]
    lines: Option<usize>,

    /// Output format: `short` (default — locale-ish "MMM DD HH:MM:SS"),
    /// `short-iso` (full RFC3339 timestamp), `cat` (just the message —
    /// no prefix, no timestamp), or `json` (one JSON object per line).
    #[arg(short = 'o', long = "output", default_value = "short")]
    output: String,

    /// List units that have journal files.
    #[arg(long = "list")]
    list: bool,

    /// User-mode (the only mode SystemL has). Accepted for
    /// systemctl-equivalence, no behavioural effect.
    #[arg(long = "user")]
    _user: bool,

    /// Disables paging. We never page anyway. Accepted for compat.
    #[arg(long = "no-pager")]
    _no_pager: bool,

    /// Time-window filter, accepted-and-ignored for compat. Our log
    /// captures have no per-line timestamps, so we cannot honour these.
    #[arg(long = "since", value_name = "TIME")]
    _since: Option<String>,

    /// Time-window filter (see `--since`).
    #[arg(long = "until", value_name = "TIME")]
    _until: Option<String>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("journalctl: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let dir = journal_dir()?;
    if !dir.exists() {
        eprintln!(
            "journalctl: no journal directory at {} — the daemon may not have written any logs yet",
            dir.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let format = parse_format(&cli.output)?;

    if cli.list {
        list_units(&dir)?;
        return Ok(ExitCode::SUCCESS);
    }

    if cli.units.is_empty() {
        eprintln!("journalctl: at least one --unit is required (or --list).");
        eprintln!("           try `journalctl --list` to see units that have logs.");
        return Ok(ExitCode::from(2));
    }

    let streams = open_streams(&dir, &cli.units)?;
    if streams.is_empty() {
        eprintln!(
            "journalctl: no journal files for unit(s): {}",
            cli.units.join(", ")
        );
        eprintln!("           try `journalctl --list` to see units that have logs.");
        return Ok(ExitCode::from(1));
    }

    if cli.follow {
        follow(streams, cli.lines, format)?;
    } else {
        replay(streams, cli.lines, format)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------
// Output format + line parsing
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Format {
    /// `MMM DD HH:MM:SS unit[stream]: line` — short systemd-style.
    Short,
    /// `<rfc3339> unit[stream]: line` — full ISO timestamp.
    ShortIso,
    /// `<line>` only — no decoration, no timestamp.
    Cat,
    /// One JSON object per line:
    /// `{"ts":"...","unit":..,"stream":..,"line":..}`.
    /// `ts` is null for lines without a parseable timestamp prefix.
    Json,
}

fn parse_format(s: &str) -> Result<Format> {
    Ok(match s {
        "short" | "" => Format::Short,
        "short-iso" => Format::ShortIso,
        "cat" => Format::Cat,
        "json" => Format::Json,
        other => {
            return Err(anyhow!(
                "unknown --output format {other:?} (try short, short-iso, cat, json)"
            ))
        }
    })
}

#[derive(Serialize)]
struct JsonRecord<'a> {
    ts: Option<&'a str>,
    unit: &'a str,
    stream: &'a str,
    line: &'a str,
}

/// Split `<RFC3339-timestamp> <message>` into (Some(ts), msg). If the
/// prefix isn't a parseable RFC3339 timestamp, returns (None, full
/// line). Lines from runs predating the timestamping logger fall into
/// the second case naturally — they just show up untimestamped.
fn split_timestamp(line: &str) -> (Option<&str>, &str) {
    let Some((head, rest)) = line.split_once(' ') else {
        return (None, line);
    };
    if OffsetDateTime::parse(head, &Rfc3339).is_ok() {
        (Some(head), rest)
    } else {
        (None, line)
    }
}

/// Render an RFC3339 string into `MMM DD HH:MM:SS` like upstream
/// journalctl's "short" format. Falls back to the raw RFC3339 if
/// parsing fails (which it shouldn't, since `split_timestamp` already
/// validated it).
fn short_ts(rfc: &str) -> String {
    match OffsetDateTime::parse(rfc, &Rfc3339) {
        Ok(t) => format!(
            "{} {:02} {:02}:{:02}:{:02}",
            month_abbrev(t.month() as u8),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ),
        Err(_) => rfc.to_owned(),
    }
}

fn month_abbrev(m: u8) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

fn emit(unit: &str, stream: &str, raw: &str, fmt: Format) {
    let (ts, msg) = split_timestamp(raw);
    match fmt {
        Format::Short => match ts {
            Some(rfc) => println!("{} {unit}[{stream}]: {msg}", short_ts(rfc)),
            None => println!("{unit}[{stream}]: {msg}"),
        },
        Format::ShortIso => match ts {
            Some(rfc) => println!("{rfc} {unit}[{stream}]: {msg}"),
            None => println!("{unit}[{stream}]: {msg}"),
        },
        Format::Cat => println!("{msg}"),
        Format::Json => {
            let rec = JsonRecord {
                ts,
                unit,
                stream,
                line: msg,
            };
            // serde_json::to_string never fails for these primitive types.
            println!("{}", serde_json::to_string(&rec).unwrap());
        }
    }
}

// ---------------------------------------------------------------------
// Filesystem layout
// ---------------------------------------------------------------------

fn journal_dir() -> Result<PathBuf> {
    systeml_unit::search::systeml_state_dir()
        .map(|d| d.join("journal"))
        .ok_or_else(|| anyhow!("could not resolve $XDG_STATE_HOME"))
}

/// Walk the journal directory and report `(unit_name, has_out, has_err)`.
fn list_units(dir: &Path) -> Result<()> {
    let mut by_unit: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
    {
        let p = entry?.path();
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".out.log") {
            by_unit.entry(stem.to_owned()).or_default().0 = true;
        } else if let Some(stem) = name.strip_suffix(".err.log") {
            by_unit.entry(stem.to_owned()).or_default().1 = true;
        }
    }
    if by_unit.is_empty() {
        eprintln!("(no journal files in {})", dir.display());
    } else {
        println!("{:<48} STREAMS", "UNIT");
        for (unit, (out, err)) in by_unit {
            let mut tags = Vec::new();
            if out {
                tags.push("out");
            }
            if err {
                tags.push("err");
            }
            println!("{unit:<48} {}", tags.join(","));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Stream openers
// ---------------------------------------------------------------------

struct Stream {
    unit: String,
    /// `"out"` or `"err"`.
    kind: &'static str,
    file: File,
    /// Last byte position we've consumed in this file. Used by follow().
    pos: u64,
}

fn open_streams(dir: &Path, units: &[String]) -> Result<Vec<Stream>> {
    let mut out = Vec::new();
    for unit in units {
        for kind in ["out", "err"] {
            let p = dir.join(format!("{unit}.{kind}.log"));
            if !p.exists() {
                continue;
            }
            let file = File::open(&p).with_context(|| format!("open {}", p.display()))?;
            out.push(Stream {
                unit: unit.clone(),
                kind,
                file,
                pos: 0,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Static replay (no -f)
// ---------------------------------------------------------------------

/// Read every stream end-to-end (or just the tail of `n` lines per stream
/// if `n` was given) and print.
fn replay(streams: Vec<Stream>, n: Option<usize>, fmt: Format) -> Result<()> {
    for mut s in streams {
        let mut buf = String::new();
        s.file.read_to_string(&mut buf).with_context(|| {
            format!("read {} {} log", s.unit, s.kind)
        })?;
        let lines: Vec<&str> = buf.lines().collect();
        let slice: &[&str] = match n {
            Some(k) if k < lines.len() => &lines[lines.len() - k..],
            _ => &lines,
        };
        for line in slice {
            emit(&s.unit, s.kind, line, fmt);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Follow (-f)
// ---------------------------------------------------------------------

/// Print the tail (`n` lines per stream) once, then poll the streams
/// every 250ms for new content. Loops forever; user kills with Ctrl-C.
fn follow(mut streams: Vec<Stream>, n: Option<usize>, fmt: Format) -> Result<()> {
    // Initial tail.
    for s in streams.iter_mut() {
        let len = s.file.metadata()?.len();
        match n {
            None => {
                s.pos = len;
            }
            Some(0) => {
                s.pos = len;
            }
            Some(k) => {
                // Read the whole file, take the last k lines, advance pos
                // to current EOF so the follow loop only emits new content.
                let mut buf = String::new();
                s.file.seek(SeekFrom::Start(0))?;
                s.file.read_to_string(&mut buf)?;
                let lines: Vec<&str> = buf.lines().collect();
                let take = lines.len().saturating_sub(k);
                for line in &lines[take..] {
                    emit(&s.unit, s.kind, line, fmt);
                }
                s.pos = len;
            }
        }
    }
    // Poll loop.
    loop {
        let mut emitted_any = false;
        for s in streams.iter_mut() {
            let len = match s.file.metadata() {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if len < s.pos {
                // Truncated (log rotation, etc.) — restart from 0.
                s.pos = 0;
            }
            if len <= s.pos {
                continue;
            }
            s.file.seek(SeekFrom::Start(s.pos))?;
            let mut reader = BufReader::new(&mut s.file);
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line)?;
                if n == 0 {
                    break;
                }
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                emit(&s.unit, s.kind, trimmed, fmt);
                emitted_any = true;
            }
            s.pos = len;
        }
        if !emitted_any {
            thread::sleep(Duration::from_millis(250));
        }
    }
}
