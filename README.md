# アニメ配信カレンダー

新作アニメの「タイトル・配信媒体・配信開始日時」を [しょぼいカレンダー](https://cal.syoboi.jp/) から取得し、
媒体を選んで登録 → カレンダーで確認 → 1話ごとに視聴記録、ができるセルフホストアプリです。

## 必要な環境

- Linux ホスト（動作確認: Proxmox VE 上の Debian 13 VM）
- Docker と Docker Compose プラグイン
- Tailscale（ホストにインストール。管理画面で MagicDNS と HTTPS Certificates を有効化）
- 外部への HTTPS 通信（`cal.syoboi.jp` へ）
- （任意）`sqlite3` コマンド（バックアップ用）

## セットアップ

```bash
cd /opt/anime-calendar          # このディレクトリを置いた場所
cp .env.example .env            # ポートや取得設定を変える場合だけ編集
docker compose up -d --build
docker compose logs -f          # 「番組 xxxx 件 / 作品 xxx 件を取得」が出れば初回取得完了（数分）

sudo tailscale serve --bg 8000  # .env の APP_PORT を変えた場合はその番号
```

- tailnet 内の端末から `https://<VMのホスト名>.<tailnet名>.ts.net/` を開きます（初回は証明書発行に数秒かかります）。
- `--bg` を付けた `tailscale serve` の設定は VM 再起動後も有効です。
- ホスト名を `anime` にしたい場合: `sudo tailscale set --hostname=anime`
- 他のサービスも `tailscale serve` している場合は別ポートにしてください（例: `--https=8443`）。このアプリはルート `/` 前提で、サブパス公開（`--set-path`）には対応していません。

## 画面と機能

| 画面 | できること |
|---|---|
| カレンダー | 月／週／リスト表示。週は時間軸つき、リストは今後2週間＋配信済み未視聴。チェックで視聴済み、クリックで編集。深夜帯の「25:30」表記切替、視聴済みを隠す |
| 新作一覧 | 新作を媒体ごとの第1話日時つきで一覧。媒体チップをクリックで登録。時期・媒体・配信系のみ・未登録のみ・タイトルで絞り込み |
| 登録作品 | 話数一覧、視聴チェック、話の追加・編集・削除、媒体の変更、タイトル/媒体名の上書き、間隔指定での残り話数作成、配信済み一括視聴、ICS 出力 |
| ログ | 取得履歴（日時・種別・取得範囲・結果・件数・エラー内容）。最新500件 |

### 手動修正したデータの扱い

- 話数・サブタイトル・日時を編集した話には「修正済」が付き、以降の取得で上書きされません（「取得値に戻す」で戻せます）。
- 取得データ由来の話を削除すると「非表示」になり、再取得しても復活しません（「復元」可）。
- 取得データに無い作品は「手動で作品を追加」で登録できます。
- 先の話数が未登録の作品は、「間隔で話を作成」で予測の話を作れます。実データが入ると自動で置き換わり、視聴記録は保持されます。

## 取得の仕組み

媒体（`ChGroupLookup` / `ChLookup`）→ 配信予定（`ProgLookup`、2日ずつ）→ タイトル（`TitleLookup`）の順に取得し、最後に登録作品へ反映します。

| 取得方法 | タイミング | 範囲 |
|---|---|---|
| 定期（起動時を含む） | 10分ごとに確認し、前回の成功から6時間以上たっていれば実行 | 初回は60日前〜120日先、以降は14日前〜120日先 |
| 今すぐ取得 | 画面右上のボタン | 定期と同じ |
| 期間指定 | 画面右上のボタンで開始日・終了日を指定（最大400日） | 指定した期間 |

- 起動時の取得は、前回の成功から6時間以上たっているか、未取得のときだけ行われます。
- 取得に失敗した場合は、成功時刻が更新されないため、10分後の確認でまた試行します。
- リクエスト間隔は既定1.5秒です。`429` などで制限されたら自動で長く待ち、間隔も広げます。
- 登録作品への反映は、取得が成功した最後に行われます（「定期取得で自動更新する」がオンの作品のみ）。手動追加の作品は対象外です。
- 先の予定が出ない、放送開始が古い作品を見たい、といったときは「期間指定」で範囲を広げて取得してください。

### 新作・配信系の判定

- **新作**: カテゴリが「アニメ」で、放送開始月が今月の3ヶ月前〜6ヶ月後の作品。放送中（今以降の予定がある）なら、開始が12ヶ月前までの作品も含みます。
- **配信系**: チャンネルグループ名に「ネット / インターネット / 配信 / web」を含む媒体。うまく判定されないときは `NET_GROUP_IDS` で指定します。

各設定は `.env` で変更できます（項目は `.env.example` を参照）。

## 運用

```bash
docker compose ps
curl -s http://127.0.0.1:8000/api/status; echo           # 取得状況
docker compose up -d --build                             # コード更新後
sqlite3 data/anime.db ".backup backup-$(date +%F).db"    # バックアップ
docker compose down                                      # 停止
sudo tailscale serve --https=443 off                     # serve の設定だけ外す（全部なら: serve reset）
```

データは `./data/anime.db` にあります。取得中に停止すると、その取得はログに「中断されました」と残り、次回起動時に再取得します。

## 注意

- コンテナは `127.0.0.1` にしか公開しないため、LAN から `VMのIP:8000` には繋がりません。アクセス経路は `tailscale serve` 経由のみです。
- 認証は持たず、Tailscale の ACL でアクセス制御する前提です。
- ICS 購読: `https://<host>.<tailnet>.ts.net/calendar.ics`（`?unwatched_only=true` で未視聴のみ）。tailnet 外のサービス（Google カレンダー等）からは取得できないので、tailnet 内のカレンダーアプリで購読してください。
- しょぼいカレンダーのデータはボランティア登録のため、媒体の網羅性や日時に誤り・遅れがあり得ます。取得間隔は短くしすぎないでください。

## API（参考）

| メソッド | パス | 内容 |
|---|---|---|
| GET | `/api/status` | 取得状況 |
| POST | `/api/sync` | 取得開始（本文 `{start, end}`（`YYYY-MM-DD`）で期間指定、省略で既定範囲） |
| GET | `/api/logs` | 取得履歴 |
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
