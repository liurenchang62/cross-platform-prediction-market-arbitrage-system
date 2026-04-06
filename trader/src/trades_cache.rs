//! 实盘会话内文本缓存：`logs/trades_cache.txt`（相对 workspace 根目录）。

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};

static CACHE: Mutex<Option<BufWriter<File>>> = Mutex::new(None);

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root (parent of trader/)")
        .to_path_buf()
}

fn cache_file_path() -> PathBuf {
    workspace_root().join("logs").join("trades_cache.txt")
}

/// 每次进程内进入实盘（`CONFIRM`）后调用：创建 `logs/` 并**清空**缓存文件。
pub fn init_on_live_session_start() -> std::io::Result<()> {
    let p = cache_file_path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let f = File::create(&p)?;
    let mut g = CACHE.lock().unwrap();
    *g = Some(BufWriter::new(f));
    Ok(())
}

pub fn append_line(s: &str) {
    let mut g = CACHE.lock().unwrap();
    if let Some(w) = g.as_mut() {
        let _ = writeln!(w, "{}", s);
        let _ = w.flush();
    }
}

pub fn trade_out_fmt(args: std::fmt::Arguments<'_>) {
    let line = args.to_string();
    println!("{}", line);
    append_line(&line);
}

pub fn append_session_banner(now: DateTime<Utc>) {
    append_line(&format!(
        "======== 实盘会话开始 {} UTC ========",
        now.format("%Y-%m-%d %H:%M:%S")
    ));
}

#[macro_export]
macro_rules! trade_out {
    ($($arg:tt)*) => {{
        $crate::trades_cache::trade_out_fmt(format_args!($($arg)*))
    }};
}
