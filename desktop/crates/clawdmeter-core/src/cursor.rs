//! Cursor usage from its dashboard: the call its own Plan & Usage page
//! makes, with the editor's sign-in.
//!
//! Cursor keeps no usage log the way Codex does. The editor stores its
//! sign-in in the state database it inherited from VS Code
//! ([`crate::paths::Paths::cursor_db`]; SQLite, table `ItemTable(key, value)`):
//! `cursorAuth/accessToken` (a JWT) and `cursorAuth/stripeMembershipAuthId`
//! (the account id, else the JWT's `sub`). `GET` [`USAGE_URL`] with the cookie
//! `WorkosCursorSessionToken=<id>%3A%3A<token>` answers:
//!
//! ```text
//! {"billingCycleStart":"2026-10-01T09:00:00.000Z","billingCycleEnd":"2026-11-01T09:00:00.000Z",
//!  "membershipType":"pro","isUnlimited":false,
//!  "individualUsage":{"plan":{"autoPercentUsed":12,"apiPercentUsed":40,"totalPercentUsed":26,…},
//!                     "onDemand":{…}}}
//! ```
//!
//! Cursor meters two monthly allowances as percentages: Auto (its own
//! models) and API (named models). They're the device's two cards (`pv`
//! `cu` names them), both resetting when the billing cycle ends. `used` /
//! `limit` aren't read: on the free plan they stay 0 while the allowance is
//! used. A team or enterprise seat without plan percentages shows its
//! `overall` used / limit as the first card.
//!
//! It's the dashboard's own endpoint, not a documented API: `cursor_usage =
//! off` stops it, and a reply it can't read is "no Cursor usage", never a
//! zero. Reading it sends no prompt and uses no allowance. The token is
//! read fresh each time (the editor renews it), the database is opened read
//! only, and neither the token nor the reply is logged.

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::usage::{UsageError, UsageReading, Window};

/// The dashboard's usage summary.
pub const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
/// The editor's keys for its sign-in.
const TOKEN_KEY: &str = "cursorAuth/accessToken";
const ACCOUNT_KEY: &str = "cursorAuth/stripeMembershipAuthId";

/// The editor's sign-in, as the dashboard's cookie wants it.
struct SignIn {
    account: String,
    token: String,
}

impl SignIn {
    fn cookie(&self) -> String {
        format!("WorkosCursorSessionToken={}%3A%3A{}", self.account, self.token)
    }
}

/// Cursor's usage at `now`, read with the sign-in in the editor's state
/// database `db` (blocking: a local read, then one HTTPS call).
pub fn read(db: &Path, now: DateTime<Utc>) -> Result<UsageReading, UsageError> {
    let sign_in = read_sign_in(db)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(crate::oauth::HTTP_TIMEOUT))
        .build()
        .into();
    let mut resp = agent
        .get(USAGE_URL)
        .header("Cookie", &sign_in.cookie())
        .header("Accept", "application/json")
        .header("User-Agent", &crate::oauth::user_agent())
        .call()
        .map_err(|e| UsageError::CursorApi(e.to_string()))?;
    match resp.status().as_u16() {
        200..=299 => parse_summary(&resp.body_mut().read_to_string().unwrap_or_default(), now),
        401 | 403 => Err(UsageError::CursorRejected),
        s => Err(UsageError::CursorApi(format!("HTTP {s}"))),
    }
}

/// The dashboard's reply → a reading: Auto as the session card, API as the
/// weekly one, both resetting when the billing cycle ends.
pub fn parse_summary(body: &str, now: DateTime<Utc>) -> Result<UsageReading, UsageError> {
    let v: Value = serde_json::from_str(body).map_err(|_| UsageError::CursorApi("the reply isn't JSON".into()))?;
    let end = v
        .get("billingCycleEnd")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .filter(|t| *t > now);
    let window = |pct: f64| Window { pct: pct.round() as u32, resets_at: end };
    let pct = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|p| p.is_finite() && *p >= 0.0);
    let usage = v.get("individualUsage");
    let plan = usage.and_then(|u| u.get("plan"));
    let (auto, api) =
        (pct(plan.and_then(|p| p.get("autoPercentUsed"))), pct(plan.and_then(|p| p.get("apiPercentUsed"))));
    if auto.is_some() || api.is_some() {
        return Ok(UsageReading { session: auto.map(window), week: api.map(window), ..Default::default() });
    }
    // A seat with one hard ceiling instead of plan percentages.
    let overall = usage.and_then(|u| u.get("overall"));
    let used = pct(overall.and_then(|o| o.get("used")));
    match (used, overall.and_then(|o| o.get("limit")).and_then(Value::as_f64).filter(|l| *l > 0.0)) {
        (Some(used), Some(limit)) => {
            Ok(UsageReading { session: Some(window(used / limit * 100.0)), ..Default::default() })
        }
        _ => Err(UsageError::NoCursorUsage),
    }
}

/// The editor's sign-in from its state database.
fn read_sign_in(db: &Path) -> Result<SignIn, UsageError> {
    let conn = open(db).map_err(|e| UsageError::CursorApi(format!("Cursor's state database: {e}")))?;
    let item = |key: &str| -> Option<String> {
        let v: String =
            conn.query_row("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?1", [key], |r| r.get(0)).ok()?;
        let v = v.trim().trim_matches('"').trim();
        (!v.is_empty()).then(|| v.to_owned())
    };
    let token = item(TOKEN_KEY).ok_or(UsageError::NoCursorSignIn)?;
    let account = item(ACCOUNT_KEY).or_else(|| jwt_account(&token)).ok_or(UsageError::NoCursorSignIn)?;
    Ok(SignIn { account, token })
}

/// Read only, never written. While the editor runs, a plain read-only
/// open sees the token it just renewed (still in the WAL); once it has quit
/// and taken its shared-memory file with it, that open can fail, and the
/// immutable one reads the (by then checkpointed) file as it is.
fn open(db: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let readable = |c: &Connection| c.query_row("SELECT count(*) FROM ItemTable", [], |r| r.get::<_, i64>(0)).is_ok();
    match Connection::open_with_flags(db, flags) {
        Ok(c) if readable(&c) => Ok(c),
        _ => Connection::open_with_flags(immutable_uri(db), flags | OpenFlags::SQLITE_OPEN_URI),
    }
}

/// `file:///…?immutable=1` for `db`, with what a URI can't hold escaped.
fn immutable_uri(db: &Path) -> String {
    let path = db.to_string_lossy().replace('\\', "/");
    let path = path.trim_start_matches('/').replace('%', "%25").replace('?', "%3F").replace('#', "%23");
    format!("file:///{path}?immutable=1")
}

/// The account id in a JWT's `sub` (`auth0|user_…` → `user_…`).
fn jwt_account(token: &str) -> Option<String> {
    let payload = base64url(token.split('.').nth(1)?)?;
    let v: Value = serde_json::from_slice(&payload).ok()?;
    let id = v.get("sub")?.as_str()?.rsplit('|').next()?;
    (!id.is_empty()).then(|| id.to_owned())
}

fn base64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().take_while(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    const NOW: &str = "2026-10-20T12:00:00Z";
    const END: &str = "2026-11-01T09:00:00.000Z";

    #[test]
    fn plan_percentages() {
        let body = format!(
            r#"{{"billingCycleStart":"2026-10-01T09:00:00.000Z","billingCycleEnd":"{END}","membershipType":"pro",
                "isUnlimited":false,"individualUsage":{{"plan":{{"enabled":true,"used":0,"limit":0,"remaining":0,
                "breakdown":{{"included":0,"bonus":19,"total":19}},"autoPercentUsed":12.4,"apiPercentUsed":39.6,
                "totalPercentUsed":26}},"onDemand":{{"enabled":false,"used":0,"limit":null}}}}}}"#
        );
        let r = parse_summary(&body, at(NOW)).unwrap();
        assert_eq!(r.session, Some(Window { pct: 12, resets_at: Some(at(END)) }));
        assert_eq!(r.week, Some(Window { pct: 40, resets_at: Some(at(END)) }));
        // 0 at the start of a cycle is a reading.
        let fresh = r#"{"individualUsage":{"plan":{"autoPercentUsed":0,"apiPercentUsed":0}}}"#;
        assert_eq!(parse_summary(fresh, at(NOW)).unwrap().session.unwrap().pct, 0);
    }

    #[test]
    fn seats_and_shapes_it_doesnt_know() {
        let seat = r#"{"membershipType":"enterprise","individualUsage":{"overall":{"enabled":true,"used":6907,
                      "limit":45000,"remaining":38093}},"teamUsage":{"onDemand":{"used":0,"limit":1000000}}}"#;
        let r = parse_summary(seat, at(NOW)).unwrap();
        assert_eq!((r.session.unwrap().pct, r.week), (15, None));
        for body in [
            r#"{"membershipType":"enterprise","individualUsage":{"overall":{"used":0,"limit":null}}}"#,
            r#"{"isUnlimited":true,"individualUsage":{}}"#,
            r#"{"individualUsage":{"plan":{"autoPercentUsed":"lots"}}}"#,
            "{}",
        ] {
            assert_eq!(parse_summary(body, at(NOW)), Err(UsageError::NoCursorUsage), "{body}");
        }
        assert!(matches!(parse_summary("<html>", at(NOW)), Err(UsageError::CursorApi(_))));
        // A cycle that ended: no reset time to count down to.
        let ended = r#"{"billingCycleEnd":"2026-10-01T00:00:00Z","individualUsage":{"plan":{"autoPercentUsed":3}}}"#;
        assert_eq!(parse_summary(ended, at(NOW)).unwrap().session.unwrap().resets_at, None);
    }

    fn jwt(payload: &str) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        for c in payload.as_bytes().chunks(3) {
            let n = c.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..=c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        format!("eyJhbGciOiJIUzI1NiJ9.{s}.c2ln")
    }

    #[test]
    fn sign_in_from_the_editors_database() {
        let dir = std::env::temp_dir().join(format!("cm-cursor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.vscdb");
        assert!(matches!(read_sign_in(&db), Err(UsageError::CursorApi(_))), "no database");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);").unwrap();
        assert!(matches!(read_sign_in(&db), Err(UsageError::NoCursorSignIn)));
        let token = jwt(r#"{"sub":"auth0|user_01ABC","exp":1800000000}"#);
        conn.execute("INSERT INTO ItemTable VALUES (?1, ?2)", [TOKEN_KEY, &token]).unwrap();
        let s = read_sign_in(&db).unwrap();
        assert_eq!(s.account, "user_01ABC", "from the token");
        assert_eq!(s.cookie(), format!("WorkosCursorSessionToken=user_01ABC%3A%3A{token}"));
        conn.execute("INSERT INTO ItemTable VALUES (?1, ?2)", [ACCOUNT_KEY, "user_02DEF"]).unwrap();
        assert_eq!(read_sign_in(&db).unwrap().account, "user_02DEF", "the editor's own id first");
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decoding() {
        assert_eq!(base64url("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64url("aGk=").unwrap(), b"hi");
        assert_eq!(base64url("a b"), None);
        assert_eq!(jwt_account(&jwt(r#"{"sub":"user_9"}"#)).as_deref(), Some("user_9"));
        assert_eq!(jwt_account("not.a-jwt"), None);
        assert_eq!(
            immutable_uri(Path::new(r"C:\Users\x\AppData\Roaming\Cursor\User\globalStorage\state.vscdb")),
            "file:///C:/Users/x/AppData/Roaming/Cursor/User/globalStorage/state.vscdb?immutable=1"
        );
        assert_eq!(immutable_uri(Path::new("/home/x/a#b?.vscdb")), "file:///home/x/a%23b%3F.vscdb?immutable=1");
    }
}
