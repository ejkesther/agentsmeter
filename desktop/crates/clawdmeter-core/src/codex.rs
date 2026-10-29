//! Codex usage from its own session logs: no network call, no token.
//!
//! Codex writes each rate-limit snapshot its backend sends into the
//! session's rollout file, `<codex home>/sessions/YYYY/MM/DD/rollout-….jsonl`
//! (inactive ones may be compressed to `.jsonl.zst`), as a `token_count`
//! event:
//!
//! ```text
//! {"timestamp":"2026-10-28T10:00:00.123Z","type":"event_msg","payload":{"type":"token_count",
//!  "info":{…},"rate_limits":{"limit_id":"codex","primary":{"used_percent":12.0,
//!  "window_minutes":300,"resets_at":1793250000},"secondary":{…},"plan_type":"plus",…}}}
//! ```
//!
//! `primary` is the 5-hour window and `secondary` the weekly one: the two the
//! device shows for Claude. The newest snapshot of the plan's own bucket
//! (`limit_id` `codex`; other ids are model-specific quotas) across the most
//! recently written rollouts is the account's usage as of that line. Reading
//! it sends nothing, so it can't start a window. A window whose reset has
//! passed since reads as over ([`as_of`]): Codex wrote nothing newer, so no
//! request here started another.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::Value;

use crate::usage::{UsageError, UsageReading, Window};

/// Where Codex keeps its rollouts, under its home.
pub const SESSIONS_DIR: &str = "sessions";
/// The plan's own rate-limit bucket.
pub const PLAN_LIMIT_ID: &str = "codex";
/// Rollouts looked at, newest first: enough for several sessions at once.
const MAX_FILES: usize = 8;
/// How much of a rollout's end is searched for a snapshot; the larger one
/// only when the first held none (a huge tool output after it).
const TAILS: [u64; 2] = [256 * 1024, 4 * 1024 * 1024];

/// One rate-limit snapshot from a rollout.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The line's timestamp: when Codex got these numbers.
    pub at: DateTime<Utc>,
    /// Its windows as a reading.
    pub reading: UsageReading,
}

/// Parse one rollout line: `Some` for a `token_count` event carrying the
/// plan's rate limits (`rate_limits` is null when Codex signs in with an API
/// key).
pub fn parse_line(line: &str) -> Option<Snapshot> {
    // Most lines are messages and tool output: skip them unparsed.
    if !line.contains("\"token_count\"") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    let payload = v.get("payload")?;
    if v.get("type")?.as_str()? != "event_msg" || payload.get("type")?.as_str()? != "token_count" {
        return None;
    }
    let at = DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok()?.with_timezone(&Utc);
    let limits = payload.get("rate_limits")?.as_object()?;
    if limits.get("limit_id").and_then(Value::as_str).is_some_and(|id| !id.eq_ignore_ascii_case(PLAN_LIMIT_ID)) {
        return None;
    }
    let (mut session, mut week) = (None, None);
    for slot in ["primary", "secondary"] {
        let Some((minutes, w)) = limits.get(slot).and_then(|w| window(w, at)) else { continue };
        // By length when Codex gives it (a plan with only a weekly window
        // has it as primary), else by slot.
        let weekly = minutes.map_or(session.is_some(), |m| m > 24 * 60);
        if weekly {
            week.get_or_insert(w);
        } else {
            session.get_or_insert(w);
        }
    }
    if session.is_none() && week.is_none() {
        return None;
    }
    Some(Snapshot { at, reading: UsageReading { session, week, ..Default::default() } })
}

/// One `primary`/`secondary` window: its length in minutes, if given, and
/// the window.
fn window(w: &Value, at: DateTime<Utc>) -> Option<(Option<i64>, Window)> {
    let used = w.get("used_percent")?.as_f64().filter(|u| u.is_finite() && *u >= 0.0)?;
    let resets_at = match w.get("resets_at").and_then(Value::as_i64) {
        Some(t) => Utc.timestamp_opt(t, 0).single(),
        // Older Codex wrote the time left instead.
        None => w.get("resets_in_seconds").and_then(Value::as_i64).map(|s| at + Duration::seconds(s)),
    };
    Some((w.get("window_minutes").and_then(Value::as_i64), Window { pct: used.round() as u32, resets_at }))
}

/// `r` at `now`: a window whose reset has passed is over, the session not
/// started and the week back at 0 % with no reset (as
/// [`crate::probe::not_started`] does for Claude).
pub fn as_of(mut r: UsageReading, now: DateTime<Utc>) -> UsageReading {
    let over = |w: &Window| w.resets_at.is_some_and(|t| t <= now);
    if r.session.as_ref().is_some_and(over) {
        r.session = None;
    }
    if let Some(w) = r.week.as_mut().filter(|w| over(w)) {
        *w = Window { pct: 0, resets_at: None };
    }
    r
}

/// Codex's usage at `now`, from its newest snapshot under `codex_home`.
/// [`UsageError::NoCodexUsage`] when there is none (Codex unused here yet,
/// or signed in with an API key).
pub fn read(codex_home: &Path, now: DateTime<Utc>) -> Result<UsageReading, UsageError> {
    match latest(&codex_home.join(SESSIONS_DIR)) {
        Ok(Some(s)) => Ok(as_of(s.reading, now)),
        Ok(None) => Err(UsageError::NoCodexUsage),
        Err(e) => Err(UsageError::CodexLogs(e.to_string())),
    }
}

/// The newest snapshot across the most recently written rollouts under
/// `sessions`. `Ok(None)` when there is none (or no `sessions` dir); an I/O
/// error only when no rollout could be read for one.
pub fn latest(sessions: &Path) -> io::Result<Option<Snapshot>> {
    let files = match rollouts(sessions) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut best: Option<Snapshot> = None;
    let mut failed = None;
    for (path, mtime) in files.into_iter().take(MAX_FILES) {
        // A file last written before the best snapshot can't hold a newer one.
        if best.as_ref().is_some_and(|b| DateTime::<Utc>::from(mtime) < b.at) {
            break;
        }
        match last_snapshot(&path) {
            Ok(Some(s)) if best.as_ref().is_none_or(|b| s.at > b.at) => best = Some(s),
            Ok(_) => {}
            // Compressed or removed under us: another rollout answers.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => failed = Some(e),
        }
    }
    match (best, failed) {
        (None, Some(e)) => Err(e),
        (best, _) => Ok(best),
    }
}

/// Every `rollout-*.jsonl` under `sessions` (the dated folders, or directly
/// in it as older Codex wrote them), newest written first. A resumed session
/// appends to its old file, so the newest isn't always in today's folder.
fn rollouts(sessions: &Path) -> io::Result<Vec<(PathBuf, SystemTime)>> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<(PathBuf, SystemTime)>) -> io::Result<()> {
        for entry in fs::read_dir(dir)?.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() && depth < 3 {
                let _ = walk(&entry.path(), depth + 1, out);
            } else if kind.is_file() && name.starts_with("rollout-") && name.ends_with(".jsonl") {
                if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                    out.push((entry.path(), mtime));
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(sessions, 0, &mut out)?;
    out.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    Ok(out)
}

/// The last snapshot in one rollout, searching back from its end.
fn last_snapshot(path: &Path) -> io::Result<Option<Snapshot>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    for tail in TAILS {
        let from = len.saturating_sub(tail);
        f.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::new();
        (&mut f).take(len - from).read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        // Started mid-file: the first line is cut off. A last line Codex is
        // still writing doesn't parse and is skipped.
        let text = if from > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
        if let Some(s) = text.lines().rev().find_map(parse_line) {
            return Ok(Some(s));
        }
        if from == 0 {
            break;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLLOUT: &str = include_str!("../tests/fixtures/codex-rollout.jsonl");

    fn utc(t: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(t, 0).unwrap()
    }

    fn token_count(ts: &str, limits: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":null,"rate_limits":{limits}}}}}"#
        )
    }

    #[test]
    fn parses_the_plan_snapshot() {
        let s = ROLLOUT.lines().rev().find_map(parse_line).unwrap();
        assert_eq!(s.at, DateTime::parse_from_rfc3339("2026-10-28T10:04:12.250Z").unwrap());
        assert_eq!(s.reading.session, Some(Window { pct: 34, resets_at: Some(utc(1_793_196_000)) }));
        assert_eq!(s.reading.week, Some(Window { pct: 8, resets_at: Some(utc(1_793_700_000)) }));
        assert!(s.reading.window_started());
        // The fixture's other token_count lines: no limits (API key), a
        // model-specific bucket, and the earlier plan snapshot.
        let all: Vec<_> = ROLLOUT.lines().filter_map(parse_line).collect();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].reading.session.as_ref().unwrap().pct, 31);
    }

    #[test]
    fn line_variants() {
        let ts = "2026-10-28T10:00:00Z";
        let w = |pct: f64, mins: &str| format!(r#"{{"used_percent":{pct},{mins}"resets_at":1793250000}}"#);
        // Only a weekly window, sent as primary.
        let weekly =
            token_count(ts, &format!(r#"{{"primary":{},"secondary":null}}"#, w(40.4, r#""window_minutes":10080,"#)));
        let s = parse_line(&weekly).unwrap();
        assert_eq!((s.reading.session, s.reading.week.map(|w| w.pct)), (None, Some(40)));
        // No window lengths: by slot.
        let slots = token_count(ts, &format!(r#"{{"primary":{},"secondary":{}}}"#, w(1.0, ""), w(2.0, "")));
        let s = parse_line(&slots).unwrap();
        assert_eq!((s.reading.session.unwrap().pct, s.reading.week.unwrap().pct), (1, 2));
        // Older Codex: seconds left instead of a reset time.
        let old = token_count(ts, r#"{"primary":{"used_percent":5,"window_minutes":300,"resets_in_seconds":600}}"#);
        assert_eq!(parse_line(&old).unwrap().reading.session.unwrap().resets_at, Some(utc(1_793_182_200)));
        // Not ours.
        for line in [
            token_count(ts, "null"),
            token_count(ts, r#"{"limit_id":"codex_bengalfox","primary":{"used_percent":90}}"#),
            token_count(ts, r#"{"primary":null,"secondary":null,"credits":{"has_credits":true}}"#),
            token_count("not a time", &format!(r#"{{"primary":{}}}"#, w(1.0, ""))),
            r#"{"timestamp":"2026-10-28T10:00:00Z","type":"response_item","payload":{"type":"message","content":"token_count"}}"#.into(),
            "{\"timestamp\":\"2026-10-28T10:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_cou".into(),
        ] {
            assert_eq!(parse_line(&line), None, "{line}");
        }
        assert!(parse_line(&token_count(ts, &format!(r#"{{"limit_id":"Codex","primary":{}}}"#, w(1.0, "")))).is_some());
    }

    #[test]
    fn windows_over_by_now() {
        let r = ROLLOUT.lines().rev().find_map(parse_line).unwrap().reading;
        assert_eq!(as_of(r.clone(), utc(1_793_195_999)), r);
        let later = as_of(r.clone(), utc(1_793_196_000));
        assert_eq!((later.session, later.week.as_ref().map(|w| w.pct)), (None, Some(8)));
        let much_later = as_of(r, utc(1_793_700_000));
        assert_eq!(much_later.week, Some(Window { pct: 0, resets_at: None }));
    }

    #[test]
    fn newest_snapshot_across_rollouts() {
        let home = std::env::temp_dir().join(format!("cm-codex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let sessions = home.join(SESSIONS_DIR);
        assert_eq!(latest(&sessions).unwrap(), None, "no sessions dir yet");
        assert_eq!(read(&home, utc(0)), Err(UsageError::NoCodexUsage));
        let limits = |pct: u32| {
            format!(
                r#"{{"limit_id":"codex","primary":{{"used_percent":{pct},"window_minutes":300,"resets_at":1793250000}}}}"#
            )
        };
        let write = |rel: &str, lines: &[String], age_secs: u64| {
            let p = sessions.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, lines.join("\n") + "\n").unwrap();
            let t = SystemTime::now() - std::time::Duration::from_secs(age_secs);
            File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
        };
        // An old session from last month, resumed and written most recently.
        write(
            "2026/09/30/rollout-2026-09-30T08-00-00-a.jsonl",
            &[token_count("2026-10-28T10:05:00Z", &limits(50))],
            10,
        );
        // Today's session, written a little earlier, with an older snapshot.
        write(
            "2026/10/28/rollout-2026-10-28T09-00-00-b.jsonl",
            &[token_count("2026-10-28T10:00:00Z", &limits(40))],
            20,
        );
        // Compressed and stray files are ignored.
        write(
            "2026/10/28/rollout-2026-10-28T07-00-00-c.jsonl.zst",
            &[token_count("2026-10-28T11:00:00Z", &limits(99))],
            0,
        );
        write("2026/10/28/notes.jsonl", &[token_count("2026-10-28T11:00:00Z", &limits(99))], 0);
        assert_eq!(latest(&sessions).unwrap().unwrap().reading.session.unwrap().pct, 50);
        // Two sessions at once: the newest line wins, whichever file was
        // touched last (its last write may be a message, not a snapshot).
        write(
            "2026/10/28/rollout-2026-10-28T09-30-00-d.jsonl",
            &[
                token_count("2026-10-28T10:01:00Z", &limits(45)),
                r#"{"timestamp":"2026-10-28T10:06:00Z","type":"response_item","payload":{}}"#.into(),
            ],
            0,
        );
        assert_eq!(latest(&sessions).unwrap().unwrap().reading.session.unwrap().pct, 50);
        assert_eq!(read(&home, utc(1_793_249_000)).unwrap().session.unwrap().pct, 50);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn snapshot_far_back_in_a_long_rollout() {
        let dir = std::env::temp_dir().join(format!("cm-codex-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rollout-x.jsonl");
        let big = format!(
            r#"{{"timestamp":"2026-10-28T10:06:00Z","type":"response_item","payload":{{"output":"{}"}}}}"#,
            "x".repeat(300 * 1024)
        );
        let snap = token_count(
            "2026-10-28T10:05:00Z",
            r#"{"primary":{"used_percent":7,"window_minutes":300,"resets_at":1793250000}}"#,
        );
        fs::write(&p, format!("{snap}\n{big}\n")).unwrap();
        assert_eq!(last_snapshot(&p).unwrap().unwrap().reading.session.unwrap().pct, 7);
        let _ = fs::remove_dir_all(&dir);
    }
}
