//! アニメ配信カレンダー API（Python 版 main.py と同じ入出力）
use crate::util::*;
use crate::{auth, config, db, notify, sync};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone};
use rusqlite::{named_params, params, params_from_iter, types::Value as SqlV, Connection};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct Req {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: Value,
}

const SEASONS: [&str; 4] = ["冬", "春", "夏", "秋"];
const MAX_EPISODES: i64 = 500; // 手動作成・予測で作れる話数の上限
const MAX_INTERVAL_DAYS: i64 = 365; // 話の間隔（日）の上限
const MAX_SYNC_DAYS: i64 = 90; // 一度に取得できる期間の上限（自主的な制限）

// ---------------------------------------------------------------- helpers
fn norm_dt(v: Option<String>) -> Result<Option<String>, ApiErr> {
    let Some(s) = v else { return Ok(None) };
    let s = s.trim().replace(' ', "T");
    if s.is_empty() {
        return Ok(None);
    }
    for fmt in ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(&s, fmt) {
            return Ok(Some(d.format(config::DT_FMT).to_string()));
        }
    }
    Err(err(422, format!("日時の形式が不正です: {}", s)))
}

fn season_of(year: i64, month: i64) -> String {
    format!("{}年{}", year, SEASONS[((month - 1) / 3).clamp(0, 3) as usize])
}

fn is_net_group(gid: Option<i64>, gname: Option<&str>) -> bool {
    let cfg = config::get();
    if let Some(g) = gid {
        if cfg.net_group_ids.contains(&g) {
            return true;
        }
    }
    let g = gname.unwrap_or("").to_lowercase();
    cfg.net_group_keywords.iter().any(|k| g.contains(&k.to_lowercase()))
}

const TRACK_SELECT: &str = "
SELECT tr.*,
  COALESCE(NULLIF(tr.title,''), t.title, '(無題)') AS display_title,
  t.title AS orig_title, t.first_year AS first_year, t.first_month AS first_month,
  (SELECT MIN(start_at) FROM episodes e WHERE e.track_id=tr.id AND e.hidden=0 AND e.start_at IS NOT NULL) AS first_ep_start,
  COALESCE(NULLIF(tr.platform,''), c.name, '') AS display_platform,
  c.name AS orig_platform,
  (SELECT COUNT(*) FROM episodes e WHERE e.track_id=tr.id AND e.hidden=0) AS ep_total,
  (SELECT COUNT(*) FROM episodes e WHERE e.track_id=tr.id AND e.hidden=0 AND e.watched=1) AS ep_watched,
  (SELECT MIN(start_at) FROM episodes e WHERE e.track_id=tr.id AND e.hidden=0 AND e.start_at>=:now) AS next_start,
  (SELECT COUNT(*) FROM episodes e WHERE e.track_id=tr.id AND e.hidden=0 AND e.watched=0
      AND e.start_at<:now) AS backlog
FROM tracks tr
LEFT JOIN titles t ON t.tid=tr.tid
LEFT JOIN channels c ON c.chid=tr.chid
";

fn jstr<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}
fn jint(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_i64())
}

/// 放送クール（作品の放送開始年月。無ければ最初の話の月）を付ける
fn with_season(mut d: Value) -> Value {
    let mut ym = match (jint(&d, "first_year"), jint(&d, "first_month")) {
        (Some(y), Some(m)) if y != 0 && m != 0 => Some((y, m)),
        _ => None,
    };
    if ym.is_none() {
        if let Some(fe) = jstr(&d, "first_ep_start") {
            if fe.len() >= 7 {
                if let (Ok(y), Ok(m)) = (fe[..4].parse::<i64>(), fe[5..7].parse::<i64>()) {
                    ym = Some((y, m));
                }
            }
        }
    }
    let o = d.as_object_mut().unwrap();
    match ym {
        Some((y, m)) => {
            o.insert("season".into(), json!(season_of(y, m)));
            o.insert("season_key".into(), json!(y * 10 + (m - 1) / 3));
        }
        None => {
            o.insert("season".into(), Value::Null);
            o.insert("season_key".into(), Value::Null);
        }
    }
    d
}

fn get_track(conn: &Connection, id: i64) -> Result<Value, ApiErr> {
    let now = config::now_s();
    let sql = format!("{} WHERE tr.id=:id AND tr.user_id=:uid", TRACK_SELECT);
    let mut st = conn.prepare(&sql)?;
    let mut rows = st.query(named_params! {":now": now, ":id": id, ":uid": auth::uid()})?;
    match rows.next()? {
        Some(r) => Ok(with_season(db::row_json(r))),
        None => Err(err(404, "作品が見つかりません")),
    }
}

fn get_episode(conn: &Connection, id: i64) -> Result<Value, ApiErr> {
    db::query_one_json(
        conn,
        "SELECT e.* FROM episodes e JOIN tracks tr ON tr.id=e.track_id WHERE e.id=? AND tr.user_id=?",
        [id, auth::uid()],
    )?
    .ok_or_else(|| err(404, "エピソードが見つかりません"))
}

fn generate_episodes(
    conn: &Connection,
    track_id: i64,
    base_number: f64,
    base_start: &str,
    until: i64,
    interval_days: i64,
    source: &str,
) -> Result<i64, ApiErr> {
    let base_dt = NaiveDateTime::parse_from_str(base_start, config::DT_FMT).map_err(|_| err(422, "日時の形式が不正です"))?;
    let now = config::now_s();
    let mut created = 0;
    for n in (base_number as i64 + 1)..=until {
        let exists: Option<i64> = conn
            .query_row("SELECT 1 FROM episodes WHERE track_id=? AND number=?", params![track_id, n as f64], |r| r.get(0))
            .ok();
        if exists.is_some() {
            continue;
        }
        let ms = (interval_days as f64 * (n as f64 - base_number) * 86_400_000.0).round() as i64;
        let st = base_dt + Duration::milliseconds(ms);
        conn.execute(
            "INSERT INTO episodes(track_id, number, start_at, source, edited, updated_at) VALUES (?,?,?,?,?,?)",
            params![track_id, n as f64, config::fmt_dt(st), source, if source == "manual" { 1 } else { 0 }, now],
        )?;
        created += 1;
    }
    Ok(created)
}

fn parse_id(s: &str) -> Result<i64, ApiErr> {
    s.parse().map_err(|_| err(422, "ID が不正です"))
}

fn qbool(q: &HashMap<String, String>, k: &str) -> Result<bool, ApiErr> {
    match q.get(k).map(|s| s.to_lowercase()) {
        None => Ok(false),
        Some(s) => match s.as_str() {
            "true" | "1" | "yes" | "on" | "t" | "y" => Ok(true),
            "false" | "0" | "no" | "off" | "f" | "n" | "" => Ok(false),
            _ => Err(err(422, format!("{} は true/false で指定してください", k))),
        },
    }
}

// ---------------------------------------------------------------- ルーティング
pub fn route(req: &Req) -> Option<ApiResult> {
    let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let m = req.method.as_str();
    let b = &req.body;
    let q = &req.query;
    Some(match (m, segs.as_slice()) {
        ("GET", ["api", "health"]) => ok(json!({"ok": true})),
        ("GET", ["api", "status"]) => status(),
        ("GET", ["api", "auth", "me"]) => auth::me(),
        ("POST", ["api", "auth", "password"]) => auth::change_password(b),
        ("POST", ["api", "auth", "ics_token"]) => auth::regenerate_ics_token(),
        ("GET", ["api", "users"]) => auth::list_users(),
        ("POST", ["api", "auth", "logout_others"]) => auth::logout_others(),
        ("GET", ["api", "auth", "sessions"]) => auth::list_sessions(),
        ("DELETE", ["api", "auth", "sessions", id]) => auth::revoke_session(id),
        ("POST", ["api", "auth", "totp", "setup"]) => auth::totp_setup(b),
        ("POST", ["api", "auth", "totp", "enable"]) => auth::totp_enable(b),
        ("POST", ["api", "auth", "totp", "disable"]) => auth::totp_disable(b),
        ("GET", ["api", "invites"]) => auth::list_invites(),
        ("POST", ["api", "invites"]) => auth::create_invite(b),
        ("DELETE", ["api", "invites", id]) => parse_id(id).and_then(auth::delete_invite),
        ("PATCH", ["api", "users", id]) => parse_id(id).and_then(|i| auth::update_user(i, b)),
        ("DELETE", ["api", "users", id]) => parse_id(id).and_then(auth::delete_user),
        ("POST", ["api", "sync"]) => trigger_sync(b),
        ("GET", ["api", "logs"]) => sync_logs(q),
        ("GET", ["api", "coverage"]) => coverage(),
        ("POST", ["api", "sync", "cancel"]) => auth::require_admin().and_then(|_| ok(json!({"cancelled": sync::cancel()}))),
        ("GET", ["api", "catalog"]) => catalog(),
        ("GET", ["api", "titles", tid, "channels"]) => title_channels(tid),
        ("GET", ["api", "tracks"]) => list_tracks(q),
        ("POST", ["api", "tracks"]) => create_track(b),
        ("POST", ["api", "tracks", id, "archive"]) => archive(id, true),
        ("POST", ["api", "tracks", id, "unarchive"]) => archive(id, false),
        ("POST", ["api", "tracks", id, "drop"]) => drop_track(id),
        ("PATCH", ["api", "tracks", id]) => update_track(id, b),
        ("DELETE", ["api", "tracks", id]) => delete_track(id),
        ("POST", ["api", "tracks", id, "resync"]) => resync_track(id),
        ("POST", ["api", "tracks", id, "generate"]) => generate(id, b),
        ("POST", ["api", "tracks", id, "watch_all"]) => watch_all(id, b),
        ("GET", ["api", "tracks", id, "episodes"]) => list_episodes(id),
        ("POST", ["api", "tracks", id, "episodes"]) => add_episode(id, b),
        ("PATCH", ["api", "episodes", id]) => update_episode(id, b),
        ("POST", ["api", "episodes", id, "revert"]) => revert_episode(id),
        ("DELETE", ["api", "episodes", id]) => delete_episode(id),
        ("GET", ["api", "calendar"]) => calendar(q),
        ("GET", ["api", "backlog"]) => backlog(),
        ("GET", ["api", "push", "key"]) => notify::get_key(),
        ("GET", ["api", "push", "settings"]) => match q.get("endpoint") {
            Some(e) => notify::get_settings(e),
            None => Err(err(422, "endpoint は必須です")),
        },
        ("POST", ["api", "push", "subscribe"]) => notify::subscribe(b),
        ("POST", ["api", "push", "unsubscribe"]) => notify::unsubscribe(b),
        ("POST", ["api", "push", "settings"]) => notify::set_settings(b),
        ("POST", ["api", "push", "test"]) => notify::test(b),
        _ => return None,
    })
}

// ---------------------------------------------------------------- status / sync
fn status() -> ApiResult {
    let conn = db::connect();
    let mut counts = serde_json::Map::new();
    for k in ["titles", "programs", "channels", "tracks", "episodes"] {
        let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {}", k), [], |r| r.get(0))?;
        counts.insert(k.into(), json!(n));
    }
    let mut v = sync::state_json();
    let o = v.as_object_mut().unwrap();
    o.insert("last_success".into(), db::get_meta(&conn, "last_success").map(Value::from).unwrap_or(Value::Null));
    o.insert("auto_paused".into(), json!(db::get_meta(&conn, "auto_paused").as_deref() == Some("1")));
    o.insert("counts".into(), Value::Object(counts));
    o.insert("now".into(), json!(config::now_s()));
    ok(v)
}

fn trigger_sync(body: &Value) -> ApiResult {
    auth::require_admin()?; // 取得は管理者のみ（しょぼいカレンダーへの負荷を管理するため）
    let m = body_obj(body);
    let (s, e) = (opt_str(&m, "start")?.filter(|x| !x.is_empty()), opt_str(&m, "end")?.filter(|x| !x.is_empty()));
    let (mut sd, mut ed) = (None, None);
    if s.is_some() || e.is_some() {
        let parse = |v: &Option<String>| v.as_deref().and_then(|x| NaiveDate::parse_from_str(x, "%Y-%m-%d").ok());
        let (Some(a), Some(b)) = (parse(&s), parse(&e)) else {
            return Err(err(422, "開始日と終了日を YYYY-MM-DD で指定してください"));
        };
        let (a, b) = (a.and_hms_opt(0, 0, 0).unwrap(), b.and_hms_opt(0, 0, 0).unwrap() + Duration::days(1));
        if b <= a {
            return Err(err(422, "終了日は開始日以降にしてください"));
        }
        if (b - a).num_days() > MAX_SYNC_DAYS {
            return Err(err(422, format!("期間は最大 {} 日までです", MAX_SYNC_DAYS)));
        }
        let limit = config::get().range_sync_daily_limit;
        if limit > 0 {
            let today = config::now().format("%Y-%m-%d").to_string();
            let conn = db::connect();
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sync_log WHERE trigger='range' AND started_at >= ?",
                [today],
                |r| r.get(0),
            )?;
            if n >= limit {
                return Err(err(429, format!(
                    "期間指定は1日 {} 回までです（今日は {} 回実行済み）。制限を避けるため、明日以降に実行してください",
                    limit, n
                )));
            }
        }
        sd = Some(a);
        ed = Some(b);
    }
    let started = sync::trigger_async(sd, ed, if sd.is_some() { "range" } else { "manual" });
    Ok((202, json!({ "started": started })))
}

fn sync_logs(q: &HashMap<String, String>) -> ApiResult {
    auth::require_admin()?; // 取得履歴は管理者だけ
    let limit: i64 = match q.get("limit") {
        None => 200,
        Some(s) => s.parse().ok().filter(|n| (1..=500).contains(n)).ok_or_else(|| err(422, "limit は 1〜500 で指定してください"))?,
    };
    let conn = db::connect();
    ok(Value::Array(db::query_json(&conn, "SELECT * FROM sync_log ORDER BY id DESC LIMIT ?", [limit])?))
}

fn coverage() -> ApiResult {
    auth::require_admin()?; // 取得済みの期間も管理者だけ
    let conn = db::connect();
    let v: Vec<Value> = db::get_coverage(&conn)
        .into_iter()
        .map(|(s, e)| {
            let days = match (NaiveDate::parse_from_str(&s, "%Y-%m-%d"), NaiveDate::parse_from_str(&e, "%Y-%m-%d")) {
                (Ok(a), Ok(b)) => (b - a).num_days() + 1,
                _ => 0,
            };
            json!({"start": s, "end": e, "days": days})
        })
        .collect();
    ok(Value::Array(v))
}

// ---------------------------------------------------------------- 作品一覧
fn catalog() -> ApiResult {
    let cfg = config::get();
    let now = config::now();
    let mask: i64 = if cfg.skip_rebroadcast { 8 } else { 0 };
    let conn = db::connect();
    let rows = db::query_json(
        &conn,
        "SELECT t.tid, t.title, t.cat, t.first_year, t.first_month,
                p.chid, c.name AS chname, c.gid, g.name AS gname,
                MIN(p.start_at) AS first_start, MAX(p.start_at) AS last_start,
                MIN(CASE WHEN p.count=1 THEN p.start_at END) AS ep1_start,
                COUNT(*) AS n
         FROM programs p
         JOIN titles t ON t.tid=p.tid
         LEFT JOIN channels c ON c.chid=p.chid
         LEFT JOIN channel_groups g ON g.gid=c.gid
         WHERE p.deleted=0 AND (p.flag & ?)=0
         GROUP BY p.tid, p.chid",
        [mask],
    )?;
    // (tid, chid) → (track id, archived)
    let mut tracked: HashMap<(i64, i64), (i64, i64)> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT id, tid, chid, archived FROM tracks WHERE tid IS NOT NULL AND user_id=?")?;
        let it = st.query_map([auth::uid()], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?)))?;
        for x in it.flatten() {
            if let Some(ch) = x.2 {
                tracked.insert((x.1, ch), (x.0, x.3));
            }
        }
    }
    let mut order: Vec<i64> = vec![];
    let mut items: HashMap<i64, (Value, Vec<Value>)> = HashMap::new();
    for r in &rows {
        let cat = jint(r, "cat");
        if !cfg.new_cats.is_empty() && !cat.map(|c| cfg.new_cats.contains(&c)).unwrap_or(false) {
            continue;
        }
        let tid = jint(r, "tid").unwrap_or(0);
        let chid = jint(r, "chid").unwrap_or(0);
        let entry = items.entry(tid).or_insert_with(|| {
            order.push(tid);
            (
                json!({"tid": tid, "title": r["title"], "first_year": r["first_year"], "first_month": r["first_month"]}),
                vec![],
            )
        });
        let (trk, arch) = tracked.get(&(tid, chid)).map(|(i, a)| (json!(i), *a)).unwrap_or((Value::Null, 0));
        let ep1 = r.get("ep1_start").cloned().unwrap_or(Value::Null);
        let first = if ep1.is_null() { r["first_start"].clone() } else { ep1.clone() };
        entry.1.push(json!({
            "chid": chid,
            "name": jstr(r, "chname").map(String::from).unwrap_or_else(|| format!("ChID:{}", chid)),
            "group": r["gname"],
            "is_net": is_net_group(jint(r, "gid"), jstr(r, "gname")),
            "first_start": first,
            "last_start": r["last_start"],
            "has_ep1": !ep1.is_null(),
            "count": r["n"],
            "track_id": trk,
            "track_archived": arch != 0,
            "track_state": arch,
        }));
    }

    let mut out: Vec<Value> = vec![];
    for tid in order {
        let (mut it, mut chs) = items.remove(&tid).unwrap();
        let earliest = chs.iter().filter_map(|c| jstr(c, "first_start")).min().unwrap_or("").to_string();
        let (mut y, mut m) = (jint(&it, "first_year").unwrap_or(0), jint(&it, "first_month").unwrap_or(0));
        if y == 0 || m == 0 {
            y = earliest.get(..4).and_then(|s| s.parse().ok()).unwrap_or(0);
            m = earliest.get(5..7).and_then(|s| s.parse().ok()).unwrap_or(1);
        }
        chs.sort_by(|a, b| {
            let ka = (!a["is_net"].as_bool().unwrap_or(false), jstr(a, "first_start").unwrap_or("").to_string());
            let kb = (!b["is_net"].as_bool().unwrap_or(false), jstr(b, "first_start").unwrap_or("").to_string());
            ka.cmp(&kb)
        });
        let o = it.as_object_mut().unwrap();
        o.insert("season".into(), json!(season_of(y, m)));
        o.insert("season_key".into(), json!(y * 10 + (m - 1) / 3));
        o.insert("first_start".into(), json!(earliest));
        o.insert("channels".into(), Value::Array(chs));
        out.push(it);
    }
    out.sort_by(|a, b| {
        let ka = (-jint(a, "season_key").unwrap_or(0), jstr(a, "first_start").unwrap_or("").to_string());
        let kb = (-jint(b, "season_key").unwrap_or(0), jstr(b, "first_start").unwrap_or("").to_string());
        ka.cmp(&kb)
    });

    let mut seasons: Vec<(i64, String)> = out.iter().map(|x| (jint(x, "season_key").unwrap_or(0), jstr(x, "season").unwrap_or("").to_string())).collect();
    seasons.sort();
    seasons.dedup();
    seasons.reverse();
    let mut plat_order: Vec<String> = vec![];
    let mut plat: HashMap<String, (bool, i64)> = HashMap::new();
    for x in &out {
        for c in x["channels"].as_array().unwrap() {
            let name = jstr(c, "name").unwrap_or("").to_string();
            let e = plat.entry(name.clone()).or_insert_with(|| {
                plat_order.push(name.clone());
                (c["is_net"].as_bool().unwrap_or(false), 0)
            });
            e.1 += 1;
        }
    }
    let mut platforms: Vec<Value> = plat_order
        .iter()
        .map(|n| json!({"name": n, "is_net": plat[n].0, "titles": plat[n].1}))
        .collect();
    platforms.sort_by(|a, b| {
        let ka = (!a["is_net"].as_bool().unwrap_or(false), -jint(a, "titles").unwrap_or(0), jstr(a, "name").unwrap_or("").to_string());
        let kb = (!b["is_net"].as_bool().unwrap_or(false), -jint(b, "titles").unwrap_or(0), jstr(b, "name").unwrap_or("").to_string());
        ka.cmp(&kb)
    });
    ok(json!({
        "items": out,
        "seasons": seasons.iter().map(|(k, l)| json!({"key": k, "label": l})).collect::<Vec<_>>(),
        "current_season_key": now.year() as i64 * 10 + ((now.month() as i64) - 1) / 3,
        "platforms": platforms,
    }))
}

fn title_channels(tid: &str) -> ApiResult {
    let tid = parse_id(tid)?;
    let conn = db::connect();
    ok(Value::Array(db::query_json(
        &conn,
        "SELECT p.chid, c.name, MIN(p.start_at) AS first_start, COUNT(*) AS n
         FROM programs p LEFT JOIN channels c ON c.chid=p.chid
         WHERE p.tid=? AND p.deleted=0 GROUP BY p.chid ORDER BY first_start",
        [tid],
    )?))
}

// ---------------------------------------------------------------- 登録作品
fn list_tracks(q: &HashMap<String, String>) -> ApiResult {
    // archived: 0=登録作品 / 1=視聴済み / 2=視聴中断
    let state: i64 = if qbool(q, "dropped")? { 2 } else if qbool(q, "archived")? { 1 } else { 0 };
    let archived = state != 0;
    let order = if archived { "tr.archived_at DESC, tr.id DESC" } else { "COALESCE(next_start, '9999'), tr.id DESC" };
    let conn = db::connect();
    let sql = format!("{} WHERE tr.archived=:arch AND tr.user_id=:uid ORDER BY {}", TRACK_SELECT, order);
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(named_params! {":now": config::now_s(), ":arch": state, ":uid": auth::uid()}, |r| Ok(db::row_json(r)))?;
    let v: Vec<Value> = rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(with_season).collect();
    ok(Value::Array(v))
}

fn archive(id: &str, to_archive: bool) -> ApiResult {
    let id = parse_id(id)?;
    db::with_tx(|conn| {
        let tr = get_track(conn, id)?;
        if to_archive {
            let (total, watched) = (jint(&tr, "ep_total").unwrap_or(0), jint(&tr, "ep_watched").unwrap_or(0));
            if total == 0 || watched < total {
                return Err(err(409, "全話を視聴済みにしてから移してください"));
            }
            conn.execute("UPDATE tracks SET archived=1, archived_at=? WHERE id=?", params![config::now_s(), id])?;
        } else {
            conn.execute("UPDATE tracks SET archived=0, archived_at=NULL WHERE id=?", [id])?;
        }
        ok(get_track(conn, id)?)
    })
}

/// 視聴中断した作品へ移す（話数に関係なくいつでも可）。戻すときは unarchive
fn drop_track(id: &str) -> ApiResult {
    let id = parse_id(id)?;
    db::with_tx(|conn| {
        get_track(conn, id)?;
        conn.execute("UPDATE tracks SET archived=2, archived_at=? WHERE id=?", params![config::now_s(), id])?;
        ok(get_track(conn, id)?)
    })
}

fn create_track(body: &Value) -> ApiResult {
    let m = body_obj(body);
    let (tid, chid) = (opt_int(&m, "tid")?, opt_int(&m, "chid")?);
    let title = opt_str(&m, "title")?;
    let platform = opt_str(&m, "platform")?;
    let first_start = opt_str(&m, "first_start")?;
    let episodes = opt_int(&m, "episodes")?;
    if let Some(e) = episodes {
        if !(1..=MAX_EPISODES).contains(&e) {
            return Err(err(422, format!("episodes は 1〜{} で指定してください", MAX_EPISODES)));
        }
    }
    let interval = opt_int(&m, "interval_days")?.unwrap_or(7);
    if !(1..=MAX_INTERVAL_DAYS).contains(&interval) {
        return Err(err(422, format!("interval_days は 1〜{} で指定してください", MAX_INTERVAL_DAYS)));
    }
    let now = config::now_s();
    let nz = |s: &Option<String>| s.clone().filter(|x| !x.is_empty());
    db::with_tx(|conn| {
        let new_id;
        if let (Some(tid), Some(chid)) = (tid, chid) {
            let ex: Option<i64> = conn.query_row("SELECT id FROM tracks WHERE tid=? AND chid=? AND user_id=?", params![tid, chid, auth::uid()], |r| r.get(0)).ok();
            if let Some(ex) = ex {
                return Err(err(409, format!("登録済みです (id={})", ex)));
            }
            conn.execute(
                "INSERT INTO tracks(user_id, tid, chid, title, platform, created_at) VALUES (?,?,?,?,?,?)",
                params![auth::uid(), tid, chid, nz(&title), nz(&platform), now],
            )?;
            new_id = conn.last_insert_rowid();
            sync::apply_track(conn, new_id)?;
        } else {
            let t = title.clone().unwrap_or_default();
            if t.trim().is_empty() {
                return Err(err(422, "タイトルを入力してください"));
            }
            let plat = platform.clone().map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
            conn.execute(
                "INSERT INTO tracks(user_id, title, platform, auto_sync, created_at) VALUES (?,?,?,0,?)",
                params![auth::uid(), t.trim(), plat, now],
            )?;
            new_id = conn.last_insert_rowid();
            if let Some(first) = norm_dt(first_start.clone())? {
                conn.execute(
                    "INSERT INTO episodes(track_id, number, start_at, source, edited, updated_at) VALUES (?,1,?,'manual',1,?)",
                    params![new_id, first, now],
                )?;
                if let Some(n) = episodes.filter(|n| *n > 1) {
                    generate_episodes(conn, new_id, 1.0, &first, n, interval, "manual")?;
                }
            }
        }
        Ok((201, get_track(conn, new_id)?))
    })
}

fn update_track(id: &str, body: &Value) -> ApiResult {
    let id = parse_id(id)?;
    let m = body_obj(body);
    let chid_in = opt_int(&m, "chid")?;
    let auto_sync = opt_bool(&m, "auto_sync")?;
    let mut texts: Vec<(&str, Option<String>)> = vec![];
    for k in ["title", "platform", "note"] {
        if m.contains_key(k) {
            let v = opt_str(&m, k)?.unwrap_or_default();
            let v = v.trim().to_string();
            texts.push((k, if v.is_empty() { None } else { Some(v) }));
        }
    }
    db::with_tx(|conn| {
        let tr = get_track(conn, id)?;
        let mut sets: Vec<String> = vec![];
        let mut args: Vec<SqlV> = vec![];
        for (k, v) in &texts {
            sets.push(format!("{}=?", k));
            args.push(v.clone().map(SqlV::Text).unwrap_or(SqlV::Null));
        }
        if let Some(a) = auto_sync {
            sets.push("auto_sync=?".into());
            args.push(SqlV::Integer(if a { 1 } else { 0 }));
        }
        let cur_chid = jint(&tr, "chid");
        let chid_changed = chid_in.is_some() && chid_in != cur_chid;
        if chid_changed {
            let Some(tid) = jint(&tr, "tid") else {
                return Err(err(400, "手動登録の作品は媒体(チャンネル)を変更できません。媒体名を編集してください"));
            };
            let dup: Option<i64> = conn
                .query_row("SELECT 1 FROM tracks WHERE tid=? AND chid=? AND id<>? AND user_id=?", params![tid, chid_in.unwrap(), id, auth::uid()], |r| r.get(0))
                .ok();
            if dup.is_some() {
                return Err(err(409, "その媒体は既に登録されています"));
            }
            sets.push("chid=?".into());
            args.push(SqlV::Integer(chid_in.unwrap()));
            if !m.contains_key("platform") {
                sets.push("platform=NULL".into());
            }
        }
        if !sets.is_empty() {
            args.push(SqlV::Integer(id));
            conn.execute(&format!("UPDATE tracks SET {} WHERE id=?", sets.join(", ")), params_from_iter(args))?;
        }
        if chid_changed {
            // 旧媒体の未編集・未視聴データを消して新媒体から取り直す（視聴記録は話数で引き継ぐ）
            conn.execute(
                "DELETE FROM episodes WHERE track_id=? AND source IN ('sync','generated') AND edited=0 AND watched=0 AND hidden=0",
                [id],
            )?;
            sync::apply_track(conn, id)?;
        }
        ok(get_track(conn, id)?)
    })
}

fn delete_track(id: &str) -> ApiResult {
    let id = parse_id(id)?;
    db::with_tx(|conn| {
        get_track(conn, id)?;
        conn.execute("DELETE FROM tracks WHERE id=?", [id])?;
        Ok((204, Value::Null))
    })
}

fn resync_track(id: &str) -> ApiResult {
    let id = parse_id(id)?;
    db::with_tx(|conn| {
        get_track(conn, id)?;
        let n = sync::apply_track(conn, id)?;
        ok(json!({"programs": n, "track": get_track(conn, id)?}))
    })
}

fn generate(id: &str, body: &Value) -> ApiResult {
    let id = parse_id(id)?;
    let m = body_obj(body);
    let until = opt_int(&m, "until")?.ok_or_else(|| err(422, "until は必須です"))?;
    if !(1..=MAX_EPISODES).contains(&until) {
        return Err(err(422, format!("until は 1〜{} で指定してください", MAX_EPISODES)));
    }
    let interval = opt_int(&m, "interval_days")?.unwrap_or(7);
    if !(1..=MAX_INTERVAL_DAYS).contains(&interval) {
        return Err(err(422, format!("interval_days は 1〜{} で指定してください", MAX_INTERVAL_DAYS)));
    }
    db::with_tx(|conn| {
        get_track(conn, id)?;
        let base = db::query_one_json(
            conn,
            "SELECT * FROM episodes WHERE track_id=? AND hidden=0 AND number IS NOT NULL AND start_at IS NOT NULL ORDER BY number DESC LIMIT 1",
            [id],
        )?
        .ok_or_else(|| err(400, "基準となる話（話数と日時のある話）がありません"))?;
        let bn = base["number"].as_f64().unwrap_or(0.0);
        if (until as f64) <= bn {
            return Err(err(400, format!("最終話数は {} 以上にしてください", bn as i64 + 1)));
        }
        let n = generate_episodes(conn, id, bn, jstr(&base, "start_at").unwrap_or(""), until, interval, "generated")?;
        ok(json!({"created": n}))
    })
}

fn watch_all(id: &str, body: &Value) -> ApiResult {
    let id = parse_id(id)?;
    let m = body_obj(body);
    let watched = opt_bool(&m, "watched")?.unwrap_or(true);
    let until_now = opt_bool(&m, "until_now")?.unwrap_or(true);
    let now = config::now_s();
    db::with_tx(|conn| {
        get_track(conn, id)?;
        let mut q = "UPDATE episodes SET watched=?, watched_at=? WHERE track_id=? AND hidden=0".to_string();
        let mut args: Vec<SqlV> = vec![
            SqlV::Integer(if watched { 1 } else { 0 }),
            if watched { SqlV::Text(now.clone()) } else { SqlV::Null },
            SqlV::Integer(id),
        ];
        if until_now {
            q.push_str(" AND start_at<=?");
            args.push(SqlV::Text(now.clone()));
        }
        let n = conn.execute(&q, params_from_iter(args))?;
        ok(json!({"updated": n}))
    })
}

// ---------------------------------------------------------------- エピソード
fn list_episodes(id: &str) -> ApiResult {
    let id = parse_id(id)?;
    let conn = db::connect();
    get_track(&conn, id)?;
    ok(Value::Array(db::query_json(
        &conn,
        "SELECT e.*, p.start_at AS orig_start_at, p.subtitle AS orig_subtitle, p.count AS orig_number
         FROM episodes e LEFT JOIN programs p ON p.pid=e.pid
         WHERE e.track_id=?
         ORDER BY e.number IS NULL, e.number, e.start_at",
        [id],
    )?))
}

fn add_episode(id: &str, body: &Value) -> ApiResult {
    let id = parse_id(id)?;
    let m = body_obj(body);
    let number = opt_f64(&m, "number")?;
    let subtitle = opt_str(&m, "subtitle")?.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let start_at = norm_dt(opt_str(&m, "start_at")?)?;
    let watched = opt_bool(&m, "watched")?.unwrap_or(false);
    let now = config::now_s();
    db::with_tx(|conn| {
        get_track(conn, id)?;
        if let Some(n) = number {
            let dup: Option<i64> = conn.query_row("SELECT 1 FROM episodes WHERE track_id=? AND number=?", params![id, n], |r| r.get(0)).ok();
            if dup.is_some() {
                return Err(err(409, "同じ話数が既にあります"));
            }
        }
        conn.execute(
            "INSERT INTO episodes(track_id, number, subtitle, start_at, watched, watched_at, source, edited, updated_at) \
             VALUES (?,?,?,?,?,?,'manual',1,?)",
            params![id, number, subtitle, start_at, if watched { 1 } else { 0 }, if watched { Some(now.clone()) } else { None }, now],
        )?;
        let eid = conn.last_insert_rowid();
        Ok((201, get_episode(conn, eid)?))
    })
}

fn update_episode(id: &str, body: &Value) -> ApiResult {
    let id = parse_id(id)?;
    let m = body_obj(body);
    let now = config::now_s();
    let has = |k: &str| m.contains_key(k);
    let number = opt_f64(&m, "number")?;
    let subtitle = opt_str(&m, "subtitle")?;
    let start_at = norm_dt(opt_str(&m, "start_at")?)?;
    let watched = opt_bool(&m, "watched")?;
    let hidden = opt_bool(&m, "hidden")?;
    db::with_tx(|conn| {
        let e = get_episode(conn, id)?;
        let mut sets: Vec<String> = vec![];
        let mut args: Vec<SqlV> = vec![];
        let mut edited = false;
        let cur_number = e["number"].as_f64();
        if has("number") && number != cur_number {
            if let Some(n) = number {
                let dup: Option<i64> = conn
                    .query_row("SELECT 1 FROM episodes WHERE track_id=? AND number=? AND id<>?", params![jint(&e, "track_id"), n, id], |r| r.get(0))
                    .ok();
                if dup.is_some() {
                    return Err(err(409, "同じ話数が既にあります"));
                }
            }
            sets.push("number=?".into());
            args.push(number.map(SqlV::Real).unwrap_or(SqlV::Null));
            edited = true;
        }
        if has("subtitle") {
            let v = subtitle.clone().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            if v.as_deref() != jstr(&e, "subtitle") {
                sets.push("subtitle=?".into());
                args.push(v.map(SqlV::Text).unwrap_or(SqlV::Null));
                edited = true;
            }
        }
        if has("start_at") && start_at.as_deref() != jstr(&e, "start_at") {
            sets.push("start_at=?".into());
            args.push(start_at.clone().map(SqlV::Text).unwrap_or(SqlV::Null));
            edited = true;
        }
        if let Some(w) = watched {
            sets.push("watched=?".into());
            sets.push("watched_at=?".into());
            args.push(SqlV::Integer(if w { 1 } else { 0 }));
            args.push(if w { SqlV::Text(now.clone()) } else { SqlV::Null });
        }
        if let Some(h) = hidden {
            sets.push("hidden=?".into());
            args.push(SqlV::Integer(if h { 1 } else { 0 }));
        }
        if edited {
            sets.push("edited=1".into());
        }
        if !sets.is_empty() {
            sets.push("updated_at=?".into());
            args.push(SqlV::Text(now.clone()));
            args.push(SqlV::Integer(id));
            conn.execute(&format!("UPDATE episodes SET {} WHERE id=?", sets.join(", ")), params_from_iter(args))?;
        }
        ok(get_episode(conn, id)?)
    })
}

fn revert_episode(id: &str) -> ApiResult {
    let id = parse_id(id)?;
    db::with_tx(|conn| {
        let e = get_episode(conn, id)?;
        let Some(pid) = jint(&e, "pid") else {
            return Err(err(400, "取得データと紐付いていないため戻せません"));
        };
        let p = db::query_one_json(conn, "SELECT * FROM programs WHERE pid=?", [pid])?
            .ok_or_else(|| err(400, "元の取得データが見つかりません"))?;
        conn.execute(
            "UPDATE episodes SET number=?, subtitle=?, start_at=?, edited=0, source='sync', updated_at=? WHERE id=?",
            params![jint(&p, "count").map(|c| c as f64), jstr(&p, "subtitle"), jstr(&p, "start_at"), config::now_s(), id],
        )?;
        ok(get_episode(conn, id)?)
    })
}

fn delete_episode(id: &str) -> ApiResult {
    let id = parse_id(id)?;
    db::with_tx(|conn| {
        let e = get_episode(conn, id)?;
        if jstr(&e, "source") == Some("manual") && e["pid"].is_null() {
            conn.execute("DELETE FROM episodes WHERE id=?", [id])?;
        } else {
            // 取得/予測データは非表示にして再取得で復活しないようにする
            conn.execute("UPDATE episodes SET hidden=1 WHERE id=?", [id])?;
        }
        Ok((204, Value::Null))
    })
}

// ---------------------------------------------------------------- カレンダー
const EP_CAL_SELECT: &str = "
SELECT e.*, tr.id AS track_id, tr.tid AS tid,
  COALESCE(NULLIF(tr.title,''), t.title, '(無題)') AS title,
  COALESCE(NULLIF(tr.platform,''), c.name, '') AS platform,
  p.end_at AS end_at
FROM episodes e
JOIN tracks tr ON tr.id=e.track_id
LEFT JOIN titles t ON t.tid=tr.tid
LEFT JOIN channels c ON c.chid=tr.chid
LEFT JOIN programs p ON p.pid=e.pid
WHERE e.hidden=0 AND e.start_at IS NOT NULL AND tr.archived=0 AND tr.user_id=?
";

fn calendar(q: &HashMap<String, String>) -> ApiResult {
    let get = |k: &str| -> Result<String, ApiErr> {
        let v = q.get(k).ok_or_else(|| err(422, format!("{} は必須です", k)))?;
        let v = if v.len() == 10 { format!("{}T00:00", v) } else { v.clone() };
        norm_dt(Some(v))?.ok_or_else(|| err(422, format!("{} は必須です", k)))
    };
    let (s, e) = (get("start")?, get("end")?);
    let conn = db::connect();
    ok(Value::Array(db::query_json(
        &conn,
        &format!("{} AND e.start_at>=? AND e.start_at<? ORDER BY e.start_at", EP_CAL_SELECT),
        params![auth::uid(), s, e],
    )?))
}

fn backlog() -> ApiResult {
    // 配信済みで未視聴のエピソード
    let conn = db::connect();
    ok(Value::Array(db::query_json(
        &conn,
        &format!("{} AND e.watched=0 AND e.start_at<=? ORDER BY e.start_at", EP_CAL_SELECT),
        params![auth::uid(), config::now_s()],
    )?))
}

// ---------------------------------------------------------------- ICS
fn ics_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n")
}

fn ics_fold(line: &str) -> String {
    let (mut out, mut buf): (Vec<String>, Vec<u8>) = (vec![], vec![]);
    for ch in line.chars() {
        let mut tmp = [0u8; 4];
        let b = ch.encode_utf8(&mut tmp).as_bytes().to_vec();
        if buf.len() + b.len() > 73 {
            out.push(String::from_utf8(std::mem::take(&mut buf)).unwrap());
            buf = b" ".to_vec();
        }
        buf.extend_from_slice(&b);
    }
    out.push(String::from_utf8(buf).unwrap());
    out.join("\r\n")
}

pub fn ics(q: &HashMap<String, String>) -> Result<String, ApiErr> {
    let unwatched_only = qbool(q, "unwatched_only")?;
    let sql = format!("{}{} ORDER BY e.start_at", EP_CAL_SELECT, if unwatched_only { " AND e.watched=0" } else { "" });
    let conn = db::connect();
    let rows = db::query_json(&conn, &sql, [auth::uid()])?;
    let tz = config::get().tz;
    let utc = |s: &str| -> Option<String> {
        let n = NaiveDateTime::parse_from_str(s, config::DT_FMT).ok()?;
        let l = tz.from_local_datetime(&n).earliest().or_else(|| tz.from_local_datetime(&n).latest())?;
        Some(l.with_timezone(&chrono::Utc).format("%Y%m%dT%H%M%SZ").to_string())
    };
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut lines: Vec<String> = vec![
        "BEGIN:VCALENDAR".into(), "VERSION:2.0".into(), "PRODID:-//anime-calendar//JA".into(),
        "CALSCALE:GREGORIAN".into(), "X-WR-CALNAME:アニメ配信カレンダー".into(),
    ];
    for r in &rows {
        let start = jstr(r, "start_at").unwrap_or("");
        let Some(dtstart) = utc(start) else { continue };
        let st = NaiveDateTime::parse_from_str(start, config::DT_FMT).unwrap();
        let ed_s = match (jstr(r, "end_at"), jint(r, "edited").unwrap_or(0)) {
            (Some(e), 0) if !e.is_empty() => e.to_string(),
            _ => config::fmt_dt(st + Duration::minutes(30)),
        };
        let dtend = utc(&ed_s).unwrap_or_else(|| dtstart.clone());
        let num = r["number"].as_f64().map(|n| format!(" #{}", notify::fmt_g(n))).unwrap_or_default();
        let title = jstr(r, "title").unwrap_or("");
        let platform = jstr(r, "platform").unwrap_or("");
        let summary = format!("{}{}{}", if jint(r, "watched").unwrap_or(0) != 0 { "✓ " } else { "" }, title, num);
        let desc = [platform, jstr(r, "subtitle").unwrap_or("")].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" / ");
        lines.extend([
            "BEGIN:VEVENT".to_string(),
            format!("UID:ep-{}@anime-calendar", jint(r, "id").unwrap_or(0)),
            format!("DTSTAMP:{}", stamp),
            format!("DTSTART:{}", dtstart),
            format!("DTEND:{}", dtend),
            ics_fold(&format!("SUMMARY:{}", ics_escape(&summary))),
            ics_fold(&format!("DESCRIPTION:{}", ics_escape(&desc))),
            ics_fold(&format!("LOCATION:{}", ics_escape(platform))),
            "END:VEVENT".to_string(),
        ]);
    }
    lines.push("END:VCALENDAR".into());
    Ok(lines.join("\r\n") + "\r\n")
}
