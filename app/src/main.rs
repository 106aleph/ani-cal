//! アニメ配信カレンダー（しょぼいカレンダーから配信予定を取得して視聴記録するセルフホストアプリ）
mod api;
mod config;
mod db;
mod log;
mod notify;
mod sync;
mod syoboi;
mod util;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, OnceLock};
use tiny_http::{Header, Method, Request, Response, Server};

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

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
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
    let _ = if status == 204 {
        rq.respond(Response::empty(204))
    } else {
        let s = body.to_string();
        rq.respond(
            Response::from_string(s)
                .with_status_code(status)
                .with_header(header("Content-Type", "application/json"))
                .with_header(header("Cache-Control", "no-store")),
        )
    };
}

fn respond_asset(rq: Request, a: &Asset) {
    let inm = rq.headers().iter().find(|h| h.field.equiv("If-None-Match")).map(|h| h.value.as_str().to_string());
    if inm.as_deref() == Some(a.etag.as_str()) {
        let _ = rq.respond(Response::empty(304).with_header(header("ETag", &a.etag)).with_header(header("Cache-Control", "no-cache")));
        return;
    }
    let _ = rq.respond(
        Response::from_data(a.bytes.to_vec())
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
        if path == "/calendar.ics" {
            return match api::ics(&query) {
                Ok(s) => {
                    let _ = rq.respond(
                        Response::from_string(s)
                            .with_header(header("Content-Type", "text/calendar; charset=utf-8"))
                            .with_header(header("Cache-Control", "no-store")),
                    );
                }
                Err(e) => respond_json(rq, e.status, &util::msg(&e.detail)),
            };
        }
    }

    // 本文（JSON）
    let mut raw = String::new();
    if matches!(method.as_str(), "POST" | "PATCH" | "PUT") {
        let _ = rq.as_reader().take(1 << 20).read_to_string(&mut raw);
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
    let req = api::Req { method, path, query, body };
    let result = catch_unwind(AssertUnwindSafe(|| api::route(&req)));
    match result {
        Ok(Some(Ok((status, v)))) => respond_json(rq, status, &v),
        Ok(Some(Err(e))) => respond_json(rq, e.status, &util::msg(&e.detail)),
        Ok(None) => respond_json(rq, 404, &json!({"detail": "Not Found"})),
        Err(_) => respond_json(rq, 500, &util::msg("internal error")),
    }
}

/// コンテナのヘルスチェック用: `ani-cal --healthcheck`
fn healthcheck() -> i32 {
    use std::io::Write;
    let port = config::get().bind.rsplit(':').next().unwrap_or("8000").to_string();
    let Ok(mut s) = std::net::TcpStream::connect(format!("127.0.0.1:{}", port)) else { return 1 };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let _ = s.write_all(b"GET /api/health HTTP/1.0\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    if out.starts_with("HTTP/1.1 200") || out.starts_with("HTTP/1.0 200") { 0 } else { 1 }
}

fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        std::process::exit(healthcheck());
    }
    let cfg = config::get();
    db::init();
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
    notify::start();

    let server = Arc::new(Server::http(&cfg.bind).unwrap_or_else(|e| panic!("{} で待ち受けできません: {}", cfg.bind, e)));
    info!("app", "listening on {}", cfg.bind);
    let mut workers = vec![];
    for i in 0..4 {
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
