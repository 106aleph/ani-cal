//! アニメ配信カレンダー（しょぼいカレンダーから配信予定を取得して視聴記録するセルフホストアプリ）
mod api;
mod auth;
mod config;
mod db;
mod log;
mod notify;
mod sync;
mod syoboi;
mod turnstile;
mod util;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};
use tiny_http::{Header, Method, Request, Response, Server};

/// 接続の読み書きの待ち時間（秒）。ヘッダや本文を、これより長く止めながら送る接続は切る。
/// 何も流れない接続（keep-alive の待ち）も、この時間で閉じる
const SOCKET_TIMEOUT_SEC: i64 = 15;
/// 本文を読み終えるまでの、合計の制限時間（秒）。本文は最大 256KB で、通常は一瞬で届く
const BODY_DEADLINE_SEC: u64 = 15;

struct Asset {
    path: &'static str,
    mime: &'static str,
    bytes: &'static [u8],
    etag: String,
}

fn fnv(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("\"{:016x}\"", h)
}

fn assets() -> &'static Vec<Asset> {
    static A: OnceLock<Vec<Asset>> = OnceLock::new();
    A.get_or_init(|| {
        let list: [(&'static str, &'static str, &'static [u8]); 9] = [
            ("index.html", "text/html; charset=utf-8", include_bytes!("../static/index.html")),
            ("app.js", "text/javascript; charset=utf-8", include_bytes!("../static/app.js")),
            ("style.css", "text/css; charset=utf-8", include_bytes!("../static/style.css")),
            ("manifest.json", "application/json", include_bytes!("../static/manifest.json")),
            ("sw.js", "text/javascript; charset=utf-8", include_bytes!("../static/sw.js")),
            ("icons/apple-touch-icon.png", "image/png", include_bytes!("../static/icons/apple-touch-icon.png")),
            ("icons/icon-192.png", "image/png", include_bytes!("../static/icons/icon-192.png")),
            ("icons/icon-512.png", "image/png", include_bytes!("../static/icons/icon-512.png")),
            ("icons/icon-maskable-512.png", "image/png", include_bytes!("../static/icons/icon-maskable-512.png")),
        ];
        list.iter().map(|(p, m, b)| Asset { path: p, mime: m, bytes: b, etag: fnv(b) }).collect()
    })
}

fn asset(name: &str) -> Option<&'static Asset> {
    assets().iter().find(|a| a.path == name)
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

/// すべての応答に付けるセキュリティ関連のヘッダ
/// CSP。Turnstile を使っている間だけ、Cloudflare の確認用の script と枠を許可する
fn csp() -> &'static str {
    const PLAIN: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'";
    const TURNSTILE: &str = "default-src 'self'; script-src 'self' https://challenges.cloudflare.com; frame-src https://challenges.cloudflare.com; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'";
    if turnstile::enabled() { TURNSTILE } else { PLAIN }
}

fn with_sec<R: std::io::Read>(r: Response<R>) -> Response<R> {
    r.with_header(header("Content-Security-Policy", csp()))
    .with_header(header("X-Content-Type-Options", "nosniff"))
    .with_header(header("X-Frame-Options", "DENY"))
    .with_header(header("Referrer-Policy", "no-referrer")) // 購読URL（トークン入り）を外部へ漏らさない
    .with_header(header("Cross-Origin-Opener-Policy", "same-origin"))
    .with_header(header("Cross-Origin-Resource-Policy", "same-origin"))
    .with_header(header("Permissions-Policy", "camera=(), microphone=(), geolocation=(), payment=(), usb=()"))
    // http で受けた場合、ブラウザは無視する。https で一度開けば、以後は https だけを使わせる
    .with_header(header("Strict-Transport-Security", "max-age=31536000; includeSubDomains"))
}

/// Host ヘッダの検査（ALLOWED_HOSTS が設定されているとき）。127.0.0.1 / localhost は常に許可する
fn host_allowed(rq: &Request) -> bool {
    let allowed = &config::get().allowed_hosts;
    if allowed.is_empty() {
        return true;
    }
    let host = req_header(rq, "Host").unwrap_or_default().to_lowercase();
    let name = if host.starts_with('[') { host.split(']').next().unwrap_or("").trim_start_matches('[').to_string() } else { host.split(':').next().unwrap_or("").to_string() };
    matches!(name.as_str(), "127.0.0.1" | "localhost" | "::1") || allowed.iter().any(|a| *a == name || *a == host)
}

/// 別のサイトからのリクエスト（CSRF）を弾く: Origin があるなら、このサーバーのホストと同じであること
fn same_origin(rq: &Request) -> bool {
    match req_header(rq, "Sec-Fetch-Site").as_deref() {
        Some("same-origin") | Some("none") => return true, // ブラウザが保証する
        Some(_) => return false,                           // same-site / cross-site
        None => {}
    }
    let Some(origin) = req_header(rq, "Origin") else { return true }; // ブラウザ以外（curl など）
    let o = origin.split_once("://").map(|(_, h)| h).unwrap_or(&origin);
    [req_header(rq, "Host"), req_header(rq, "X-Forwarded-Host")].into_iter().flatten().any(|h| h.eq_ignore_ascii_case(o))
}

/// 直接の接続元が、信頼するプロキシか（既定は、同じ機械と私的なアドレスだけ。`TRUSTED_PROXIES` で変えられる）
fn from_trusted_proxy(rq: &Request) -> bool {
    rq.remote_addr().map(|a| config::ip_in(&config::get().trusted_proxies, a.ip())).unwrap_or(false)
}

/// 失敗回数・回数制限のための接続元。
/// 信頼するプロキシ（tailscale serve・cloudflared など）から来た要求だけ、X-Forwarded-For を使う。
/// それ以外（インターネットから直接届いた要求）は、X-Forwarded-For を無視して、直接の接続元を使う。
/// X-Forwarded-For は、クライアントが勝手に書き足せるので、プロキシが追記する**いちばん右**の値だけを信用する
/// （左側は偽装できる）。右端が IP アドレスとして読めないときは、プロキシ自身のアドレスにする
fn client_id(rq: &Request) -> String {
    let peer = rq.remote_addr().map(|a| a.ip());
    if from_trusted_proxy(rq) {
        // ヘッダが複数あっても、最後に追記された値を見る
        let last = rq.headers().iter().filter(|h| h.field.equiv("X-Forwarded-For")).last().map(|h| h.value.as_str().to_string());
        if let Some(ip) = last.and_then(|v| v.rsplit(',').next().and_then(|s| s.trim().parse::<std::net::IpAddr>().ok())) {
            return ip.to_string();
        }
    }
    peer.map(|ip| ip.to_string()).unwrap_or_default()
}

/// 待ち受けの口に、読み書きの待ち時間（秒）を設定する。accept した接続は、この設定を引き継ぐ（Linux）。
/// 何も送らない・少しずつしか送らない接続が、いつまでも居座るのを防ぐ。
/// tiny_http には待ち時間の設定がないので、ソケットに直接設定する。待ち時間を過ぎると、接続は閉じられる
fn set_socket_timeouts(l: &std::net::TcpListener, secs: i64) {
    use std::os::fd::AsRawFd;
    let tv = libc::timeval { tv_sec: secs as libc::time_t, tv_usec: 0 };
    for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: fd は生きている TcpListener のもの。tv は有効な timeval で、大きさも合っている
        let r = unsafe {
            libc::setsockopt(
                l.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                &tv as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if r != 0 {
            warn!("app", "ソケットの待ち時間を設定できませんでした: {}", std::io::Error::last_os_error());
        }
    }
}

/// 待ち受けの口に待ち時間を設定すると、新しい接続が長く来ないときに accept も時間切れになり、
/// tiny_http はそれを致命的な失敗として、受け付けをやめてしまう。それを避けるため、待ち時間より短い間隔で、
/// 自分自身に空の接続をして、accept を起こし続ける（空の接続は、すぐに閉じられる）
fn keep_accepting(l: &std::net::TcpListener) {
    let Ok(mut addr) = l.local_addr() else { return };
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    std::thread::Builder::new()
        .name("accept-wake".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs((SOCKET_TIMEOUT_SEC as u64 / 4).max(1)));
            let _ = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2));
        })
        .ok();
}

/// 本文を、全体の制限時間つきで読む。1回の読み取りは上の待ち時間で切れるが、1バイトずつ送り続ける相手には、
/// それだけでは足りないので、読み始めてからの合計も制限する。時間切れ・読み取りの失敗は None
fn read_body(rq: &mut Request, limit: usize) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(BODY_DEADLINE_SEC);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let reader = rq.as_reader();
    loop {
        if std::time::Instant::now() > deadline {
            return None;
        }
        // 上限(+1)までしか読まない
        let want = chunk.len().min(limit + 1 - buf.len());
        if want == 0 {
            break;
        }
        match reader.read(&mut chunk[..want]) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    // 以前の read_to_string と同じく、UTF-8 として読めなければ空にする
    Some(String::from_utf8(buf).unwrap_or_default())
}

fn is_json(rq: &Request) -> bool {
    req_header(rq, "Content-Type").map(|v| v.trim().to_lowercase().starts_with("application/json")).unwrap_or(false)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn respond_json(rq: Request, status: u16, body: &Value) {
    respond_json_cookie(rq, status, body, None)
}

/// cookie: Set-Cookie に入れる値（ログイン・ログアウト時だけ）
fn respond_json_cookie(rq: Request, status: u16, body: &Value, cookie: Option<String>) {
    let _ = if status == 204 {
        let mut r = with_sec(Response::empty(204));
        if let Some(c) = &cookie {
            r = r.with_header(header("Set-Cookie", c));
        }
        rq.respond(r)
    } else {
        let mut r = with_sec(Response::from_string(body.to_string()))
            .with_status_code(status)
            .with_header(header("Content-Type", "application/json"))
            .with_header(header("Cache-Control", "no-store"));
        // 本文を受け取りきれなかった・断った要求は、残りを読み捨てて待たずに、接続ごと閉じる
        if status == 408 || status == 413 {
            r = r.with_header(header("Connection", "close"));
        }
        if let Some(c) = &cookie {
            r = r.with_header(header("Set-Cookie", c));
        }
        rq.respond(r)
    };
}

fn req_header(rq: &Request, name: &'static str) -> Option<String> {
    rq.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str().to_string())
}

/// Cookie ヘッダからセッションのトークンを取り出す
fn session_token(rq: &Request) -> Option<String> {
    let c = req_header(rq, "Cookie")?;
    auth::token_from_cookie_header(&c, is_https(rq))
}

/// HTTPS 終端のプロキシ（tailscale serve / funnel など）の後ろにいるときは、Cookie に Secure を付ける。
/// プロキシが X-Forwarded-Proto を付けない構成では、COOKIE_SECURE=1 で常に Secure にする
fn is_https(rq: &Request) -> bool {
    // X-Forwarded-Proto も、信頼するプロキシが付けたものだけを見る
    config::get().cookie_secure
        || (from_trusted_proxy(rq) && req_header(rq, "X-Forwarded-Proto").map(|v| v.to_lowercase().contains("https")).unwrap_or(false))
}

fn respond_asset(rq: Request, a: &Asset) {
    let inm = rq.headers().iter().find(|h| h.field.equiv("If-None-Match")).map(|h| h.value.as_str().to_string());
    if inm.as_deref() == Some(a.etag.as_str()) {
        let _ = rq.respond(Response::empty(304).with_header(header("ETag", &a.etag)).with_header(header("Cache-Control", "no-cache")));
        return;
    }
    let _ = rq.respond(
        with_sec(Response::from_data(a.bytes.to_vec()))
            .with_header(header("Content-Type", a.mime))
            .with_header(header("ETag", &a.etag))
            .with_header(header("Cache-Control", "no-cache")),
    );
}

fn handle(mut rq: Request) {
    let method = match rq.method() {
        Method::Get | Method::Head => "GET",
        Method::Post => "POST",
        Method::Patch => "PATCH",
        Method::Put => "PUT",
        Method::Delete => "DELETE",
        _ => "OTHER",
    }
    .to_string();
    let url = rq.url().to_string();
    let (path, qs) = url.split_once('?').unwrap_or((&url, ""));
    let path = percent_decode(path);
    let query = parse_query(qs);
    // このリクエストの接続元と User-Agent。ログインしたとき (と、情報のない古いセッションが使われたとき) に、セッションへ記録する
    auth::set_meta(&client_id(&rq), &req_header(&rq, "User-Agent").unwrap_or_default());

    if !host_allowed(&rq) {
        return respond_json(rq, 421, &util::msg("このホスト名では受け付けていません"));
    }
    if method == "OTHER" {
        return respond_json(rq, 405, &util::msg("Method Not Allowed"));
    }

    // 静的ファイル
    if method == "GET" {
        let found = match path.as_str() {
            "/" => asset("index.html"),
            "/sw.js" => asset("sw.js"), // scope を「/」にするためルートから配信する
            p if p.starts_with("/static/") => asset(&p["/static/".len()..]),
            _ => None,
        };
        if let Some(a) = found {
            return respond_asset(rq, a);
        }
    }

    // CSRF 対策: 状態を変える要求は、同じオリジンからで、本文があれば JSON であること
    // （JSON 以外の Content-Type は、別サイトのフォームからも送れてしまうため）
    if method != "GET" && path.starts_with("/api/") {
        if !same_origin(&rq) {
            return respond_json(rq, 403, &util::msg("別のサイトからのリクエストは受け付けません"));
        }
        if rq.body_length().unwrap_or(0) > 0 && !is_json(&rq) {
            return respond_json(rq, 415, &util::msg("Content-Type は application/json にしてください"));
        }
    }

    // 本文（JSON）
    let mut raw = String::new();
    if matches!(method.as_str(), "POST" | "PATCH" | "PUT") {
        // 本文の大きさに上限（認証まわりは小さく）。超えたら読まずに断る
        let limit: usize = if path.starts_with("/api/auth/") { 8 * 1024 } else { 256 * 1024 };
        if rq.body_length().unwrap_or(0) > limit {
            return respond_json(rq, 413, &util::msg("リクエストが大きすぎます"));
        }
        let Some(b) = read_body(&mut rq, limit) else {
            return respond_json(rq, 408, &util::msg("リクエストの受信に時間がかかりすぎました"));
        };
        raw = b;
        if raw.len() > limit {
            return respond_json(rq, 413, &util::msg("リクエストが大きすぎます"));
        }
    }
    let body: Value = if raw.trim().is_empty() {
        Value::Null
    } else {
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return respond_json(rq, 422, &util::msg("JSON の形式が不正です")),
        }
    };

    if !path.starts_with("/api/") {
        return respond_json(rq, 404, &json!({"detail": "Not Found"}));
    }
    let secure = is_https(&rq);
    // 認証が要らないのは、稼働確認とログインだけ
    if method == "GET" && path == "/api/health" {
        return respond_json(rq, 200, &json!({"ok": true}));
    }
    // ログイン・登録の連打を、接続元ごとに制限する（Argon2 の計算で CPU・メモリを使わせる攻撃の抑止）
    if method == "POST" && matches!(path.as_str(), "/api/auth/login" | "/api/auth/register" | "/api/auth/setup")
        && !auth::rate_ok("auth", &client_id(&rq), 20, 60)
    {
        return respond_json(rq, 429, &util::msg("リクエストが多すぎます。しばらく待ってからやり直してください"));
    }
    // Botプロテクション（Turnstile）は、失敗の回数にも数えず、パスワードの計算にも入る前に済ませる
    if method == "POST" && matches!(path.as_str(), "/api/auth/login" | "/api/auth/register") {
        if let Err(e) = turnstile::check(&body, &client_id(&rq)) {
            return respond_json(rq, e.status, &util::msg(&e.detail));
        }
    }
    if method == "POST" && path == "/api/auth/login" {
        let client = client_id(&rq);
        return match auth::login(&body, &client) {
            // 2段階認証の入力待ちのときは、トークンが空（Cookie は発行しない）
            Ok((me, token)) if token.is_empty() => respond_json(rq, 200, &me),
            Ok((me, token)) => respond_json_cookie(rq, 200, &me, Some(auth::cookie_value(&token, secure))),
            Err(e) => respond_json(rq, e.status, &util::msg(&e.detail)),
        };
    }
    if method == "GET" && path == "/api/auth/setup" {
        return respond_json(rq, 200, &json!({"needed": auth::setup_needed(), "turnstile_site_key": turnstile::site_key()}));
    }
    if method == "POST" && path == "/api/auth/setup" {
        return match auth::setup(&body) {
            Ok((me, token)) => respond_json_cookie(rq, 201, &me, Some(auth::cookie_value(&token, secure))),
            Err(e) => respond_json(rq, e.status, &util::msg(&e.detail)),
        };
    }
    if method == "POST" && path == "/api/auth/register" {
        let client = client_id(&rq);
        return match auth::register(&body, &client) {
            Ok((me, token)) => respond_json_cookie(rq, 201, &me, Some(auth::cookie_value(&token, secure))),
            Err(e) => respond_json(rq, e.status, &util::msg(&e.detail)),
        };
    }
    let who = session_token(&rq).and_then(|t| auth::from_session(&t));
    if method == "POST" && path == "/api/auth/logout" {
        if let Some(w) = who {
            auth::set_current(Some(w));
            auth::logout();
            auth::set_current(None);
        }
        return respond_json_cookie(rq, 204, &Value::Null, Some(auth::clear_cookie(secure)));
    }
    let Some(who) = who else {
        return respond_json(rq, 401, &util::msg("ログインが必要です"));
    };
    // 2段階認証が必須の設定なのに、まだ有効にしていないユーザーは、設定に必要な操作しかできない
    if auth::needs_2fa(who.uid, who.admin) && !matches!(path.as_str(), "/api/auth/me" | "/api/auth/password") && !path.starts_with("/api/auth/totp/") {
        return respond_json(rq, 403, &json!({"detail": "2段階認証の設定が必要です", "code": "2fa_required"}));
    }
    auth::set_current(Some(who));
    let req = api::Req { method, path, query, body };
    let result = catch_unwind(AssertUnwindSafe(|| api::route(&req)));
    auth::set_current(None);
    match result {
        Ok(Some(Ok((status, v)))) => respond_json(rq, status, &v),
        Ok(Some(Err(e))) => respond_json(rq, e.status, &util::msg(&e.detail)),
        Ok(None) => respond_json(rq, 404, &json!({"detail": "Not Found"})),
        Err(_) => {
            error!("app", "panic while handling {} {}", req.method, req.path);
            respond_json(rq, 500, &util::msg("internal error"))
        }
    }
}

/// コンテナのヘルスチェック用: `ani-cal --healthcheck`
fn healthcheck() -> i32 {
    use std::io::Write;
    let port = config::get().bind.rsplit(':').next().unwrap_or("8000").to_string();
    let Ok(mut s) = std::net::TcpStream::connect(format!("127.0.0.1:{}", port)) else { return 1 };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    // Host を付ける（ALLOWED_HOSTS が設定されていても、127.0.0.1 は常に許可される）
    let _ = s.write_all(b"GET /api/health HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    if out.starts_with("HTTP/1.1 200") || out.starts_with("HTTP/1.0 200") { 0 } else { 1 }
}

fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        std::process::exit(healthcheck());
    }
    // 管理用コマンド（サーバーは起動しない）: `ani-cal user ...`
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s.as_str()) == Some("user") {
        std::process::exit(auth::cli(&args[1..]));
    }
    if args.first().map(|s| s.as_str()) == Some("turnstile") {
        std::process::exit(turnstile::cli(&args[1..]));
    }
    let cfg = config::get();
    db::init();
    turnstile::load();
    if turnstile::enabled() {
        info!("app", "Cloudflare Turnstile: 有効（ログイン・新規登録でBotプロテクション）");
    }
    db::restrict_permissions();
    if cfg.syoboi_ua_url.is_empty() {
        warn!("app", "SYOBOI_UA_URL が未設定です。連絡方法を載せた公開ページのURLを .env に設定してください（README 参照）");
    }
    // docker stop（SIGTERM）や Ctrl+C で、すぐ終了する（PID 1 は既定ではSIGTERMで止まらないため）。
    // 取得中だった履歴は、次回の起動時に「中断されました」になる
    let _ = ctrlc::set_handler(|| {
        info!("app", "shutting down");
        std::process::exit(0);
    });
    sync::start_scheduler();
    std::thread::Builder::new().name("warm".into()).spawn(api::warm_catalog).ok(); // 作品一覧の集計を先に作る
    notify::start();

    let listener = std::net::TcpListener::bind(&cfg.bind).unwrap_or_else(|e| panic!("{} で待ち受けできません: {}", cfg.bind, e));
    set_socket_timeouts(&listener, SOCKET_TIMEOUT_SEC);
    keep_accepting(&listener);
    let server = Arc::new(Server::from_listener(listener, None).unwrap_or_else(|e| panic!("{} で待ち受けできません: {}", cfg.bind, e)));
    info!("app", "listening on {}", cfg.bind);
    let mut workers = vec![];
    for i in 0..8 {
        let s = server.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("http-{}", i))
                .spawn(move || {
                    for rq in s.incoming_requests() {
                        let _ = catch_unwind(AssertUnwindSafe(|| handle(rq)));
                    }
                })
                .unwrap(),
        );
    }
    for w in workers {
        let _ = w.join();
    }
}
