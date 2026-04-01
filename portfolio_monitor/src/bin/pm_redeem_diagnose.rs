//! 诊断 **Relayer 赎回反复 `STATE_FAILED`**：对比 Data API 与 Polygon 上 CTF 的 payout / 余额。
//! 仅需与主程序 / `pm_relayer_automation` **相同的 Proxy 私钥环境** + **公开 RPC**（无需登录 Polymarket 网页）。
//!
//! ```text
//! set POLYMARKET_SIGNATURE_TYPE=1
//! cargo run -p portfolio-monitor --bin portfolio-pm-redeem-diagnose --release
//!
//! # 自定义 RPC（若默认节点不可用）
//! cargo run -p portfolio-monitor --bin portfolio-pm-redeem-diagnose --release -- --rpc-url https://polygon-bor-rpc.publicnode.com
//! ```

use std::collections::BTreeMap;
use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use clap::Parser;
use polymarket_client_sdk::data::Client as DataClient;
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::types::response::Position;
use polymarket_client_sdk::types::address;
use polymarket_client_sdk::types::Decimal;
use polymarket_client_sdk::{contract_config, POLYGON};
use serde_json::json;

use portfolio_monitor::pm;

const DEFAULT_RPC: &str = "https://polygon-bor-rpc.publicnode.com";
const USDC_POLYGON: polymarket_client_sdk::types::Address =
    address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");

sol! {
    interface IConditionalTokensView {
        function getOutcomeSlotCount(bytes32 conditionId) external view returns (uint256);
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
        function getCollectionId(bytes32 parentCollectionId, bytes32 conditionId, uint256 indexSet) external view returns (bytes32);
        function getPositionId(address collateralToken, bytes32 collectionId) external view returns (uint256);
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

#[derive(Parser)]
#[command(name = "portfolio-pm-redeem-diagnose")]
struct Args {
    #[arg(long, default_value = DEFAULT_RPC, env = "POLYGON_RPC_URL")]
    rpc_url: String,
}

async fn fetch_all_positions(user: polymarket_client_sdk::types::Address) -> Result<Vec<Position>> {
    let client = DataClient::default();
    let mut all = Vec::new();
    let mut offset: i32 = 0;
    let zero = Decimal::from(0);
    loop {
        let req = PositionsRequest::builder()
            .user(user)
            .limit(500)
            .context("positions limit")?
            .offset(offset)
            .context("positions offset")?
            .size_threshold(zero)
            .build();
        let batch = client
            .positions(&req)
            .await
            .map_err(|e| anyhow::anyhow!("Data API positions: {}", e))?;
        let n = batch.len();
        if n == 0 {
            break;
        }
        all.extend(batch);
        if n < 500 {
            break;
        }
        offset += 500;
        if offset > 10_000 {
            break;
        }
    }
    Ok(all)
}

async fn eth_call(http: &reqwest::Client, rpc: &str, to: Address, data: Vec<u8>) -> Result<Vec<u8>> {
    let payload = json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{
            "to": format!("{:#x}", to),
            "data": format!("0x{}", hex::encode(&data)),
        }, "latest"],
        "id": 1u64,
    });
    let v: serde_json::Value = http
        .post(rpc)
        .json(&payload)
        .send()
        .await
        .context("eth_call HTTP")?
        .json()
        .await
        .context("eth_call JSON")?;

    if let Some(err) = v.get("error") {
        anyhow::bail!("eth_call error: {}", err);
    }
    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .context("eth_call missing result")?;
    let hexs = result.strip_prefix("0x").unwrap_or(result);
    if hexs.is_empty() {
        return Ok(Vec::new());
    }
    hex::decode(hexs)
        .map_err(|e| anyhow::anyhow!("eth_call hex: {}", e))
}

fn decode_u256_word(bytes: &[u8]) -> Result<U256> {
    anyhow::ensure!(
        bytes.len() == 32,
        "expected 32 bytes, got {}",
        bytes.len()
    );
    let mut a = [0u8; 32];
    a.copy_from_slice(bytes);
    Ok(U256::from_be_bytes(a))
}

fn fmt_micro6(x: U256) -> String {
    let s = x.to_string();
    if s == "0" {
        return "0".to_string();
    }
    // USDC / conditional 多为 6 位小数
    if s.len() <= 6 {
        let pad = 6 - s.len();
        format!("0.{}{}", "0".repeat(pad), s)
    } else {
        let split = s.len() - 6;
        format!("{}.{}", &s[..split], &s[split..])
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let rpc = args.rpc_url.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let proxy = pm::polymarket_data_api_user_address().context("Proxy / EOA（私钥 + POLYMARKET_SIGNATURE_TYPE）")?;
    let ctf = contract_config(POLYGON, false)
        .map(|c| c.conditional_tokens)
        .context("CTF contract_config")?;

    println!("Polygon RPC: {}", rpc);
    println!("Proxy（Data API user）: {:#x}", proxy);
    println!("CTF: {:#x}", ctf);

    let positions = fetch_all_positions(proxy).await?;
    println!("Data API 仓位条数: {}\n", positions.len());

    let mut groups: BTreeMap<(B256, bool), Vec<Position>> = BTreeMap::new();
    for p in positions {
        let sz = p.size.to_string().parse::<f64>().unwrap_or(0.0);
        if sz.abs() < 1e-9 {
            continue;
        }
        groups
            .entry((p.condition_id, p.negative_risk))
            .or_default()
            .push(p);
    }

    if groups.is_empty() {
        println!("无有余额分组，退出。");
        return Ok(());
    }

    let usdc_a: Address = USDC_POLYGON;

    for ((condition_id, neg_risk), ps) in groups {
        let redeemable_any = ps.iter().any(|x| x.redeemable);
        let titles: String = ps
            .iter()
            .map(|x| format!("{}[{}] sz={}", x.outcome, x.outcome_index, x.size))
            .collect::<Vec<_>>()
            .join(" | ");

        println!("======== condition {:#x}", condition_id);
        println!("  neg_risk={}  redeemable_any={}", neg_risk, redeemable_any);
        println!("  API: {}", titles);

        // CTF 视图
        let slot_call = IConditionalTokensView::getOutcomeSlotCountCall { conditionId: condition_id };
        let slot_raw = eth_call(&http, rpc, ctf, slot_call.abi_encode()).await;
        let payout_call = IConditionalTokensView::payoutDenominatorCall { conditionId: condition_id };
        let pay_raw = eth_call(&http, rpc, ctf, payout_call.abi_encode()).await;

        match (slot_raw, pay_raw) {
            (Ok(s), Ok(p)) if s.len() >= 32 && p.len() >= 32 => {
                let slots = decode_u256_word(&s[s.len() - 32..])?;
                let denom = decode_u256_word(&p[p.len() - 32..])?;
                println!("  CTF getOutcomeSlotCount = {}", slots);
                println!("  CTF payoutDenominator   = {}", denom);
                if denom.is_zero() {
                    println!("  ⚠ payoutDenominator=0：链上条件尚未报告结果，`redeemPositions` 会 revert（与 Relayer STATE_FAILED 一致）。");
                }
            }
            (Err(e), _) | (_, Err(e)) => println!("  ⚠ CTF 查询失败: {}", e),
            _ => println!("  ⚠ CTF 返回空数据"),
        }

        if neg_risk {
            println!("  (NegRisk：主程序内 Relayer 路径走适配器赎回；此处未查适配器内余额。若 API.neg_risk 与真实市场不一致会导致走错合约并 revert。)");
            continue;
        }

        for idx in [1u64, 2u64] {
            let col_call = IConditionalTokensView::getCollectionIdCall {
                parentCollectionId: B256::ZERO,
                conditionId: condition_id,
                indexSet: U256::from(idx),
            };
            let col = match eth_call(&http, rpc, ctf, col_call.abi_encode()).await {
                Ok(b) if b.len() >= 32 => {
                    let mut w = [0u8; 32];
                    w.copy_from_slice(&b[b.len() - 32..]);
                    B256::from(w)
                }
                Ok(_) => {
                    println!("  index_set {}：getCollectionId 空", idx);
                    continue;
                }
                Err(e) => {
                    println!("  index_set {}：getCollectionId err {}", idx, e);
                    continue;
                }
            };
            let pid_call = IConditionalTokensView::getPositionIdCall {
                collateralToken: usdc_a,
                collectionId: col,
            };
            let pos_id = match eth_call(&http, rpc, ctf, pid_call.abi_encode()).await {
                Ok(b) if b.len() >= 32 => decode_u256_word(&b[b.len() - 32..])?,
                _ => {
                    println!("  index_set {}：getPositionId 失败", idx);
                    continue;
                }
            };
            let bal_call = IConditionalTokensView::balanceOfCall {
                account: proxy,
                id: pos_id,
            };
            let bal = match eth_call(&http, rpc, ctf, bal_call.abi_encode()).await {
                Ok(b) if b.len() >= 32 => decode_u256_word(&b[b.len() - 32..])?,
                _ => U256::ZERO,
            };
            println!(
                "  ERC1155 index_set={} position_id={} balance_raw={} (~{} 单位，若 6 位小数≈ USDC)",
                idx, pos_id, bal, fmt_micro6(bal)
            );
        }

        println!();
    }

    println!("提示：若 `payoutDenominator=0` 或 Proxy 在 CTF 上 YES/NO 余额均为 0，则当前 `redeemPositions` 会失败。");
    println!("     在 Polygonscan 打开失败交易的 `to` 应为 RelayHub；内层 calldata 与 relayer 提交一致。");

    Ok(())
}
