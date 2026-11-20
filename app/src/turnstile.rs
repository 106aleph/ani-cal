//! Cloudflare Turnstile（ボットプロテクション）。管理画面でキーを設定したときだけ有効
//! ログイン・新規登録の前に、画面が取った確認トークンを Cloudflare に問い合わせて確かめる
use crate::util::*;
use crate::{auth, config, db, info, warn};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;

const MAX_TOKEN_LEN: usize = 2048; // Cloudflare が示す上限
const MAX_CONCURRENT: usize = 4; // 問い合わせの同時実行数（Cloudflare が遅いとき、処理スレッドを占有されないように）
const META_SITE: &str = "turnstile_site_key";
const META_SECRET: &str = "turnstile_secret";
static BUSY: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct Keys {
    site: String,
    secret: String,
}
static KEYS: RwLock<Option<Keys>> = RwLock::new(None);

/// 保存してあるキーを、この間隔で読み直す（ホストの CLI で無効にしたとき、再起動なしで反映するため）
const RELOAD_SEC: u64 = 10;
static LAST_LOAD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn keys() -> Option<Keys> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let last = LAST_LOAD.load(Ordering::SeqCst);
    if now.saturating_sub(last) >= RELOAD_SEC && LAST_LOAD.compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        load();
    }
    KEYS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_keys(k: Option<Keys>) {
    *KEYS.write().unwrap_or_else(|e| e.into_inner()) = k;
}

/// 保存してある値を読む。未設定は空文字。DB の読み取りに失敗したときは Err
fn read_meta(conn: &rusqlite::Connection, key: &str) -> Result<String, ()> {
    match conn.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get::<_, Option<String>>(0)) {
        Ok(v) => Ok(v.unwrap_or_default()),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
        Err(_) => Err(()),
    }
}

/// 保存してあるキーを読み込む。読み取りに失敗したときは、今の状態を保つ（一時的な失敗で、確認が無効にならないように）
pub fn load() {
    let conn = db::connect();
    if let (Ok(site), Ok(secret)) = (read_meta(&conn, META_SITE), read_meta(&conn, META_SECRET)) {
        set_keys((!site.is_empty() && !secret.is_empty()).then_some(Keys { site, secret }));
    }
}

pub fn enabled() -> bool {
    keys().is_some()
}

/// 画面に渡すサイトキー（無効なら None）
pub fn site_key() -> Option<String> {
    keys().map(|k| k.site)
}

struct Slot;
impl Slot {
    fn acquire() -> Option<Slot> {
        if BUSY.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT {
            BUSY.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Cloudflare の siteverify を呼ぶ（通信・応答の形に問題があれば Err）
fn siteverify(secret: &str, token: &str, remoteip: Option<&str>) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(5)).redirects(0).build();
    let mut form: Vec<(&str, &str)> = vec![("secret", secret), ("response", token)];
    if let Some(ip) = remoteip.filter(|ip| ip.parse::<std::net::IpAddr>().is_ok()) {
        form.push(("remoteip", ip));
    }
    agent
        .post(&config::get().turnstile_verify_url)
        .send_form(&form)
        .map_err(|e| e.to_string())
        .and_then(|r| r.into_string().map_err(|e| e.to_string()))
        .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()))
}

fn has_code(v: &Value, code: &str) -> bool {
    v["error-codes"].as_array().is_some_and(|a| a.iter().any(|c| c.as_str() == Some(code)))
}

/// ログイン・登録の本文にある `turnstile`（確認トークン）を検証する。無効な設定なら何もしない。
/// 失敗したときは通さない（Cloudflare に届かないときも同じ）。
pub fn check(body: &Value, client: &str) -> Result<(), ApiErr> {
    let Some(k) = keys() else { return Ok(()) };
    let token = opt_str(&body_obj(body), "turnstile").ok().flatten().unwrap_or_default();
    if token.is_empty() || token.len() > MAX_TOKEN_LEN {
        return Err(err(400, "ボットプロテクションの確認が終わっていません。表示が完了するまで待ってから、もう一度お試しください"));
    }
    let Some(_slot) = Slot::acquire() else {
        return Err(err(503, "混み合っています。しばらく待ってからやり直してください"));
    };
    let resp = match siteverify(&k.secret, &token, Some(client)) {
        Ok(v) => v,
        Err(e) => {
            warn!("auth", "turnstile verify unavailable: {}", auth::clean(&e));
            return Err(err(503, "確認サービスに接続できません。しばらくしてからやり直してください"));
        }
    };
    if resp["success"].as_bool() != Some(true) {
        warn!("auth", "turnstile rejected: ip={} codes={}", auth::clean(client), auth::clean(&resp["error-codes"].to_string()));
        return Err(err(400, "ボットプロテクションの確認に失敗しました。もう一度お試しください"));
    }
    // 別のサイトで取られたトークンの使い回しを防ぐ（ALLOWED_HOSTS があるときだけ照合できる）
    let allowed = &config::get().allowed_hosts;
    if !allowed.is_empty() {
        let host = resp["hostname"].as_str().unwrap_or("").to_lowercase();
        if !allowed.contains(&host) && host != "127.0.0.1" && host != "localhost" {
            warn!("auth", "turnstile hostname mismatch: ip={} host={}", auth::clean(client), auth::clean(&host));
            return Err(err(400, "ボットプロテクションの確認に失敗しました。もう一度お試しください"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- 管理画面（admin のみ）
fn status_json() -> Value {
    let k = keys();
    // シークレットキーは返さない（設定済みかどうかだけ）
    json!({"enabled": k.is_some(), "site_key": k.map(|k| k.site)})
}

pub fn status() -> ApiResult {
    auth::require_admin()?;
    ok(status_json())
}

fn valid_key(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// キーを保存して有効にする。パスワードの確認が要る（セッションを盗まれても、確認の差し替え・無効化ができないように）。
/// シークレットキーは、空なら今のものを使う（サイトキーだけの変更用）
pub fn save(body: &Value) -> ApiResult {
    let admin = auth::confirm_admin_password(body)?;
    let m = body_obj(body);
    let site = opt_str(&m, "site_key")?.unwrap_or_default().trim().to_string();
    let given = opt_str(&m, "secret_key")?.unwrap_or_default().trim().to_string();
    if !valid_key(&site, 100) {
        return Err(err(422, "サイトキーは、半角英数字と - _ の100文字以内で入力してください"));
    }
    let secret = if given.is_empty() {
        keys().map(|k| k.secret).ok_or_else(|| err(422, "シークレットキーを入力してください"))?
    } else if valid_key(&given, 200) {
        given
    } else {
        return Err(err(422, "シークレットキーは、半角英数字と - _ の200文字以内で入力してください"));
    };
    // 保存前に、シークレットキーが正しいか確かめる（誤ったキーだと、全員がログインできなくなるため）。
    // ダミーのトークンを送り、「シークレットキーが不正」という応答でなければ受け入れる
    let probe = siteverify(&secret, "ani-cal-config-check", None)
        .map_err(|e| {
            warn!("auth", "turnstile config check failed: {}", auth::clean(&e));
            err(503, "Cloudflare に接続できないため、キーを確認できません。しばらくしてからやり直してください")
        })?;
    if has_code(&probe, "invalid-input-secret") || has_code(&probe, "missing-input-secret") {
        return Err(err(422, "シークレットキーが正しくありません。Cloudflare の Turnstile で確認してください"));
    }
    let conn = db::connect();
    db::set_meta(&conn, META_SITE, &site);
    db::set_meta(&conn, META_SECRET, &secret);
    set_keys(Some(Keys { site, secret }));
    info!("auth", "turnstile enabled by admin {}", auth::clean(&admin));
    ok(status_json())
}

/// 無効にする（パスワードの確認が要る）
pub fn disable(body: &Value) -> ApiResult {
    let admin = auth::confirm_admin_password(body)?;
    let conn = db::connect();
    db::set_meta(&conn, META_SITE, "");
    db::set_meta(&conn, META_SECRET, "");
    set_keys(None);
    info!("auth", "turnstile disabled by admin {}", auth::clean(&admin));
    ok(status_json())
}

/// ホストでの復旧用: `ani-cal turnstile status|off`
/// （キーを誤ってログインできなくなったとき用。管理画面のログインが残っていれば、画面から直せる）
pub fn cli(args: &[String]) -> i32 {
    if auth::refuse_root() {
        return 2;
    }
    let Some(cmd) = args.first() else {
        eprintln!("使い方:\n  ani-cal turnstile status   設定を表示する\n  ani-cal turnstile off      ボットプロテクションを無効にする");
        return 2;
    };
    if !std::path::Path::new(&config::get().db_path).exists() {
        eprintln!("DB が見つかりません: {}", config::get().db_path);
        return 1;
    }
    let conn = db::connect();
    match cmd.as_str() {
        "status" => {
            let site = db::get_meta(&conn, META_SITE).unwrap_or_default();
            if site.is_empty() || db::get_meta(&conn, META_SECRET).unwrap_or_default().is_empty() {
                println!("Turnstile: 無効");
            } else {
                println!("Turnstile: 有効（サイトキー {}）", site);
            }
            0
        }
        "off" => {
            db::set_meta(&conn, META_SITE, "");
            db::set_meta(&conn, META_SECRET, "");
            println!("Turnstile を無効にしました。稼働中のアプリには、10秒ほどで反映されます。");
            info!("auth", "turnstile disabled by CLI");
            0
        }
        _ => {
            eprintln!("不明なコマンド: {}", cmd);
            2
        }
    }
}
