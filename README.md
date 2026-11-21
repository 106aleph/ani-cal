# アニメ配信カレンダー

[しょぼいカレンダー](https://cal.syoboi.jp/) からアニメの配信予定を取得し、見たい作品を配信媒体ごとに登録できます。カレンダーで予定を確認しながら、1話ずつ視聴記録をつけられます。セルフホスト型で、仲間内の数人で使うことを想定しています。

## 必要な環境

- Linux ホスト (動作確認: Debian 13)、2コア・2GB・空きディスク 5GB 以上 (ビルド時。起動後のメモリは 10〜15MB ほど)
- Docker と Docker Compose v2.24 以上、Git (Rust はホストに不要)
- Tailscale (tailnet で使う場合。MagicDNS と HTTPS Certificates を有効化)
- 外向きの HTTPS 通信: `cal.syoboi.jp`・各社 Push サービス (Turnstile 利用時は `challenges.cloudflare.com` も)
- sqlite3 (バックアップを取る場合のみ)

## セットアップ

```bash
git clone https://github.com/106aleph/ani-cal.git && cd ani-cal
cp .env.example .env              # SYOBOI_UA_URL を設定 (下記)
docker compose up -d --build      # 初回取得は数分 (docker compose logs -f で確認)
```

1. **管理者を登録**: ホスト上で `http://127.0.0.1:8000/` を開いて登録します。最初のユーザーが管理者になります。
   - 「セットアップコード」が要ります (`docker compose logs | grep セットアップコード`。有効は起動から60分。切れたら再起動)。
   - 登録できるのは、サーバーと同じ機械 (Docker の私的なアドレスを含む) からだけです。SSH なら `ssh -L 8000:127.0.0.1:8000 <ホスト>` で転送します。
   - 画面が使えないときは `docker compose exec -u app app ani-cal user create-admin <ユーザー名>` (ユーザーがいないときだけ)。
2. **公開**: `sudo tailscale serve --bg 8000` を実行し、`https://<ホスト名>.<tailnet名>.ts.net/` を開きます。PWA としてインストールもできます。サブパス公開 (`--set-path`) は非対応です。

`SYOBOI_UA_URL` には、**自分への**連絡方法を載せた公開ページの URL を設定します (しょぼいカレンダーへの User-Agent に入ります)。このリポジトリの連絡先は使わないでください。

## 使い方

- **作品一覧**: 媒体ごとの第1話日時つき。媒体チップで登録。
- **カレンダー**: 月／週／リスト。視聴チェック、編集。
- **登録作品**: 話の追加・編集・削除、一括視聴、間隔指定での話の作成 (予測。実データで置き換わる)。
- **視聴済み／視聴中断**: 置き場。カレンダー・通知には出ません。視聴済みは新しい話が追加されると自動で戻ります。
- **管理** (管理者だけ): 取得操作、ユーザーと招待コード、Botプロテクション (Turnstile)、取得履歴。

編集した話は「修正済」になり、再取得で上書きされません (「取得値に戻す」で戻せます)。取得データにない作品は「手動で作品を追加」します。

## ユーザーと権限

- 登録作品・視聴記録・通知はユーザーごとに別です (取得した作品データは共有)。
- 新規登録には、管理者が「管理」タブで発行する招待コード (1回限り・7日有効) が要ります。
- 2段階認証 (TOTP) はアカウント画面で設定します。リカバリーコードは**1度だけ**表示されます。管理者は既定で必須です (`REQUIRE_2FA`)。
- パスワードは10文字以上、ユーザー名は半角英数字と `_ . -` (32文字以内)。最初の管理者は削除も降格もできません。
- ログイン中の端末は、アカウント画面で確認・ログアウトできます。

## 公開するときのセキュリティ

アプリのポートをインターネットへ直接公開しないでください。

1. HTTPS を終端するプロキシ (Tailscale Funnel、Caddy、Cloudflare Tunnel など) の後ろに置きます。
2. 公開する前に、管理者を登録します。
3. `.env` を設定します。
   - `ALLOWED_HOSTS=cal.example.com`: 別のホスト名での要求を断ります (カンマ区切り)。
   - `REQUIRE_2FA=all`: 全員に2段階認証を必須にします (公開なら推奨)。
   - `COOKIE_SECURE=1`: プロキシが `X-Forwarded-Proto: https` を付けないときだけ必要です。
4. `TRUSTED_PROXIES` (信頼するプロキシのアドレス) から来た要求だけ、`X-Forwarded-For` の**右端**を接続元として使います。プロキシがこの値に接続元を追記することを確認してください。
   - Docker では、`compose.yaml` の指定 (ループバックと `172.16.0.0/12`) のままで動きます。ネットワークの範囲を変えたときは `.env` で指定します。
   - プロキシが別のホストにあるときは、そのアドレスを入れ、`COOKIE_SECURE=1` も設定します。
5. 大量の接続は、前段のプロキシでも制限してください (Caddy は `timeouts`、nginx は `client_header_timeout` と `limit_conn` など)。
6. 必要なら、「管理」タブで Cloudflare Turnstile を設定します (先に `ALLOWED_HOSTS` が要ります。tailnet だけなら不要)。

**Cloudflare Tunnel の場合**: `ALLOWED_HOSTS` に公開ホスト名と tailnet のホスト名の両方を入れます。Cloudflare Access を使うなら、経路より先にアプリケーションを作ります。Service URL は `http://localhost:8000` です。Turnstile の許可ホスト名は公開ドメインだけにし、キーは公開 URL でログインして入力します。公開後は、ログイン記録 (`docker compose logs | grep auth`) の `ip=` が本物の接続元か確認してください。

アプリ自身も、遅い接続の切断、接続数・本文サイズの上限、`chunked`・`Expect`・`Connection: upgrade` の拒否などを行います。HTTP サーバーの修正内容は `app/vendor/tiny_http/PATCHES.md` にあります。

## 通知

右上の「通知」→「通知を有効にする」で、未視聴の話が開始の N 分前 (既定10分) に届きます。HTTPS が必要です。iPhone / iPad は、ホーム画面に追加したアプリ (iOS 16.4 以降) から有効にします。ログアウトした端末では止まるので、ログインして有効にし直してください。

## 取得の仕組み

公式 API を、[bot 向けルール](https://docs.cal.syoboi.jp/rule/bot/)に沿って、直列・1.5秒間隔で使います。

- 初回: DB が空のとき、60日前〜120日先を取得 (数分)。
- 定期: 6時間ごと (`SYNC_INTERVAL_HOURS`。`0` で自動取得なし)。差分だけ取得します。
- 期間指定: 「管理」タブから。1回最大90日、1日3回まで。古い作品や取りこぼしの取り直しに使います。
- `429` を受けると自動取得は3時間止まります。連続5回失敗でも停止します (「今すぐ取得」の成功で再開)。

設定項目は `.env.example` にあります。

## 運用

```bash
docker compose ps                                        # healthy なら OK
docker compose logs --tail 20
git pull && docker compose up -d --build                 # 更新
(umask 077; sudo sqlite3 data/anime.db ".backup $HOME/backup-$(date +%F).db")   # バックアップ (ハッシュや鍵が入るので自分だけが読める権限で)
docker compose down                                      # 停止
```

ログインできなくなったときは、ホストで次を実行します (`-u app` は必須)。

```bash
docker compose exec -u app app ani-cal user list
docker compose exec -u app app ani-cal user reset-password <ユーザー名>   # 仮パスワードを1回だけ表示
docker compose exec -u app app ani-cal user reset-2fa <ユーザー名>        # 2段階認証を解除
docker compose exec -u app app ani-cal turnstile off                      # Turnstile を無効にする (turnstile status で確認)
```

データは `./data/anime.db` にあります。

## 開発

```bash
cd app
DB_PATH=./dev.db BIND=127.0.0.1:8000 cargo run   # http://127.0.0.1:8000/
cargo test
cargo build && bash tests/regression.sh target/debug/ani-cal   # 回帰テスト (curl が要る)
```

ビルドは Rust 1.100 (`app/Dockerfile`) で確認しています。画面 (`app/static/`) はバイナリに埋め込むので、変更したら再ビルドしてください。API のルートは `app/src/api.rs` にあります。

## 連絡先

個人用のアプリで、サポートや機能追加は保証しません。しょぼいカレンダーへの取得について問題があれば、[Misskey](https://misskey.106aleph.xyz/@106aleph) からご連絡ください。脆弱性の報告は [SECURITY.md](SECURITY.md) をご覧ください。

## ライセンス

[MIT License](LICENSE)
