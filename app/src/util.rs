//! API 共通の型
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ApiErr {
    pub status: u16,
    pub detail: String,
}

pub fn err(status: u16, detail: impl Into<String>) -> ApiErr {
    ApiErr { status, detail: detail.into() }
}

impl From<rusqlite::Error> for ApiErr {
    fn from(e: rusqlite::Error) -> Self {
        crate::error!("api", "db error: {}", e);
        err(500, "internal error")
    }
}

/// (HTTP ステータス, 本文)。本文が Null なら空
pub type ApiResult = Result<(u16, Value), ApiErr>;

pub fn ok(v: Value) -> ApiResult {
    Ok((200, v))
}

pub fn body_obj(body: &Value) -> serde_json::Map<String, Value> {
    body.as_object().cloned().unwrap_or_default()
}

/// 整数として読む（null / 未指定は None。整数でなければ 422）
pub fn opt_int(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<i64>, ApiErr> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .map(Some)
            .ok_or_else(|| err(422, format!("{} は整数で指定してください", k))),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| err(422, format!("{} は整数で指定してください", k))),
        _ => Err(err(422, format!("{} は整数で指定してください", k))),
    }
}

pub fn opt_str(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<String>, ApiErr> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        _ => Err(err(422, format!("{} は文字列で指定してください", k))),
    }
}

pub fn opt_bool(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<bool>, ApiErr> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::Number(n)) if n.as_i64() == Some(0) || n.as_i64() == Some(1) => Ok(Some(n.as_i64() == Some(1))),
        Some(Value::String(s)) if matches!(s.as_str(), "true" | "false") => Ok(Some(s == "true")),
        _ => Err(err(422, format!("{} は真偽値で指定してください", k))),
    }
}

pub fn opt_f64(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<f64>, ApiErr> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| err(422, format!("{} は数値で指定してください", k))),
        _ => Err(err(422, format!("{} は数値で指定してください", k))),
    }
}

pub fn msg(detail: &str) -> Value {
    json!({ "detail": detail })
}
