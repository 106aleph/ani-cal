//! しょぼいカレンダーからの取得と、登録作品への反映
use crate::config;
use crate::db;
use crate::syoboi::{Client, SyErr, CANCEL};
use crate::{error, info};
use chrono::{Duration, NaiveDateTime};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

static RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Default, Clone)]
pub struct State {
    pub running: bool,
    pub progress: Option<String>,
    pub last_started: Option<String>,
    pub last_finished: Option<String>,
    pub last_error: Option<String>,
    pub last_message: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    running: false,
    progress: None,
    last_started: None,
    last_finished: None,
    last_error: None,
    last_message: None,
});

fn update(f: impl FnOnce(&mut State)) {
    f(&mut STATE.lock().unwrap());
}

pub fn state_json() -> Value {
    let s = STATE.lock().unwrap().clone();
    json!({
        "running": s.running, "progress": s.progress, "last_started": s.last_started,
        "last_finished": s.last_finished, "last_error": s.last_error, "last_message": s.last_message,
    })
}

// ---------------------------------------------------------------- 反映処理
/// 取得済み番組データを登録作品のエピソードへ反映する。
/// - 話数(Count)が同じエピソードがあれば更新（edited=1 なら pid の紐付けだけ）
/// - 無ければ追加
/// - 取得元から消えた未編集・未視聴の取得エピソードは削除
pub fn apply_track(conn: &Connection, track_id: i64) -> rusqlite::Result<i64> {
    let tr: Option<(Option<i64>, Option<i64>, i64)> = conn
        .query_row("SELECT tid, chid, archived FROM tracks WHERE id=?", [track_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .ok();
    let Some((Some(tid), Some(chid), archived)) = tr else { return Ok(0) };
    let mask: i64 = if config::get().skip_rebroadcast { 8 } else { 0 };
    let mut st = conn.prepare(
        "SELECT pid, start_at, count, subtitle FROM programs WHERE tid=? AND chid=? AND deleted=0 AND (flag & ?)=0 \
         ORDER BY start_at, pid",
    )?;
    let progs: Vec<(i64, String, Option<i64>, Option<String>)> = st
        .query_map(params![tid, chid, mask], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // 同じ話数が複数あれば最も早いものを採用
    let mut chosen = vec![];
    let mut seen = HashSet::new();
    for p in progs {
        if let Some(c) = p.2 {
            if !seen.insert(c) {
                continue;
            }
        }
        chosen.push(p);
    }

    let now = config::now_s();
    let mut kept: HashSet<i64> = HashSet::new();
    for (pid, start_at, count, subtitle) in &chosen {
        let number: Option<f64> = count.map(|c| c as f64);
        let mut e: Option<(i64, i64, Option<i64>)> = None; // id, edited, pid
        if let Some(n) = number {
            e = conn
                .query_row("SELECT id, edited, pid FROM episodes WHERE track_id=? AND number=?", params![track_id, n], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .ok();
        }
        if e.is_none() {
            e = conn
                .query_row("SELECT id, edited, pid FROM episodes WHERE track_id=? AND pid=?", params![track_id, pid], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .ok();
        }
        match e {
            None => {
                conn.execute(
                    "INSERT INTO episodes(track_id, number, subtitle, start_at, source, pid, updated_at) \
                     VALUES (?,?,?,?, 'sync', ?, ?)",
                    params![track_id, number, subtitle, start_at, pid, now],
                )?;
                kept.insert(conn.last_insert_rowid());
            }
            Some((id, edited, epid)) => {
                kept.insert(id);
                if edited != 0 {
                    if epid != Some(*pid) {
                        conn.execute("UPDATE episodes SET pid=? WHERE id=?", params![pid, id])?;
                    }
                } else {
                    conn.execute(
                        "UPDATE episodes SET number=?, subtitle=?, start_at=?, pid=?, source='sync', updated_at=? WHERE id=?",
                        params![number, subtitle, start_at, pid, now, id],
                    )?;
                }
            }
        }
    }

    let mut st = conn.prepare("SELECT id FROM episodes WHERE track_id=? AND source='sync' AND edited=0 AND watched=0")?;
    let stale: Vec<i64> = st.query_map([track_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in stale {
        if !kept.contains(&id) {
            conn.execute("DELETE FROM episodes WHERE id=?", [id])?;
        }
    }
    // 視聴済みに移した作品に、新しい未視聴の話が増えたら登録作品へ戻す
    if archived == 1 {
        let any: Option<i64> = conn
            .query_row("SELECT 1 FROM episodes WHERE track_id=? AND hidden=0 AND watched=0", [track_id], |r| r.get(0))
            .ok();
        if any.is_some() {
            conn.execute("UPDATE tracks SET archived=0, archived_at=NULL WHERE id=?", [track_id])?;
            info!("sync", "track {} returned from archive (new unwatched episodes)", track_id);
        }
    }
    Ok(chosen.len() as i64)
}

pub fn apply_all_tracks(conn: &Connection) -> rusqlite::Result<()> {
    let mut st = conn.prepare("SELECT id FROM tracks WHERE auto_sync=1 AND tid IS NOT NULL")?;
    let ids: Vec<i64> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in ids {
        apply_track(conn, id)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 取得処理
fn finish_log(log_id: Option<i64>, status: &str, programs: usize, titles: usize, message: &str, duration: f64) {
    let Some(id) = log_id else { return };
    let conn = db::connect();
    let r = conn.execute(
        "UPDATE sync_log SET finished_at=?, status=?, programs=?, titles=?, message=?, duration_sec=? WHERE id=?",
        params![config::now_s(), status, programs as i64, titles as i64, message, duration, id],
    );
    if r.is_ok() {
        let _ = conn.execute("DELETE FROM sync_log WHERE id <= (SELECT MAX(id) FROM sync_log) - 500", []);
    } else {
        error!("sync", "failed to write sync_log");
    }
}

/// 429 を受けた後に自動取得を止める時間（分）
const RATE_LIMIT_COOLDOWN_MIN: i64 = 180;

fn meta_i64(conn: &Connection, key: &str) -> i64 {
    db::get_meta(conn, key).and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// 自動取得の連続失敗を数え、上限に達したら自動取得を停止する。成功でリセット
fn record_result(ok: bool, trigger: &str) {
    let conn = db::connect();
    if ok {
        db::set_meta(&conn, "fail_count", "0");
        db::set_meta(&conn, "auto_paused", "0");
        return;
    }
    if trigger != "auto" && trigger != "startup" {
        return;
    }
    let n = meta_i64(&conn, "fail_count") + 1;
    db::set_meta(&conn, "fail_count", &n.to_string());
    db::set_meta(&conn, "last_fail_at", &config::now_s());
    let max = config::get().sync_max_failures;
    if max > 0 && n >= max {
        db::set_meta(&conn, "auto_paused", "1");
        error!("sync", "auto sync paused after {} consecutive failures", n);
    }
}

fn set_progress(s: &str) {
    update(|st| st.progress = Some(s.to_string()));
}

/// 取得を実行する。
/// - start/end あり: その期間の放送日時で取得（期間指定）
/// - 初回（差分の基準時刻が無い）: 60日前〜120日先を取得
/// - 2回目以降: 前回以降に更新された番組・タイトルだけを取得（差分取得）
pub fn run_sync(start_date: Option<NaiveDateTime>, end_date: Option<NaiveDateTime>, trigger: &str) -> bool {
    if RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return false;
    }
    update(|s| {
        s.running = true;
        s.progress = Some("開始".into());
        s.last_started = Some(config::now_s());
        s.last_error = None;
    });
    CANCEL.store(false, Ordering::SeqCst);
    let t0 = Instant::now();
    let mut log_id: Option<i64> = None;
    let (mut total, mut n_titles) = (0usize, 0usize);
    let mut ok = false;
    let mut cancelled = false;
    let mut trigger = trigger.to_string();

    let result = (|| -> Result<(), SyErr> {
        let mut client = Client::new();
        let now = config::now();
        let (since_s, chan_at, n_chans) = {
            let conn = db::connect();
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))?;
            (db::get_meta(&conn, "prog_since"), db::get_meta(&conn, "channels_at"), n)
        };
        let custom = start_date.is_some() && end_date.is_some();
        let since = since_s.as_deref().and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok());
        let day = |d: NaiveDateTime| d.format("%Y-%m-%d").to_string();
        let (mode, start, end, label_s, label_e): (&str, NaiveDateTime, NaiveDateTime, String, String);
        if custom {
            let (s, e) = (start_date.unwrap(), end_date.unwrap());
            mode = "range";
            start = s;
            end = e;
            trigger = "range".into();
            label_s = day(s);
            label_e = day(e - Duration::seconds(1));
        } else if since.is_none() {
            let cfg = config::get();
            let s = (now - Duration::days(cfg.sync_days_back)).date().and_hms_opt(0, 0, 0).unwrap();
            let e = now + Duration::days(cfg.sync_days_forward);
            mode = "full";
            start = s;
            end = e;
            label_s = day(s);
            label_e = day(e - Duration::seconds(1));
        } else {
            let s = since.unwrap();
            mode = "diff";
            start = s;
            end = now;
            label_s = format!("{}以降の更新分", s.format("%Y-%m-%d %H:%M"));
            label_e = now.format("%Y-%m-%d %H:%M").to_string();
        }
        {
            let conn = db::connect();
            conn.execute(
                "INSERT INTO sync_log(started_at, trigger, range_start, range_end) VALUES(?,?,?,?)",
                params![config::fmt_dt(now), trigger, label_s, label_e],
            )?;
            log_id = Some(conn.last_insert_rowid());
        }

        // 1. チャンネル（媒体）: 変化が少ないので24時間に1回だけ取得
        let chans_stale = match &chan_at {
            Some(c) if n_chans > 0 => match NaiveDateTime::parse_from_str(c, config::DT_FMT) {
                Ok(t) => now - t >= Duration::hours(24),
                Err(_) => true,
            },
            _ => true,
        };
        if chans_stale {
            set_progress("チャンネル取得");
            let groups = match client.channel_groups() {
                Ok(g) => g,
                Err(SyErr::Cancelled) => return Err(SyErr::Cancelled),
                Err(e) => {
                    crate::warn!("sync", "ChGroupLookup failed: {}", e);
                    vec![]
                }
            };
            let chans = client.channels()?;
            let mut conn = db::connect();
            let tx = conn.transaction()?;
            for g in &groups {
                tx.execute(
                    "INSERT INTO channel_groups(gid,name,sort) VALUES(?,?,?) ON CONFLICT(gid) DO UPDATE SET name=excluded.name, sort=excluded.sort",
                    params![g.gid, g.name, g.sort],
                )?;
            }
            for c in &chans {
                tx.execute(
                    "INSERT INTO channels(chid,name,gid,sort) VALUES(?,?,?,?) ON CONFLICT(chid) DO UPDATE SET name=excluded.name, gid=excluded.gid, sort=excluded.sort",
                    params![c.chid, c.name, c.gid, c.sort],
                )?;
            }
            db::set_meta(&tx, "channels_at", &config::fmt_dt(now));
            tx.commit()?;
        }
        let chids: Option<Vec<i64>> = {
            let cfg = config::get();
            if cfg.sync_chgids.is_empty() {
                None
            } else {
                let conn = db::connect();
                let mut st = conn.prepare("SELECT chid, gid FROM channels")?;
                let v: Vec<i64> = st
                    .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))?
                    .filter_map(|x| x.ok())
                    .filter(|(_, g)| g.map(|g| cfg.sync_chgids.contains(&g)).unwrap_or(false))
                    .map(|(c, _)| c)
                    .collect();
                if v.is_empty() { None } else { Some(v) }
            }
        };

        // 2. 番組（配信予定）
        let stamp = config::fmt_dt(now);
        let mut tids: BTreeSet<i64> = BTreeSet::new();
        let mut save = |progs: Vec<crate::syoboi::Program>, total: &mut usize| -> rusqlite::Result<()> {
            let mut conn = db::connect();
            let tx = conn.transaction()?;
            for p in &progs {
                tx.execute(
                    "INSERT INTO programs(pid,tid,chid,start_at,end_at,count,subtitle,flag,deleted,updated_at) \
                     VALUES(?,?,?,?,?,?,?,?,?,?) \
                     ON CONFLICT(pid) DO UPDATE SET tid=excluded.tid, chid=excluded.chid, \
                     start_at=excluded.start_at, end_at=excluded.end_at, count=excluded.count, \
                     subtitle=excluded.subtitle, flag=excluded.flag, deleted=excluded.deleted, \
                     updated_at=excluded.updated_at",
                    params![p.pid, p.tid, p.chid, p.start_at, p.end_at, p.count, p.subtitle, p.flag, p.deleted, stamp],
                )?;
                tids.insert(p.tid);
            }
            tx.commit()?;
            *total += progs.len();
            Ok(())
        };
        let mut local_total = 0usize;
        let res: Result<(), SyErr> = (|| {
            if mode == "diff" {
                set_progress("番組取得（更新分）");
                let p = client.programs_updated(start, now, &chids)?;
                save(p, &mut local_total)?;
            } else {
                let mut cur = start;
                while cur < end {
                    let nxt = (cur + Duration::days(config::get().sync_chunk_days)).min(end);
                    set_progress(&format!("番組取得 {}〜{}", cur.format("%Y-%m-%d"), nxt.format("%m-%d")));
                    let p = client.programs(cur, nxt - Duration::seconds(1), &chids)?;
                    save(p, &mut local_total)?;
                    cur = nxt;
                }
            }
            Ok(())
        })();
        total = local_total;
        res?;

        // 3. タイトル: 未取得のもの＋（差分取得時は）更新されたもの
        let known: HashSet<i64> = {
            let conn = db::connect();
            let mut st = conn.prepare("SELECT tid FROM titles")?;
            let v = st.query_map([], |r| r.get::<_, i64>(0))?.filter_map(|x| x.ok()).collect();
            v
        };
        let mut need: BTreeSet<i64> = tids.iter().filter(|t| !known.contains(t)).copied().collect();
        if mode == "diff" {
            set_progress("タイトル更新確認");
            for t in client.titles_updated(start)? {
                if known.contains(&t) {
                    need.insert(t);
                }
            }
        }
        let need: Vec<i64> = need.into_iter().collect();
        n_titles = need.len();
        for (i, batch) in need.chunks(100).enumerate() {
            set_progress(&format!("タイトル取得 {}/{}", i * 100 + batch.len(), need.len()));
            let titles = client.titles(batch)?;
            let mut conn = db::connect();
            let tx = conn.transaction()?;
            for t in &titles {
                tx.execute(
                    "INSERT INTO titles(tid,title,cat,first_year,first_month,updated_at) VALUES(?,?,?,?,?,?) \
                     ON CONFLICT(tid) DO UPDATE SET title=excluded.title, cat=excluded.cat, \
                     first_year=excluded.first_year, first_month=excluded.first_month, updated_at=excluded.updated_at",
                    params![t.tid, t.title, t.cat, t.first_year, t.first_month, stamp],
                )?;
            }
            tx.commit()?;
        }

        // 4. 登録作品へ反映
        set_progress("登録作品へ反映");
        {
            let mut conn = db::connect();
            let tx = conn.transaction()?;
            apply_all_tracks(&tx)?;
            db::set_meta(&tx, "last_success", &stamp);
            if mode != "diff" {
                db::add_coverage(&tx, &label_s, &label_e);
            }
            if mode != "range" {
                // 期間指定の取得では差分の基準時刻を進めない（余裕を見て5分前）
                db::set_meta(&tx, "prog_since", &(now - Duration::minutes(5)).format("%Y-%m-%dT%H:%M:%S").to_string());
            }
            tx.commit()?;
        }
        let kind = match mode {
            "diff" => "差分",
            "full" => "初回",
            _ => "期間指定",
        };
        let msg = format!(
            "{}: 番組 {} 件 / 作品 {} 件 / タイトル {} 件を取得（{}〜{}）",
            kind, total, tids.len(), n_titles, label_s, label_e
        );
        update(|s| s.last_message = Some(msg.clone()));
        info!("sync", "{}", msg);
        finish_log(log_id, "ok", total, n_titles, &msg, t0.elapsed().as_secs_f64());
        ok = true;
        Ok(())
    })();

    match result {
        Ok(()) => {}
        Err(SyErr::Cancelled) => {
            cancelled = true;
            let msg = format!("中断しました（取得済みの {} 件は保存済み）", total);
            update(|s| s.last_message = Some(msg.clone()));
            info!("sync", "{}", msg);
            finish_log(log_id, "cancelled", total, n_titles, &msg, t0.elapsed().as_secs_f64());
        }
        Err(SyErr::Msg(e)) => {
            error!("sync", "sync failed: {}", e);
            update(|s| s.last_error = Some(e.clone()));
            if e.contains("制限されています") {
                // 429 を受けたら、手動・自動を問わず一定時間は自動取得を送らない
                let until = config::now() + Duration::minutes(RATE_LIMIT_COOLDOWN_MIN);
                db::set_meta(&db::connect(), "limited_until", &until.format(config::DT_FMT).to_string());
            }
            finish_log(log_id, "error", total, n_titles, &e, t0.elapsed().as_secs_f64());
        }
    }
    if !cancelled {
        // 中断は失敗として数えない
        record_result(ok, &trigger);
    }
    // 成功・失敗・中断のどれでも、途中まで入った配信予定があり得るので、作品一覧の集計を捨てる
    crate::api::invalidate_catalog();
    update(|s| {
        s.running = false;
        s.progress = None;
        s.last_finished = Some(config::now_s());
    });
    RUNNING.store(false, Ordering::SeqCst);
    true
}

/// 実行中の取得に中断を要求する。実行中でなければ false
pub fn cancel() -> bool {
    if !RUNNING.load(Ordering::SeqCst) {
        return false;
    }
    CANCEL.store(true, Ordering::SeqCst);
    set_progress("中断しています…");
    true
}

pub fn trigger_async(start: Option<NaiveDateTime>, end: Option<NaiveDateTime>, trigger: &str) -> bool {
    if RUNNING.load(Ordering::SeqCst) {
        return false;
    }
    update(|s| {
        s.running = true;
        s.progress = Some("開始".into());
    });
    let trigger = trigger.to_string();
    std::thread::Builder::new().name("sync".into()).spawn(move || {
        run_sync(start, end, &trigger);
    }).ok();
    true
}

fn scheduler_loop() {
    let cfg = config::get();
    let mut first = true;
    loop {
        let r = std::panic::catch_unwind(|| {
            let (last, paused, fails, last_fail, limited_until) = {
                let conn = db::connect();
                (
                    db::get_meta(&conn, "last_success"),
                    db::get_meta(&conn, "auto_paused").as_deref() == Some("1"),
                    meta_i64(&conn, "fail_count"),
                    db::get_meta(&conn, "last_fail_at"),
                    db::get_meta(&conn, "limited_until"),
                )
            };
            let now = config::now();
            let interval = Duration::seconds((cfg.sync_interval_hours * 3600.0) as i64);
            let mut due = last.is_none();
            if let Some(l) = &last {
                if let Ok(t) = NaiveDateTime::parse_from_str(l, config::DT_FMT) {
                    due = now - t >= interval;
                } else {
                    due = true;
                }
            }
            if due && fails > 0 {
                if let Some(lf) = last_fail.as_deref().and_then(|s| NaiveDateTime::parse_from_str(s, config::DT_FMT).ok()) {
                    // 失敗後は待ち時間を倍々に延ばす（10分→20分→…最大 SYNC_INTERVAL_HOURS）
                    let wait = Duration::minutes(10 * (1i64 << (fails - 1).min(20))).min(interval);
                    due = now - lf >= wait;
                }
            }
            if let Some(u) = limited_until.as_deref().and_then(|s| NaiveDateTime::parse_from_str(s, config::DT_FMT).ok()) {
                if now < u {
                    due = false; // 429 の直後は間隔を空ける
                }
            }
            if paused {
                due = false; // 連続失敗で停止中。画面から手動で取得して成功すると再開する
            }
            if due && (!first || cfg.sync_on_startup) {
                run_sync(None, None, if first { "startup" } else { "auto" });
            }
        });
        if r.is_err() {
            error!("sync", "scheduler error");
        }
        first = false;
        std::thread::sleep(std::time::Duration::from_secs_f64(540.0 + fastrand::f64() * 120.0)); // 実行時刻が揃わないよう揺らす
    }
}

pub fn start_scheduler() {
    if config::get().sync_interval_hours <= 0.0 {
        return;
    }
    std::thread::Builder::new().name("scheduler".into()).spawn(scheduler_loop).ok();
}
