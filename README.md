# アニメ配信カレンダー

[しょぼいカレンダー](https://cal.syoboi.jp/) から新作アニメの配信予定を取得し、媒体を選んで登録 → カレンダーで確認 → 1話ごとに視聴記録できるセルフホストアプリです（FastAPI + SQLite、1コンテナ）。

## 必要な環境

- Linux ホスト（動作確認: Debian 13）
- Docker と Docker Compose
- Tailscale（管理画面で MagicDNS と HTTPS Certificates を有効化）
- `cal.syoboi.jp` と各社 Push サービスへの HTTPS 通信（外向き）

## セットアップ

```bash
cp .env.example .env              # SYOBOI_UA_URL を設定（下記）
docker compose up -d --build
docker compose logs -f            # 初回取得は数分
sudo tailscale serve --bg 8000    # APP_PORT を変えた場合はその番号
```

`https://<ホスト名>.<tailnet名>.ts.net/` を tailnet 内の端末で開きます。ブラウザからホーム画面やデスクトップにインストール（PWA）できます。

- `.env` の `SYOBOI_UA_URL` に、連絡方法を載せた公開ページのURLを設定します。しょぼいカレンダーへの User-Agent（`AnimeCalendar/1.0 (+URL)`）に入り、問題があれば運営から連絡できます。変更後は `docker compose up -d`。
- コンテナは `127.0.0.1` にしか公開せず、アクセスは `tailscale serve` 経由のみです。認証は持たず、Tailscale の ACL で制御する前提です。
- サブパス公開（`--set-path`）は非対応です。他のサービスと併用するときは `--https=8443` など別ポートにしてください。

## 画面

| 画面 | できること |
|---|---|
| カレンダー | 月／週（時間軸・現在時刻の線つき）／リスト（月ごと）。視聴チェック、クリックで編集、深夜帯の「25:30」表記切替 |
| 新作一覧 | 媒体ごとの第1話日時つき一覧。媒体チップをクリックで登録。時期・媒体・配信系・未登録・タイトルで絞り込み |
| 登録作品 | 話数一覧、視聴チェック、話の追加・編集・削除、媒体の変更、タイトル／媒体名の上書き、間隔指定での話の作成、一括視聴、ICS 出力 |
| 視聴済み | 全話視聴した作品の置き場。「登録作品」の「視聴済みに移す」（全話視聴済みのときだけ押せる）で移動、「登録作品に戻す」で復帰。カレンダー・未視聴・ICS には出ない。新しい話が追加されると自動で戻る |
| ログ | 取得履歴（日時・種別・範囲・結果・所要時間・件数）。最新500件 |

- 編集した話は「修正済」になり、再取得で上書きされません（「取得値に戻す」可）。削除した話は復活しません（「復元」可）。
- 取得データにない作品は「手動で作品を追加」、先の話数が未登録の作品は「間隔で話を作成」（予測。実データが入ると置き換わり、視聴記録は保持）で扱えます。
- ICS 購読: `/calendar.ics`（`?unwatched_only=true` で未視聴のみ）。tailnet 外のサービスからは取得できません。

## 通知

右上の「通知」→「通知を有効にする」で、登録作品の**未視聴の話**が開始の N 分前（既定10分、0〜1440）に、アプリを開いていなくても届きます。視聴済みに移した作品・非表示の話は対象外です。

- 分数は端末ごとの設定です。同じ画面から、テスト通知と停止もできます。
- HTTPS での接続が必要です。iPhone / iPad は、ホーム画面に追加したアプリ（iOS 16.4 以降）から有効にします。
- 送信先は Google / Mozilla / Apple / Microsoft の Push サービスに限定しています。
- 30秒おきに確認し、開始後3分までの話を送ります（それ以降は送りません）。
- VAPID 鍵は初回起動で生成して DB に保存します。連絡先は `PUSH_SUBJECT`（既定は `SYOBOI_UA_URL`）。

## 取得の仕組み

公式 API（`db.php`）を、[bot向けルール](https://docs.cal.syoboi.jp/rule/bot/)に沿って使います。

| 種類 | タイミング | 内容 |
|---|---|---|
| 初回 | DBが空のときの最初の1回 | 60日前〜120日先を2日ずつ（約90リクエスト、数分） |
| 定期・起動時・今すぐ | **6時間ごと**（10分おきに確認し、前回成功から6時間以上で実行） | 前回以降の更新分だけを**差分取得**（1回数リクエスト） |
| 期間指定 | 右上のボタン（片方の日付を選ぶともう片方が自動入力） | 指定した期間の予定を取得。**1回あたり最大90日**。定期取得の「前回以降」の基準には影響しない |

- 間隔は `SYNC_INTERVAL_HOURS`（既定6、`0` で自動取得なし）。起動時も、前回成功から6時間以上のときだけ取得します。
- リクエストは直列・1.5秒間隔。`429`/`503` は `Retry-After` に従い、再試行は5回まで。待ちが5分を超えるときは待たず失敗として終え、ログに「制限されています」と残ります（`SYOBOI_MAX_WAIT_SEC`）。
- 自動取得が連続5回失敗すると停止します（画面に表示）。「今すぐ取得」の成功で再開します。
- 「中断」で取得を止められます。取得済みの分は保存され、失敗には数えません。
- 登録作品への反映は取得成功時の最後に行います（「定期取得で自動更新」がオンの作品のみ）。
- 放送開始が古い作品や取りこぼしは「期間指定」で取り直します。広い範囲は90日ずつ分け、429 を受けたら30分〜1時間空けます。
- **新作**: カテゴリがアニメで、放送開始が今月の3ヶ月前〜6ヶ月後（放送中は12ヶ月前まで）。**配信系**: チャンネルグループ名に「ネット/インターネット/配信/web」を含む媒体（`NET_GROUP_IDS` で指定も可）。
- 設定項目は `.env.example` にあります。

## 運用

```bash
docker compose ps
curl -s http://127.0.0.1:8000/api/status; echo
docker compose up -d --build                             # 更新
sqlite3 data/anime.db ".backup backup-$(date +%F).db"    # バックアップ（要 sqlite3）
docker compose down                                      # 停止
sudo tailscale serve --https=443 off                     # serve の設定を外す
```

データは `./data/anime.db` にあります。取得中に停止すると、ログに「中断されました」と残り、次回の取得でやり直します。

## API（参考）

| メソッド | パス | 内容 |
|---|---|---|
| GET | `/api/status` | 取得状況 |
| POST | `/api/sync` | 取得開始（本文 `{start, end}` で期間指定） |
| POST | `/api/sync/cancel` | 取得の中断 |
| GET | `/api/logs` | 取得履歴 |
| GET | `/api/catalog` | 新作一覧 |
| GET/POST | `/api/tracks` | 登録作品の一覧（`?archived=true` で視聴済み）／登録 |
| PATCH/DELETE | `/api/tracks/{id}` | 作品の編集／解除 |
| POST | `/api/tracks/{id}/generate` | 予測話の作成 |
| POST | `/api/tracks/{id}/resync` | 取得データの再反映 |
| POST | `/api/tracks/{id}/archive` / `unarchive` | 視聴済みへ移す／登録作品へ戻す |
| GET/POST | `/api/tracks/{id}/episodes` | 話の一覧／追加 |
| PATCH/DELETE | `/api/episodes/{id}` | 話の編集／削除 |
| POST | `/api/episodes/{id}/revert` | 取得値に戻す |
| GET/POST | `/api/push/{key,settings,subscribe,unsubscribe,test}` | 通知の購読と設定 |
| GET | `/api/calendar?start=&end=` | 期間内の話 |
| GET | `/calendar.ics` | iCalendar |

## 連絡先

個人用のアプリです。取得について問題があれば、[GitHub の Issues](https://github.com/106aleph/ani-cal/issues) か [Misskey](https://misskey.106aleph.xyz/@106aleph) からご連絡ください。User-Agent は `AnimeCalendar/1.0 (+https://github.com/106aleph/ani-cal)` です。
