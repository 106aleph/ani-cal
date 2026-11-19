//! SQLite スキーマと接続
use crate::config;
use rusqlite::{types::ValueRef, Connection, Row};
use serde_json::{Map, Number, Value};

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

const SCHEMA: &str = include_str!("schema.sql");

pub fn connect() -> Connection {
    let mut conn = Connection::open(&config::get().db_path).expect("DB を開けません");
    conn.busy_timeout(std::time::Duration::from_secs(30)).ok();
    conn.execute_batch("PRAGMA foreign_keys=ON").ok();
    // 読んでから書くトランザクションを、最初から書き込みとして始める。
    // 途中で書き込みに切り替えると、その間に他の接続が書き込んでいた場合、待ち時間（busy_timeout）が効かず、
    // 「database is locked」で即失敗する。最初から書き込みの順番を待てば、待ち時間のぶんだけ待って通る
    conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
    conn
}

/// DB ファイルを、所有者だけが読み書きできるようにする（パスワードのハッシュや各種の鍵が入っているため）
pub fn restrict_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let p = &config::get().db_path;
    for f in [p.clone(), format!("{}-wal", p), format!("{}-shm", p)] {
        if std::path::Path::new(&f).exists() {
            let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// 複数の文をまとめて反映する。Err なら巻き戻す
pub fn with_tx<T, E: From<rusqlite::Error>>(
    f: impl FnOnce(&Connection) -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let mut conn = connect();
    let tx = conn.transaction()?;
    match f(&tx) {
        Ok(v) => {
            tx.commit()?;
            Ok(v)
        }
        Err(e) => Err(e), // tx は drop で rollback
    }
}

pub fn init() {
    let path = &config::get().db_path;
    if let Some(d) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let conn = connect();
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap_or_default();
    conn.execute_batch(SCHEMA).expect("スキーマの作成に失敗");
    // 既存DBへの追加
    let cols = |t: &str| -> Vec<String> {
        let mut st = conn.prepare(&format!("PRAGMA table_info({})", t)).unwrap();
        st.query_map([], |r| r.get::<_, String>(1)).unwrap().filter_map(|x| x.ok()).collect()
    };
    let tcols = cols("tracks");
    if !tcols.iter().any(|c| c == "archived") {
        conn.execute_batch(
            "ALTER TABLE tracks ADD COLUMN archived INTEGER NOT NULL DEFAULT 0; ALTER TABLE tracks ADD COLUMN archived_at TEXT",
        )
        .ok();
    }
    // ユーザーごとの登録（旧バージョンのDBには user_id が無い）
    if !tcols.iter().any(|c| c == "user_id") {
        conn.execute_batch("ALTER TABLE tracks ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE CASCADE").ok();
    }
    if !cols("push_subs").iter().any(|c| c == "user_id") {
        conn.execute_batch("ALTER TABLE push_subs ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE CASCADE").ok();
    }
    // 2段階認証（旧バージョンのDBには列が無い）
    let ucols = cols("users");
    for (c, ddl) in [
        ("totp_secret", "ALTER TABLE users ADD COLUMN totp_secret TEXT"),
        ("totp_enabled", "ALTER TABLE users ADD COLUMN totp_enabled INTEGER NOT NULL DEFAULT 0"),
        ("totp_last_step", "ALTER TABLE users ADD COLUMN totp_last_step INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !ucols.iter().any(|x| x == c) {
            conn.execute_batch(ddl).ok();
        }
    }
    // 廃止した ICS 購読のトークン列（NOT NULL なので、残すと新規ユーザーを作れない）
    if ucols.iter().any(|x| x == "ics_token") {
        conn.execute_batch("ALTER TABLE users DROP COLUMN ics_token").ok();
    }
    // 最終ログイン日時。列を足すときは、いま残っているセッションの作成日時（＝直近のログイン）で埋める
    if !ucols.iter().any(|x| x == "last_login_at") {
        conn.execute_batch("ALTER TABLE users ADD COLUMN last_login_at TEXT").ok();
        conn.execute_batch("UPDATE users SET last_login_at=(SELECT MAX(created_at) FROM sessions WHERE sessions.user_id=users.id)").ok();
    }
    // ログイン中の端末の一覧用（旧バージョンのセッションには、端末の情報がない）
    let scols = cols("sessions");
    for (c, ddl) in [
        ("user_agent", "ALTER TABLE sessions ADD COLUMN user_agent TEXT"),
        ("ip", "ALTER TABLE sessions ADD COLUMN ip TEXT"),
        ("last_used_at", "ALTER TABLE sessions ADD COLUMN last_used_at TEXT"),
    ] {
        if !scols.iter().any(|x| x == c) {
            conn.execute_batch(ddl).ok();
        }
    }
    conn.execute_batch(
        "DROP INDEX IF EXISTS uq_tracks_tid_chid;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_tracks_user_tid_chid
           ON tracks(user_id, tid, chid) WHERE tid IS NOT NULL AND chid IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_tracks_user ON tracks(user_id);",
    )
    .expect("インデックスの作成に失敗");
    crate::auth::bootstrap(&conn);
    if !cols("sync_log").iter().any(|c| c == "duration_sec") {
        conn.execute_batch("ALTER TABLE sync_log ADD COLUMN duration_sec REAL").ok();
    }
    // 再起動などで途中終了した取得を「中断」にする
    conn.execute("UPDATE sync_log SET status='error', message='中断されました' WHERE status='running'", []).ok();
    // 取得済み期間の記録が無ければ、既存のログから作る
    let has: Option<i64> = conn.query_row("SELECT 1 FROM meta WHERE key='covered'", [], |r| r.get(0)).ok();
    if has.is_none() {
        let mut st = conn.prepare("SELECT range_start, range_end FROM sync_log WHERE status='ok'").unwrap();
        let rs: Vec<(String, String)> = st
            .query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)))
            .unwrap()
            .filter_map(|x| x.ok())
            .filter_map(|(a, b)| match (a, b) {
                (Some(a), Some(b)) if is_day(&a) && is_day(&b) => Some((a, b)),
                _ => None,
            })
            .collect();
        set_meta(&conn, "covered", &serde_json::to_string(&merge_ranges(rs)).unwrap());
    }
}

// ---------------------------------------------------------------- meta
pub fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get::<_, Option<String>>(0)).ok().flatten()
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) {
    let _ = conn.execute(
        "INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    );
}

// ---------------------------------------------------------------- 取得済みの期間
fn is_day(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() && s.len() == 10
}

/// 日付範囲（YYYY-MM-DD, 両端を含む）を、重なる／隣り合うものどうしでまとめる
pub fn merge_ranges(mut ranges: Vec<(String, String)>) -> Vec<(String, String)> {
    ranges.retain(|(s, e)| s <= e);
    ranges.sort();
    let mut out: Vec<(String, String)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = out.last_mut() {
            let next_day = chrono::NaiveDate::parse_from_str(&last.1, "%Y-%m-%d").ok().map(|d| d + chrono::Duration::days(1));
            let sd = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok();
            if let (Some(sd), Some(nd)) = (sd, next_day) {
                if sd <= nd {
                    if e > last.1 {
                        last.1 = e;
                    }
                    continue;
                }
            }
        }
        out.push((s, e));
    }
    out
}

pub fn get_coverage(conn: &Connection) -> Vec<(String, String)> {
    get_meta(conn, "covered")
        .and_then(|s| serde_json::from_str::<Vec<(String, String)>>(&s).ok())
        .unwrap_or_default()
}

pub fn add_coverage(conn: &Connection, start: &str, end: &str) {
    let mut rs = get_coverage(conn);
    rs.push((start.to_string(), end.to_string()));
    set_meta(conn, "covered", &serde_json::to_string(&merge_ranges(rs)).unwrap());
}

// ---------------------------------------------------------------- 行 → JSON
fn value_of(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(_) => Value::Null,
    }
}

/// 列名をキーにした JSON オブジェクト（Python の dict(row) と同じ形）
pub fn row_json(row: &Row) -> Value {
    let mut m = Map::new();
    for (i, name) in row.as_ref().column_names().enumerate() {
        m.insert(name.to_string(), value_of(row.get_ref(i).unwrap_or(ValueRef::Null)));
    }
    Value::Object(m)
}

pub fn query_json(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Value>> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(params, |r| Ok(row_json(r)))?;
    rows.collect()
}

pub fn query_one_json(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Option<Value>> {
    let mut v = query_json(conn, sql, params)?;
    Ok(if v.is_empty() { None } else { Some(v.remove(0)) })
}
