//! 校验实盘相关环境变量是否就绪；**不打印**秘钥、Key ID 全文或 PEM 正文。
//! 用法：`cargo run --bin check_env`
//!
//! 环境变量名与 `src/system_params.rs` 保持一致。
#![allow(dead_code)] // `#[path]` 引入完整 `system_params`，本 bin 仅用其中一部分

#[path = "../system_params.rs"]
mod system_params;

use std::path::Path;
use std::str::FromStr as _;

use anyhow::{Context, Result};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::RsaPrivateKey;

// 与 system_params 同步（本 bin 为独立 crate 根，不引用 lib）
const KALSHI_API_KEY_ID_ENV: &str = "KALSHI_API_KEY_ID";
const KALSHI_PRIVATE_KEY_PATH_ENV: &str = "KALSHI_PRIVATE_KEY_PATH";

/// 你在 Polymarket 生成的 CLOB API Key（常为 UUID 字符串）；可与私钥并存，但**不能代替私钥**。
const POLYMARKET_API_KEY_USER_ENV: &str = "POLYMARKET_API_KEY";
/// 钱包地址（`0x…`）；仅存环境变量即可，无需文件路径。仅地址**无法**对订单做 EIP-712 签名。
const POLYMARKET_ADDRESS_USER_ENV: &str = "POLYMARKET_ADDRESS";

fn mask_len(label: &str, s: Option<String>) {
    match s {
        None => println!("  {label}: （未设置）"),
        Some(v) => {
            let t = v.trim();
            if t.is_empty() {
                println!("  {label}: （已设置但为空）");
            } else {
                println!("  {label}: 已设置（长度 {} 字符）", t.len());
            }
        }
    }
}

fn check_kalshi() -> Result<()> {
    println!("—— Kalshi ——");
    let key_id = std::env::var(KALSHI_API_KEY_ID_ENV).ok();
    mask_len(KALSHI_API_KEY_ID_ENV, key_id.clone());

    let path_raw = std::env::var(KALSHI_PRIVATE_KEY_PATH_ENV)
        .with_context(|| format!("请设置 {KALSHI_PRIVATE_KEY_PATH_ENV}（PEM 文件路径，可为 .txt）"))?;
    let path = system_params::normalize_env_file_path(&path_raw);
    if path.is_empty() {
        anyhow::bail!("{KALSHI_PRIVATE_KEY_PATH_ENV} 去掉空白与引号后为空");
    }
    println!(
        "  {}: 路径非空（{} 字符），不显示绝对路径",
        KALSHI_PRIVATE_KEY_PATH_ENV,
        path.len()
    );

    let p = Path::new(path.as_str());
    if !p.exists() {
        anyhow::bail!("私钥路径指向的文件不存在（请核对路径与扩展名）");
    }
    if !p.is_file() {
        anyhow::bail!("私钥路径不是常规文件");
    }

    let mut pem = std::fs::read_to_string(p).context("读取私钥文件")?;
    if pem.starts_with('\u{FEFF}') {
        pem.remove(0);
        println!("  PEM: 已去掉 UTF-8 BOM");
    }
    let pem_trim = pem.trim();
    let first = pem_trim.lines().next().unwrap_or("").trim();
    let last = pem_trim
        .lines()
        .filter(|l| !l.trim().is_empty())
        .last()
        .unwrap_or("")
        .trim();

    let begin_ok = first.contains("BEGIN") && first.contains("PRIVATE KEY");
    let end_ok = last.contains("END") && last.contains("PRIVATE KEY");
    if begin_ok && end_ok {
        println!("  PEM 首尾标记: 符合 PRIVATE KEY 封装");
    } else {
        println!("  PEM 第一行（仅示意类型，不含内容）: {}", summarize_line_kind(first));
        println!("  PEM 末行（仅示意类型，不含内容）: {}", summarize_line_kind(last));
        anyhow::bail!("PEM 首尾行不符合预期（需要含 BEGIN/END … PRIVATE KEY）");
    }

    let _key = RsaPrivateKey::from_pkcs1_pem(pem_trim).context(
        "RSA PKCS#1 PEM 解析失败（需 `-----BEGIN RSA PRIVATE KEY-----` 一类）",
    )?;
    println!("  RSA 私钥: 解析成功（与主程序 kalshi_trade 一致）");

    if key_id.as_ref().map(|s| s.trim().is_empty()).unwrap_or(true) {
        anyhow::bail!("{KALSHI_API_KEY_ID_ENV} 未设置或为空");
    }

    Ok(())
}

fn summarize_line_kind(line: &str) -> &'static str {
    if line.is_empty() {
        "（空）"
    } else if line.contains("BEGIN") {
        "BEGIN …"
    } else if line.contains("END") {
        "END …"
    } else {
        "（非 BEGIN/END 行，不展示）"
    }
}

fn norm_hex_addr(s: &str) -> String {
    s.trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_ascii_lowercase()
}

fn check_polymarket_clob() -> Result<()> {
    println!("\n—— Polymarket CLOB（`polymarket-client-sdk` / 本项目 polymarket_trade）——");
    println!(
        "  说明：API Key、地址可以是环境变量里的**直接字符串**（不必是路径）；\
         但官方 SDK 在 `post_order` 前要对订单做 **EIP-712 签名**，必须用**钱包私钥**，不能用 API Key 或地址代替。"
    );

    mask_len(
        system_params::POLYMARKET_PRIVATE_KEY_PATH_ENV,
        std::env::var(system_params::POLYMARKET_PRIVATE_KEY_PATH_ENV).ok(),
    );
    mask_len(
        system_params::POLYMARKET_PRIVATE_KEY_ENV,
        std::env::var(system_params::POLYMARKET_PRIVATE_KEY_ENV).ok(),
    );

    let pk_loaded = system_params::load_polymarket_private_key_hex();

    if let Ok(pk_hex) = &pk_loaded {
        let signer = alloy_signer_local::PrivateKeySigner::from_str(pk_hex.as_str())
            .context("Polymarket 私钥 hex 解析失败（检查文件或环境变量中是否含空格/换行）")?;
        let addr = signer.address();
        println!(
            "  私钥加载: OK（{}）\n    address: {addr:#x}",
            if std::env::var(system_params::POLYMARKET_PRIVATE_KEY_PATH_ENV)
                .map(|p| !p.trim().is_empty())
                .unwrap_or(false)
            {
                "来自 POLYMARKET_PRIVATE_KEY_PATH"
            } else {
                "来自 POLYMARKET_PRIVATE_KEY"
            }
        );

        if let Ok(want_raw) = std::env::var(POLYMARKET_ADDRESS_USER_ENV) {
            if !want_raw.trim().is_empty() {
                let want = norm_hex_addr(&want_raw);
                let got = norm_hex_addr(&format!("{addr:#x}"));
                if want == got {
                    println!("  与 {POLYMARKET_ADDRESS_USER_ENV}: 一致（仅比较地址，不显示你的环境变量值）");
                } else {
                    println!(
                        "  ⚠ 与 {POLYMARKET_ADDRESS_USER_ENV} 不一致：私钥对应地址与所填地址不是同一钱包，请核对。"
                    );
                }
            }
        }
    } else {
        println!(
            "  ⚠ 私钥未就绪：请设置 {}（txt 路径，推荐）或 {}",
            system_params::POLYMARKET_PRIVATE_KEY_PATH_ENV,
            system_params::POLYMARKET_PRIVATE_KEY_ENV
        );
    }

    mask_len(
        system_params::POLYMARKET_SIGNATURE_TYPE_ENV,
        std::env::var(system_params::POLYMARKET_SIGNATURE_TYPE_ENV).ok(),
    );

    println!(
        "\n  你已配置的 Polymarket 相关变量（值直接放在环境变量里即可；本 bin 只报长度）:"
    );
    mask_len(
        POLYMARKET_API_KEY_USER_ENV,
        std::env::var(POLYMARKET_API_KEY_USER_ENV).ok(),
    );
    mask_len(
        POLYMARKET_ADDRESS_USER_ENV,
        std::env::var(POLYMARKET_ADDRESS_USER_ENV).ok(),
    );
    println!(
        "  注：CLOB 的 **secret / passphrase** 若与 API Key 成套导出，SDK 可用于 HTTP 鉴权；\
         本仓库发单仍会在每笔调用 `sign(&signer, order)`，故私钥文件或 {} 必不可少。",
        system_params::POLYMARKET_PRIVATE_KEY_ENV
    );

    pk_loaded.context("Polymarket 私钥")?;
    Ok(())
}

fn main() -> Result<()> {
    println!("环境变量自检（不输出秘钥内容）\n");

    let mut ok = true;
    if let Err(e) = check_kalshi() {
        ok = false;
        println!("\n❌ Kalshi: {}", e);
    } else {
        println!("\n✅ Kalshi: 配置可用");
    }

    if let Err(e) = check_polymarket_clob() {
        ok = false;
        println!("\n❌ Polymarket: {}", e);
        println!("   {}", system_params::polymarket_private_key_env_diag());
    } else {
        println!("\n✅ Polymarket CLOB: 配置可用");
    }

    if !ok {
        std::process::exit(1);
    }
    println!("\n全部通过。可运行主程序 `cargo run -p trader --release` 或 `cargo run -p portfolio-monitor --bin portfolio-snapshot` 等做进一步联调。");
    Ok(())
}
