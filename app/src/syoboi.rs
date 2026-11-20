//! しょぼいカレンダー db.php クライアント
//!
//! 使用コマンド: ChGroupLookup / ChLookup / ProgLookup(Range or LastUpdate, JOIN=SubTitles) / TitleLookup
use crate::config;
use crate::{info, warn};
use chrono::{Duration, NaiveDateTime, Timelike};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};

/// 取得の失敗。Cancelled はユーザー操作による中断（失敗として数えない）
#[derive(Debug)]
pub enum SyErr {
    Cancelled,
    Msg(String),
}

impl std::fmt::Display for SyErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyErr::Cancelled => write!(f, "cancelled"),
            SyErr::Msg(m) => write!(f, "{}", m),
        }
    }
}
impl From<rusqlite::Error> for SyErr {
    fn from(e: rusqlite::Error) -> Self {
        SyErr::Msg(e.to_string())
    }
}

/// 中断要求（取得スレッドとAPIで共有）
pub static CANCEL: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct Group {
    pub gid: i64,
    pub name: String,
    pub sort: Option<i64>,
}
#[derive(Debug, Clone)]
pub struct Channel {
    pub chid: i64,
    pub name: String,
    pub gid: Option<i64>,
    pub sort: Option<i64>,
}
#[derive(Debug, Clone)]
pub struct Title {
    pub tid: i64,
    pub title: String,
    pub cat: Option<i64>,
    pub first_year: Option<i64>,
    pub first_month: Option<i64>,
}
#[derive(Debug, Clone)]
pub struct Program {
    pub pid: i64,
    pub tid: i64,
    pub chid: i64,
    pub start_at: String,
    pub end_at: Option<String>,
    pub count: Option<i64>,
    pub subtitle: Option<String>,
    pub flag: i64,
    pub deleted: i64,
}

type Item = std::collections::HashMap<String, String>;

fn items(xml: &roxmltree::Document, tag: &str) -> Vec<Item> {
    xml.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == tag)
        .map(|n| {
            n.children()
                .filter(|c| c.is_element())
                .map(|c| (c.tag_name().name().to_string(), c.text().unwrap_or("").trim().to_string()))
                .collect()
        })
        .collect()
}

fn int(v: Option<&String>) -> Option<i64> {
    v.and_then(|s| s.trim().parse().ok())
}

fn parse_dt(v: Option<&String>) -> Option<NaiveDateTime> {
    let s = v?.trim();
    if s.is_empty() {
        return None;
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y%m%d%H%M%S"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(d);
        }
    }
    None
}

fn non_empty(v: Option<&String>) -> Option<String> {
    v.filter(|s| !s.is_empty()).cloned()
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    pub interval: f64,
    last: Option<Instant>,
}

impl Client {
    pub fn new() -> Client {
        let cfg = config::get();
        Client {
            agent: ureq::AgentBuilder::new()
                .timeout(StdDuration::from_secs(60))
                .redirects(5)
                .user_agent(&cfg.syoboi_ua)
                .build(),
            base: cfg.syoboi_base.clone(),
            interval: cfg.syoboi_interval_sec,
            last: None,
        }
    }

    /// 待機。中断要求があれば即座に Cancelled
    fn sleep(&self, sec: f64) -> Result<(), SyErr> {
        let end = Instant::now() + StdDuration::from_secs_f64(sec.max(0.0));
        loop {
            if CANCEL.load(Ordering::SeqCst) {
                return Err(SyErr::Cancelled);
            }
            let now = Instant::now();
            if now >= end {
                return Ok(());
            }
            std::thread::sleep((end - now).min(StdDuration::from_millis(100)));
        }
    }

    fn get(&mut self, params: &[(&str, String)]) -> Result<String, SyErr> {
        let cmd = params.iter().find(|(k, _)| *k == "Command").map(|(_, v)| v.clone()).unwrap_or_default();
        let mut last_err = String::new();
        for attempt in 0..5u32 {
            if CANCEL.load(Ordering::SeqCst) {
                return Err(SyErr::Cancelled);
            }
            if let Some(l) = self.last {
                let wait = l.elapsed().as_secs_f64();
                if wait < self.interval {
                    self.sleep(self.interval - wait)?;
                }
            }
            let mut backoff = (5 * 2u64.pow(attempt)) as f64; // 5,10,20,40,80秒
            let mut req = self.agent.get(&self.base);
            for (k, v) in params {
                req = req.query(k, v);
            }
            let result = req.call();
            self.last = Some(Instant::now());
            match result {
                Ok(resp) => {
                    let body = match resp.into_string() {
                        Ok(b) => b,
                        Err(e) => {
                            last_err = format!("{}", e);
                            backoff *= 1.0 + fastrand::f64() * 0.3;
                            warn!("syoboi", "syoboi request failed ({}), retry in {:.0}s", last_err, backoff);
                            if attempt < 4 {
                                self.sleep(backoff)?;
                            }
                            continue;
                        }
                    };
                    let doc = match roxmltree::Document::parse(&body) {
                        Ok(d) => d,
                        Err(e) => {
                            last_err = format!("{}", e);
                            backoff *= 1.0 + fastrand::f64() * 0.3;
                            warn!("syoboi", "syoboi request failed ({}), retry in {:.0}s", last_err, backoff);
                            if attempt < 4 {
                                self.sleep(backoff)?;
                            }
                            continue;
                        }
                    };
                    let code = doc
                        .root_element()
                        .children()
                        .find(|n| n.has_tag_name("Result"))
                        .and_then(|r| r.children().find(|n| n.has_tag_name("Code")))
                        .and_then(|c| c.text())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    if let Some(code) = code {
                        if code != "200" && code != "404" {
                            // 404=該当データなし→0件
                            let msg = doc
                                .root_element()
                                .children()
                                .find(|n| n.has_tag_name("Result"))
                                .and_then(|r| r.children().find(|n| n.has_tag_name("Message")))
                                .and_then(|c| c.text())
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            return Err(SyErr::Msg(format!("{}: code={} {}", cmd, code, msg)));
                        }
                    }
                    return Ok(body);
                }
                Err(ureq::Error::Status(status, resp)) => {
                    if status == 429 || status == 503 {
                        // 制限中: Retry-After に従う（無ければ長めに待つ）。以降の取得間隔も広げる
                        let ra = resp.header("Retry-After").unwrap_or("").trim().to_string();
                        backoff = if !ra.is_empty() && ra.chars().all(|c| c.is_ascii_digit()) {
                            ra.parse::<u64>().unwrap_or(600).min(600) as f64
                        } else {
                            (30 * 2u64.pow(attempt)) as f64
                        };
                        self.interval = (self.interval * 2.0).max(3.0).min(15.0);
                        warn!("syoboi", "syoboi rate limited ({}), wait {}s, interval -> {:.1}s", status, backoff as u64, self.interval);
                        if backoff > config::get().syoboi_max_wait_sec as f64 {
                            // 長時間の制限中は待ち続けず、この取得を失敗として終える（時間を置いて再実行）
                            return Err(SyErr::Msg(format!(
                                "しょぼいカレンダーから制限されています（{}、約{}分後に再試行してください）",
                                status,
                                backoff as u64 / 60
                            )));
                        }
                    } else if (400..500).contains(&status) && status != 408 {
                        // 同じ要求を繰り返しても解決しないエラーは再試行しない
                        return Err(SyErr::Msg(format!("HTTP {}: {}", status, cmd)));
                    }
                    last_err = format!("HTTP status {} for url ({})", status, self.base);
                    backoff *= 1.0 + fastrand::f64() * 0.3; // 再試行の集中を避ける
                    warn!("syoboi", "syoboi request failed ({}), retry in {:.0}s", last_err, backoff);
                    if attempt < 4 {
                        self.sleep(backoff)?;
                    }
                }
                Err(e) => {
                    last_err = format!("{}", e);
                    backoff *= 1.0 + fastrand::f64() * 0.3;
                    warn!("syoboi", "syoboi request failed ({}), retry in {:.0}s", last_err, backoff);
                    if attempt < 4 {
                        self.sleep(backoff)?;
                    }
                }
            }
        }
        Err(SyErr::Msg(format!("request failed: {}", last_err)))
    }

    fn doc<R>(&mut self, params: &[(&str, String)], f: impl FnOnce(&roxmltree::Document) -> R) -> Result<R, SyErr> {
        let body = self.get(params)?;
        let doc = roxmltree::Document::parse(&body).map_err(|e| SyErr::Msg(e.to_string()))?;
        Ok(f(&doc))
    }

    // ---- API ----
    pub fn channel_groups(&mut self) -> Result<Vec<Group>, SyErr> {
        self.doc(&[("Command", "ChGroupLookup".into())], |d| {
            items(d, "ChGroupItem")
                .iter()
                .filter_map(|it| {
                    let gid = int(it.get("ChGID"))?;
                    Some(Group {
                        gid,
                        name: non_empty(it.get("ChGroupName")).unwrap_or_else(|| format!("group{}", gid)),
                        sort: int(it.get("ChGroupOrder")),
                    })
                })
                .collect()
        })
    }

    pub fn channels(&mut self) -> Result<Vec<Channel>, SyErr> {
        self.doc(&[("Command", "ChLookup".into())], |d| {
            items(d, "ChItem")
                .iter()
                .filter_map(|it| {
                    let chid = int(it.get("ChID"))?;
                    Some(Channel {
                        chid,
                        name: non_empty(it.get("ChName")).unwrap_or_else(|| format!("ChID:{}", chid)),
                        gid: int(it.get("ChGID")),
                        sort: int(it.get("ChNumber")),
                    })
                })
                .collect()
        })
    }

    pub fn titles(&mut self, tids: &[i64]) -> Result<Vec<Title>, SyErr> {
        if tids.is_empty() {
            return Ok(vec![]);
        }
        let ids = tids.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(",");
        self.doc(&[("Command", "TitleLookup".into()), ("TID", ids)], |d| {
            items(d, "TitleItem")
                .iter()
                .filter_map(|it| {
                    let tid = int(it.get("TID"))?;
                    Some(Title {
                        tid,
                        title: non_empty(it.get("Title")).unwrap_or_else(|| format!("TID:{}", tid)),
                        cat: int(it.get("Cat")),
                        first_year: int(it.get("FirstYear")),
                        first_month: int(it.get("FirstMonth")),
                    })
                })
                .collect()
        })
    }

    /// key は Range（放送日時で絞る）か LastUpdate（更新日時で絞る）。件数が多いときは期間を分割する
    fn fetch_programs(
        &mut self,
        key: &str,
        start: NaiveDateTime,
        end: NaiveDateTime,
        chids: &Option<Vec<i64>>,
        end_inclusive: bool,
    ) -> Result<Vec<Program>, SyErr> {
        let f = "%Y%m%d_%H%M%S";
        let mut params: Vec<(&str, String)> = vec![
            ("Command", "ProgLookup".into()),
            (key, format!("{}-{}", start.format(f), end.format(f))),
            ("JOIN", "SubTitles".into()),
        ];
        if let Some(ch) = chids {
            params.push(("ChID", ch.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")));
        }
        let rows = self.doc(&params, |d| items(d, "ProgItem"))?;
        if rows.len() >= config::get().syoboi_split_threshold && (end - start) > Duration::minutes(10) {
            let half = (end - start) / 2;
            let mid = (start + half).with_second(0).unwrap().with_nanosecond(0).unwrap();
            info!("syoboi", "split {} {} - {} ({} items)", key, start, end, rows.len());
            let gap = if end_inclusive { Duration::seconds(1) } else { Duration::zero() };
            let first_end = if end_inclusive { mid - gap } else { mid };
            let mut a = self.fetch_programs(key, start, first_end, chids, end_inclusive)?;
            let b = self.fetch_programs(key, mid, end, chids, end_inclusive)?;
            a.extend(b);
            return Ok(a);
        }
        Ok(parse_programs(&rows))
    }

    /// 放送日時が start〜end の番組
    pub fn programs(&mut self, start: NaiveDateTime, end: NaiveDateTime, chids: &Option<Vec<i64>>) -> Result<Vec<Program>, SyErr> {
        self.fetch_programs("Range", start, end, chids, true)
    }

    /// since〜until に更新された番組（差分取得）
    pub fn programs_updated(&mut self, since: NaiveDateTime, until: NaiveDateTime, chids: &Option<Vec<i64>>) -> Result<Vec<Program>, SyErr> {
        self.fetch_programs("LastUpdate", since, until, chids, false)
    }

    /// since 以降に更新されたタイトルの TID 一覧
    pub fn titles_updated(&mut self, since: NaiveDateTime) -> Result<std::collections::HashSet<i64>, SyErr> {
        let p = [
            ("Command", "TitleLookup".to_string()),
            ("TID", "*".to_string()),
            ("LastUpdate", format!("{}-", since.format("%Y%m%d_%H%M%S"))),
        ];
        self.doc(&p, |d| items(d, "TitleItem").iter().filter_map(|it| int(it.get("TID"))).collect())
    }
}

fn parse_programs(rows: &[Item]) -> Vec<Program> {
    let mut out = vec![];
    for it in rows {
        let (pid, tid, chid) = (int(it.get("PID")), int(it.get("TID")), int(it.get("ChID")));
        let st = parse_dt(it.get("StTime"));
        let (Some(pid), Some(tid), Some(chid), Some(st)) = (pid, tid, chid, st) else { continue };
        // 上流の値は信用しない。範囲外の値で、日時の計算が失敗（panic）して、取得が止まらないようにする（±30日まで）
        let off = int(it.get("StOffset")).unwrap_or(0).clamp(-30 * 86400, 30 * 86400);
        let Some(st) = st.checked_add_signed(Duration::seconds(off)) else { continue };
        let ed = parse_dt(it.get("EdTime"));
        out.push(Program {
            pid,
            tid,
            chid,
            start_at: config::fmt_dt(st),
            end_at: ed.map(config::fmt_dt),
            count: int(it.get("Count")),
            subtitle: non_empty(it.get("STSubTitle")).or_else(|| non_empty(it.get("SubTitle"))),
            flag: int(it.get("Flag")).unwrap_or(0),
            deleted: if it.get("Deleted").map(|s| s.as_str()) == Some("1") { 1 } else { 0 },
        });
    }
    out
}
