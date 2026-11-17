"""しょぼいカレンダーからの取得と、登録作品への反映"""
import logging
import random
import threading
import time
from datetime import datetime, timedelta

import config
import db
from syoboi import SyoboiClient

log = logging.getLogger("sync")

_lock = threading.Lock()
STATE = {
    "running": False,
    "progress": None,
    "last_started": None,
    "last_finished": None,
    "last_error": None,
    "last_message": None,
}


def _chunks(seq, n):
    seq = list(seq)
    for i in range(0, len(seq), n):
        yield seq[i:i + n]


# ---------------------------------------------------------------- 反映処理
def apply_track(conn, track_id: int) -> int:
    """取得済み番組データを登録作品のエピソードへ反映する。
    - 話数(Count)が同じエピソードがあれば更新（edited=1 なら pid の紐付けだけ）
    - 無ければ追加
    - 取得元から消えた未編集・未視聴の取得エピソードは削除
    """
    tr = conn.execute("SELECT * FROM tracks WHERE id=?", (track_id,)).fetchone()
    if not tr or tr["tid"] is None or tr["chid"] is None:
        return 0
    mask = 8 if config.SKIP_REBROADCAST else 0  # Flag bit 8 = 再放送
    progs = conn.execute(
        "SELECT * FROM programs WHERE tid=? AND chid=? AND deleted=0 AND (flag & ?)=0 "
        "ORDER BY start_at, pid",
        (tr["tid"], tr["chid"], mask),
    ).fetchall()

    chosen, seen = [], set()
    for p in progs:  # 同じ話数が複数あれば最も早いものを採用
        if p["count"] is not None:
            if p["count"] in seen:
                continue
            seen.add(p["count"])
        chosen.append(p)

    now = config.fmt_dt(config.now_jst())
    kept = set()
    for p in chosen:
        e = None
        if p["count"] is not None:
            e = conn.execute(
                "SELECT * FROM episodes WHERE track_id=? AND number=?",
                (track_id, float(p["count"])),
            ).fetchone()
        if e is None:
            e = conn.execute(
                "SELECT * FROM episodes WHERE track_id=? AND pid=?", (track_id, p["pid"])
            ).fetchone()
        if e is None:
            cur = conn.execute(
                "INSERT INTO episodes(track_id, number, subtitle, start_at, source, pid, updated_at) "
                "VALUES (?,?,?,?, 'sync', ?, ?)",
                (track_id, None if p["count"] is None else float(p["count"]),
                 p["subtitle"], p["start_at"], p["pid"], now),
            )
            kept.add(cur.lastrowid)
            continue
        kept.add(e["id"])
        if e["edited"]:
            if e["pid"] != p["pid"]:
                conn.execute("UPDATE episodes SET pid=? WHERE id=?", (p["pid"], e["id"]))
        else:
            conn.execute(
                "UPDATE episodes SET number=?, subtitle=?, start_at=?, pid=?, source='sync', updated_at=? "
                "WHERE id=?",
                (None if p["count"] is None else float(p["count"]),
                 p["subtitle"], p["start_at"], p["pid"], now, e["id"]),
            )

    stale = conn.execute(
        "SELECT id FROM episodes WHERE track_id=? AND source='sync' AND edited=0 AND watched=0",
        (track_id,),
    ).fetchall()
    for r in stale:
        if r["id"] not in kept:
            conn.execute("DELETE FROM episodes WHERE id=?", (r["id"],))
    return len(chosen)


def apply_all_tracks(conn) -> None:
    for r in conn.execute("SELECT id FROM tracks WHERE auto_sync=1 AND tid IS NOT NULL").fetchall():
        apply_track(conn, r["id"])


# ---------------------------------------------------------------- 取得処理
def _finish_log(log_id, status, programs, titles, message) -> None:
    if log_id is None:
        return
    try:
        with db.connect() as conn:
            conn.execute(
                "UPDATE sync_log SET finished_at=?, status=?, programs=?, titles=?, message=? WHERE id=?",
                (config.fmt_dt(config.now_jst()), status, programs, titles, message, log_id))
            conn.execute("DELETE FROM sync_log WHERE id <= (SELECT MAX(id) FROM sync_log) - 500")
    except Exception:
        log.exception("failed to write sync_log")


def _record_result(ok: bool, trigger: str) -> None:
    """自動取得の連続失敗を数え、上限に達したら自動取得を停止する。成功でリセット"""
    with db.connect() as conn:
        if ok:
            db.set_meta(conn, "fail_count", 0)
            db.set_meta(conn, "auto_paused", 0)
            return
        if trigger not in ("auto", "startup"):
            return
        n = int(db.get_meta(conn, "fail_count", 0) or 0) + 1
        db.set_meta(conn, "fail_count", n)
        db.set_meta(conn, "last_fail_at", config.fmt_dt(config.now_jst()))
        if config.SYNC_MAX_FAILURES > 0 and n >= config.SYNC_MAX_FAILURES:
            db.set_meta(conn, "auto_paused", 1)
            log.error("auto sync paused after %d consecutive failures", n)


def run_sync(start_date: datetime | None = None, end_date: datetime | None = None, trigger: str = "manual") -> bool:
    """取得を実行する。
    - start_date/end_date あり: その期間の放送日時で取得（期間指定）
    - 初回（差分の基準時刻が無い）: 60日前〜120日先を取得
    - 2回目以降: 前回以降に更新された番組・タイトルだけを取得（差分取得）
    """
    if not _lock.acquire(blocking=False):
        return False
    STATE.update(running=True, progress="開始", last_started=config.fmt_dt(config.now_jst()),
                 last_error=None)
    client = SyoboiClient()
    log_id = None
    total = n_titles = 0
    ok = False
    try:
        now = config.now_jst()
        with db.connect() as conn:
            since_s = db.get_meta(conn, "prog_since")
            chan_at = db.get_meta(conn, "channels_at")
            n_chans = conn.execute("SELECT COUNT(*) FROM channels").fetchone()[0]
        custom = start_date is not None and end_date is not None
        since = datetime.strptime(since_s, "%Y-%m-%dT%H:%M:%S") if since_s else None
        if custom:
            mode, start, end = "range", start_date, end_date
            trigger = "range"
            label_s, label_e = start.strftime("%Y-%m-%d"), (end - timedelta(seconds=1)).strftime("%Y-%m-%d")
        elif since is None:
            mode = "full"
            start = (now - timedelta(days=config.SYNC_DAYS_BACK)).replace(hour=0, minute=0, second=0)
            end = now + timedelta(days=config.SYNC_DAYS_FORWARD)
            label_s, label_e = start.strftime("%Y-%m-%d"), (end - timedelta(seconds=1)).strftime("%Y-%m-%d")
        else:
            mode, start, end = "diff", since, now
            label_s, label_e = f"{since:%Y-%m-%d %H:%M}以降の更新分", f"{now:%Y-%m-%d %H:%M}"
        with db.connect() as conn:
            log_id = conn.execute(
                "INSERT INTO sync_log(started_at, trigger, range_start, range_end) VALUES(?,?,?,?)",
                (config.fmt_dt(now), trigger, label_s, label_e)).lastrowid

        # 1. チャンネル（媒体）: 変化が少ないので24時間に1回だけ取得
        chans_stale = (not chan_at or not n_chans or
                       now - datetime.strptime(chan_at, "%Y-%m-%dT%H:%M") >= timedelta(hours=24))
        if chans_stale:
            STATE["progress"] = "チャンネル取得"
            try:
                groups = client.channel_groups()
            except Exception as e:  # グループは無くても動く
                log.warning("ChGroupLookup failed: %s", e)
                groups = []
            chans = client.channels()
            with db.connect() as conn:
                conn.executemany(
                    "INSERT INTO channel_groups(gid,name,sort) VALUES(:gid,:name,:sort) "
                    "ON CONFLICT(gid) DO UPDATE SET name=excluded.name, sort=excluded.sort", groups)
                conn.executemany(
                    "INSERT INTO channels(chid,name,gid,sort) VALUES(:chid,:name,:gid,:sort) "
                    "ON CONFLICT(chid) DO UPDATE SET name=excluded.name, gid=excluded.gid, sort=excluded.sort",
                    chans)
                db.set_meta(conn, "channels_at", config.fmt_dt(now))
        with db.connect() as conn:
            chids = None
            if config.SYNC_CHGIDS:
                chids = [r["chid"] for r in conn.execute("SELECT chid, gid FROM channels")
                         if r["gid"] in config.SYNC_CHGIDS] or None

        # 2. 番組（配信予定）
        stamp = config.fmt_dt(now)
        tids: set[int] = set()

        def save(progs):
            nonlocal total
            for p in progs:
                p["updated_at"] = stamp
            with db.connect() as conn:
                conn.executemany(
                    "INSERT INTO programs(pid,tid,chid,start_at,end_at,count,subtitle,flag,deleted,updated_at) "
                    "VALUES(:pid,:tid,:chid,:start_at,:end_at,:count,:subtitle,:flag,:deleted,:updated_at) "
                    "ON CONFLICT(pid) DO UPDATE SET tid=excluded.tid, chid=excluded.chid, "
                    "start_at=excluded.start_at, end_at=excluded.end_at, count=excluded.count, "
                    "subtitle=excluded.subtitle, flag=excluded.flag, deleted=excluded.deleted, "
                    "updated_at=excluded.updated_at", progs)
            tids.update(p["tid"] for p in progs)
            total += len(progs)

        if mode == "diff":
            STATE["progress"] = "番組取得（更新分）"
            save(client.programs_updated(since, now, chids))
        else:
            cur = start
            while cur < end:
                nxt = min(cur + timedelta(days=config.SYNC_CHUNK_DAYS), end)
                STATE["progress"] = f"番組取得 {cur:%Y-%m-%d}〜{nxt:%m-%d}"
                save(client.programs(cur, nxt - timedelta(seconds=1), chids))
                cur = nxt

        # 3. タイトル: 未取得のもの＋（差分取得時は）更新されたもの
        with db.connect() as conn:
            known = {r["tid"] for r in conn.execute("SELECT tid FROM titles")}
        need = set(tids - known)
        if mode == "diff":
            STATE["progress"] = "タイトル更新確認"
            need |= client.titles_updated(since) & known
        need = sorted(need)
        n_titles = len(need)
        for i, batch in enumerate(_chunks(need, 100)):
            STATE["progress"] = f"タイトル取得 {i * 100 + len(batch)}/{len(need)}"
            titles = client.titles(batch)
            for t in titles:
                t["updated_at"] = stamp
            with db.connect() as conn:
                conn.executemany(
                    "INSERT INTO titles(tid,title,cat,first_year,first_month,updated_at) "
                    "VALUES(:tid,:title,:cat,:first_year,:first_month,:updated_at) "
                    "ON CONFLICT(tid) DO UPDATE SET title=excluded.title, cat=excluded.cat, "
                    "first_year=excluded.first_year, first_month=excluded.first_month, "
                    "updated_at=excluded.updated_at", titles)

        # 4. 登録作品へ反映
        STATE["progress"] = "登録作品へ反映"
        with db.connect() as conn:
            apply_all_tracks(conn)
            db.set_meta(conn, "last_success", stamp)
            if mode != "range":  # 期間指定の取得では差分の基準時刻を進めない（余裕を見て5分前）
                db.set_meta(conn, "prog_since", (now - timedelta(minutes=5)).strftime("%Y-%m-%dT%H:%M:%S"))
        kind = {"diff": "差分", "full": "初回", "range": "期間指定"}[mode]
        STATE["last_message"] = f"{kind}: 番組 {total} 件 / 作品 {len(tids)} 件 / タイトル {n_titles} 件を取得（{label_s}〜{label_e}）"
        log.info(STATE["last_message"])
        _finish_log(log_id, "ok", total, n_titles, STATE["last_message"])
        ok = True
    except Exception as e:
        log.exception("sync failed")
        STATE["last_error"] = str(e)
        _finish_log(log_id, "error", total, n_titles, str(e))
    finally:
        client.close()
        try:
            _record_result(ok, trigger)
        except Exception:
            log.exception("failed to record result")
        STATE.update(running=False, progress=None, last_finished=config.fmt_dt(config.now_jst()))
        _lock.release()
    return True


def trigger_async(start_date: datetime | None = None, end_date: datetime | None = None, trigger: str = "manual") -> bool:
    if _lock.locked():
        return False
    STATE.update(running=True, progress="開始")
    threading.Thread(target=run_sync, args=(start_date, end_date, trigger), name="sync", daemon=True).start()
    return True


def _scheduler_loop():
    first = True
    while True:
        try:
            with db.connect() as conn:
                last = db.get_meta(conn, "last_success")
                paused = db.get_meta(conn, "auto_paused", "0") == "1"
                fails = int(db.get_meta(conn, "fail_count", 0) or 0)
                last_fail = db.get_meta(conn, "last_fail_at")
            now = config.now_jst()
            due = last is None
            if last and not due:
                elapsed = now - datetime.strptime(last, "%Y-%m-%dT%H:%M")
                due = elapsed >= timedelta(hours=config.SYNC_INTERVAL_HOURS)
            if due and fails and last_fail:  # 失敗後は待ち時間を倍々に延ばす（10分→20分→…最大 SYNC_INTERVAL_HOURS）
                wait = min(timedelta(minutes=10 * 2 ** (fails - 1)), timedelta(hours=config.SYNC_INTERVAL_HOURS))
                due = now - datetime.strptime(last_fail, "%Y-%m-%dT%H:%M") >= wait
            if paused:
                due = False  # 連続失敗で停止中。画面から手動で取得して成功すると再開する
            if due and (not first or config.SYNC_ON_STARTUP):
                run_sync(trigger="startup" if first else "auto")
        except Exception:
            log.exception("scheduler error")
        first = False
        time.sleep(random.uniform(540, 660))  # 実行時刻が特定の時刻に揃わないよう揺らす


def start_scheduler():
    if config.SYNC_INTERVAL_HOURS <= 0:
        return
    threading.Thread(target=_scheduler_loop, name="scheduler", daemon=True).start()
