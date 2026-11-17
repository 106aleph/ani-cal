"""環境変数による設定"""
import os
from datetime import datetime
from zoneinfo import ZoneInfo


def _int(name: str, default: int) -> int:
    try:
        return int(os.environ.get(name, default))
    except ValueError:
        return default


def _float(name: str, default: float) -> float:
    try:
        return float(os.environ.get(name, default))
    except ValueError:
        return default


def _bool(name: str, default: bool) -> bool:
    v = os.environ.get(name)
    if v is None:
        return default
    return v.strip().lower() in ("1", "true", "yes", "on")


def _intset(name: str, default: str = "") -> set[int]:
    raw = os.environ.get(name, default)
    return {int(x) for x in raw.replace(" ", "").split(",") if x.strip().isdigit()}


TZ = ZoneInfo(os.environ.get("APP_TZ", "Asia/Tokyo"))
DB_PATH = os.environ.get("DB_PATH", "/data/anime.db")

# しょぼいカレンダー
SYOBOI_BASE = os.environ.get("SYOBOI_BASE", "https://cal.syoboi.jp/db.php")
SYOBOI_UA = os.environ.get("SYOBOI_UA", "self-hosted-anime-calendar/1.0")
SYOBOI_INTERVAL_SEC = _float("SYOBOI_INTERVAL_SEC", 1.5)   # リクエスト間隔（サーバー負荷配慮）
SYOBOI_SPLIT_THRESHOLD = _int("SYOBOI_SPLIT_THRESHOLD", 2000)  # 1回の取得件数がこれ以上なら期間を分割

# 同期
SYNC_INTERVAL_HOURS = _float("SYNC_INTERVAL_HOURS", 6)
SYNC_ON_STARTUP = _bool("SYNC_ON_STARTUP", True)
SYNC_DAYS_BACK = _int("SYNC_DAYS_BACK", 60)                  # 初回取得で遡る日数
SYNC_DAYS_BACK_INCREMENTAL = _int("SYNC_DAYS_BACK_INCREMENTAL", 14)
SYNC_DAYS_FORWARD = _int("SYNC_DAYS_FORWARD", 120)
SYNC_CHUNK_DAYS = max(1, _int("SYNC_CHUNK_DAYS", 2))
SYNC_CHGIDS = _intset("SYNC_CHGIDS")                         # 空=全チャンネルグループ
SKIP_REBROADCAST = _bool("SKIP_REBROADCAST", True)

# 新作一覧
NEW_MONTHS_BACK = _int("NEW_MONTHS_BACK", 3)
NEW_MONTHS_FORWARD = _int("NEW_MONTHS_FORWARD", 6)
NEW_CATS = _intset("NEW_CATS", "1")                          # しょぼいカレンダーのCat (1=アニメ)
NET_GROUP_IDS = _intset("NET_GROUP_IDS")
NET_GROUP_KEYWORDS = [k for k in os.environ.get(
    "NET_GROUP_KEYWORDS", "ネット,インターネット,配信,web").split(",") if k]


def now_jst() -> datetime:
    """タイムゾーン情報なしのJST現在時刻"""
    return datetime.now(TZ).replace(tzinfo=None, microsecond=0)


def fmt_dt(dt: datetime) -> str:
    return dt.strftime("%Y-%m-%dT%H:%M")
