//! Web Push 通知: 配信開始の N 分前にブラウザ/PWA へ通知する
//!
//! - 購読（端末ごと）と「何分前に通知するか」は push_subs テーブルに保存
//! - VAPID 鍵は初回起動時に生成して meta テーブルに保存（Python 版の PKCS#8 PEM と互換）
//! - 通知ループが30秒ごとに「開始 N 分前〜開始3分後」の未視聴の話を探して送信する（送信済みは push_sent で重複防止）
//! - 暗号化は RFC 8291（aes128gcm）、認証は RFC 8292（VAPID, ES256）
use crate::auth;
use crate::util::*;
use crate::{config, db, error, info, warn};
use aes_gcm::{aead::Aead, Aes128Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use chrono::{Duration, NaiveDateTime};
use hkdf::Hkdf;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use p256::{PublicKey, SecretKey};
use rand_core::OsRng;
use rusqlite::params;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::{Mutex, OnceLock};

const DEFAULT_LEAD_MIN: i64 = 10;
const MAX_LEAD_MIN: i64 = 1440;
const GRACE_MIN: i64 = 3; // 開始時刻を過ぎてからでも、この分数以内なら通知する（取得や再起動の遅れ対策）
/// 購読先として許可する Push サービス（任意URLへサーバーから送信しないため）
const PUSH_HOSTS: [&str; 4] = ["fcm.googleapis.com", "push.services.mozilla.com", "push.apple.com", "notify.windows.com"];

/// 1人が通知を登録できる端末の数
const MAX_SUBS_PER_USER: i64 = 20;

static KEY: OnceLock<SecretKey> = OnceLock::new();
static KEY_LOCK: Mutex<()> = Mutex::new(());

fn secret_key() -> &'static SecretKey {
    if let Some(k) = KEY.get() {
        return k;
    }
    let _g = KEY_LOCK.lock().unwrap();
    if let Some(k) = KEY.get() {
        return k;
    }
    let conn = db::connect();
    let sk = db::get_meta(&conn, "vapid_private")
        .and_then(|pem| SecretKey::from_pkcs8_pem(&pem).ok().or_else(|| SecretKey::from_sec1_pem(&pem).ok()))
        .unwrap_or_else(|| {
            let sk = SecretKey::random(&mut OsRng);
            let pem = sk.to_pkcs8_pem(LineEnding::LF).expect("鍵の保存に失敗");
            db::set_meta(&conn, "vapid_private", pem.as_str());
            info!("notify", "generated a new VAPID key");
            sk
        });
    let _ = KEY.set(sk);
    KEY.get().unwrap()
}

fn public_bytes(sk: &SecretKey) -> Vec<u8> {
    sk.public_key().to_encoded_point(false).as_bytes().to_vec()
}

pub fn public_key() -> String {
    B64.encode(public_bytes(secret_key()))
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let t: String = s.trim().trim_end_matches('=').replace('+', "-").replace('/', "_");
    B64.decode(t).ok()
}

fn origin_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next()?;
    Some(format!("https://{}", host))
}

/// https の URL からホスト名を取り出す。ユーザー情報（`@`）・バックスラッシュ・`#`・`?`・空白などが
/// 混じる URL は、URL パーサーによって解釈が分かれて許可リストをすり抜けられるので、受け付けない
fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && !host.contains("..")
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    let port_ok = port.is_none_or(|p| p == "443");
    (host_ok && port_ok && !url.contains(|c: char| c == '\\' || c == '@' || c.is_whitespace() || c.is_control())).then(|| host.to_lowercase())
}

fn check_endpoint(endpoint: &str) -> Result<(), ApiErr> {
    let ok = host_of(endpoint)
        .map(|h| PUSH_HOSTS.iter().any(|p| h == *p || h.ends_with(&format!(".{}", p))))
        .unwrap_or(false);
    if ok { Ok(()) } else { Err(err(422, "対応していない通知サービスです")) }
}

// ---------------------------------------------------------------- 送信
pub enum PushErr {
    Gone,
    Other(String),
}

struct Sub {
    endpoint: String,
    p256dh: String,
    auth: String,
}

/// RFC 8291 aes128gcm でペイロードを暗号化する
#[allow(deprecated)] // generic-array 0.14 の from_slice（aes-gcm 0.10 が要求）
fn encrypt(sub: &Sub, payload: &[u8]) -> Result<Vec<u8>, String> {
    let ua_pub_bytes = b64_decode(&sub.p256dh).ok_or("p256dh が不正です")?;
    let auth = b64_decode(&sub.auth).ok_or("auth が不正です")?;
    let ua_pub = PublicKey::from_sec1_bytes(&ua_pub_bytes).map_err(|_| "p256dh が不正です".to_string())?;
    let as_secret = SecretKey::random(&mut OsRng);
    let as_pub = as_secret.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_pub.as_affine());

    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&ua_pub_bytes);
    key_info.extend_from_slice(as_pub.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|e| e.to_string())?;

    let mut salt = [0u8; 16];
    getrandom_fill(&mut salt);
    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|e| e.to_string())?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|e| e.to_string())?;

    let mut plain = payload.to_vec();
    plain.push(0x02); // 最後（唯一）のレコードの区切り
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|e| e.to_string())?;
    let ct = cipher.encrypt(Nonce::from_slice(&nonce[..]), plain.as_slice()).map_err(|e| e.to_string())?;

    let mut body = Vec::with_capacity(21 + 65 + ct.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(65);
    body.extend_from_slice(as_pub.as_bytes());
    body.extend_from_slice(&ct);
    Ok(body)
}

fn getrandom_fill(buf: &mut [u8]) {
    use rand_core::RngCore;
    OsRng.fill_bytes(buf);
}

/// RFC 8292 VAPID の Authorization ヘッダー値
fn vapid_header(endpoint: &str) -> Result<String, String> {
    let aud = origin_of(endpoint).ok_or("endpoint が不正です")?;
    let sk = secret_key();
    let exp = chrono::Utc::now().timestamp() + 12 * 3600;
    let head = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = B64.encode(json!({"aud": aud, "exp": exp, "sub": config::get().push_subject}).to_string());
    let signing_input = format!("{}.{}", head, claims);
    let sig: Signature = SigningKey::from(sk).sign(signing_input.as_bytes());
    Ok(format!("vapid t={}.{}, k={}", signing_input, B64.encode(sig.to_bytes()), public_key()))
}

fn send(sub: &Sub, payload: &Value, ttl: i64) -> Result<(), PushErr> {
    let body = encrypt(sub, payload.to_string().as_bytes()).map_err(PushErr::Other)?;
    let auth = vapid_header(&sub.endpoint).map_err(PushErr::Other)?;
    // リダイレクトは追わない（許可した Push サービスから、内部のアドレスなどへ飛ばされないように）
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(10)).redirects(0).build();
    let r = agent
        .post(&sub.endpoint)
        .set("Authorization", &auth)
        .set("Content-Encoding", "aes128gcm")
        .set("Content-Type", "application/octet-stream")
        .set("TTL", &ttl.to_string())
        // Urgency: high … 端末が省電力（Doze）中でも即時配信するよう Push サービスに要求する（時刻通知なので）
        .set("Urgency", "high")
        .send_bytes(&body);
    match r {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(404, _)) | Err(ureq::Error::Status(410, _)) => Err(PushErr::Gone),
        Err(ureq::Error::Status(code, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            Err(PushErr::Other(format!("Push failed: {} {}", code, text.chars().take(200).collect::<String>())))
        }
        Err(e) => Err(PushErr::Other(e.to_string())),
    }
}

fn drop_sub(endpoint: &str) {
    let conn = db::connect();
    let _ = conn.execute("DELETE FROM push_sent WHERE endpoint=?", [endpoint]);
    let _ = conn.execute("DELETE FROM push_subs WHERE endpoint=?", [endpoint]);
}

// ---------------------------------------------------------------- API
fn lead_of(m: &serde_json::Map<String, Value>, default: Option<i64>) -> Result<i64, ApiErr> {
    let v = opt_int(m, "lead_min")?.or(default).ok_or_else(|| err(422, "lead_min は必須です"))?;
    if !(0..=MAX_LEAD_MIN).contains(&v) {
        return Err(err(422, format!("lead_min は 0〜{} で指定してください", MAX_LEAD_MIN)));
    }
    Ok(v)
}

fn endpoint_of(m: &serde_json::Map<String, Value>) -> Result<String, ApiErr> {
    opt_str(m, "endpoint")?.ok_or_else(|| err(422, "endpoint は必須です"))
}

pub fn get_key() -> ApiResult {
    ok(json!({ "key": public_key() }))
}

pub fn get_settings(endpoint: &str) -> ApiResult {
    let conn = db::connect();
    let lead: Option<i64> = conn
        .query_row("SELECT lead_min FROM push_subs WHERE endpoint=? AND user_id=?", params![endpoint, auth::uid()], |r| r.get(0))
        .ok();
    ok(json!({ "subscribed": lead.is_some(), "lead_min": lead.unwrap_or(DEFAULT_LEAD_MIN) }))
}

pub fn subscribe(body: &Value) -> ApiResult {
    let m = body_obj(body);
    let endpoint = endpoint_of(&m)?;
    let lead = lead_of(&m, Some(DEFAULT_LEAD_MIN))?;
    check_endpoint(&endpoint)?;
    let keys = m.get("keys").and_then(|k| k.as_object()).ok_or_else(|| err(422, "keys は必須です"))?;
    let p256dh = keys.get("p256dh").and_then(|v| v.as_str()).unwrap_or("");
    let auth = keys.get("auth").and_then(|v| v.as_str()).unwrap_or("");
    if p256dh.is_empty() || auth.is_empty() {
        return Err(err(422, "購読情報が不正です"));
    }
    let conn = db::connect();
    // 別のユーザーが登録済みのエンドポイントは、鍵（その端末だけが持つ）が一致するときしか引き継げない
    let owner: Option<(Option<i64>, String, String)> = conn
        .query_row("SELECT user_id, p256dh, auth FROM push_subs WHERE endpoint=?", [&endpoint], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .ok();
    if let Some((Some(u), k1, k2)) = owner {
        if u != auth::uid() && (k1 != p256dh || k2 != auth) {
            return Err(err(409, "この通知の購読は、別のユーザーが使っています"));
        }
    }
    // 1人が登録できる端末の数に上限を付ける（購読を大量に作られて、通知の送信と DB が膨らむのを防ぐ）
    let mine: i64 = conn.query_row("SELECT COUNT(*) FROM push_subs WHERE user_id=?", [auth::uid()], |r| r.get(0))?;
    let already: bool = conn
        .query_row("SELECT 1 FROM push_subs WHERE endpoint=? AND user_id=?", params![endpoint, auth::uid()], |r| r.get::<_, i64>(0))
        .is_ok();
    if !already && mine >= MAX_SUBS_PER_USER {
        return Err(err(409, format!("通知を登録できる端末は、1人{}台までです。使っていない端末の通知を止めてください", MAX_SUBS_PER_USER)));
    }
    conn.execute(
        "INSERT INTO push_subs(endpoint,user_id,p256dh,auth,lead_min,created_at) VALUES(?,?,?,?,?,?) \
         ON CONFLICT(endpoint) DO UPDATE SET user_id=excluded.user_id, p256dh=excluded.p256dh, auth=excluded.auth, lead_min=excluded.lead_min",
        params![endpoint, auth::uid(), p256dh, auth, lead, config::now_s()],
    )?;
    ok(json!({ "subscribed": true, "lead_min": lead }))
}

pub fn unsubscribe(body: &Value) -> ApiResult {
    let endpoint = endpoint_of(&body_obj(body))?;
    // 自分の購読だけ解除できる
    let mine: Option<i64> = db::connect()
        .query_row("SELECT 1 FROM push_subs WHERE endpoint=? AND user_id=?", params![endpoint, auth::uid()], |r| r.get(0))
        .ok();
    if mine.is_some() {
        drop_sub(&endpoint);
    }
    ok(json!({ "subscribed": false }))
}

pub fn set_settings(body: &Value) -> ApiResult {
    let m = body_obj(body);
    let endpoint = endpoint_of(&m)?;
    let lead = lead_of(&m, None)?;
    let conn = db::connect();
    let n = conn.execute("UPDATE push_subs SET lead_min=? WHERE endpoint=? AND user_id=?", params![lead, endpoint, auth::uid()])?;
    if n == 0 {
        return Err(err(404, "この端末は通知が有効になっていません"));
    }
    ok(json!({ "subscribed": true, "lead_min": lead }))
}

pub fn test(body: &Value) -> ApiResult {
    let endpoint = endpoint_of(&body_obj(body))?;
    let sub = {
        let conn = db::connect();
        conn.query_row("SELECT endpoint, p256dh, auth FROM push_subs WHERE endpoint=? AND user_id=?", params![endpoint, auth::uid()], |r| {
            Ok(Sub { endpoint: r.get(0)?, p256dh: r.get(1)?, auth: r.get(2)? })
        })
        .map_err(|_| err(404, "この端末は通知が有効になっていません"))?
    };
    let payload = json!({"title": "アニメ配信カレンダー", "body": "テスト通知です。届いていれば設定は完了です。", "tag": "test", "url": "/"});
    match send(&sub, &payload, 60) {
        Ok(()) => ok(json!({ "ok": true })),
        Err(PushErr::Gone) => {
            drop_sub(&endpoint);
            Err(err(410, "通知の購読が無効になりました。通知を有効にし直してください"))
        }
        Err(PushErr::Other(e)) => {
            warn!("notify", "push test failed ({}): {}", host_of(&endpoint).unwrap_or_default(), e);
            Err(err(502, format!("通知サービスへの送信に失敗しました: {}", e)))
        }
    }
}

// ---------------------------------------------------------------- 通知ループ
/// Python の `{x:g}` 相当（1.0 → "1", 12.5 → "12.5"）
pub fn fmt_g(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let s = format!("{:.6}", x);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Python の round()（偶数丸め）
fn round_even(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 {
        let t = x.trunc() as i64;
        if t % 2 == 0 { t } else { t + x.signum() as i64 }
    } else {
        r as i64
    }
}

struct Ep {
    id: i64,
    number: Option<f64>,
    subtitle: Option<String>,
    start_at: String,
    title: String,
    platform: String,
}

fn fmt_ep(r: &Ep, now: NaiveDateTime) -> Value {
    let st = NaiveDateTime::parse_from_str(&r.start_at, config::DT_FMT).unwrap_or(now);
    let mins = round_even((st - now).num_milliseconds() as f64 / 1000.0 / 60.0);
    let num = r.number.map(|n| format!(" 第{}話", fmt_g(n))).unwrap_or_default();
    let when = if mins <= 0 { "まもなく開始".to_string() } else { format!("あと{}分", mins) };
    let place = if r.platform.is_empty() { String::new() } else { format!("（{}）", r.platform) };
    let sub = r.subtitle.as_ref().filter(|s| !s.is_empty()).map(|s| format!(" 「{}」", s)).unwrap_or_default();
    json!({
        "title": format!("{}{}", r.title, num),
        "body": format!("{} 配信開始{} {}{}", st.format("%H:%M"), place, when, sub),
        "tag": format!("ep-{}", r.id), "url": "/",
    })
}

/// 送信すべき通知を送る。送った件数を返す
pub fn notify_due() -> usize {
    let now = config::now();
    let mut sent = 0;
    let subs: Vec<(Sub, i64, i64)> = {
        let conn = db::connect();
        let _ = conn.execute("DELETE FROM push_sent WHERE start_at<?", [config::fmt_dt(now - Duration::days(2))]);
        let mut st = conn.prepare("SELECT endpoint, p256dh, auth, lead_min, user_id FROM push_subs WHERE user_id IS NOT NULL").unwrap();
        st.query_map([], |r| Ok((Sub { endpoint: r.get(0)?, p256dh: r.get(1)?, auth: r.get(2)? }, r.get(3)?, r.get::<_, i64>(4)?)))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect()
    };
    for (sub, lead, user) in subs {
        let lo = config::fmt_dt(now - Duration::minutes(GRACE_MIN));
        let hi = config::fmt_dt(now + Duration::minutes(lead));
        let rows: Vec<Ep> = {
            let conn = db::connect();
            let mut st = conn
                .prepare(
                    "SELECT e.id, e.number, e.subtitle, e.start_at, \
                       COALESCE(NULLIF(tr.title,''), t.title, '(無題)') AS title, \
                       COALESCE(NULLIF(tr.platform,''), c.name, '') AS platform \
                     FROM episodes e JOIN tracks tr ON tr.id=e.track_id \
                     LEFT JOIN titles t ON t.tid=tr.tid LEFT JOIN channels c ON c.chid=tr.chid \
                     WHERE e.hidden=0 AND e.watched=0 AND tr.archived=0 AND tr.user_id=? \
                       AND e.start_at IS NOT NULL AND e.start_at>=? AND e.start_at<=? \
                       AND NOT EXISTS (SELECT 1 FROM push_sent s WHERE s.endpoint=? AND s.ep_id=e.id AND s.start_at=e.start_at) \
                     ORDER BY e.start_at",
                )
                .unwrap();
            st.query_map(params![user, lo, hi, sub.endpoint], |r| {
                Ok(Ep {
                    id: r.get(0)?,
                    number: r.get(1)?,
                    subtitle: r.get(2)?,
                    start_at: r.get(3)?,
                    title: r.get(4)?,
                    platform: r.get(5)?,
                })
            })
            .unwrap()
            .filter_map(|x| x.ok())
            .collect()
        };
        for r in rows {
            match send(&sub, &fmt_ep(&r, now), 600) {
                Ok(()) => {}
                Err(PushErr::Gone) => {
                    info!("notify", "push subscription gone, removed");
                    drop_sub(&sub.endpoint);
                    break;
                }
                Err(PushErr::Other(e)) => {
                    warn!("notify", "push failed: {}", e);
                    continue;
                }
            }
            let conn = db::connect();
            let _ = conn.execute(
                "INSERT OR IGNORE INTO push_sent(endpoint,ep_id,start_at) VALUES(?,?,?)",
                params![sub.endpoint, r.id, r.start_at],
            );
            sent += 1;
        }
    }
    sent
}

pub fn start() {
    std::thread::Builder::new()
        .name("notify".into())
        .spawn(|| loop {
            if std::panic::catch_unwind(notify_due).is_err() {
                error!("notify", "notify error");
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 暗号化と VAPID 署名を書き出す（Python 側の http_ece / cryptography で復号・検証するための試験用）
    /// 実行: DB_PATH=... PUSHTEST_P256DH=... PUSHTEST_AUTH=... PUSHTEST_OUT=... cargo test dump_push -- --ignored
    #[test]
    #[ignore]
    fn dump_push() {
        db::init();
        let sub = Sub {
            endpoint: "https://fcm.googleapis.com/fcm/send/abc".into(),
            p256dh: std::env::var("PUSHTEST_P256DH").unwrap(),
            auth: std::env::var("PUSHTEST_AUTH").unwrap(),
        };
        let payload = json!({"title": "テスト作品 第1話", "body": "20:05 配信開始（ABEMA） あと10分", "tag": "ep-1", "url": "/"});
        let body = encrypt(&sub, payload.to_string().as_bytes()).unwrap();
        let out = json!({
            "body": base64::engine::general_purpose::STANDARD.encode(&body),
            "authorization": vapid_header(&sub.endpoint).unwrap(),
            "payload": payload.to_string(),
            "public_key": public_key(),
        });
        std::fs::write(std::env::var("PUSHTEST_OUT").unwrap(), out.to_string()).unwrap();
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn endpoint_host_parsing_rejects_ambiguous_urls() {
        assert_eq!(host_of("https://fcm.googleapis.com/fcm/send/abc").as_deref(), Some("fcm.googleapis.com"));
        assert!(check_endpoint("https://fcm.googleapis.com/fcm/send/abc").is_ok());
        // URL パーサーによって、実際の接続先が evil.example と解釈されるもの
        for bad in [
            "https://evil.example\\@fcm.googleapis.com/x",
            "https://evil.example#@fcm.googleapis.com/x",
            "https://fcm.googleapis.com@evil.example/x",
            "https://user:pw@fcm.googleapis.com/x",
            "https://fcm.googleapis.com:8443/x",
            "https://fcm.googleapis.com.evil.example/x",
            "https://evilfcm.googleapis.com.evil.example/x",
            "http://fcm.googleapis.com/x",
            "https://fcm.googleapis.com /x",
            "https://-fcm.googleapis.com/x",
        ] {
            assert!(check_endpoint(bad).is_err(), "通ってはいけない: {}", bad);
        }
    }
}
