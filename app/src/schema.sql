
CREATE TABLE IF NOT EXISTS meta(
  key TEXT PRIMARY KEY,
  value TEXT
);

-- しょぼいカレンダーから取得した生データ（自動で上書きされる）
CREATE TABLE IF NOT EXISTS channel_groups(
  gid INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  sort INTEGER
);
CREATE TABLE IF NOT EXISTS channels(
  chid INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  gid INTEGER,
  sort INTEGER
);
CREATE TABLE IF NOT EXISTS titles(
  tid INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  cat INTEGER,
  first_year INTEGER,
  first_month INTEGER,
  updated_at TEXT
);
CREATE TABLE IF NOT EXISTS programs(
  pid INTEGER PRIMARY KEY,
  tid INTEGER NOT NULL,
  chid INTEGER NOT NULL,
  start_at TEXT NOT NULL,
  end_at TEXT,
  count INTEGER,
  subtitle TEXT,
  flag INTEGER NOT NULL DEFAULT 0,
  deleted INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_programs_tid_chid ON programs(tid, chid);
CREATE INDEX IF NOT EXISTS idx_programs_start ON programs(start_at);

-- ユーザーが登録した作品（媒体選択済み）。title/platform は手動上書き用（NULL=取得値を表示）
CREATE TABLE IF NOT EXISTS tracks(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tid INTEGER,
  chid INTEGER,
  title TEXT,
  platform TEXT,
  auto_sync INTEGER NOT NULL DEFAULT 1,
  note TEXT,
  created_at TEXT NOT NULL,
  archived INTEGER NOT NULL DEFAULT 0,   -- 1=視聴済みに移した作品（カレンダー・未視聴一覧に出さない）
  archived_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_tracks_tid_chid
  ON tracks(tid, chid) WHERE tid IS NOT NULL AND chid IS NOT NULL;

-- 1話ごとの配信予定と視聴記録
--   source: sync=取得データ / manual=手動追加 / generated=週次予測
--   edited=1 の行は再取得で上書きしない
--   hidden=1 は削除扱い（取得データが再出現しても復活させない）
CREATE TABLE IF NOT EXISTS episodes(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
  number REAL,
  subtitle TEXT,
  start_at TEXT,
  watched INTEGER NOT NULL DEFAULT 0,
  watched_at TEXT,
  source TEXT NOT NULL DEFAULT 'manual',
  pid INTEGER,
  edited INTEGER NOT NULL DEFAULT 0,
  hidden INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_episodes_track ON episodes(track_id);
CREATE INDEX IF NOT EXISTS idx_episodes_start ON episodes(start_at);

-- 取得履歴（いつ・どの範囲を取得したか）。trigger: auto=定期 / startup=起動時 / manual=今すぐ / range=期間指定
CREATE TABLE IF NOT EXISTS sync_log(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  trigger TEXT NOT NULL,
  range_start TEXT,
  range_end TEXT,
  status TEXT NOT NULL DEFAULT 'running',
  programs INTEGER,
  titles INTEGER,
  message TEXT,
  duration_sec REAL
);
-- Web Push 通知（端末ごとの購読と、開始何分前に通知するか）
CREATE TABLE IF NOT EXISTS push_subs(
  endpoint TEXT PRIMARY KEY,
  p256dh TEXT NOT NULL,
  auth TEXT NOT NULL,
  lead_min INTEGER NOT NULL DEFAULT 10,
  created_at TEXT NOT NULL
);
-- 送信済み（同じ話・同じ開始時刻を二重に通知しない）
CREATE TABLE IF NOT EXISTS push_sent(
  endpoint TEXT NOT NULL,
  ep_id INTEGER NOT NULL,
  start_at TEXT NOT NULL,
  PRIMARY KEY(endpoint, ep_id, start_at)
);
