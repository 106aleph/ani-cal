"""しょぼいカレンダーからの取得と、登録作品への反映"""
import logging
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
def run_sync() -> bool:
    if not _lock.acquire(blocking=False):
        return False
    STATE.update(running=True, progress="開始", last_started=config.fmt_dt(config.now_jst()),
                 last_error=None)
    client = SyoboiClient()
    try:
        with db.connect() as conn:
            last_full = db.get_meta(conn, "last_full_success")

        # 1. チャンネル（媒体）
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
        chids = None
        if config.SYNC_CHGIDS:
            chids = [c["chid"] for c in chans if c["gid"] in config.SYNC_CHGIDS] or None

        # 2. 番組（配信予定）
        now = config.now_jst()
        back = config.SYNC_DAYS_BACK_INCREMENTAL if last_full else config.SYNC_DAYS_BACK
        start = (now - timedelta(days=back)).replace(hour=0, minute=0, second=0)
        end = now + timedelta(days=config.SYNC_DAYS_FORWARD)
        tids, total, cur = set(), 0, start
        stamp = config.fmt_dt(now)
        while cur < end:
            nxt = min(cur + timedelta(days=config.SYNC_CHUNK_DAYS), end)
            STATE["progress"] = f"番組取得 {cur:%Y-%m-%d}〜{nxt:%m-%d}"
            progs = client.programs(cur, nxt - timedelta(seconds=1), chids)
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
            cur = nxt

        # 3. タイトル（未取得 or 7日以上前に取得したもの）
        with db.connect() as conn:
            known = {r["tid"]: r["updated_at"] or "" for r in conn.execute("SELECT tid, updated_at FROM titles")}
        stale_before = config.fmt_dt(now - timedelta(days=7))
        need = sorted(t for t in tids if t not in known or known[t] < stale_before)
        for i, batch in enumerate(_chunks(need, 50)):
            STATE["progress"] = f"タイトル取得 {i * 50 + len(batch)}/{len(need)}"
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
            db.set_meta(conn, "last_full_success", stamp)
        STATE["last_message"] = f"番組 {total} 件 / 作品 {len(tids)} 件を取得（{start:%m/%d}〜{end:%m/%d}）"
        log.info(STATE["last_message"])
    except Exception as e:
        log.exception("sync failed")
        STATE["last_error"] = str(e)
    finally:
        client.close()
        STATE.update(running=False, progress=None, last_finished=config.fmt_dt(config.now_jst()))
        _lock.release()
    return True


def trigger_async() -> bool:
    if _lock.locked():
        return False
    STATE.update(running=True, progress="開始")
    threading.Thread(target=run_sync, name="sync", daemon=True).start()
    return True


def _scheduler_loop():
    first = True
    while True:
        try:
            with db.connect() as conn:
                last = db.get_meta(conn, "last_success")
            due = last is None
            if last and not due:
                elapsed = config.now_jst() - datetime.strptime(last, "%Y-%m-%dT%H:%M")
                due = elapsed >= timedelta(hours=config.SYNC_INTERVAL_HOURS)
            if due and (not first or config.SYNC_ON_STARTUP):
                run_sync()
        except Exception:
            log.exception("scheduler error")
        first = False
        time.sleep(600)


def start_scheduler():
    if config.SYNC_INTERVAL_HOURS <= 0:
        return
    threading.Thread(target=_scheduler_loop, name="scheduler", daemon=True).start()
