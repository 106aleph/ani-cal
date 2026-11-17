"""Web Push 通知: 配信開始の N 分前にブラウザ/PWA へ通知する

- 購読（端末ごと）と「何分前に通知するか」は push_subs テーブルに保存
- VAPID 鍵は初回起動時に生成して meta テーブルに保存
- 通知ループが30秒ごとに「開始 N 分前〜開始3分後」の未視聴の話を探して送信する（送信済みは push_sent で重複防止）
"""
import json
import logging
import threading
import time
from datetime import datetime, timedelta
from urllib.parse import urlparse

from cryptography.hazmat.primitives import serialization
from fastapi import APIRouter, HTTPException, Query
from pydantic import BaseModel, Field
from py_vapid import Vapid02
from pywebpush import WebPushException, webpush

import config
import db

log = logging.getLogger("notify")
router = APIRouter(prefix="/api/push")

DEFAULT_LEAD_MIN = 10
MAX_LEAD_MIN = 1440
GRACE_MIN = 3  # 開始時刻を過ぎてからでも、この分数以内なら通知する（取得や再起動の遅れ対策）
# 購読先として許可する Push サービス（任意URLへサーバーから送信しないため）
PUSH_HOSTS = ("googleapis.com", "push.services.mozilla.com", "push.apple.com", "notify.windows.com")

_lock = threading.Lock()


def _b64(b: bytes) -> str:
    import base64
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _vapid() -> Vapid02:
    """保存済みの VAPID 鍵を返す（無ければ生成）"""
    with _lock, db.connect() as conn:
        pem = db.get_meta(conn, "vapid_private")
        if pem:
            v = Vapid02.from_pem(pem.encode())
            v.conf["no-strict"] = True  # sub に https://host/path 形式のURLを許可（既定の検査は path を拒否する）
            return v
        v = Vapid02()
        v.conf["no-strict"] = True
        v.generate_keys()
        db.set_meta(conn, "vapid_private",
                    v.private_pem().decode())
        return v


def public_key() -> str:
    raw = _vapid().public_key.public_bytes(
        serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint)
    return _b64(raw)


def _subject() -> str:
    return config.PUSH_SUBJECT


def _check_endpoint(endpoint: str) -> None:
    u = urlparse(endpoint)
    host = u.hostname or ""
    if u.scheme != "https" or not any(host == h or host.endswith("." + h) for h in PUSH_HOSTS):
        raise HTTPException(422, "対応していない通知サービスです")


def _send(sub: dict, payload: dict, ttl: int = 600) -> None:
    webpush(
        subscription_info={"endpoint": sub["endpoint"], "keys": {"p256dh": sub["p256dh"], "auth": sub["auth"]}},
        data=json.dumps(payload, ensure_ascii=False),
        vapid_private_key=_vapid(), vapid_claims={"sub": _subject()},
        # Urgency: high … 端末が省電力（Doze）中でも即時配信するよう Push サービスに要求する（時刻通知なので）
        ttl=ttl, timeout=10, headers={"Urgency": "high"})


def _drop(endpoint: str) -> None:
    with db.connect() as conn:
        conn.execute("DELETE FROM push_sent WHERE endpoint=?", (endpoint,))
        conn.execute("DELETE FROM push_subs WHERE endpoint=?", (endpoint,))


def _gone(e: WebPushException) -> bool:
    return getattr(e.response, "status_code", None) in (404, 410)


# ---------------------------------------------------------------- API
class SubIn(BaseModel):
    endpoint: str
    keys: dict[str, str]
    lead_min: int = Field(DEFAULT_LEAD_MIN, ge=0, le=MAX_LEAD_MIN)


class EndpointIn(BaseModel):
    endpoint: str


class LeadIn(BaseModel):
    endpoint: str
    lead_min: int = Field(..., ge=0, le=MAX_LEAD_MIN)


@router.get("/key")
def get_key():
    return {"key": public_key()}


@router.get("/settings")
def get_settings(endpoint: str = Query(...)):
    with db.connect() as conn:
        r = conn.execute("SELECT lead_min FROM push_subs WHERE endpoint=?", (endpoint,)).fetchone()
    return {"subscribed": r is not None, "lead_min": r["lead_min"] if r else DEFAULT_LEAD_MIN}


@router.post("/subscribe")
def subscribe(body: SubIn):
    _check_endpoint(body.endpoint)
    p256dh, auth = body.keys.get("p256dh"), body.keys.get("auth")
    if not p256dh or not auth:
        raise HTTPException(422, "購読情報が不正です")
    with db.connect() as conn:
        conn.execute(
            "INSERT INTO push_subs(endpoint,p256dh,auth,lead_min,created_at) VALUES(?,?,?,?,?) "
            "ON CONFLICT(endpoint) DO UPDATE SET p256dh=excluded.p256dh, auth=excluded.auth, lead_min=excluded.lead_min",
            (body.endpoint, p256dh, auth, body.lead_min, config.fmt_dt(config.now_jst())))
    return {"subscribed": True, "lead_min": body.lead_min}


@router.post("/unsubscribe")
def unsubscribe(body: EndpointIn):
    _drop(body.endpoint)
    return {"subscribed": False}


@router.post("/settings")
def set_settings(body: LeadIn):
    with db.connect() as conn:
        n = conn.execute("UPDATE push_subs SET lead_min=? WHERE endpoint=?", (body.lead_min, body.endpoint)).rowcount
    if not n:
        raise HTTPException(404, "この端末は通知が有効になっていません")
    return {"subscribed": True, "lead_min": body.lead_min}


@router.post("/test")
def test(body: EndpointIn):
    with db.connect() as conn:
        r = conn.execute("SELECT * FROM push_subs WHERE endpoint=?", (body.endpoint,)).fetchone()
    if not r:
        raise HTTPException(404, "この端末は通知が有効になっていません")
    try:
        _send(dict(r), {"title": "アニメ配信カレンダー", "body": "テスト通知です。届いていれば設定は完了です。",
                        "tag": "test", "url": "/"}, ttl=60)
    except WebPushException as e:
        if _gone(e):
            _drop(body.endpoint)
            raise HTTPException(410, "通知の購読が無効になりました。通知を有効にし直してください")
        log.warning("push test failed (%s): %s", urlparse(body.endpoint).hostname, e)
        raise HTTPException(502, f"通知サービスへの送信に失敗しました: {e}")
    except Exception as e:  # ネットワーク不通など
        log.warning("push test failed (%s): %r", urlparse(body.endpoint).hostname, e)
        raise HTTPException(502, f"通知サービスへの送信に失敗しました: {e}")
    return {"ok": True}


# ---------------------------------------------------------------- 通知ループ
def _fmt_ep(r: dict, now: datetime) -> dict:
    st = datetime.strptime(r["start_at"], "%Y-%m-%dT%H:%M")
    mins = round((st - now).total_seconds() / 60)
    num = "" if r["number"] is None else f" 第{r['number']:g}話"
    when = "まもなく開始" if mins <= 0 else f"あと{mins}分"
    place = f"（{r['platform']}）" if r["platform"] else ""
    sub = f" 「{r['subtitle']}」" if r["subtitle"] else ""
    return {"title": f"{r['title']}{num}",
            "body": f"{st:%H:%M} 配信開始{place} {when}{sub}",
            "tag": f"ep-{r['id']}", "url": "/"}


def notify_due() -> int:
    """送信すべき通知を送る。送った件数を返す"""
    now = config.now_jst()
    sent = 0
    with db.connect() as conn:
        subs = [dict(r) for r in conn.execute("SELECT * FROM push_subs")]
        conn.execute("DELETE FROM push_sent WHERE start_at<?",
                     (config.fmt_dt(now - timedelta(days=2)),))
    for sub in subs:
        lo = config.fmt_dt(now - timedelta(minutes=GRACE_MIN))
        hi = config.fmt_dt(now + timedelta(minutes=sub["lead_min"]))
        with db.connect() as conn:
            rows = [dict(r) for r in conn.execute(
                """SELECT e.id, e.number, e.subtitle, e.start_at,
                     COALESCE(NULLIF(tr.title,''), t.title, '(無題)') AS title,
                     COALESCE(NULLIF(tr.platform,''), c.name, '') AS platform
                   FROM episodes e JOIN tracks tr ON tr.id=e.track_id
                   LEFT JOIN titles t ON t.tid=tr.tid LEFT JOIN channels c ON c.chid=tr.chid
                   WHERE e.hidden=0 AND e.watched=0 AND tr.archived=0
                     AND e.start_at IS NOT NULL AND e.start_at>=? AND e.start_at<=?
                     AND NOT EXISTS (SELECT 1 FROM push_sent s
                                     WHERE s.endpoint=? AND s.ep_id=e.id AND s.start_at=e.start_at)
                   ORDER BY e.start_at""", (lo, hi, sub["endpoint"]))]
        for r in rows:
            try:
                _send(sub, _fmt_ep(r, now))
            except WebPushException as e:
                if _gone(e):
                    log.info("push subscription gone, removed")
                    _drop(sub["endpoint"])
                    break
                log.warning("push failed: %s", e)
                continue
            except Exception as e:
                log.warning("push failed: %s", e)
                continue
            with db.connect() as conn:
                conn.execute("INSERT OR IGNORE INTO push_sent(endpoint,ep_id,start_at) VALUES(?,?,?)",
                             (sub["endpoint"], r["id"], r["start_at"]))
            sent += 1
    return sent


def _loop():
    while True:
        try:
            notify_due()
        except Exception:
            log.exception("notify error")
        time.sleep(30)


def start():
    threading.Thread(target=_loop, name="notify", daemon=True).start()
