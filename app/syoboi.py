"""しょぼいカレンダー db.php クライアント

使用コマンド:
  ChGroupLookup  チャンネルグループ
  ChLookup       チャンネル（地上波・BS・ネット配信など＝媒体）
  ProgLookup     放送/配信予定（Range 指定, JOIN=SubTitles）
  TitleLookup    作品タイトル
"""
import logging
import random
import threading
import time
import xml.etree.ElementTree as ET
from datetime import datetime, timedelta

import httpx

import config

log = logging.getLogger("syoboi")


class SyoboiError(RuntimeError):
    pass


class SyncCancelled(BaseException):  # except Exception で握りつぶされないよう BaseException 系にする
    """ユーザー操作による取得の中断"""


def _items(root: ET.Element, tag: str) -> list[dict]:
    return [{c.tag: (c.text or "").strip() for c in it} for it in root.iter(tag)]


def _int(v, default=None):
    try:
        return int(v)
    except (TypeError, ValueError):
        return default


def _dt(v: str):
    if not v:
        return None
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y%m%d%H%M%S"):
        try:
            return datetime.strptime(v, fmt)
        except ValueError:
            pass
    return None


class SyoboiClient:
    def __init__(self, base: str = None, interval: float = None, cancel: threading.Event | None = None):
        self.cancel = cancel or threading.Event()
        self.base = base or config.SYOBOI_BASE
        self.interval = config.SYOBOI_INTERVAL_SEC if interval is None else interval
        self._last = 0.0
        self.http = httpx.Client(
            timeout=60, follow_redirects=True,
            headers={"User-Agent": config.SYOBOI_UA},
        )

    def close(self):
        self.http.close()

    def _sleep(self, sec: float) -> None:
        """待機。中断要求があれば即座に SyncCancelled"""
        if self.cancel.wait(max(0.0, sec)):
            raise SyncCancelled()

    def _get(self, params: dict) -> ET.Element:
        last_exc = None
        for attempt in range(5):
            if self.cancel.is_set():
                raise SyncCancelled()
            wait = self._last + self.interval - time.monotonic()
            if wait > 0:
                self._sleep(wait)
            backoff = 5 * 2 ** attempt  # 5,10,20,40,80秒
            try:
                r = self.http.get(self.base, params=params)
                self._last = time.monotonic()
                if r.status_code in (429, 503):
                    # 制限中: Retry-After に従う（無ければ長めに待つ）。以降の取得間隔も広げる
                    ra = r.headers.get("Retry-After", "")
                    backoff = min(int(ra), 600) if ra.isdigit() else 30 * 2 ** attempt
                    self.interval = min(max(self.interval * 2, 3.0), 15.0)
                    if backoff > config.SYOBOI_MAX_WAIT_SEC:
                        # 長時間の制限中は待ち続けず、この取得を失敗として終える（時間を置いて再実行）
                        raise SyoboiError(f"しょぼいカレンダーから制限されています（{r.status_code}、約{backoff // 60}分後に再試行してください）")
                    log.warning("syoboi rate limited (%s), wait %ss, interval -> %.1fs",
                                r.status_code, backoff, self.interval)
                elif 400 <= r.status_code < 500 and r.status_code != 408:
                    # 同じ要求を繰り返しても解決しないエラーは再試行しない
                    raise SyoboiError(f"HTTP {r.status_code}: {params.get('Command')}")
                r.raise_for_status()
                root = ET.fromstring(r.content)
                code = root.findtext("Result/Code")
                if code and code not in ("200", "404"):  # 404=該当データなし→0件
                    msg = root.findtext("Result/Message") or ""
                    raise SyoboiError(f"{params.get('Command')}: code={code} {msg}")
                return root
            except (httpx.HTTPError, ET.ParseError) as e:
                self._last = time.monotonic()
                last_exc = e
                backoff *= random.uniform(1.0, 1.3)  # 再試行の集中を避ける
                log.warning("syoboi request failed (%s), retry in %.0fs", e, backoff)
                if attempt < 4:
                    self._sleep(backoff)
        raise SyoboiError(f"request failed: {last_exc}")

    # ---- API ----
    def channel_groups(self) -> list[dict]:
        root = self._get({"Command": "ChGroupLookup"})
        out = []
        for it in _items(root, "ChGroupItem"):
            gid = _int(it.get("ChGID"))
            if gid is None:
                continue
            out.append({"gid": gid, "name": it.get("ChGroupName") or f"group{gid}",
                        "sort": _int(it.get("ChGroupOrder"))})
        return out

    def channels(self) -> list[dict]:
        root = self._get({"Command": "ChLookup"})
        out = []
        for it in _items(root, "ChItem"):
            chid = _int(it.get("ChID"))
            if chid is None:
                continue
            out.append({"chid": chid, "name": it.get("ChName") or f"ChID:{chid}",
                        "gid": _int(it.get("ChGID")), "sort": _int(it.get("ChNumber"))})
        return out

    def titles(self, tids: list[int]) -> list[dict]:
        if not tids:
            return []
        root = self._get({"Command": "TitleLookup", "TID": ",".join(map(str, tids))})
        out = []
        for it in _items(root, "TitleItem"):
            tid = _int(it.get("TID"))
            if tid is None:
                continue
            out.append({
                "tid": tid,
                "title": it.get("Title") or f"TID:{tid}",
                "cat": _int(it.get("Cat")),
                "first_year": _int(it.get("FirstYear")),
                "first_month": _int(it.get("FirstMonth")),
            })
        return out

    def _fetch_programs(self, key: str, start: datetime, end: datetime, chids, end_inclusive: bool) -> list[dict]:
        """key は Range（放送日時で絞る）か LastUpdate（更新日時で絞る）。件数が多いときは期間を分割する"""
        params = {
            "Command": "ProgLookup",
            key: f"{start:%Y%m%d_%H%M%S}-{end:%Y%m%d_%H%M%S}",
            "JOIN": "SubTitles",
        }
        if chids:
            params["ChID"] = ",".join(map(str, chids))
        root = self._get(params)
        items = _items(root, "ProgItem")
        if len(items) >= config.SYOBOI_SPLIT_THRESHOLD and (end - start) > timedelta(minutes=10):
            mid = (start + (end - start) / 2).replace(second=0, microsecond=0)
            log.info("split %s %s - %s (%d items)", key, start, end, len(items))
            gap = timedelta(seconds=1) if end_inclusive else timedelta(0)
            return (self._fetch_programs(key, start, mid - gap if end_inclusive else mid, chids, end_inclusive)
                    + self._fetch_programs(key, mid, end, chids, end_inclusive))
        return self._parse_programs(items)

    def programs(self, start: datetime, end: datetime, chids: list[int] | None = None) -> list[dict]:
        """放送日時が start〜end の番組"""
        return self._fetch_programs("Range", start, end, chids, True)

    def programs_updated(self, since: datetime, until: datetime, chids: list[int] | None = None) -> list[dict]:
        """since〜until に更新された番組（差分取得）"""
        return self._fetch_programs("LastUpdate", since, until, chids, False)

    def titles_updated(self, since: datetime) -> set[int]:
        """since 以降に更新されたタイトルの TID 一覧"""
        root = self._get({"Command": "TitleLookup", "TID": "*", "LastUpdate": f"{since:%Y%m%d_%H%M%S}-"})
        return {t for t in (_int(it.get("TID")) for it in _items(root, "TitleItem")) if t is not None}

    @staticmethod
    def _parse_programs(items: list[dict]) -> list[dict]:
        out = []
        for it in items:
            pid, tid, chid = _int(it.get("PID")), _int(it.get("TID")), _int(it.get("ChID"))
            st = _dt(it.get("StTime"))
            if None in (pid, tid, chid) or st is None:
                continue
            st = st + timedelta(seconds=_int(it.get("StOffset"), 0) or 0)
            ed = _dt(it.get("EdTime"))
            out.append({
                "pid": pid, "tid": tid, "chid": chid,
                "start_at": config.fmt_dt(st),
                "end_at": config.fmt_dt(ed) if ed else None,
                "count": _int(it.get("Count")),
                "subtitle": it.get("STSubTitle") or it.get("SubTitle") or None,
                "flag": _int(it.get("Flag"), 0) or 0,
                "deleted": 1 if it.get("Deleted") == "1" else 0,
            })
        return out
