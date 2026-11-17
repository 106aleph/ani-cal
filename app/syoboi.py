"""しょぼいカレンダー db.php クライアント

使用コマンド:
  ChGroupLookup  チャンネルグループ
  ChLookup       チャンネル（地上波・BS・ネット配信など＝媒体）
  ProgLookup     放送/配信予定（Range 指定, JOIN=SubTitles）
  TitleLookup    作品タイトル
"""
import logging
import time
import xml.etree.ElementTree as ET
from datetime import datetime, timedelta

import httpx

import config

log = logging.getLogger("syoboi")


class SyoboiError(RuntimeError):
    pass


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
    def __init__(self, base: str = None, interval: float = None):
        self.base = base or config.SYOBOI_BASE
        self.interval = config.SYOBOI_INTERVAL_SEC if interval is None else interval
        self._last = 0.0
        self.http = httpx.Client(
            timeout=60, follow_redirects=True,
            headers={"User-Agent": config.SYOBOI_UA},
        )

    def close(self):
        self.http.close()

    def _get(self, params: dict) -> ET.Element:
        last_exc = None
        for attempt in range(4):
            wait = self._last + self.interval - time.monotonic()
            if wait > 0:
                time.sleep(wait)
            try:
                r = self.http.get(self.base, params=params)
                self._last = time.monotonic()
                r.raise_for_status()
                root = ET.fromstring(r.content)
                code = root.findtext("Result/Code")
                if code and code != "200":
                    msg = root.findtext("Result/Message") or ""
                    raise SyoboiError(f"{params.get('Command')}: code={code} {msg}")
                return root
            except (httpx.HTTPError, ET.ParseError) as e:
                self._last = time.monotonic()
                last_exc = e
                backoff = 5 * (attempt + 1)
                log.warning("syoboi request failed (%s), retry in %ss", e, backoff)
                time.sleep(backoff)
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

    def programs(self, start: datetime, end: datetime, chids: list[int] | None = None) -> list[dict]:
        """start〜end の番組。件数が多い場合は期間を分割して再取得する"""
        params = {
            "Command": "ProgLookup",
            "Range": f"{start:%Y%m%d_%H%M%S}-{end:%Y%m%d_%H%M%S}",
            "JOIN": "SubTitles",
        }
        if chids:
            params["ChID"] = ",".join(map(str, chids))
        root = self._get(params)
        items = _items(root, "ProgItem")
        if len(items) >= config.SYOBOI_SPLIT_THRESHOLD and (end - start) > timedelta(hours=6):
            mid = start + (end - start) / 2
            mid = mid.replace(second=0, microsecond=0)
            log.info("split range %s - %s (%d items)", start, end, len(items))
            return self.programs(start, mid, chids) + self.programs(mid + timedelta(seconds=1), end, chids)

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
