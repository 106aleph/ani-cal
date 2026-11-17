# アニメ配信カレンダー

新作アニメの「タイトル・配信媒体・配信開始日時」を [しょぼいカレンダー](https://cal.syoboi.jp/) から自動取得し、
ブラウザで **媒体を選んで登録 → カレンダー表示 → 1話ごとに視聴記録** できるセルフホストアプリです。

- FastAPI + SQLite（1コンテナ）。コンテナは `127.0.0.1:8000` にだけ公開
- ホスト（VM）の Tailscale で `tailscale serve` を使い、`https://<VMのホスト名>.<tailnet>.ts.net/` に HTTPS で公開（tailnet 内からのみ）

## 機能

| 画面 | できること |
|---|---|
| 新作一覧 | 今期前後の新作を、媒体（ABEMA・dアニメストア・地上波各局…）ごとの第1話日時つきで一覧。媒体チップをクリックで登録。時期・媒体・配信系のみ・未登録のみ・タイトルで絞り込み |
| カレンダー | 月表示／週表示（日〜土の7日間）／リスト表示（今後2週間＋配信済み未視聴）。チェックで視聴済み、クリックで編集。深夜帯の「25:30」表記切替 |
| 登録作品 | 作品ごとの話数一覧、視聴チェック、話の追加・編集・削除、媒体の変更、タイトル/媒体名の上書き、「毎週」などの間隔で残り話数を作成、配信済み一括視聴、ICS 出力 |

### 取得データの手動修正について

- 話数・サブタイトル・配信日時を編集した話には **「修正済」** が付き、以降の自動取得で上書きされません。
  「取得値に戻す」で取得データに戻せます（編集画面に取得値も表示されます）。
- 取得データ由来の話を削除すると **非表示** 扱いになり、再取得しても復活しません（「復元」可）。
- タイトル・媒体名は作品編集で上書き表示できます（空欄に戻すと取得値を表示）。
- 取得データに無い作品は「手動で作品を追加」で登録できます。
- しょぼいカレンダーは先の話数がまだ登録されていないことが多いので、「間隔で話を作成」で **予測** の話を作れます。
  後で実データが入ると自動で置き換わります（視聴記録は保持）。

### 取得の仕組み

`db.php` の `ChGroupLookup` / `ChLookup`（媒体）→ `ProgLookup`（配信予定, 2日ずつ）→ `TitleLookup`（タイトル）の順で取得します。
既定では 6 時間ごとに自動取得（初回は 60 日前〜120 日先、以降は 14 日前〜120 日先）。リクエスト間隔は 1.5 秒にしてあります。
初回は数分かかります。画面右上の「今すぐ取得」で手動実行もできます。

> 「新作」の判定は、しょぼいカレンダーの作品カテゴリが「アニメ」かつ放送開始月が今月の 3 ヶ月前〜6 ヶ月後のものです（`.env` で変更可）。
> 「配信系」は、チャンネルグループ名に「ネット / インターネット / 配信 / web」を含むものです。うまく判定されない場合は `NET_GROUP_IDS` で指定してください。

---

## セットアップ（Proxmox VE 上の Debian 13 VM）

### 0. Tailscale 管理画面での準備

<https://login.tailscale.com/admin/dns> で **MagicDNS** と **HTTPS Certificates** を有効化しておきます。

### 1. Docker のインストール（Debian 13 / trixie）

```bash
sudo apt-get update
sudo apt-get install -y ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
  | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
sudo usermod -aG docker $USER   # 再ログイン後 sudo なしで docker が使える
```

### 2. Tailscale のインストール（ホスト）※導入済みならスキップ

```bash
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up
# URL を https://anime.<tailnet>.ts.net/ にしたい場合（任意）
sudo tailscale set --hostname=anime
```

### 3. アプリの配置と起動

```bash
# このディレクトリを VM に置く（例: /opt/anime-calendar）
cd /opt/anime-calendar
cp .env.example .env        # ポートや取得設定を変える場合だけ編集
mkdir -p data
docker compose up -d --build
docker compose logs -f
```

ログに `sync: 番組 xxxx 件 / 作品 xxx 件を取得` が出たら取得完了です。

### 4. tailscale serve で HTTPS 公開

```bash
sudo tailscale serve --bg 8000      # .env の APP_PORT を変えた場合はその番号
tailscale serve status
```

`--bg` を付けると設定が保存され、VM 再起動後も有効です。
tailnet 内の端末から `https://<VMのホスト名>.<tailnet名>.ts.net/` を開きます（初回アクセス時の証明書発行に数秒かかります）。

> 同じ VM で他のサービスも `tailscale serve` している場合は、パスで分けるか別ポートにしてください。
> 例: `sudo tailscale serve --bg --https=8443 8000` → `https://<host>.<tailnet>.ts.net:8443/`
> （このアプリはルート `/` 前提なので `--set-path` でのサブパス公開には対応していません）

### 確認用コマンド

```bash
docker compose ps
curl -s http://127.0.0.1:8000/api/status; echo
tailscale serve status
```

### 停止・解除

```bash
docker compose down
sudo tailscale serve --https=443 off   # serve の設定だけ外す（全部消すなら: sudo tailscale serve reset）
```

### 更新・バックアップ

```bash
docker compose up -d --build                             # コード更新後
sqlite3 data/anime.db ".backup backup-$(date +%F).db"    # 要 apt install sqlite3
```

データは `./data/anime.db` にあります。

---

## 補足

- コンテナのポートは `127.0.0.1` にしか公開していないので、LAN から VM の IP:8000 には繋がりません。
  アクセス経路は tailscale serve 経由だけです。
- **ICS 購読**：`https://<host>.<tailnet>.ts.net/calendar.ics`（`?unwatched_only=true` で未視聴のみ）。
  tailnet 外のサービス（Google カレンダー等）からは取得できないので、tailnet 内のカレンダーアプリで購読してください。
- 認証は持たず、Tailscale の ACL でアクセス制御する前提です。
- しょぼいカレンダーのデータはボランティアによる登録のため、配信媒体の網羅性や日時には誤り・遅れがあり得ます。
  サーバーに負荷をかけないよう、取得間隔は短くしすぎないでください。

## API（参考）

| メソッド | パス | 内容 |
|---|---|---|
| GET | `/api/status` | 取得状況 |
| POST | `/api/sync` | 取得開始 |
| GET | `/api/catalog` | 新作一覧（作品×媒体） |
| GET/POST | `/api/tracks` | 登録作品の一覧／登録（`{tid, chid}` または `{title, platform, first_start, episodes, interval_days}`） |
| PATCH/DELETE | `/api/tracks/{id}` | 作品の編集（`title`, `platform`, `chid`, `auto_sync`, `note`）／解除 |
| POST | `/api/tracks/{id}/generate` | `{until, interval_days}` で予測話を作成 |
| POST | `/api/tracks/{id}/resync` | 取得データを再反映 |
| GET/POST | `/api/tracks/{id}/episodes` | 話の一覧／追加 |
| PATCH/DELETE | `/api/episodes/{id}` | 話の編集（`number`, `subtitle`, `start_at`, `watched`, `hidden`）／削除 |
| POST | `/api/episodes/{id}/revert` | 取得値に戻す |
| GET | `/api/calendar?start=YYYY-MM-DD&end=YYYY-MM-DD` | 期間内の話 |
| GET | `/calendar.ics` | iCalendar |
