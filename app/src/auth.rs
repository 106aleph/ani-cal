//! ログイン・セッション・ユーザー管理
//!
//! - パスワードは Argon2id でハッシュ化して保存する。
//! - セッションは、ランダムなトークンを Cookie（HttpOnly）に入れ、DB には SHA-256 だけを保存する。
//! - 権限は admin / user の2種類。取得（今すぐ・期間指定・中断）とユーザー管理は admin のみ。
use crate::util::*;
use crate::{config, db, info, warn};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::Engine;
use chrono::Duration;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

const COOKIE: &str = "ani_session";
const COOKIE_HOST: &str = "__Host-ani_session";
const SESSION_DAYS: i64 = 30;
const MIN_PASSWORD: usize = 10;
const SESSION_MAX_DAYS: i64 = 90; // 使い続けても、これを超えたら必ずログインし直す
const MAX_PASSWORD: usize = 128;
const MAX_FAILS: u32 = 5; // 同じ接続元から連続で間違えたらロック
const MAX_FAILS_ACCOUNT: u32 = 20; // 接続元を問わず、同じユーザー名に対する上限
const MAX_FAILS_PER_CLIENT: u32 = 30; // ユーザー名を問わず、同じ接続元に対する上限
const LOCK_SEC: u64 = 300;

// ---------------------------------------------------------------- 現在のユーザー（リクエストごと）
#[derive(Clone)]
pub struct Cur {
    pub uid: i64,
    pub admin: bool,
    pub session: Option<String>, // セッショントークンのハッシュ
}

thread_local! {
    static CUR: RefCell<Option<Cur>> = const { RefCell::new(None) };
    /// このリクエストの接続元と User-Agent（ログインしたときに、セッションに記録する）
    static META: RefCell<(String, String)> = const { RefCell::new((String::new(), String::new())) };
}

pub fn set_current(c: Option<Cur>) {
    CUR.with(|x| *x.borrow_mut() = c);
}

pub fn set_meta(ip: &str, user_agent: &str) {
    META.with(|m| *m.borrow_mut() = (ip.chars().take(64).collect(), user_agent.chars().take(200).collect()));
}

fn cur() -> Cur {
    CUR.with(|x| x.borrow().clone()).expect("未ログインのリクエストが認証を通過しました")
}

pub fn uid() -> i64 {
    cur().uid
}

pub fn require_admin() -> Result<(), ApiErr> {
    if cur().admin {
        Ok(())
    } else {
        Err(err(403, "この操作は管理者のみ実行できます"))
    }
}

// ---------------------------------------------------------------- ハッシュ
fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{:02x}", b)).collect()
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn hash_password(pw: &str) -> Result<String, ApiErr> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|_| err(500, "internal error"))?;
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| err(500, "internal error"))
}

fn verify_password(pw: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(h) => Argon2::default().verify_password(pw.as_bytes(), &h).is_ok(),
        Err(_) => false,
    }
}

/// よく使われる弱いパスワード（小文字にして比較）。長さの条件を満たしていても弾く
const COMMON_PASSWORDS: &[&str] = &[
    "password", "password1", "password12", "password123", "password1234", "passw0rd123", "1234567890", "12345678910",
    "0123456789", "123456789a", "qwertyuiop", "qwerty1234", "qwerty12345", "1q2w3e4r5t", "1qaz2wsx3edc", "abcdefghij",
    "abcd123456", "iloveyou12", "iloveyou123", "letmein123", "welcome123", "admin12345", "administrator", "adminadmin",
    "changeme123", "trustno1234", "monkey12345", "dragon12345", "baseball123", "football123", "sunshine123", "princess123",
    "11111111111", "aaaaaaaaaa", "0000000000", "9876543210", "animeanime", "anime12345", "ani-cal1234", "123456789012",
];

/// パスワードの条件: 長さ、よくあるパスワードでないこと、ユーザー名を含まないこと、同じ文字の繰り返しでないこと
fn check_password(pw: &str, username: &str) -> Result<(), ApiErr> {
    let n = pw.chars().count();
    if n < MIN_PASSWORD {
        return Err(err(422, format!("パスワードは{}文字以上にしてください", MIN_PASSWORD)));
    }
    if n > MAX_PASSWORD {
        return Err(err(422, format!("パスワードは{}文字以内にしてください", MAX_PASSWORD)));
    }
    let low = pw.to_lowercase();
    let first = pw.chars().next().unwrap_or(' ');
    let uname = username.to_lowercase();
    if COMMON_PASSWORDS.contains(&low.as_str())
        || pw.chars().all(|c| c == first)
        || (uname.chars().count() >= 3 && low.contains(&uname))
    {
        return Err(err(422, "推測されやすいパスワードです。ほかのパスワードにしてください（ユーザー名を含めない・定番の文字列を避ける）"));
    }
    Ok(())
}

/// 同時に走らせるパスワードのハッシュ計算の数を制限する（Argon2 はメモリを使うので、大量のログイン要求で枯渇させられないように）
const MAX_HASHING: usize = 3;
static HASHING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct HashSlot;
impl HashSlot {
    fn acquire() -> Result<HashSlot, ApiErr> {
        use std::sync::atomic::Ordering::SeqCst;
        if HASHING.fetch_add(1, SeqCst) >= MAX_HASHING {
            HASHING.fetch_sub(1, SeqCst);
            return Err(err(503, "混み合っています。少し待ってからやり直してください"));
        }
        Ok(HashSlot)
    }
}
impl Drop for HashSlot {
    fn drop(&mut self) {
        HASHING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// ログに書く文字列から、改行などの制御文字を除く（ログの偽造防止）。長すぎるものは切る
pub fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(40).collect()
}

fn check_username(name: &str) -> Result<(), ApiErr> {
    let ok = !name.is_empty() && name.chars().count() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(err(422, "ユーザー名は半角英数字と _ . - の32文字以内にしてください"))
    }
}

fn check_role(role: &str) -> Result<(), ApiErr> {
    if role == "admin" || role == "user" {
        Ok(())
    } else {
        Err(err(422, "権限は admin か user を指定してください"))
    }
}

// ---------------------------------------------------------------- 初期管理者
/// 起動時の確認。管理者は、最初に画面を開いた人が登録する（パスワードを設定ファイルに書かせない）
pub fn bootstrap(conn: &Connection) {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap_or(0);
    if n == 0 {
        warn!("auth", "ユーザーがまだいません。最初に画面を開いた人が、管理者として登録できます。早めに登録してください");
    }
    adopt_legacy(conn);
}

/// 旧バージョンのデータ（持ち主のいない登録作品・通知）は、最初の管理者のものにする
fn adopt_legacy(conn: &Connection) {
    if let Ok(id) = conn.query_row("SELECT id FROM users WHERE role='admin' ORDER BY id LIMIT 1", [], |r| r.get::<_, i64>(0)) {
        let a = conn.execute("UPDATE tracks SET user_id=? WHERE user_id IS NULL", [id]).unwrap_or(0);
        let b = conn.execute("UPDATE push_subs SET user_id=? WHERE user_id IS NULL", [id]).unwrap_or(0);
        if a + b > 0 {
            info!("auth", "既存の登録作品 {} 件・通知 {} 件を管理者に引き継ぎました", a, b);
        }
    }
}

/// 最初のユーザー（管理者）を、まだ誰もいないときだけ登録できる。招待コードは要らない。
/// 成功したら (ユーザー情報, セッショントークン)
pub fn setup(body: &Value) -> Result<(Value, String), ApiErr> {
    let m = body_obj(body);
    let name = opt_str(&m, "username")?.unwrap_or_default().trim().to_string();
    let pw = opt_str(&m, "password")?.unwrap_or_default();
    check_username(&name)?;
    check_password(&pw, &name)?;
    let _slot = HashSlot::acquire()?;
    let hash = hash_password(&pw)?;
    let conn = db::connect();
    // 誰もいないときだけ入る（同時に2人が登録しようとしても、1人しか通らない）
    let n = conn.execute(
        "INSERT INTO users(username, password_hash, role, created_at)
         SELECT ?,?,'admin',? WHERE NOT EXISTS (SELECT 1 FROM users)",
        params![name, hash, config::now_s()],
    )?;
    if n != 1 {
        return Err(err(409, "管理者は既に登録されています。ログインしてください"));
    }
    let uid = conn.last_insert_rowid();
    info!("auth", "管理者を登録しました: {}", name);
    adopt_legacy(&conn);
    let token = create_session(&conn, uid);
    Ok((me_json(&conn, uid)?, token))
}

/// 最初のユーザーの登録待ちか（ログイン前の画面が、登録画面を出すために使う）
pub fn setup_needed() -> bool {
    let conn = db::connect();
    conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0)).unwrap_or(1) == 0
}

// ---------------------------------------------------------------- セッション
/// Cookie のトークンからログイン中のユーザーを調べる（期限が近ければ延長。ただし、作ってから SESSION_MAX_DAYS 日で必ず切れる）
pub fn from_session(token: &str) -> Option<Cur> {
    if token.len() > 128 {
        return None;
    }
    let h = sha256_hex(token);
    let conn = db::connect();
    let now = config::now();
    let (uid, role, exp, created, used): (i64, String, String, String, Option<String>) = conn
        .query_row(
            "SELECT u.id, u.role, s.expires_at, s.created_at, s.last_used_at FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=?",
            [&h],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok()?;
    let exp_t = chrono::NaiveDateTime::parse_from_str(&exp, config::DT_FMT).ok()?;
    let created_t = chrono::NaiveDateTime::parse_from_str(&created, config::DT_FMT).ok()?;
    let hard_limit = created_t + Duration::days(SESSION_MAX_DAYS);
    if exp_t < now || hard_limit < now {
        let _ = conn.execute("DELETE FROM sessions WHERE token_hash=?", [&h]);
        return None;
    }
    if exp_t - now < Duration::days(SESSION_DAYS / 2) {
        let ext = (now + Duration::days(SESSION_DAYS)).min(hard_limit);
        let _ = conn.execute("UPDATE sessions SET expires_at=? WHERE token_hash=?", params![config::fmt_dt(ext), h]);
    }
    // 端末の情報がまだ無いセッション (この機能より前にログインしたもの) は、最初に使われたときに記録する
    let (ip, ua) = META.with(|m| m.borrow().clone());
    if !ua.is_empty() || !ip.is_empty() {
        let _ = conn.execute(
            "UPDATE sessions SET user_agent=?, ip=? WHERE token_hash=? AND user_agent IS NULL AND ip IS NULL",
            params![ua, ip, h],
        );
    }
    // 最後に使われた日時（要求のたびに書き込まないよう、10分以上空いたときだけ更新する）
    let stale = used
        .as_deref()
        .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, config::DT_FMT).ok())
        .is_none_or(|t| now - t >= Duration::minutes(10));
    if stale {
        let _ = conn.execute("UPDATE sessions SET last_used_at=? WHERE token_hash=?", params![config::fmt_dt(now), h]);
    }
    Some(Cur { uid, admin: role == "admin", session: Some(h) })
}

fn create_session(conn: &Connection, uid: i64) -> String {
    let token = random_token();
    let now = config::now();
    let _ = conn.execute("DELETE FROM sessions WHERE expires_at<?", [config::fmt_dt(now)]);
    let (ip, ua) = META.with(|m| m.borrow().clone());
    let _ = conn.execute(
        "INSERT INTO sessions(token_hash, user_id, created_at, expires_at, user_agent, ip, last_used_at) VALUES (?,?,?,?,?,?,?)",
        params![sha256_hex(&token), uid, config::fmt_dt(now), config::fmt_dt(now + Duration::days(SESSION_DAYS)), ua, ip, config::fmt_dt(now)],
    );
    // ログイン・登録・最初の管理者の登録のいずれも、ここでセッションを作る
    let _ = conn.execute("UPDATE users SET last_login_at=? WHERE id=?", params![config::fmt_dt(now), uid]);
    token
}

/// https のときは `__Host-` 付きの名前にする（Secure・Path=/・Domain なしのときだけブラウザが受け付ける。
/// 兄弟ドメインやサブドメインから、Cookie を上書き・注入されるのを防ぐ）
fn cookie_name(secure: bool) -> &'static str {
    if secure { COOKIE_HOST } else { COOKIE }
}

pub fn cookie_value(token: &str, secure: bool) -> String {
    format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        cookie_name(secure),
        token,
        SESSION_DAYS * 86400,
        if secure { "; Secure" } else { "" }
    )
}

pub fn clear_cookie(secure: bool) -> String {
    format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}", cookie_name(secure), if secure { "; Secure" } else { "" })
}

/// Cookie ヘッダの値から、セッションのトークンを取り出す。https のときは `__Host-` 付きだけを受け付ける
pub fn token_from_cookie_header(header: &str, secure: bool) -> Option<String> {
    let want = cookie_name(secure);
    header.split(';').filter_map(|p| p.trim().split_once('=')).find(|(k, _)| *k == want).map(|(_, v)| v.to_string())
}

/// 同じ接続元からの要求を、一定時間あたりの回数で制限する（ログイン・登録の連打や、他人の端末を使った総当たりの抑止）。
/// 超えたら false
pub fn rate_ok(bucket: &str, client: &str, max: usize, window_sec: u64) -> bool {
    static HITS: Mutex<Option<HashMap<String, std::collections::VecDeque<Instant>>>> = Mutex::new(None);
    let Ok(mut g) = HITS.lock() else { return true };
    let m = g.get_or_insert_with(HashMap::new);
    if m.len() > 5000 {
        m.retain(|_, q| q.back().is_some_and(|t| t.elapsed().as_secs() < window_sec));
        if m.len() > 5000 {
            m.clear();
        }
    }
    let q = m.entry(format!("{}|{}", bucket, client)).or_default();
    while q.front().is_some_and(|t| t.elapsed().as_secs() >= window_sec) {
        q.pop_front();
    }
    if q.len() >= max {
        return false;
    }
    q.push_back(Instant::now());
    true
}

// ---------------------------------------------------------------- ログイン
static FAILS: Mutex<Option<HashMap<String, (u32, Instant)>>> = Mutex::new(None);

/// 失敗の制限は3段構え:
/// - 「ユーザー名＋接続元」ごとに5回: 同じ端末からの総当たりを止める。別の端末の人が、本人を巻き込んでロックすることはない
/// - 「ユーザー名」ごとに20回: 接続元を変えながらの総当たりの上限（接続元の判別を偽装されても効く）
/// - 「接続元」ごとに30回: ユーザー名を変えながらの総当たり（パスワードスプレー）を止める。成功しても数えは戻らない
/// 先頭の2つだけ、成功すると数えを戻す（`&key[..2]`）
fn limits(name: &str, client: &str) -> [(String, u32); 3] {
    let name: String = name.chars().take(64).collect();
    [
        (format!("{}|{}", name, client), MAX_FAILS),
        (name, MAX_FAILS_ACCOUNT),
        (format!("\u{0}ip|{}", client), MAX_FAILS_PER_CLIENT),
    ]
}

/// ロック中なら、あと何秒か（枠は、最後の失敗から LOCK_SEC 秒で解ける）
fn locked(keys: &[(String, u32)]) -> Option<u64> {
    let g = FAILS.lock().ok()?;
    let m = g.as_ref()?;
    keys.iter()
        .filter_map(|(k, max)| {
            let (n, at) = m.get(k)?;
            let el = at.elapsed().as_secs();
            (*n >= *max && el < LOCK_SEC).then_some(LOCK_SEC - el)
        })
        .max()
}

fn record_fail(keys: &[(String, u32)]) {
    if let Ok(mut g) = FAILS.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        if m.len() > 1000 {
            // 存在しないユーザー名で大量に試されても、メモリが増え続けないよう、古い記録を捨てる
            m.retain(|_, (_, at)| at.elapsed().as_secs() < LOCK_SEC);
            if m.len() > 1000 {
                m.clear();
            }
        }
        for (key, _) in keys {
            let e = m.entry(key.clone()).or_insert((0, Instant::now()));
            if e.1.elapsed().as_secs() >= LOCK_SEC {
                e.0 = 0; // 前回の失敗から時間が空いたら数え直す
            }
            e.0 += 1;
            e.1 = Instant::now();
        }
    }
}

fn clear_fails(keys: &[(String, u32)]) {
    if let Ok(mut g) = FAILS.lock() {
        if let Some(m) = g.as_mut() {
            for (k, _) in keys {
                m.remove(k);
            }
        }
    }
}

/// ログイン。成功したら (ユーザー情報, セッショントークン)
pub fn login(body: &Value, client: &str) -> Result<(Value, String), ApiErr> {
    let m = body_obj(body);
    let name = opt_str(&m, "username")?.unwrap_or_default().trim().to_string();
    let pw = opt_str(&m, "password")?.unwrap_or_default();
    let key = limits(&name.to_lowercase(), client);
    if let Some(s) = locked(&key) {
        warn!("auth", "login blocked (too many failures): user={} ip={}", clean(&name), clean(client));
        return Err(err(429, format!("ログインに失敗した回数が多すぎます。{}分ほど待ってからやり直してください", s.div_ceil(60))));
    }
    let _slot = HashSlot::acquire()?;
    let conn = db::connect();
    let row: Option<(i64, String, String)> = conn
        .query_row("SELECT id, password_hash, role FROM users WHERE username=?", [&name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .ok();
    // ユーザーの有無で処理時間が変わらないよう、存在しないときもハッシュ計算を行う
    let good = match &row {
        Some((_, h, _)) => verify_password(&pw, h),
        None => {
            let _ = verify_password(&pw, &dummy_hash());
            false
        }
    };
    if !good {
        record_fail(&key);
        warn!("auth", "login failed: user={} ip={}", clean(&name), clean(client));
        std::thread::sleep(std::time::Duration::from_millis(300));
        return Err(err(401, "ユーザー名かパスワードが違います"));
    }
    let (uid, _, _) = row.unwrap();
    // 2段階認証がオンなら、認証コード（またはリカバリーコード）も必要。まだ無ければ入力を求める
    if totp_enabled(&conn, uid) {
        let otp = opt_str(&m, "otp")?.unwrap_or_default();
        if otp.trim().is_empty() {
            return Ok((json!({"totp_required": true}), String::new()));
        }
        if !check_second_factor(&conn, uid, &otp) {
            record_fail(&key);
            warn!("auth", "2fa failed: user={} ip={}", clean(&name), clean(client));
            std::thread::sleep(std::time::Duration::from_millis(300));
            return Err(err(401, "認証コードが違います"));
        }
    }
    clear_fails(&key[..2]);
    info!("auth", "login ok: user={} ip={}", clean(&name), clean(client));
    let token = create_session(&conn, uid);
    Ok((me_json(&conn, uid)?, token))
}

fn dummy_hash() -> String {
    static D: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    D.get_or_init(|| hash_password("dummy-password").unwrap_or_default()).clone()
}

pub fn logout() {
    if let Some(h) = cur().session {
        let conn = db::connect();
        let _ = conn.execute("DELETE FROM sessions WHERE token_hash=?", [h]);
    }
}

fn me_json(conn: &Connection, uid: i64) -> Result<Value, ApiErr> {
    let (name, role): (String, String) = conn
        .query_row("SELECT username, role FROM users WHERE id=?", [uid], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| err(401, "ログインが必要です"))?;
    Ok(json!({"id": uid, "username": name, "role": role, "admin": role == "admin",
              "totp_enabled": totp_enabled(conn, uid), "needs_2fa": needs_2fa(uid, role == "admin")}))
}

pub fn me() -> ApiResult {
    let conn = db::connect();
    ok(me_json(&conn, uid())?)
}

/// 自分のパスワード変更。ほかの端末のログインは解除する
pub fn change_password(body: &Value) -> ApiResult {
    let m = body_obj(body);
    let current = opt_str(&m, "current")?.unwrap_or_default();
    let new = opt_str(&m, "new")?.unwrap_or_default();
    let c = cur();
    let conn = db::connect();
    let (hash, name): (String, String) = conn.query_row("SELECT password_hash, username FROM users WHERE id=?", [c.uid], |r| Ok((r.get(0)?, r.get(1)?)))?;
    check_password(&new, &name)?;
    verify_current_password(c.uid, &current, &hash, "現在のパスワードが違います")?;
    let _slot = HashSlot::acquire()?;
    conn.execute("UPDATE users SET password_hash=? WHERE id=?", params![hash_password(&new)?, c.uid])?;
    conn.execute("DELETE FROM sessions WHERE user_id=? AND token_hash<>?", params![c.uid, c.session.unwrap_or_default()])?;
    info!("auth", "password changed: {}", clean(&name));
    ok(json!({"ok": true}))
}

/// ログイン中の操作でパスワードを確認するとき（変更・2段階認証の設定など）。
/// セッションを盗んだ人がここでパスワードを総当たりできないよう、失敗が続いたら止める
fn verify_current_password(uid: i64, given: &str, hash: &str, msg: &str) -> Result<(), ApiErr> {
    let key = [(format!("\u{0}pw|{}", uid), MAX_FAILS)];
    if let Some(s) = locked(&key) {
        return Err(err(429, format!("パスワードの確認に失敗した回数が多すぎます。{}分ほど待ってからやり直してください", s.div_ceil(60))));
    }
    let _slot = HashSlot::acquire()?;
    if !verify_password(given, hash) {
        record_fail(&key);
        warn!("auth", "password check failed: uid {}", uid);
        std::thread::sleep(std::time::Duration::from_millis(300));
        return Err(err(403, msg));
    }
    clear_fails(&key);
    Ok(())
}

/// ログイン中の端末の一覧（自分のセッションだけ。新しく使われた順）。
/// id は、セッションのトークンのハッシュの先頭16文字（トークン本体は復元できない）
pub fn list_sessions() -> ApiResult {
    let c = cur();
    let conn = db::connect();
    let mine = c.session.as_deref().unwrap_or_default();
    let rows = db::query_json(
        &conn,
        "SELECT substr(token_hash,1,16) AS id, token_hash=? AS current, created_at, last_used_at, ip, user_agent
         FROM sessions WHERE user_id=? AND expires_at>=? ORDER BY COALESCE(last_used_at, created_at) DESC",
        params![mine, c.uid, config::now_s()],
    )?;
    ok(Value::Array(rows))
}

/// 端末を1つだけログアウトさせる。今の端末は、ログアウトで解除する
pub fn revoke_session(id: &str) -> ApiResult {
    let c = cur();
    if id.len() != 16 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(err(422, "id が不正です"));
    }
    let id = id.to_lowercase();
    if c.session.as_deref().is_some_and(|s| s.starts_with(&id)) {
        return Err(err(400, "この端末は、ログアウトで解除してください"));
    }
    let conn = db::connect();
    // 自分のセッションだけ消せる（他のユーザーの id を指定しても、何も消えない）
    let n = conn.execute("DELETE FROM sessions WHERE user_id=? AND substr(token_hash,1,16)=?", params![c.uid, id])?;
    if n == 0 {
        return Err(err(404, "その端末は見つかりません"));
    }
    info!("auth", "session revoked: uid {}", c.uid);
    Ok((204, Value::Null))
}

/// このユーザーの、今のセッション以外をすべて削除する。削除した数を返す
fn revoke_other_sessions(conn: &Connection, uid: i64, keep: Option<&str>) -> usize {
    conn.execute("DELETE FROM sessions WHERE user_id=? AND token_hash<>?", params![uid, keep.unwrap_or_default()]).unwrap_or(0)
}

/// 自分のほかの端末のログインをすべて解除する（今の端末は残す）
pub fn logout_others() -> ApiResult {
    let c = cur();
    let conn = db::connect();
    let n = revoke_other_sessions(&conn, c.uid, c.session.as_deref());
    info!("auth", "logout other devices: uid {} ({} sessions)", c.uid, n);
    ok(json!({"ok": true, "count": n}))
}

/// 2段階認証が必須の設定なのに、まだ有効にしていないユーザーか
pub fn needs_2fa(uid: i64, admin: bool) -> bool {
    let need = match config::get().require_2fa.as_str() {
        "all" => true,
        "admin" => admin,
        _ => false,
    };
    need && !totp_enabled(&db::connect(), uid)
}

// ---------------------------------------------------------------- 2段階認証（TOTP, RFC 6238）
// Authy / Google Authenticator / 1Password などで使える標準方式（SHA-1, 6桁, 30秒）。
const TOTP_ISSUER: &str = "AnimeCalendar";
const B32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn b32_encode(data: &[u8]) -> String {
    let (mut out, mut buf, mut bits) = (String::new(), 0u32, 0);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn b32_decode(s: &str) -> Option<Vec<u8>> {
    let (mut out, mut buf, mut bits) = (Vec::new(), 0u32, 0);
    for c in s.bytes() {
        let v = B32.iter().position(|&x| x == c)? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = <Hmac<sha1::Sha1> as Mac>::new_from_slice(secret).expect("HMAC は任意長の鍵を受け付ける");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0xf) as usize;
    (u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]])) % 1_000_000
}

/// 6桁のコードを検証する。前後1枠（±30秒）まで許容し、使用済みの枠は受け付けない。合っていれば、その枠の番号を返す
fn totp_match(secret_b32: &str, code: &str, last_step: i64) -> Option<i64> {
    let secret = b32_decode(secret_b32)?;
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let want: u32 = code.parse().ok()?;
    let now = chrono::Utc::now().timestamp() / 30;
    (now - 1..=now + 1).find(|&s| s > last_step && hotp(&secret, s as u64) == want)
}

fn totp_enabled(conn: &Connection, uid: i64) -> bool {
    conn.query_row("SELECT totp_enabled FROM users WHERE id=?", [uid], |r| r.get::<_, i64>(0)).unwrap_or(0) == 1
}

fn recovery_hash(code: &str) -> String {
    sha256_hex(&code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase())
}

/// 認証コード（6桁）またはリカバリーコードを検証する。使えたコードは再利用できなくなる
fn check_second_factor(conn: &Connection, uid: i64, input: &str) -> bool {
    let input = input.trim();
    let row: Option<(Option<String>, i64)> =
        conn.query_row("SELECT totp_secret, totp_last_step FROM users WHERE id=? AND totp_enabled=1", [uid], |r| Ok((r.get(0)?, r.get(1)?))).ok();
    let Some((Some(secret), last)) = row else { return false };
    let compact: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() == 6 && compact.bytes().all(|b| b.is_ascii_digit()) {
        return match totp_match(&secret, input, last) {
            // 同時に同じコードが使われても、1回しか通らないよう、枠の更新を条件にする
            Some(step) => conn.execute("UPDATE users SET totp_last_step=? WHERE id=? AND totp_last_step<?", params![step, uid, step]).unwrap_or(0) == 1,
            None => false,
        };
    }
    conn.execute("DELETE FROM recovery_codes WHERE user_id=? AND code_hash=?", params![uid, recovery_hash(input)]).unwrap_or(0) == 1
}

fn clear_totp(conn: &Connection, uid: i64) -> Result<(), ApiErr> {
    conn.execute("UPDATE users SET totp_secret=NULL, totp_enabled=0, totp_last_step=0 WHERE id=?", [uid])?;
    conn.execute("DELETE FROM recovery_codes WHERE user_id=?", [uid])?;
    Ok(())
}

/// 設定の開始: 新しい鍵を作って返す（確認のコードを入力するまで、有効にはならない）
pub fn totp_setup(body: &Value) -> ApiResult {
    let c = cur();
    let pw = opt_str(&body_obj(body), "password")?.unwrap_or_default();
    let conn = db::connect();
    // セッションを盗まれても、攻撃者の鍵で2段階認証を有効にされ、本人が締め出されることがないよう、パスワードを求める
    let hash: String = conn.query_row("SELECT password_hash FROM users WHERE id=?", [c.uid], |r| r.get(0))?;
    verify_current_password(c.uid, &pw, &hash, "パスワードが違います")?;
    if totp_enabled(&conn, c.uid) {
        return Err(err(409, "2段階認証は既にオンです"));
    }
    let name: String = conn.query_row("SELECT username FROM users WHERE id=?", [c.uid], |r| r.get(0))?;
    let mut raw = [0u8; 20];
    OsRng.fill_bytes(&mut raw);
    let secret = b32_encode(&raw);
    conn.execute("UPDATE users SET totp_secret=?, totp_enabled=0, totp_last_step=0 WHERE id=?", params![secret, c.uid])?;
    let uri = format!("otpauth://totp/{0}:{1}?secret={2}&issuer={0}&algorithm=SHA1&digits=6&period=30", TOTP_ISSUER, name, secret);
    let qr = qrcode::QrCode::new(uri.as_bytes())
        .map(|q| q.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).build())
        .unwrap_or_default();
    ok(json!({"secret": secret, "uri": uri, "qr_svg": qr}))
}

/// 確認のコードが合えば、2段階認証をオンにして、リカバリーコードを返す（このとき1回だけ表示）
pub fn totp_enable(body: &Value) -> ApiResult {
    let c = cur();
    let m = body_obj(body);
    let code = opt_str(&m, "code")?.unwrap_or_default();
    let conn = db::connect();
    let secret: Option<String> = conn
        .query_row("SELECT totp_secret FROM users WHERE id=? AND totp_enabled=0", [c.uid], |r| r.get(0))
        .map_err(|_| err(409, "2段階認証は既にオンです"))?;
    let secret = secret.ok_or_else(|| err(409, "先に設定を始めてください"))?;
    // 6桁の総当たりを防ぐ。オフにするときと同じ失敗の数え方（ユーザーごとに5回で、5分間止める）
    let okey = [(format!("\u{0}otp|{}", c.uid), MAX_FAILS)];
    if let Some(s) = locked(&okey) {
        return Err(err(429, format!("認証コードの確認に失敗した回数が多すぎます。{}分ほど待ってからやり直してください", s.div_ceil(60))));
    }
    let Some(step) = totp_match(&secret, &code, 0) else {
        record_fail(&okey);
        std::thread::sleep(std::time::Duration::from_millis(300));
        return Err(err(400, "認証コードが違います。アプリに表示されている6桁を入力してください"));
    };
    clear_fails(&okey);
    conn.execute("UPDATE users SET totp_enabled=1, totp_last_step=? WHERE id=?", params![step, c.uid])?;
    conn.execute("DELETE FROM recovery_codes WHERE user_id=?", [c.uid])?;
    let mut codes = Vec::new();
    for _ in 0..8 {
        let raw = new_invite_code();
        let code = format!("{}-{}", &raw[..5], &raw[5..10]);
        conn.execute("INSERT OR IGNORE INTO recovery_codes(user_id, code_hash) VALUES (?,?)", params![c.uid, recovery_hash(&code)])?;
        codes.push(code);
    }
    // 盗まれたセッションが残っていても、2段階認証を入れた時点で無効にする（今の端末だけ残す）
    let others = revoke_other_sessions(&conn, c.uid, c.session.as_deref());
    info!("auth", "2fa enabled: uid {} (other sessions revoked: {})", c.uid, others);
    ok(json!({"recovery_codes": codes}))
}

/// オフにする: パスワードと、認証コード（またはリカバリーコード）が必要
pub fn totp_disable(body: &Value) -> ApiResult {
    let c = cur();
    let m = body_obj(body);
    let pw = opt_str(&m, "password")?.unwrap_or_default();
    let code = opt_str(&m, "code")?.unwrap_or_default();
    let conn = db::connect();
    let hash: String = conn.query_row("SELECT password_hash FROM users WHERE id=?", [c.uid], |r| r.get(0))?;
    verify_current_password(c.uid, &pw, &hash, "パスワードが違います")?;
    let okey = [(format!("\u{0}otp|{}", c.uid), MAX_FAILS)];
    if let Some(s) = locked(&okey) {
        return Err(err(429, format!("認証コードの確認に失敗した回数が多すぎます。{}分ほど待ってからやり直してください", s.div_ceil(60))));
    }
    if !check_second_factor(&conn, c.uid, &code) {
        record_fail(&okey);
        std::thread::sleep(std::time::Duration::from_millis(300));
        return Err(err(403, "認証コードが違います"));
    }
    clear_totp(&conn, c.uid)?;
    let others = revoke_other_sessions(&conn, c.uid, c.session.as_deref());
    info!("auth", "2fa disabled: uid {} (other sessions revoked: {})", c.uid, others);
    ok(json!({"ok": true}))
}

// ---------------------------------------------------------------- ユーザー管理（admin のみ）
fn admin_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM users WHERE role='admin'", [], |r| r.get(0)).unwrap_or(0)
}

fn user_json(conn: &Connection, id: i64) -> Result<Value, ApiErr> {
    let v = db::query_one_json(
        conn,
        "SELECT u.id, u.username, u.role, u.created_at, u.last_login_at, u.totp_enabled,
           (SELECT MAX(COALESCE(s.last_used_at, s.created_at)) FROM sessions s WHERE s.user_id=u.id) AS last_used_at,
           (SELECT COUNT(*) FROM tracks t WHERE t.user_id=u.id) AS tracks
         FROM users u WHERE u.id=?",
        [id],
    )?;
    v.ok_or_else(|| err(404, "ユーザーが見つかりません"))
}

pub fn list_users() -> ApiResult {
    require_admin()?;
    let conn = db::connect();
    ok(Value::Array(db::query_json(
        &conn,
        "SELECT u.id, u.username, u.role, u.created_at, u.last_login_at, u.totp_enabled,
           (SELECT MAX(COALESCE(s.last_used_at, s.created_at)) FROM sessions s WHERE s.user_id=u.id) AS last_used_at,
           (SELECT COUNT(*) FROM tracks t WHERE t.user_id=u.id) AS tracks
         FROM users u ORDER BY u.id",
        [],
    )?))
}

// ---------------------------------------------------------------- 招待コード
const INVITE_DAYS: i64 = 7;
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // 0/O・1/I など紛らわしい文字を除く

fn new_invite_code() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|x| INVITE_ALPHABET[(*x as usize) % INVITE_ALPHABET.len()] as char).collect()
}

/// 入力された招待コードを、保存形式（大文字・記号なし）にそろえる
fn norm_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase()
}

pub fn list_invites() -> ApiResult {
    require_admin()?;
    let conn = db::connect();
    ok(Value::Array(db::query_json(
        &conn,
        "SELECT i.id, i.code, i.role, i.created_at, i.expires_at
         FROM invites i WHERE i.used_at IS NULL AND i.expires_at>? ORDER BY i.id DESC",
        [config::now_s()],
    )?))
}

pub fn create_invite(body: &Value) -> ApiResult {
    require_admin()?;
    let m = body_obj(body);
    let role = opt_str(&m, "role")?.unwrap_or_else(|| "user".into());
    check_role(&role)?;
    let conn = db::connect();
    let now = config::now();
    conn.execute(
        "INSERT INTO invites(code, role, created_by, created_at, expires_at) VALUES (?,?,?,?,?)",
        params![new_invite_code(), role, uid(), config::fmt_dt(now), config::fmt_dt(now + Duration::days(INVITE_DAYS))],
    )?;
    info!("auth", "invite created ({})", role);
    let id = conn.last_insert_rowid();
    let v = db::query_json(&conn, "SELECT id, code, role, created_at, expires_at FROM invites WHERE id=?", [id])?;
    Ok((201, v.into_iter().next().unwrap_or(Value::Null)))
}

pub fn delete_invite(id: i64) -> ApiResult {
    require_admin()?;
    let conn = db::connect();
    conn.execute("DELETE FROM invites WHERE id=? AND used_at IS NULL", [id])?;
    Ok((204, Value::Null))
}

/// 招待コードでユーザーを登録し、そのままログインする。成功したら (ユーザー情報, セッショントークン)
pub fn register(body: &Value, client: &str) -> Result<(Value, String), ApiErr> {
    let m = body_obj(body);
    let code = norm_code(&opt_str(&m, "code")?.unwrap_or_default());
    let name = opt_str(&m, "username")?.unwrap_or_default().trim().to_string();
    let pw = opt_str(&m, "password")?.unwrap_or_default();
    // コードの総当たりを防ぐため、失敗が続いたら止める（接続元ごとと、全体の上限）
    let key = limits("\u{0}invite", client);
    if let Some(s) = locked(&key) {
        return Err(err(429, format!("招待コードの入力に失敗した回数が多すぎます。{}分ほど待ってからやり直してください", s.div_ceil(60))));
    }
    let conn = db::connect();
    let role: Option<String> = if code.is_empty() {
        None
    } else {
        conn.query_row(
            "SELECT role FROM invites WHERE code=? AND used_at IS NULL AND expires_at>?",
            params![code, config::now_s()],
            |r| r.get(0),
        )
        .ok()
    };
    let Some(role) = role else {
        record_fail(&key);
        warn!("auth", "invite code rejected: ip={}", clean(client));
        std::thread::sleep(std::time::Duration::from_millis(300));
        return Err(err(400, "招待コードが正しくないか、期限切れ・使用済みです"));
    };
    check_username(&name)?;
    check_password(&pw, &name)?;
    let _slot = HashSlot::acquire()?;
    let hash = hash_password(&pw)?;
    let tx = conn.unchecked_transaction()?;
    // 同じコードの同時使用を防ぐため、先に使用済みにする（以降で失敗すれば巻き戻る）
    let claimed = tx.execute("UPDATE invites SET used_at=? WHERE code=? AND used_at IS NULL", params![config::now_s(), code])?;
    if claimed != 1 {
        return Err(err(400, "招待コードが正しくないか、期限切れ・使用済みです"));
    }
    let dup: Option<i64> = tx.query_row("SELECT 1 FROM users WHERE username=?", [&name], |r| r.get(0)).ok();
    if dup.is_some() {
        return Err(err(409, "そのユーザー名は既に使われています"));
    }
    tx.execute(
        "INSERT INTO users(username, password_hash, role, created_at) VALUES (?,?,?,?)",
        params![name, hash, role, config::now_s()],
    )?;
    let new_id = tx.last_insert_rowid();
    tx.execute("UPDATE invites SET used_by=? WHERE code=?", params![new_id, code])?;
    tx.commit()?;
    clear_fails(&key[..2]);
    info!("auth", "user registered by invite: {} ({}) ip={}", clean(&name), role, clean(client));
    let token = create_session(&conn, new_id);
    Ok((me_json(&conn, new_id)?, token))
}

/// 最初の管理者（いま管理者の中で、いちばん早く作られた人）。削除も降格もできない
fn first_admin_id(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT id FROM users WHERE role='admin' ORDER BY id LIMIT 1", [], |r| r.get(0)).ok()
}

pub fn update_user(id: i64, body: &Value) -> ApiResult {
    require_admin()?;
    let m = body_obj(body);
    let role = opt_str(&m, "role")?;
    let pw = opt_str(&m, "password")?;
    if let Some(r) = &role {
        check_role(r)?;
    }
    let reset_totp = m.get("reset_totp").and_then(|v| v.as_bool()) == Some(true);
    let me = uid();
    let my_session = cur().session.unwrap_or_default();
    // 重いパスワードの計算は、書き込みの順番を取る前に済ませる
    let new_hash = match &pw {
        Some(p) => {
            let name: String = db::connect()
                .query_row("SELECT username FROM users WHERE id=?", [id], |r| r.get(0))
                .map_err(|_| err(404, "ユーザーが見つかりません"))?;
            check_password(p, &name)?;
            let _slot = HashSlot::acquire()?;
            Some(hash_password(p)?)
        }
        None => None,
    };
    // 確認から更新までを、1つのトランザクション（最初から書き込みの順番を取る）で行う。同時の操作とは順番に処理される
    db::with_tx(|conn| {
        let (cur_role, name): (String, String) = conn
            .query_row("SELECT role, username FROM users WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|_| err(404, "ユーザーが見つかりません"))?;
        if let Some(r) = &role {
            if cur_role == "admin" && r != "admin" && (first_admin_id(conn) == Some(id) || admin_count(conn) <= 1) {
                return Err(err(409, "最初の管理者の権限は変更できません"));
            }
        }
        // 認証アプリを失くした人のための、2段階認証の解除
        if reset_totp {
            clear_totp(conn, id)?;
            let keep = if id == me { my_session.clone() } else { String::new() };
            conn.execute("DELETE FROM sessions WHERE user_id=? AND token_hash<>?", params![id, keep])?;
            info!("auth", "2fa reset by admin: {}", name);
        }
        if let Some(r) = &role {
            conn.execute("UPDATE users SET role=? WHERE id=?", params![r, id])?;
            info!("auth", "role changed: {} -> {}", name, r);
        }
        if let Some(h) = &new_hash {
            conn.execute("UPDATE users SET password_hash=? WHERE id=?", params![h, id])?;
            // 再設定されたユーザーのログインは解除する（自分自身は今の端末だけ残す）
            let keep = if id == me { my_session.clone() } else { String::new() };
            conn.execute("DELETE FROM sessions WHERE user_id=? AND token_hash<>?", params![id, keep])?;
            info!("auth", "password reset: {}", name);
        }
        ok(user_json(conn, id)?)
    })
}

pub fn delete_user(id: i64) -> ApiResult {
    require_admin()?;
    if id == uid() {
        return Err(err(409, "自分自身は削除できません"));
    }
    db::with_tx(|conn| {
        let (role, name): (String, String) = conn
            .query_row("SELECT role, username FROM users WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|_| err(404, "ユーザーが見つかりません"))?;
        if role == "admin" && (first_admin_id(conn) == Some(id) || admin_count(conn) <= 1) {
            return Err(err(409, "最初の管理者は削除できません"));
        }
        // 登録作品・視聴記録・通知・ログインは、外部キーでまとめて消える
        conn.execute("DELETE FROM push_sent WHERE endpoint IN (SELECT endpoint FROM push_subs WHERE user_id=?)", [id])?;
        conn.execute("DELETE FROM users WHERE id=?", [id])?;
        info!("auth", "user deleted: {}", name);
        Ok((204, Value::Null))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotp_rfc_vectors() {
        // RFC 4226 付録D / RFC 6238 付録B（SHA-1）のテストベクトル
        let k = b"12345678901234567890";
        assert_eq!(hotp(k, 0), 755224);
        assert_eq!(hotp(k, 1), 287082);
        assert_eq!(hotp(k, 37_037_036), 81804); // 2005-03-18 01:58:29Z の枠（8桁 07081804 の下6桁）
    }

    #[test]
    fn base32_roundtrip() {
        assert_eq!(b32_encode(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(b32_decode("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap(), b"12345678901234567890");
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn password_policy() {
        assert!(check_password("short1", "bob").is_err());
        assert!(check_password("password123", "bob").is_err()); // 定番
        assert!(check_password("PASSWORD123", "bob").is_err()); // 大文字小文字を無視
        assert!(check_password("aaaaaaaaaaaa", "bob").is_err()); // 同じ文字の繰り返し
        assert!(check_password("my-bobby-secret", "bobby").is_err()); // ユーザー名を含む
        assert!(check_password("correct horse battery", "bob").is_ok());
        assert!(check_password(&"x1".repeat(100), "bob").is_err()); // 長すぎる
    }

    #[test]
    fn cookie_names_and_xff() {
        assert_eq!(token_from_cookie_header("a=1; ani_session=T1", false).as_deref(), Some("T1"));
        // https では __Host- 付きだけ。平文用の名前（サブドメインなどから注入できる）は受け付けない
        assert_eq!(token_from_cookie_header("ani_session=EVIL", true), None);
        assert_eq!(token_from_cookie_header("ani_session=EVIL; __Host-ani_session=GOOD", true).as_deref(), Some("GOOD"));
    }

    #[test]
    fn rate_limiter_counts_per_client() {
        for _ in 0..3 {
            assert!(rate_ok("t-bucket", "1.1.1.1", 3, 60));
        }
        assert!(!rate_ok("t-bucket", "1.1.1.1", 3, 60));
        assert!(rate_ok("t-bucket", "2.2.2.2", 3, 60));
    }
}

// ---------------------------------------------------------------- 管理用コマンド（締め出されたときの復旧）
/// `ani-cal user list` / `reset-password <名前>` / `reset-2fa <名前>`
/// コンテナ内で実行する（root だと DB の関連ファイルが root 所有になり、アプリが起動できなくなるため、`-u app` を付ける）:
///   docker compose exec -u app app ani-cal user reset-password <名前>
/// root で実行されていたら、案内を出して true を返す（DB のファイルが root の所有になり、アプリが起動できなくなるため）
pub fn refuse_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    if std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1) == 0 {
        eprintln!("root では実行できません（DB のファイルが root の所有になり、アプリが起動できなくなります）。");
        eprintln!("`docker compose exec -u app app ani-cal user ...` のように、-u app を付けて実行してください。");
        return true;
    }
    false
}

/// 管理者の操作で、本文の `password` が本人のものか確かめる（誤りが続いたら止める）。ユーザー名を返す
pub fn confirm_admin_password(body: &Value) -> Result<String, ApiErr> {
    require_admin()?;
    let c = cur();
    let pw = opt_str(&body_obj(body), "password")?.unwrap_or_default();
    let conn = db::connect();
    let (name, hash): (String, String) =
        conn.query_row("SELECT username, password_hash FROM users WHERE id=?", [c.uid], |r| Ok((r.get(0)?, r.get(1)?)))?;
    verify_current_password(c.uid, &pw, &hash, "パスワードが違います")?;
    Ok(name)
}

pub fn cli(args: &[String]) -> i32 {
    if refuse_root() {
        return 2;
    }
    let usage = || {
        eprintln!("使い方:\n  ani-cal user list\n  ani-cal user reset-password <ユーザー名>   新しい仮パスワードを作って表示する（ログインはすべて解除）\n  ani-cal user reset-2fa <ユーザー名>        2段階認証を解除する（ログインはすべて解除）");
        2
    };
    let Some(cmd) = args.first() else { return usage() };
    if !std::path::Path::new(&config::get().db_path).exists() {
        eprintln!("DB が見つかりません: {}", config::get().db_path);
        return 1;
    }
    let conn = db::connect();
    if cmd == "list" {
        let rows = db::query_json(&conn, "SELECT username, role, totp_enabled, created_at, last_login_at FROM users ORDER BY id", []).unwrap_or_default();
        for r in rows {
            println!(
                "{:<32} {:<6} 2FA:{} 登録:{} 最終ログイン:{}",
                r["username"].as_str().unwrap_or(""),
                r["role"].as_str().unwrap_or(""),
                if r["totp_enabled"].as_i64() == Some(1) { "あり" } else { "なし" },
                r["created_at"].as_str().unwrap_or(""),
                r["last_login_at"].as_str().unwrap_or("記録なし")
            );
        }
        return 0;
    }
    let Some(name) = args.get(1) else { return usage() };
    let uid: Option<i64> = conn.query_row("SELECT id FROM users WHERE username=?", [name], |r| r.get(0)).ok();
    let Some(uid) = uid else {
        eprintln!("ユーザーが見つかりません: {}", name);
        return 1;
    };
    match cmd.as_str() {
        "reset-password" => {
            // 16文字のランダムな文字列（英数字）。表示は、この1回だけ
            let raw = random_token().replace(['-', '_'], "x");
            let temp: String = raw.chars().take(16).collect();
            let hash = match hash_password(&temp) {
                Ok(h) => h,
                Err(_) => return 1,
            };
            let ok = conn.execute("UPDATE users SET password_hash=? WHERE id=?", params![hash, uid]).is_ok()
                && conn.execute("DELETE FROM sessions WHERE user_id=?", [uid]).is_ok();
            if !ok {
                eprintln!("更新に失敗しました");
                return 1;
            }
            println!("{} の仮パスワード: {}", name, temp);
            println!("ログイン後、すぐにアカウント画面からパスワードを変更してください。");
            info!("auth", "password reset by CLI: {}", clean(name));
            0
        }
        "reset-2fa" => {
            if clear_totp(&conn, uid).is_err() || conn.execute("DELETE FROM sessions WHERE user_id=?", [uid]).is_err() {
                eprintln!("更新に失敗しました");
                return 1;
            }
            println!("{} の2段階認証を解除しました。", name);
            info!("auth", "2fa reset by CLI: {}", clean(name));
            0
        }
        _ => usage(),
    }
}
