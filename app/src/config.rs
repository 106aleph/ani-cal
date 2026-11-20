//! 環境変数による設定
use chrono::{NaiveDateTime, Utc};
use chrono_tz::Tz;
use std::collections::HashSet;
use std::sync::OnceLock;

pub struct Config {
    pub tz: Tz,
    pub db_path: String,
    pub bind: String,
    pub syoboi_base: String,
    pub syoboi_ua_url: String,
    pub syoboi_ua: String,
    pub push_subject: String,
    pub syoboi_interval_sec: f64,
    pub syoboi_split_threshold: usize,
    pub syoboi_max_wait_sec: u64,
    pub sync_interval_hours: f64,
    pub sync_on_startup: bool,
    pub sync_days_back: i64,
    pub sync_days_forward: i64,
    pub sync_chunk_days: i64,
    pub sync_chgids: HashSet<i64>,
    pub sync_max_failures: i64,
    pub range_sync_daily_limit: i64,
    /// 受け付けるホスト名（Host ヘッダ）。空なら制限しない。127.0.0.1 / localhost は常に許可
    pub allowed_hosts: Vec<String>,
    /// X-Forwarded-For / X-Forwarded-Proto を信用する、プロキシの接続元（IP か CIDR）。それ以外の接続元が付けた値は無視する
    pub trusted_proxies: Vec<(std::net::IpAddr, u8)>,
    /// 2段階認証を必須にする対象: "off" / "admin" / "all"
    pub require_2fa: String,
    /// Cookie に Secure を常に付ける（プロキシが X-Forwarded-Proto を付けない構成用）
    pub cookie_secure: bool,
    /// Cloudflare Turnstile の確認先（キーは管理画面で設定して DB に保存する。ここはテスト用に差し替えられる）
    pub turnstile_verify_url: String,
    pub skip_rebroadcast: bool,
    pub new_cats: HashSet<i64>,
    pub net_group_ids: HashSet<i64>,
    pub net_group_keywords: Vec<String>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}
fn int(name: &str, default: i64) -> i64 {
    env(name).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}
fn float(name: &str, default: f64) -> f64 {
    env(name).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}
fn boolean(name: &str, default: bool) -> bool {
    match env(name) {
        None => default,
        Some(v) => matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
    }
}
fn intset(name: &str, default: &str) -> HashSet<i64> {
    env(name)
        .unwrap_or_else(|| default.to_string())
        .replace(' ', "")
        .split(',')
        .filter(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|x| x.parse().ok())
        .collect()
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get() -> &'static Config {
    CONFIG.get_or_init(|| {
        let ua_url = env("SYOBOI_UA_URL").unwrap_or_default();
        let ua = env("SYOBOI_UA").filter(|s| !s.is_empty()).unwrap_or_else(|| {
            let mut s = "AnimeCalendar/1.0".to_string();
            if !ua_url.is_empty() {
                s.push_str(&format!(" (+{})", ua_url));
            }
            s
        });
        let push_subject = env("PUSH_SUBJECT")
            .filter(|s| !s.is_empty())
            .or_else(|| Some(ua_url.clone()).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "mailto:admin@example.com".to_string());
        let kw = env("NET_GROUP_KEYWORDS").unwrap_or_else(|| "ネット,インターネット,配信,web".to_string());
        Config {
            tz: env("APP_TZ").and_then(|s| s.parse().ok()).unwrap_or(chrono_tz::Asia::Tokyo),
            db_path: env("DB_PATH").unwrap_or_else(|| "/data/anime.db".to_string()),
            bind: env("BIND").unwrap_or_else(|| "0.0.0.0:8000".to_string()),
            syoboi_base: env("SYOBOI_BASE").unwrap_or_else(|| "https://cal.syoboi.jp/db.php".to_string()),
            syoboi_ua_url: ua_url,
            syoboi_ua: ua,
            push_subject,
            syoboi_interval_sec: float("SYOBOI_INTERVAL_SEC", 1.5),
            syoboi_split_threshold: int("SYOBOI_SPLIT_THRESHOLD", 2000) as usize,
            syoboi_max_wait_sec: int("SYOBOI_MAX_WAIT_SEC", 300) as u64,
            sync_interval_hours: float("SYNC_INTERVAL_HOURS", 6.0),
            sync_on_startup: boolean("SYNC_ON_STARTUP", true),
            sync_days_back: int("SYNC_DAYS_BACK", 60),
            sync_days_forward: int("SYNC_DAYS_FORWARD", 120),
            sync_chunk_days: int("SYNC_CHUNK_DAYS", 2).max(1),
            sync_chgids: intset("SYNC_CHGIDS", ""),
            sync_max_failures: int("SYNC_MAX_FAILURES", 5),
            range_sync_daily_limit: int("RANGE_SYNC_DAILY_LIMIT", 3),
            trusted_proxies: parse_cidrs(&env("TRUSTED_PROXIES").unwrap_or_else(|| DEFAULT_TRUSTED_PROXIES.to_string())),
            allowed_hosts: env("ALLOWED_HOSTS")
                .unwrap_or_default()
                .split(',')
                .map(|h| h.trim().to_lowercase())
                .filter(|h| !h.is_empty())
                .collect(),
            require_2fa: match env("REQUIRE_2FA").unwrap_or_default().trim().to_lowercase().as_str() {
                "off" | "false" | "0" | "no" => "off".to_string(),
                "all" => "all".to_string(),
                _ => "admin".to_string(), // 既定: 管理者は2段階認証が必須
            },
            cookie_secure: boolean("COOKIE_SECURE", false),
            turnstile_verify_url: env("TURNSTILE_VERIFY_URL")
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "https://challenges.cloudflare.com/turnstile/v0/siteverify".to_string()),
            skip_rebroadcast: boolean("SKIP_REBROADCAST", true),
            new_cats: intset("NEW_CATS", "1,10"),
            net_group_ids: intset("NET_GROUP_IDS", ""),
            net_group_keywords: kw.split(',').filter(|k| !k.is_empty()).map(String::from).collect(),
        }
    })
}

/// タイムゾーン情報なしの現在時刻（秒未満は切り捨て）
pub fn now() -> NaiveDateTime {
    Utc::now().with_timezone(&get().tz).naive_local().with_nanosecond_zero()
}

trait NanoZero {
    fn with_nanosecond_zero(self) -> Self;
}
impl NanoZero for NaiveDateTime {
    fn with_nanosecond_zero(self) -> Self {
        use chrono::Timelike;
        self.with_nanosecond(0).unwrap_or(self)
    }
}

pub const DT_FMT: &str = "%Y-%m-%dT%H:%M";

pub fn fmt_dt(dt: NaiveDateTime) -> String {
    dt.format(DT_FMT).to_string()
}

pub fn now_s() -> String {
    fmt_dt(now())
}

/// 既定: 同じ機械（127.0.0.1）と、Docker・家庭内などの私的なアドレス。インターネットから直接来る接続は含まない
const DEFAULT_TRUSTED_PROXIES: &str = "127.0.0.0/8,::1/128,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,fc00::/7,fe80::/10";

/// "1.2.3.4, 10.0.0.0/8, fc00::/7" のような一覧を読む。読めない項目は捨てる
fn parse_cidrs(s: &str) -> Vec<(std::net::IpAddr, u8)> {
    s.split(',')
        .filter_map(|p| {
            let p = p.trim();
            let (ip, bits) = match p.split_once('/') {
                Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
                None => (p, None),
            };
            let ip: std::net::IpAddr = ip.parse().ok()?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let bits = bits.unwrap_or(max);
            (bits <= max).then_some((ip, bits))
        })
        .collect()
}

/// ip が、一覧のどれかの範囲に入るか（IPv4 射影の IPv6 アドレスは IPv4 として見る）
pub fn ip_in(list: &[(std::net::IpAddr, u8)], ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    list.iter().any(|&(net, bits)| match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(a)) => {
            let m = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
            u32::from(n) & m == u32::from(a) & m
        }
        (IpAddr::V6(n), IpAddr::V6(a)) => {
            let m = if bits == 0 { 0 } else { u128::MAX << (128 - bits as u32) };
            u128::from(n) & m == u128::from(a) & m
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_proxy_ranges() {
        let d = parse_cidrs(DEFAULT_TRUSTED_PROXIES);
        for ok in ["127.0.0.1", "::1", "172.18.0.1", "10.1.2.3", "192.168.0.5", "::ffff:172.17.0.1", "fd00::1"] {
            assert!(ip_in(&d, ok.parse().unwrap()), "{}", ok);
        }
        for ng in ["8.8.8.8", "172.32.0.1", "100.64.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!ip_in(&d, ng.parse().unwrap()), "{}", ng);
        }
        assert!(!ip_in(&parse_cidrs(""), "127.0.0.1".parse().unwrap())); // 空なら、だれも信用しない
        assert!(ip_in(&parse_cidrs("203.0.113.7, bad, 1.2.3.4/99"), "203.0.113.7".parse().unwrap()));
    }
}
