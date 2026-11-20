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

DB_PATH=$DIR/a.db BIND=127.0.0.1:$PORT REQUIRE_2FA=off SYNC_INTERVAL_HOURS=0 SYNC_ON_STARTUP=false \
  SYOBOI_UA_URL=https://example.com TRUSTED_PROXIES=127.0.0.1 "$BIN" >"$DIR/log" 2>&1 &
PID=$!
for _ in $(seq 1 50); do curl -s "$B/api/health" >/dev/null && break; sleep 0.2; done
TOK=$(grep -o 'ます）: [A-Za-z0-9]*' "$DIR/log" | awk '{print $2}')
PW=correct-horse-battery-1
setup() { echo "{\"username\":\"first\",\"password\":\"$PW\",\"setup_token\":\"$1\"}"; }

echo "M-1 最初の管理者の登録"
chk "セットアップコードなしは断る"        "$(code "${J[@]}" -d '{"username":"first","password":"'$PW'"}' $B/api/auth/setup)" 403
chk "違うコードは断る"                    "$(code "${J[@]}" -d "$(setup wrong)" $B/api/auth/setup)" 403
chk "正しいコードでも、公開 IP からは断る" "$(code "${J[@]}" -H 'X-Forwarded-For: 8.8.8.8' -d "$(setup "$TOK")" $B/api/auth/setup)" 403
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
b2='GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n'; len=$(printf "$b2" | wc -c)
exec 6<>/dev/tcp/127.0.0.1/$PORT
{ printf "POST /api/auth/login HTTP/1.1\r\n${H}Content-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n%x\r\n" "$len"; printf "$b2"; printf "\r\n0\r\n\r\nGET /api/health HTTP/1.1\r\n${H}Connection: close\r\n\r\n"; } >&6
chk "chunked の読み残し（接続を閉じる。応答は1つだけ）" "$(count 6)" 1; exec 6>&-

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

echo "L-1 chunked で JSON 以外を送れない"
chk "text/plain の chunked は 415" "$(code -b "$DIR/c1" -H 'Content-Type: text/plain' -H 'Transfer-Encoding: chunked' -d '{}' $B/api/sync)" 415

[ $FAIL = 0 ] && echo "すべて通りました" || { echo "失敗があります"; exit 1; }
