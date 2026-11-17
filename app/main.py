"""アニメ配信カレンダー API"""
import logging
from contextlib import asynccontextmanager
from datetime import datetime, timedelta
from pathlib import Path
from zoneinfo import ZoneInfo

from fastapi import FastAPI, HTTPException, Query
from fastapi.responses import FileResponse, Response
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field

import config
import db
import notify
import sync

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
STATIC = Path(__file__).parent / "static"
SEASONS = ["冬", "春", "夏", "秋"]


@asynccontextmanager
async def lifespan(_app):
    db.init()
    if not config.SYOBOI_UA_URL:
        logging.getLogger("app").warning(
            "SYOBOI_UA_URL が未設定です。連絡方法を載せた公開ページのURLを .env に設定してください（README 参照）")
    sync.start_scheduler()
    notify.start()
    yield


app = FastAPI(title="アニメ配信カレンダー", lifespan=lifespan)
app.include_router(notify.router)


# ---------------------------------------------------------------- helpers
def norm_dt(s):
    if s is None:
        return None
    s = str(s).strip().replace(" ", "T")
    if not s:
        return None
    for fmt in ("%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"):
        try:
            return datetime.strptime(s, fmt).strftime("%Y-%m-%dT%H:%M")
        except ValueError:
            pass
    raise HTTPException(422, f"日時の形式が不正です: {s}")


def season_of(year, month):
    if not year or not month:
        return None
    return f"{year}年{SEASONS[(month - 1) // 3]}"


def is_net_group(gid, gname) -> bool:
    if gid is not None and gid in config.NET_GROUP_IDS:
        return True
    g = (gname or "").lower()
    return any(k.lower() in g for k in config.NET_GROUP_KEYWORDS)


TRACK_SELECT = """
SELECT tr.*,
  COALESCE(NULLIF(tr.title,''), t.title, '(無題)') AS display_title,
  t.title AS orig_title,
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
"""


def get_track(conn, track_id: int) -> dict:
    row = conn.execute(TRACK_SELECT + " WHERE tr.id=:id",
                       {"now": config.fmt_dt(config.now_jst()), "id": track_id}).fetchone()
    if not row:
        raise HTTPException(404, "作品が見つかりません")
    return dict(row)


def get_episode(conn, ep_id: int):
    row = conn.execute("SELECT * FROM episodes WHERE id=?", (ep_id,)).fetchone()
    if not row:
        raise HTTPException(404, "エピソードが見つかりません")
    return row


def generate_episodes(conn, track_id, base_number, base_start, until, interval_days, source):
    base_dt = datetime.strptime(base_start, "%Y-%m-%dT%H:%M")
    created = 0
    now = config.fmt_dt(config.now_jst())
    for n in range(int(base_number) + 1, int(until) + 1):
        if conn.execute("SELECT 1 FROM episodes WHERE track_id=? AND number=?",
                        (track_id, float(n))).fetchone():
            continue
        st = base_dt + timedelta(days=interval_days * (n - base_number))
        conn.execute(
            "INSERT INTO episodes(track_id, number, start_at, source, edited, updated_at) "
            "VALUES (?,?,?,?,?,?)",
            (track_id, float(n), config.fmt_dt(st), source, 1 if source == "manual" else 0, now))
        created += 1
    return created


# ---------------------------------------------------------------- models
MAX_EPISODES = 500        # 手動作成・予測で作れる話数の上限
MAX_INTERVAL_DAYS = 365   # 話の間隔（日）の上限


class TrackCreate(BaseModel):
    tid: int | None = None
    chid: int | None = None
    title: str | None = None
    platform: str | None = None
    first_start: str | None = None
    episodes: int | None = Field(None, ge=1, le=MAX_EPISODES)
    interval_days: int = Field(7, ge=1, le=MAX_INTERVAL_DAYS)


class TrackUpdate(BaseModel):
    title: str | None = None
    platform: str | None = None
    chid: int | None = None
    auto_sync: bool | None = None
    note: str | None = None


class EpisodeIn(BaseModel):
    number: float | None = None
    subtitle: str | None = None
    start_at: str | None = None
    watched: bool | None = None
    hidden: bool | None = None


class GenerateIn(BaseModel):
    until: int = Field(..., ge=1, le=MAX_EPISODES)
    interval_days: int = Field(7, ge=1, le=MAX_INTERVAL_DAYS)


class BulkWatchIn(BaseModel):
    watched: bool = True
    until_now: bool = True


# ---------------------------------------------------------------- status / sync
@app.get("/api/health")
def health():
    return {"ok": True}


@app.get("/api/status")
def status():
    with db.connect() as conn:
        counts = {k: conn.execute(f"SELECT COUNT(*) FROM {k}").fetchone()[0]
                  for k in ("titles", "programs", "channels", "tracks", "episodes")}
        return {**sync.STATE, "last_success": db.get_meta(conn, "last_success"),
                "auto_paused": db.get_meta(conn, "auto_paused", "0") == "1",
                "counts": counts, "now": config.fmt_dt(config.now_jst())}


class SyncIn(BaseModel):
    start: str | None = None  # YYYY-MM-DD
    end: str | None = None    # YYYY-MM-DD（この日を含む）


MAX_SYNC_DAYS = 90  # 一度に取得できる期間の上限（自主的な制限。しょぼいカレンダーの規定値ではない）


@app.post("/api/sync", status_code=202)
def trigger_sync(body: SyncIn | None = None):
    s = e = None
    if body and (body.start or body.end):
        try:
            s = datetime.strptime(body.start, "%Y-%m-%d")
            e = datetime.strptime(body.end, "%Y-%m-%d") + timedelta(days=1)
        except (TypeError, ValueError):
            raise HTTPException(422, "開始日と終了日を YYYY-MM-DD で指定してください")
        if e <= s:
            raise HTTPException(422, "終了日は開始日以降にしてください")
        if (e - s).days > MAX_SYNC_DAYS:
            raise HTTPException(422, f"期間は最大 {MAX_SYNC_DAYS} 日までです")
    return {"started": sync.trigger_async(s, e, "range" if s else "manual")}


@app.get("/api/logs")
def sync_logs(limit: int = Query(200, ge=1, le=500)):
    with db.connect() as conn:
        return [dict(r) for r in conn.execute(
            "SELECT * FROM sync_log ORDER BY id DESC LIMIT ?", (limit,))]


@app.post("/api/sync/cancel")
def cancel_sync():
    return {"cancelled": sync.cancel()}


# ---------------------------------------------------------------- 新作一覧
@app.get("/api/catalog")
def catalog():
    now = config.now_jst()
    idx_now = now.year * 12 + now.month - 1
    lo, hi = idx_now - config.NEW_MONTHS_BACK, idx_now + config.NEW_MONTHS_FORWARD
    mask = 8 if config.SKIP_REBROADCAST else 0
    with db.connect() as conn:
        rows = conn.execute(
            """SELECT t.tid, t.title, t.cat, t.first_year, t.first_month,
                      p.chid, c.name AS chname, c.gid, g.name AS gname,
                      MIN(p.start_at) AS first_start, MAX(p.start_at) AS last_start,
                      MIN(CASE WHEN p.count=1 THEN p.start_at END) AS ep1_start,
                      COUNT(*) AS n
               FROM programs p
               JOIN titles t ON t.tid=p.tid
               LEFT JOIN channels c ON c.chid=p.chid
               LEFT JOIN channel_groups g ON g.gid=c.gid
               WHERE p.deleted=0 AND (p.flag & ?)=0
               GROUP BY p.tid, p.chid""", (mask,)).fetchall()
        tracked = {(r["tid"], r["chid"]): (r["id"], bool(r["archived"])) for r in
                   conn.execute("SELECT id, tid, chid, archived FROM tracks WHERE tid IS NOT NULL")}

    items: dict[int, dict] = {}
    for r in rows:
        if config.NEW_CATS and r["cat"] not in config.NEW_CATS:
            continue
        it = items.setdefault(r["tid"], {
            "tid": r["tid"], "title": r["title"],
            "first_year": r["first_year"], "first_month": r["first_month"],
            "channels": [],
        })
        it["channels"].append({
            "chid": r["chid"],
            "name": r["chname"] or f"ChID:{r['chid']}",
            "group": r["gname"],
            "is_net": is_net_group(r["gid"], r["gname"]),
            "first_start": r["ep1_start"] or r["first_start"],
            "last_start": r["last_start"],
            "has_ep1": r["ep1_start"] is not None,
            "count": r["n"],
            "track_id": tracked.get((r["tid"], r["chid"]), (None, False))[0],
            "track_archived": tracked.get((r["tid"], r["chid"]), (None, False))[1],
        })

    out = []
    for it in items.values():
        earliest = min(c["first_start"] for c in it["channels"])
        y, m = it["first_year"], it["first_month"]
        if not (y and m):
            y, m = int(earliest[:4]), int(earliest[5:7])
        idx = y * 12 + m - 1
        # 放送開始が範囲内、または「まだ放送中」（今以降の予定がある）なら、開始が古い分割・長期クールも載せる
        airing = max(c["last_start"] for c in it["channels"]) >= config.fmt_dt(now)
        if not (lo <= idx <= hi or (airing and idx_now - config.NEW_MONTHS_BACK_AIRING <= idx <= hi)):
            continue
        it["season"] = season_of(y, m)
        it["season_key"] = y * 10 + (m - 1) // 3
        it["first_start"] = earliest
        it["channels"].sort(key=lambda c: (not c["is_net"], c["first_start"]))
        out.append(it)
    out.sort(key=lambda x: (-x["season_key"], x["first_start"]))

    seasons = sorted({(x["season_key"], x["season"]) for x in out}, reverse=True)
    plat: dict[str, dict] = {}
    for x in out:
        for c in x["channels"]:
            p = plat.setdefault(c["name"], {"name": c["name"], "is_net": c["is_net"], "titles": 0})
            p["titles"] += 1
    platforms = sorted(plat.values(), key=lambda p: (not p["is_net"], -p["titles"], p["name"]))
    return {
        "items": out,
        "seasons": [{"key": k, "label": lbl} for k, lbl in seasons],
        "current_season_key": now.year * 10 + (now.month - 1) // 3,
        "platforms": platforms,
    }


@app.get("/api/titles/{tid}/channels")
def title_channels(tid: int):
    with db.connect() as conn:
        rows = conn.execute(
            """SELECT p.chid, c.name, MIN(p.start_at) AS first_start, COUNT(*) AS n
               FROM programs p LEFT JOIN channels c ON c.chid=p.chid
               WHERE p.tid=? AND p.deleted=0 GROUP BY p.chid ORDER BY first_start""",
            (tid,)).fetchall()
    return [dict(r) for r in rows]


# ---------------------------------------------------------------- 登録作品
@app.get("/api/tracks")
def list_tracks(archived: bool = False):
    """archived=false: 登録作品 / archived=true: 視聴済みに移した作品"""
    order = "tr.archived_at DESC, tr.id DESC" if archived else "COALESCE(next_start, '9999'), tr.id DESC"
    with db.connect() as conn:
        rows = conn.execute(TRACK_SELECT + f" WHERE tr.archived=:arch ORDER BY {order}",
                            {"now": config.fmt_dt(config.now_jst()), "arch": 1 if archived else 0}).fetchall()
    return [dict(r) for r in rows]


@app.post("/api/tracks/{track_id}/archive")
def archive_track(track_id: int):
    """全話視聴済みの作品を「視聴済み」へ移す"""
    with db.connect() as conn:
        tr = get_track(conn, track_id)
        if tr["ep_total"] == 0 or tr["ep_watched"] < tr["ep_total"]:
            raise HTTPException(409, "全話を視聴済みにしてから移してください")
        conn.execute("UPDATE tracks SET archived=1, archived_at=? WHERE id=?",
                     (config.fmt_dt(config.now_jst()), track_id))
        return get_track(conn, track_id)


@app.post("/api/tracks/{track_id}/unarchive")
def unarchive_track(track_id: int):
    """視聴済みから登録作品へ戻す"""
    with db.connect() as conn:
        get_track(conn, track_id)
        conn.execute("UPDATE tracks SET archived=0, archived_at=NULL WHERE id=?", (track_id,))
        return get_track(conn, track_id)


@app.post("/api/tracks", status_code=201)
def create_track(body: TrackCreate):
    now = config.fmt_dt(config.now_jst())
    with db.connect() as conn:
        if body.tid is not None and body.chid is not None:
            ex = conn.execute("SELECT id FROM tracks WHERE tid=? AND chid=?",
                              (body.tid, body.chid)).fetchone()
            if ex:
                raise HTTPException(409, f"登録済みです (id={ex['id']})")
            cur = conn.execute(
                "INSERT INTO tracks(tid, chid, title, platform, created_at) VALUES (?,?,?,?,?)",
                (body.tid, body.chid, body.title or None, body.platform or None, now))
            tid = cur.lastrowid
            sync.apply_track(conn, tid)
        else:
            if not (body.title or "").strip():
                raise HTTPException(422, "タイトルを入力してください")
            cur = conn.execute(
                "INSERT INTO tracks(title, platform, auto_sync, created_at) VALUES (?,?,0,?)",
                (body.title.strip(), (body.platform or "").strip() or None, now))
            tid = cur.lastrowid
            first = norm_dt(body.first_start)
            if first:
                conn.execute(
                    "INSERT INTO episodes(track_id, number, start_at, source, edited, updated_at) "
                    "VALUES (?,1,?,'manual',1,?)", (tid, first, now))
                if body.episodes and body.episodes > 1:
                    generate_episodes(conn, tid, 1, first, body.episodes,
                                      body.interval_days or 7, "manual")
        return get_track(conn, tid)


@app.patch("/api/tracks/{track_id}")
def update_track(track_id: int, body: TrackUpdate):
    data = body.model_dump(exclude_unset=True)
    with db.connect() as conn:
        tr = get_track(conn, track_id)
        sets, args = [], []
        for k in ("title", "platform", "note"):
            if k in data:
                v = (data[k] or "").strip() or None
                sets.append(f"{k}=?")
                args.append(v)
        if "auto_sync" in data and data["auto_sync"] is not None:
            sets.append("auto_sync=?")
            args.append(1 if data["auto_sync"] else 0)
        chid_changed = "chid" in data and data["chid"] is not None and data["chid"] != tr["chid"]
        if chid_changed:
            if tr["tid"] is None:
                raise HTTPException(400, "手動登録の作品は媒体(チャンネル)を変更できません。媒体名を編集してください")
            if conn.execute("SELECT 1 FROM tracks WHERE tid=? AND chid=? AND id<>?",
                            (tr["tid"], data["chid"], track_id)).fetchone():
                raise HTTPException(409, "その媒体は既に登録されています")
            sets.append("chid=?")
            args.append(data["chid"])
            if "platform" not in data:
                sets.append("platform=NULL")
        if sets:
            conn.execute(f"UPDATE tracks SET {', '.join(sets)} WHERE id=?", (*args, track_id))
        if chid_changed:
            # 旧媒体の未編集・未視聴データを消して新媒体から取り直す（視聴記録は話数で引き継ぐ）
            conn.execute(
                "DELETE FROM episodes WHERE track_id=? AND source IN ('sync','generated') "
                "AND edited=0 AND watched=0 AND hidden=0", (track_id,))
            sync.apply_track(conn, track_id)
        return get_track(conn, track_id)


@app.delete("/api/tracks/{track_id}", status_code=204)
def delete_track(track_id: int):
    with db.connect() as conn:
        get_track(conn, track_id)
        conn.execute("DELETE FROM tracks WHERE id=?", (track_id,))
    return Response(status_code=204)


@app.post("/api/tracks/{track_id}/resync")
def resync_track(track_id: int):
    with db.connect() as conn:
        get_track(conn, track_id)
        n = sync.apply_track(conn, track_id)
        return {"programs": n, "track": get_track(conn, track_id)}


@app.post("/api/tracks/{track_id}/generate")
def generate(track_id: int, body: GenerateIn):
    if body.interval_days < 1:
        raise HTTPException(422, "間隔は1日以上にしてください")
    with db.connect() as conn:
        get_track(conn, track_id)
        base = conn.execute(
            "SELECT * FROM episodes WHERE track_id=? AND hidden=0 AND number IS NOT NULL "
            "AND start_at IS NOT NULL ORDER BY number DESC LIMIT 1", (track_id,)).fetchone()
        if not base:
            raise HTTPException(400, "基準となる話（話数と日時のある話）がありません")
        if body.until <= base["number"]:
            raise HTTPException(400, f"最終話数は {int(base['number']) + 1} 以上にしてください")
        n = generate_episodes(conn, track_id, base["number"], base["start_at"],
                              body.until, body.interval_days, "generated")
        return {"created": n}


@app.post("/api/tracks/{track_id}/watch_all")
def watch_all(track_id: int, body: BulkWatchIn):
    now = config.fmt_dt(config.now_jst())
    with db.connect() as conn:
        get_track(conn, track_id)
        q = "UPDATE episodes SET watched=?, watched_at=? WHERE track_id=? AND hidden=0"
        args = [1 if body.watched else 0, now if body.watched else None, track_id]
        if body.until_now:
            q += " AND start_at<=?"
            args.append(now)
        n = conn.execute(q, args).rowcount
        return {"updated": n}


# ---------------------------------------------------------------- エピソード
@app.get("/api/tracks/{track_id}/episodes")
def list_episodes(track_id: int):
    with db.connect() as conn:
        get_track(conn, track_id)
        rows = conn.execute(
            """SELECT e.*, p.start_at AS orig_start_at, p.subtitle AS orig_subtitle, p.count AS orig_number
               FROM episodes e LEFT JOIN programs p ON p.pid=e.pid
               WHERE e.track_id=?
               ORDER BY e.number IS NULL, e.number, e.start_at""", (track_id,)).fetchall()
    return [dict(r) for r in rows]


@app.post("/api/tracks/{track_id}/episodes", status_code=201)
def add_episode(track_id: int, body: EpisodeIn):
    now = config.fmt_dt(config.now_jst())
    with db.connect() as conn:
        get_track(conn, track_id)
        if body.number is not None and conn.execute(
                "SELECT 1 FROM episodes WHERE track_id=? AND number=?",
                (track_id, body.number)).fetchone():
            raise HTTPException(409, "同じ話数が既にあります")
        cur = conn.execute(
            "INSERT INTO episodes(track_id, number, subtitle, start_at, watched, watched_at, source, edited, updated_at) "
            "VALUES (?,?,?,?,?,?,'manual',1,?)",
            (track_id, body.number, (body.subtitle or "").strip() or None, norm_dt(body.start_at),
             1 if body.watched else 0, now if body.watched else None, now))
        return dict(get_episode(conn, cur.lastrowid))


@app.patch("/api/episodes/{ep_id}")
def update_episode(ep_id: int, body: EpisodeIn):
    data = body.model_dump(exclude_unset=True)
    now = config.fmt_dt(config.now_jst())
    with db.connect() as conn:
        e = get_episode(conn, ep_id)
        sets, args, edited = [], [], False
        if "number" in data and data["number"] != e["number"]:
            if data["number"] is not None and conn.execute(
                    "SELECT 1 FROM episodes WHERE track_id=? AND number=? AND id<>?",
                    (e["track_id"], data["number"], ep_id)).fetchone():
                raise HTTPException(409, "同じ話数が既にあります")
            sets.append("number=?"); args.append(data["number"]); edited = True
        if "subtitle" in data:
            v = (data["subtitle"] or "").strip() or None
            if v != e["subtitle"]:
                sets.append("subtitle=?"); args.append(v); edited = True
        if "start_at" in data:
            v = norm_dt(data["start_at"])
            if v != e["start_at"]:
                sets.append("start_at=?"); args.append(v); edited = True
        if "watched" in data and data["watched"] is not None:
            sets += ["watched=?", "watched_at=?"]
            args += [1 if data["watched"] else 0, now if data["watched"] else None]
        if "hidden" in data and data["hidden"] is not None:
            sets.append("hidden=?"); args.append(1 if data["hidden"] else 0)
        if edited:
            sets.append("edited=1")
        if sets:
            sets.append("updated_at=?"); args.append(now)
            conn.execute(f"UPDATE episodes SET {', '.join(sets)} WHERE id=?", (*args, ep_id))
        return dict(get_episode(conn, ep_id))


@app.post("/api/episodes/{ep_id}/revert")
def revert_episode(ep_id: int):
    """手動修正を取り消し、取得データの値に戻す"""
    with db.connect() as conn:
        e = get_episode(conn, ep_id)
        if e["pid"] is None:
            raise HTTPException(400, "取得データと紐付いていないため戻せません")
        p = conn.execute("SELECT * FROM programs WHERE pid=?", (e["pid"],)).fetchone()
        if not p:
            raise HTTPException(400, "元の取得データが見つかりません")
        conn.execute(
            "UPDATE episodes SET number=?, subtitle=?, start_at=?, edited=0, source='sync', updated_at=? WHERE id=?",
            (None if p["count"] is None else float(p["count"]), p["subtitle"], p["start_at"],
             config.fmt_dt(config.now_jst()), ep_id))
        return dict(get_episode(conn, ep_id))


@app.delete("/api/episodes/{ep_id}", status_code=204)
def delete_episode(ep_id: int):
    with db.connect() as conn:
        e = get_episode(conn, ep_id)
        if e["source"] == "manual" and e["pid"] is None:
            conn.execute("DELETE FROM episodes WHERE id=?", (ep_id,))
        else:  # 取得/予測データは非表示にして再取得で復活しないようにする
            conn.execute("UPDATE episodes SET hidden=1 WHERE id=?", (ep_id,))
    return Response(status_code=204)


# ---------------------------------------------------------------- カレンダー
EP_CAL_SELECT = """
SELECT e.*, tr.id AS track_id,
  COALESCE(NULLIF(tr.title,''), t.title, '(無題)') AS title,
  COALESCE(NULLIF(tr.platform,''), c.name, '') AS platform,
  p.end_at AS end_at
FROM episodes e
JOIN tracks tr ON tr.id=e.track_id
LEFT JOIN titles t ON t.tid=tr.tid
LEFT JOIN channels c ON c.chid=tr.chid
LEFT JOIN programs p ON p.pid=e.pid
WHERE e.hidden=0 AND e.start_at IS NOT NULL AND tr.archived=0
"""


@app.get("/api/calendar")
def calendar(start: str = Query(...), end: str = Query(...)):
    s, e = norm_dt(start + "T00:00" if len(start) == 10 else start), \
        norm_dt(end + "T00:00" if len(end) == 10 else end)
    with db.connect() as conn:
        rows = conn.execute(EP_CAL_SELECT + " AND e.start_at>=? AND e.start_at<? ORDER BY e.start_at",
                            (s, e)).fetchall()
    return [dict(r) for r in rows]


@app.get("/api/backlog")
def backlog():
    """配信済みで未視聴のエピソード"""
    now = config.fmt_dt(config.now_jst())
    with db.connect() as conn:
        rows = conn.execute(EP_CAL_SELECT + " AND e.watched=0 AND e.start_at<=? ORDER BY e.start_at",
                            (now,)).fetchall()
    return [dict(r) for r in rows]


def _ics_escape(s: str) -> str:
    return (s or "").replace("\\", "\\\\").replace(";", "\\;").replace(",", "\\,").replace("\n", "\\n")


def _ics_fold(line: str) -> str:
    out, buf = [], b""
    for ch in line:
        b = ch.encode()
        if len(buf) + len(b) > 73:
            out.append(buf.decode())
            buf = b" "
        buf += b
    out.append(buf.decode())
    return "\r\n".join(out)


@app.get("/calendar.ics")
def ics(unwatched_only: bool = False):
    """他のカレンダーアプリ購読用（tailnet 内からのみ到達可能）"""
    utc = ZoneInfo("UTC")
    q = EP_CAL_SELECT + (" AND e.watched=0" if unwatched_only else "") + " ORDER BY e.start_at"
    with db.connect() as conn:
        rows = conn.execute(q).fetchall()
    stamp = datetime.now(utc).strftime("%Y%m%dT%H%M%SZ")
    lines = ["BEGIN:VCALENDAR", "VERSION:2.0", "PRODID:-//anime-calendar//JA",
             "CALSCALE:GREGORIAN", "X-WR-CALNAME:アニメ配信カレンダー"]
    for r in rows:
        st = datetime.strptime(r["start_at"], "%Y-%m-%dT%H:%M").replace(tzinfo=config.TZ)
        ed = (datetime.strptime(r["end_at"], "%Y-%m-%dT%H:%M").replace(tzinfo=config.TZ)
              if r["end_at"] and not r["edited"] else st + timedelta(minutes=30))
        num = "" if r["number"] is None else f" #{r['number']:g}"
        summary = f"{'✓ ' if r['watched'] else ''}{r['title']}{num}"
        desc = " / ".join(x for x in [r["platform"], r["subtitle"]] if x)
        lines += ["BEGIN:VEVENT", f"UID:ep-{r['id']}@anime-calendar", f"DTSTAMP:{stamp}",
                  f"DTSTART:{st.astimezone(utc):%Y%m%dT%H%M%SZ}",
                  f"DTEND:{ed.astimezone(utc):%Y%m%dT%H%M%SZ}",
                  _ics_fold(f"SUMMARY:{_ics_escape(summary)}"),
                  _ics_fold(f"DESCRIPTION:{_ics_escape(desc)}"),
                  _ics_fold(f"LOCATION:{_ics_escape(r['platform'])}"),
                  "END:VEVENT"]
    lines.append("END:VCALENDAR")
    return Response("\r\n".join(lines) + "\r\n", media_type="text/calendar; charset=utf-8")


# ---------------------------------------------------------------- static
app.mount("/static", StaticFiles(directory=STATIC), name="static")


@app.get("/sw.js")
def service_worker():
    # Service Worker は scope を「/」にするためルートから配信する
    return FileResponse(STATIC / "sw.js", media_type="text/javascript", headers={"Cache-Control": "no-cache"})


@app.get("/")
def index():
    return FileResponse(STATIC / "index.html", headers={"Cache-Control": "no-cache"})
