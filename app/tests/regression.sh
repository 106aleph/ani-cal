#!/usr/bin/env bash
# 脆弱性診断で見つかった問題の回帰テスト。実際にサーバーを起動して、HTTP で確かめる。
# 使い方: tests/regression.sh <ani-cal のバイナリ>   (curl と bash が要る)
set -u
BIN=${1:?"使い方: $0 <ani-cal のバイナリ>"}
DIR=$(mktemp -d); trap 'kill $PID 2>/dev/null; rm -rf "$DIR"' EXIT
PORT=${PORT:-18123}; B=http://127.0.0.1:$PORT; J=(-H "Content-Type: application/json")
FAIL=0
ok()  { echo "  ok   $1"; }
ng()  { echo "  FAIL $1"; FAIL=1; }
chk() { if [ "$2" = "$3" ]; then ok "$1"; else ng "$1 (期待: $3 / 実際: $2)"; fi; }
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

MAX_CONNECTIONS_PER_IP=8 DB_PATH=$DIR/a.db BIND=0.0.0.0:$PORT REQUIRE_2FA=off SYNC_INTERVAL_HOURS=0 SYNC_ON_STARTUP=false \
  SYOBOI_UA_URL=https://example.com TRUSTED_PROXIES=127.0.0.1 ALLOWED_HOSTS=cal.example.com "$BIN" >"$DIR/log" 2>&1 &
PID=$!
for _ in $(seq 1 50); do curl -s "$B/api/health" >/dev/null && break; sleep 0.2; done
TOK=$(grep -o '分間）: [A-Za-z0-9]*' "$DIR/log" | awk '{print $2}')
PW=correct-horse-battery-1
setup() { echo "{\"username\":\"first\",\"password\":\"$PW\",\"setup_token\":\"$1\"}"; }

echo "M-1 最初の管理者の登録"
chk "セットアップコードなしは断る"        "$(code "${J[@]}" -d '{"username":"first","password":"'$PW'"}' $B/api/auth/setup)" 403
chk "違うコードは断る"                    "$(code "${J[@]}" -d "$(setup wrong)" $B/api/auth/setup)" 403
chk "正しいコードでも、公開 IP からは断る" "$(code "${J[@]}" -H 'Host: cal.example.com' -H 'X-Forwarded-For: 8.8.8.8' -d "$(setup "$TOK")" $B/api/auth/setup)" 403
chk "正しいコード・ローカルからは通る"    "$(code -c "$DIR/c1" "${J[@]}" -d "$(setup "$TOK")" $B/api/auth/setup)" 201
chk "登録後は断る"                        "$(code "${J[@]}" -d "$(setup "$TOK")" $B/api/auth/setup)" 409

echo "M-3 最初の管理者は、降格も削除もできない"
CODE=$(curl -s -b "$DIR/c1" "${J[@]}" -X POST -d '{"role":"admin"}' $B/api/invites | grep -o '"code":"[^"]*"' | cut -d'"' -f4)
curl -s -c "$DIR/c2" "${J[@]}" -d "{\"username\":\"second\",\"password\":\"correct-horse-battery-2\",\"code\":\"$CODE\"}" $B/api/auth/register >/dev/null
chk "降格は 409"  "$(code -b "$DIR/c2" "${J[@]}" -X PATCH -d '{"role":"user"}' $B/api/users/1)" 409
chk "削除は 409"  "$(code -b "$DIR/c2" -X DELETE $B/api/users/1)" 409

echo "N-2 / L-4 ログイン失敗の記録のキーが衝突しない・認証コードの総当たりを止める"
curl -s -b "$DIR/c1" "${J[@]}" -X POST -d '{"password":"'$PW'"}' $B/api/auth/totp/setup >/dev/null
for _ in 1 2 3 4 5 6; do code "${J[@]}" -d '{"username":"\u0000otp|1","password":"x"}' $B/api/auth/login >/dev/null; done
chk "細工したユーザー名で失敗させても、認証コードの確認は止まらない" "$(code -b "$DIR/c1" "${J[@]}" -X POST -d '{"code":"000000"}' $B/api/auth/totp/enable)" 400
for _ in 1 2 3 4; do code -b "$DIR/c1" "${J[@]}" -X POST -d '{"code":"000000"}' $B/api/auth/totp/enable >/dev/null; done
chk "認証コードを5回間違えると止まる"     "$(code -b "$DIR/c1" "${J[@]}" -X POST -d '{"code":"000000"}' $B/api/auth/totp/enable)" 429

echo "N-3 本文を読み残した要求が、次の要求として解釈されない"
H='Host: 127.0.0.1\r\n'
count() { timeout 3 cat <&"$1" | grep -ao 'HTTP/1.1 [0-9]*' | wc -l; }
body=$(printf 'GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n'; head -c 4940 /dev/zero | tr '\0' x)
exec 5<>/dev/tcp/127.0.0.1/$PORT
{ printf "POST /api/auth/login HTTP/1.1\r\n${H}Content-Type: text/plain\r\nContent-Length: ${#body}\r\n\r\n"; printf '%s' "$body"; printf "GET /api/health HTTP/1.1\r\n${H}Connection: close\r\n\r\n"; } >&5
chk "Content-Length の読み残し（応答は415と200の2つだけ）" "$(count 5)" 2; exec 5>&-

echo "巨大な Content-Length でも落ちない"
exec 7<>/dev/tcp/127.0.0.1/$PORT
printf "POST /api/auth/login HTTP/1.1\r\n${H}Content-Type: application/json\r\nContent-Length: 1099511627776\r\n\r\n" >&7
sleep 1
chk "プロセスが生きている" "$(code -m 3 $B/api/health)" 200; exec 7>&-

echo "N-7 本文を1バイトずつ送り続ける接続が、ワーカーを占有しない"
DRIP=(); for _ in $(seq 1 12); do ( exec 8<>/dev/tcp/127.0.0.1/$PORT; printf "POST /api/auth/login HTTP/1.1\r\n${H}Content-Type: application/json\r\nContent-Length: 100000\r\n\r\n" >&8; for _ in 1 2 3 4 5 6 7 8; do printf x >&8 2>/dev/null; sleep 1; done ) & DRIP+=($!); done
sleep 3
t=$(curl -s -o /dev/null -m 3 -w '%{time_total}' $B/api/health)
chk "攻撃中も、1秒以内に応答する" "$(awk -v t="$t" 'BEGIN{print (t<1.0)?"fast":"slow"}')" fast
wait "${DRIP[@]}"

echo "M-4 読む本文（1KB を超える JSON）を1バイトずつ送り続ける接続が、ワーカーを占有しない"
DRIP=(); for _ in $(seq 1 12); do ( exec 9<>/dev/tcp/127.0.0.1/$PORT; printf "POST /api/auth/login HTTP/1.1\r\n${H}Content-Type: application/json\r\nContent-Length: 5000\r\n\r\n" >&9; for _ in 1 2 3 4 5 6 7 8; do printf ' ' >&9 2>/dev/null; sleep 1; done ) & DRIP+=($!); done
sleep 3
t=$(curl -s -o /dev/null -m 3 -w '%{time_total}' $B/api/health)
chk "攻撃中も、1秒以内に応答する" "$(awk -v t="$t" 'BEGIN{print (t<1.0)?"fast":"slow"}')" fast
wait "${DRIP[@]}"

echo "M-1 話数の範囲外の値・巨大な値では、話を作れない（DB の書き込みを止められない）"
TID=$(curl -s -b "$DIR/c1" "${J[@]}" -d '{"title":"x"}' $B/api/tracks | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
chk "話数 -1e15 は 422"       "$(code -b "$DIR/c1" "${J[@]}" -d '{"number":-1000000000000000,"start_at":"2026-01-01T00:00"}' $B/api/tracks/$TID/episodes)" 422
chk "話数 1e15 は 422"        "$(code -b "$DIR/c1" "${J[@]}" -d '{"number":1000000000000000,"start_at":"2026-01-01T00:00"}' $B/api/tracks/$TID/episodes)" 422
chk "正常な話数は作れる"      "$(code -b "$DIR/c1" "${J[@]}" -d '{"number":1,"start_at":"2026-01-01T00:00"}' $B/api/tracks/$TID/episodes)" 201
s=$(date +%s%N); g=$(code -m 10 -b "$DIR/c1" "${J[@]}" -d '{"until":20,"interval_days":7}' $B/api/tracks/$TID/generate)
chk "予測の作成は、すぐ終わる" "$g" 200
chk "長すぎるタイトルは 422"  "$(code -b "$DIR/c1" "${J[@]}" -d "{\"title\":\"$(head -c 300 /dev/zero | tr '\0' a)\"}" $B/api/tracks)" 422

echo "L-2 /api/status の登録数は、管理者以外には自分の分だけ"
CODE2=$(curl -s -b "$DIR/c1" "${J[@]}" -X POST -d '{"role":"user"}' $B/api/invites | grep -o '"code":"[^"]*"' | cut -d'"' -f4)
curl -s -c "$DIR/c3" "${J[@]}" -d "{\"username\":\"third\",\"password\":\"correct-horse-battery-3\",\"code\":\"$CODE2\"}" $B/api/auth/register >/dev/null
chk "管理者: 作品が1件以上"   "$(curl -s -b "$DIR/c1" $B/api/status | grep -o '"tracks":[0-9]*')" '"tracks":1'
chk "一般ユーザー: 自分の分(0)" "$(curl -s -b "$DIR/c3" $B/api/status | grep -o '"tracks":[0-9]*')" '"tracks":0'

echo "M-2 / M-3 大きすぎるヘッダ・あいまいな本文の境界は、400 で断る"
raw() { exec 4<>/dev/tcp/127.0.0.1/$PORT; printf "$1" >&4; timeout 3 head -c 200 <&4 | head -1 | tr -d '\r'; exec 4>&-; }
long=$(head -c 20000 /dev/zero | tr '\0' a)
chk "巨大なヘッダ行は 400"            "$(raw "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nX-A: $long\r\n\r\n")" "HTTP/1.1 400 Bad Request"
many=$(for i in $(seq 1 150); do printf "X-H$i: v\r\n"; done)
chk "ヘッダが多すぎる要求は 400"      "$(raw "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n$many\r\n")" "HTTP/1.1 400 Bad Request"
chk "Content-Length が2つは 400"      "$(raw "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}")" "HTTP/1.1 400 Bad Request"
chk "CL と TE の両方は 400"           "$(raw "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n")" "HTTP/1.1 400 Bad Request"
chk "TE が chunked 以外は 400"        "$(raw "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nTransfer-Encoding: identity\r\n\r\n")" "HTTP/1.1 400 Bad Request"
chk "Content-Length が数字でないものは 400" "$(raw "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 1x\r\n\r\n{}")" "HTTP/1.1 400 Bad Request"
chk "その後も動いている"              "$(code -m 3 $B/api/health)" 200

echo "L-6 ALLOWED_HOSTS があるとき、外からの要求は、Host を 127.0.0.1 にしても通らない"
chk "ローカルの 127.0.0.1 は通る"     "$(code $B/api/health)" 200
chk "公開 IP からの Host: 127.0.0.1 は 421" "$(code -H 'X-Forwarded-For: 8.8.8.8' $B/api/health)" 421
chk "許可したホスト名は通る"          "$(code -H 'Host: cal.example.com' -H 'X-Forwarded-For: 8.8.8.8' $B/api/health)" 200

echo "R-1 ヘッダを少しずつ送り続ける接続は、合計の期限で切れる。同じ接続元からの接続数にも上限がある"
LAN=$(hostname -I 2>/dev/null | awk '{print $1}')
# ヘッダを、3秒ごとに1バイトずつ送る（無通信の15秒は超えない）
( exec 5<>/dev/tcp/127.0.0.1/$PORT; printf 'GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nX-A: ' >&5
  for _ in 1 2 3 4 5; do sleep 3; printf a >&5 2>/dev/null; done
  a=$(date +%s%N); timeout 2 cat <&5 >/dev/null; echo $(( ($(date +%s%N)-a)/1000000 )) > "$DIR/drip_ms" ) &
DRIPH=$!
if [ -n "$LAN" ] && [ "$LAN" != 127.0.0.1 ]; then
  HOLD=(); for _ in $(seq 1 10); do ( exec 6<>/dev/tcp/$LAN/$PORT; printf 'G' >&6; sleep 8 ) 2>/dev/null & HOLD+=($!); done
  sleep 1
  a=$(date +%s%N); ( exec 7<>/dev/tcp/$LAN/$PORT; timeout 3 cat <&7 >/dev/null ) 2>/dev/null; ms=$(( ($(date +%s%N)-a)/1000000 ))
  chk "上限（接続元ごと8本）を超えた接続は、すぐに閉じられる" "$(awk -v m="$ms" 'BEGIN{print (m<1500)?"closed":"open"}')" closed
  chk "その間も、同じ機械（ループバック）からは応答する"   "$(code -m 3 $B/api/health)" 200
  wait "${HOLD[@]}"
else
  echo "  skip 接続元ごとの上限（ループバック以外のアドレスが無い）"
fi
wait $DRIPH
chk "ヘッダを15秒近く送り続けた接続は、期限（10秒）で切れる" "$(awk -v m="$(cat "$DIR/drip_ms")" 'BEGIN{print (m<1500)?"closed":"open"}')" closed

echo "R-2 取得データの話が範囲外の値でも、ほかの項目だけを直せる"
sqlexec() { if command -v sqlite3 >/dev/null; then sqlite3 "$DIR/a.db" "$1"; elif command -v python3 >/dev/null; then python3 -c "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute(sys.argv[2]); c.commit()" "$DIR/a.db" "$1"; else return 1; fi; }
EID=$(curl -s -b "$DIR/c1" "${J[@]}" -d '{"number":55,"start_at":"2026-02-01T00:00"}' $B/api/tracks/$TID/episodes | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
if sqlexec "UPDATE episodes SET number=-5, subtitle='$(head -c 400 /dev/zero | tr '\0' a)', source='sync' WHERE id=$EID"; then
  LONGSUB=$(head -c 400 /dev/zero | tr '\0' a)
  chk "値を変えずに視聴済みだけ直す（範囲外の話数・長いサブタイトルのまま）" "$(code -b "$DIR/c1" "${J[@]}" -X PATCH -d "{\"number\":-5,\"subtitle\":\"$LONGSUB\",\"watched\":true}" $B/api/episodes/$EID)" 200
  chk "話数を、別の範囲外の値に変えるのは 422" "$(code -b "$DIR/c1" "${J[@]}" -X PATCH -d '{"number":-9}' $B/api/episodes/$EID)" 422
  chk "サブタイトルを、別の長い値に変えるのは 422" "$(code -b "$DIR/c1" "${J[@]}" -X PATCH -d "{\"subtitle\":\"b$LONGSUB\"}" $B/api/episodes/$EID)" 422
else
  echo "  skip（sqlite3 も python3 も無い）"
fi

echo "L-1 chunked / Expect は、本文を読む前に断る"
chk "chunked は 411" "$(raw "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n")" "HTTP/1.1 411 Length Required"
chk "Expect: 100-continue は 417" "$(raw "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n")" "HTTP/1.1 417 Expectation Failed"

echo "N-8 終わらない chunk-size 行（CRLF なし）を送り続けても、メモリを使わず、ワーカーも占有しない"
DRIP=(); for _ in $(seq 1 12); do ( exec 8<>/dev/tcp/127.0.0.1/$PORT; printf "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n1;" >&8; for _ in 1 2 3 4 5 6 7 8; do printf a >&8 2>/dev/null; sleep 1; done ) & DRIP+=($!); done
sleep 3
t=$(curl -s -o /dev/null -m 3 -w '%{time_total}' $B/api/health)
chk "攻撃中も、1秒以内に応答する" "$(awk -v t="$t" 'BEGIN{print (t<1.0)?"fast":"slow"}')" fast
wait "${DRIP[@]}"

[ $FAIL = 0 ] && echo "すべて通りました" || { echo "失敗があります"; exit 1; }
