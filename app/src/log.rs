//! 簡易ログ（Python 版と同じ書式: `2026-11-17 22:48:22,197 INFO sync: ...`）
use std::fmt::Arguments;

pub fn write(level: &str, name: &str, args: Arguments) {
    let now = chrono::Utc::now().with_timezone(&crate::config::get().tz);
    // 外部の入力（上流の応答・ユーザー名・ヘッダなど）に改行や制御文字が入っていても、ログの行を偽造できないよう、1行にそろえる
    let msg: String = args.to_string().chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    eprintln!("{},{:03} {} {}: {}", now.format("%Y-%m-%d %H:%M:%S"), now.timestamp_subsec_millis(), level, name, msg);
}

#[macro_export]
macro_rules! info { ($name:expr, $($a:tt)*) => { $crate::log::write("INFO", $name, format_args!($($a)*)) }; }
#[macro_export]
macro_rules! warn { ($name:expr, $($a:tt)*) => { $crate::log::write("WARNING", $name, format_args!($($a)*)) }; }
#[macro_export]
macro_rules! error { ($name:expr, $($a:tt)*) => { $crate::log::write("ERROR", $name, format_args!($($a)*)) }; }
