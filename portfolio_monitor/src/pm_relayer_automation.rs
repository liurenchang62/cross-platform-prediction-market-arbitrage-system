//! 实盘桌面：Proxy 钱包经 Relayer **批量授权**、**按条件赎回**、**刷新 CLOB Collateral**。  
//! 由主程序 `trader` 在**启动进入主循环前**调用一次；不要求独立 CLI。

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr as _;
use std::time::Duration;

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::signers::Signer;
use alloy::signers::SignerSync;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result};
use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk_v2::clob::types::AssetType;
use polymarket_client_sdk_v2::data::Client as DataClient;
use polymarket_client_sdk_v2::data::types::request::PositionsRequest;
use polymarket_client_sdk_v2::data::types::response::Position;
use polymarket_client_sdk_v2::types::address;
use polymarket_client_sdk_v2::types::Decimal;
use polymarket_client_sdk_v2::{contract_config, derive_proxy_wallet, ContractConfig, POLYGON};
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use serde_json::json;

use crate::pm;

const DEFAULT_RELAYER_URL: &str = "https://relayer-v2.polymarket.com";
const DEFAULT_POLYGON_RPC: &str = "https://polygon-bor-rpc.publicnode.com";
/// pUSD（Polymarket USD）—— Polymarket V2（2026-04-28）迁移后的抵押品，替换原 USDC.e。
const PUSD_POLYGON: polymarket_client_sdk_v2::types::Address =
    address!("0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB");
/// USDC.e on Polygon（V2 迁移前的抵押品，仍用于 wrap 输入）。
const USDCE_POLYGON: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
/// Polymarket V2 CollateralOnramp：wrap(USDC.e → pUSD)。
const ONRAMP_POLYGON: Address = address!("0x93070a847efEf7F70739046A929D47a521F5B8ee");
const RELAY_HUB_POLYGON: Address = address!("0xD216153c06E857cD7f72665E0aF1d7D82172F494");
const PROXY_FACTORY_POLYGON: Address = address!("0xaB45c5A4B0c941a2F231C04C3f49182e1A254052");
const PROXY_CALL_CALL: u8 = 1;
const MAX_SIGNED_PROXY_GAS: u64 = 10_000_000;
const FALLBACK_RELAYER_PROXY_GAS: u64 = 2_000_000;
const MIN_SIGNED_PROXY_GAS: u64 = 400_000;

sol! {
    interface IProxyWalletFactory {
        struct ProxyCall {
            uint8 typeCode;
            address to;
            uint256 value;
            bytes data;
        }
        function proxy(ProxyCall[] calldata calls) external payable returns (bytes[] memory returnValues);
    }

    interface ICTFRedeemEncode {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets
        ) external;
    }

    interface INegRiskRedeemEncode {
        function redeemPositions(
            bytes32 conditionId,
            uint256[] calldata amounts
        ) external;
    }

    interface IERC20ApproveEncode {
        function approve(address spender, uint256 value) external returns (bool);
    }

    interface IERC1155SetApprovalEncode {
        function setApprovalForAll(address operator, bool approved) external;
    }

    interface IERC20Balance {
        function balanceOf(address account) external view returns (uint256);
    }

    interface ICTFRedeemPrecheck {
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
        function getCollectionId(bytes32 parentCollectionId, bytes32 conditionId, uint256 indexSet) external view returns (bytes32);
        function getPositionId(address collateralToken, bytes32 collectionId) external view returns (uint256);
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }

    interface ICollateralOnramp {
        /// wrap USDC.e → pUSD，1:1；_asset=USDC.e 地址，_to=pUSD 接收方，_amount=micro 单位（6位小数）。
        function wrap(address _asset, address _to, uint256 _amount) external;
    }
}

#[derive(Deserialize)]
struct RelayPayloadResp {
    address: String,
    nonce: String,
}

#[derive(Deserialize)]
struct SubmitResp {
    #[serde(rename = "transactionID")]
    transaction_id: String,
    state: String,
}

#[derive(Deserialize, Clone)]
struct TxStatusRow {
    state: String,
    #[serde(default, rename = "transactionHash")]
    transaction_hash: String,
}

/// 每轮桌面「PM 侧」链上/ CLOB 维护结果（飞书摘要可据此生成）。
#[derive(Debug, Clone, Default)]
pub struct PmDeskHydrateReport {
    pub poly_proxy_mode: bool,
    pub relayer_env_ok: bool,
    pub relayer_approvals_submitted: bool,
    pub redeem_ok: usize,
    pub redeem_fail: usize,
    pub redeem_skipped: usize,
    pub clob_refresh_ok: bool,
    pub note: Option<String>,
}

pub fn polygon_rpc_for_automation() -> String {
    std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| DEFAULT_POLYGON_RPC.to_string())
}

pub fn relayer_url_for_automation() -> String {
    std::env::var("POLYMARKET_RELAYER_URL").unwrap_or_else(|_| DEFAULT_RELAYER_URL.to_string())
}

pub fn feishu_pm_redeem_line(
    report: &PmDeskHydrateReport,
    session_cycle: usize,
    global_tick: usize,
) -> Option<String> {
    if report.redeem_ok == 0 && report.redeem_fail == 0 {
        return None;
    }
    Some(format!(
        "【机器人】PM赎回｜周期#{} tick{}｜成功{} 失败{} 跳过{}｜CLOB刷新{}",
        session_cycle as u64,
        global_tick as u64,
        report.redeem_ok,
        report.redeem_fail,
        report.redeem_skipped,
        if report.clob_refresh_ok { "是" } else { "否" }
    ))
}

fn signature_type_u8() -> u8 {
    std::env::var(crate::system_params::POLYMARKET_SIGNATURE_TYPE_ENV)
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0)
}

fn relayer_api_key() -> Result<String> {
    std::env::var("RELAYER_API_KEY")
        .or_else(|_| std::env::var("POLYMARKET_API_KEY"))
        .map_err(|_| anyhow::anyhow!("RELAYER_API_KEY / POLYMARKET_API_KEY"))
}

fn relayer_api_key_address() -> Result<String> {
    std::env::var("RELAYER_API_KEY_ADDRESS")
        .or_else(|_| std::env::var("POLYMARKET_ADDRESS"))
        .map_err(|_| anyhow::anyhow!("RELAYER_API_KEY_ADDRESS / POLYMARKET_ADDRESS"))
}

fn try_relayer_creds() -> Option<(String, String)> {
    match (relayer_api_key(), relayer_api_key_address()) {
        (Ok(k), Ok(a)) => Some((k, a)),
        _ => None,
    }
}

fn relayer_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

fn create_proxy_struct_hash(
    from: Address,
    to_factory: Address,
    factory_calldata: &[u8],
    relayer_fee: u128,
    gas_price: u128,
    gas_limit: &str,
    nonce: &str,
    relay_hub: Address,
    relay: Address,
) -> B256 {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    buf.extend_from_slice(b"rlx:");
    buf.extend_from_slice(from.as_slice());
    buf.extend_from_slice(to_factory.as_slice());
    buf.extend_from_slice(factory_calldata);
    buf.extend_from_slice(&U256::from(relayer_fee).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(gas_price).to_be_bytes::<32>());
    buf.extend_from_slice(
        &U256::from_str_radix(gas_limit, 10)
            .unwrap_or(U256::ZERO)
            .to_be_bytes::<32>(),
    );
    buf.extend_from_slice(
        &U256::from_str_radix(nonce, 10)
            .unwrap_or(U256::ZERO)
            .to_be_bytes::<32>(),
    );
    buf.extend_from_slice(relay_hub.as_slice());
    buf.extend_from_slice(relay.as_slice());
    keccak256(&buf)
}

async fn rpc_estimate_gas(
    http: &reqwest::Client,
    rpc: &str,
    from: Address,
    to: Address,
    data: &[u8],
) -> Result<u64> {
    let payload = json!({
        "jsonrpc": "2.0",
        "method": "eth_estimateGas",
        "params": [{
            "from": format!("{:#x}", from),
            "to": format!("{:#x}", to),
            "data": format!("0x{}", hex::encode(data)),
        }, "latest"],
        "id": 1u64,
    });
    let v: serde_json::Value = http
        .post(rpc)
        .json(&payload)
        .send()
        .await
        .context("RPC eth_estimateGas HTTP")?
        .json()
        .await
        .context("RPC eth_estimateGas JSON")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("RPC eth_estimateGas: {}", err);
    }
    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .context("estimateGas missing result")?;
    let hexs = result.strip_prefix("0x").unwrap_or(result);
    u64::from_str_radix(hexs, 16).context("estimateGas parse hex")
}

async fn relayer_proxy_gas_limit_for_signature(
    http: &reqwest::Client,
    rpc: &str,
    from: Address,
    factory_data: &[u8],
) -> String {
    match rpc_estimate_gas(http, rpc, from, PROXY_FACTORY_POLYGON, factory_data).await {
        Ok(est) => {
            let buffered = est
                .saturating_mul(13)
                .saturating_div(10)
                .saturating_add(80_000);
            buffered
                .clamp(MIN_SIGNED_PROXY_GAS, MAX_SIGNED_PROXY_GAS)
                .to_string()
        }
        Err(e) => {
            eprintln!(
                "   ⚠ eth_estimateGas 失败（{}），签名 gasLimit 使用兜底 {}",
                e, FALLBACK_RELAYER_PROXY_GAS
            );
            FALLBACK_RELAYER_PROXY_GAS.to_string()
        }
    }
}

async fn rpc_eth_call(http: &reqwest::Client, rpc: &str, to: Address, data: Vec<u8>) -> Result<Vec<u8>> {
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
        .context("RPC eth_call HTTP")?
        .json()
        .await
        .context("RPC eth_call JSON")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("RPC eth_call: {}", err);
    }
    let result = v
        .get("result")
        .and_then(|x| x.as_str())
        .context("RPC missing result")?;
    let hexs = result.strip_prefix("0x").unwrap_or(result);
    if hexs.is_empty() {
        return Ok(Vec::new());
    }
    hex::decode(hexs).map_err(|e| anyhow::anyhow!("RPC hex: {}", e))
}

fn decode_abi_u256_word(bytes: &[u8]) -> Result<U256> {
    anyhow::ensure!(bytes.len() == 32, "ABI word len {}", bytes.len());
    let mut a = [0u8; 32];
    a.copy_from_slice(bytes);
    Ok(U256::from_be_bytes(a))
}

async fn standard_ctf_eligible_for_redeem(
    http: &reqwest::Client,
    rpc: &str,
    ctf: Address,
    proxy: polymarket_client_sdk_v2::types::Address,
    condition_id: B256,
) -> Result<(bool, String)> {
    let pd = ICTFRedeemPrecheck::payoutDenominatorCall { conditionId: condition_id };
    let pd_raw = rpc_eth_call(http, rpc, ctf, pd.abi_encode()).await?;
    if pd_raw.len() < 32 {
        return Ok((false, "链上 payoutDenominator 无返回".into()));
    }
    let denom = decode_abi_u256_word(&pd_raw[pd_raw.len() - 32..])?;
    if denom.is_zero() {
        return Ok((false, "链上 payoutDenominator=0（未结算）".into()));
    }
    let mut sum = U256::ZERO;
    for idx in [1u64, 2u64] {
        let col_call = ICTFRedeemPrecheck::getCollectionIdCall {
            parentCollectionId: B256::ZERO,
            conditionId: condition_id,
            indexSet: U256::from(idx),
        };
        let col_raw = match rpc_eth_call(http, rpc, ctf, col_call.abi_encode()).await {
            Ok(b) => b,
            Err(e) => return Ok((false, format!("getCollectionId: {}", e))),
        };
        if col_raw.len() < 32 {
            continue;
        }
        let mut w = [0u8; 32];
        w.copy_from_slice(&col_raw[col_raw.len() - 32..]);
        let collection_id = B256::from(w);
        let pid_call = ICTFRedeemPrecheck::getPositionIdCall {
            collateralToken: PUSD_POLYGON,
            collectionId: collection_id,
        };
        let pid_raw = match rpc_eth_call(http, rpc, ctf, pid_call.abi_encode()).await {
            Ok(b) => b,
            Err(e) => return Ok((false, format!("getPositionId: {}", e))),
        };
        if pid_raw.len() < 32 {
            continue;
        }
        let pos_id = decode_abi_u256_word(&pid_raw[pid_raw.len() - 32..])?;
        let bal_call = ICTFRedeemPrecheck::balanceOfCall {
            account: proxy,
            id: pos_id,
        };
        let bal_raw = rpc_eth_call(http, rpc, ctf, bal_call.abi_encode()).await.unwrap_or_default();
        if bal_raw.len() >= 32 {
            let b = decode_abi_u256_word(&bal_raw[bal_raw.len() - 32..]).unwrap_or(U256::ZERO);
            sum = sum.saturating_add(b);
        }
    }
    if sum.is_zero() {
        return Ok((false, "Proxy 在 CTF partition 1/2 上无可赎余额".into()));
    }
    Ok((true, String::new()))
}

fn decimal_to_raw_micro(d: &Decimal) -> U256 {
    let s = d.to_string();
    if let Ok(f) = s.parse::<rust_decimal::Decimal>() {
        let micros = (f * rust_decimal::Decimal::from(1_000_000u64)).trunc();
        let as_u = micros.to_u128().unwrap_or(0);
        return U256::from(as_u);
    }
    U256::ZERO
}

async fn fetch_all_positions(user: polymarket_client_sdk_v2::types::Address) -> Result<Vec<Position>> {
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

async fn get_relay_payload(
    http: &reqwest::Client,
    base: &str,
    eoa: Address,
) -> Result<RelayPayloadResp> {
    let url = format!(
        "{}/relay-payload?address={:#x}&type=PROXY",
        base, eoa
    );
    let r = http.get(&url).send().await.context("relayer relay-payload")?;
    let status = r.status();
    if !status.is_success() {
        let t = r.text().await.unwrap_or_default();
        anyhow::bail!("relay-payload HTTP {}: {}", status, t);
    }
    Ok(r.json().await.context("relay-payload JSON")?)
}

async fn submit_proxy_tx(
    http: &reqwest::Client,
    base: &str,
    api_key: &str,
    key_address: &str,
    body: serde_json::Value,
) -> Result<SubmitResp> {
    let url = format!("{}/submit", base);
    let r = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("RELAYER_API_KEY", api_key)
        .header("RELAYER_API_KEY_ADDRESS", key_address)
        .json(&body)
        .send()
        .await
        .context("relayer submit")?;
    let status = r.status();
    if !status.is_success() {
        let t = r.text().await.unwrap_or_default();
        anyhow::bail!("submit HTTP {}: {}", status, t);
    }
    r.json().await.context("submit JSON")
}

async fn get_transaction_status(
    http: &reqwest::Client,
    base: &str,
    id: &str,
) -> Result<Vec<TxStatusRow>> {
    let url = format!("{}/transaction?id={}", base, id);
    let r = http.get(&url).send().await.context("relayer transaction")?;
    let status = r.status();
    if !status.is_success() {
        let t = r.text().await.unwrap_or_default();
        anyhow::bail!("transaction HTTP {}: {}", status, t);
    }
    r.json().await.context("transaction JSON")
}

async fn poll_until_done(http: &reqwest::Client, base: &str, id: &str) -> Result<TxStatusRow> {
    for _ in 0..80 {
        let rows = get_transaction_status(http, base, id).await?;
        if let Some(row) = rows.first() {
            match row.state.as_str() {
                "STATE_CONFIRMED" | "STATE_MINED" => return Ok(row.clone()),
                "STATE_FAILED" | "STATE_INVALID" => {
                    anyhow::bail!(
                        "relayer tx {} 终态 {} hash={}",
                        id,
                        row.state,
                        row.transaction_hash
                    );
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    anyhow::bail!("relayer 轮询超时 transactionID={}", id)
}

async fn relay_submit_proxy(
    http: &reqwest::Client,
    base: &str,
    rpc: &str,
    eoa: Address,
    signer: &PrivateKeySigner,
    data_user: polymarket_client_sdk_v2::types::Address,
    api_key: &str,
    key_address: &str,
    factory_data: Vec<u8>,
    metadata: &str,
) -> Result<TxStatusRow> {
    let rp = get_relay_payload(http, base, eoa).await?;
    let relay_worker: Address = rp
        .address
        .parse()
        .map_err(|e| anyhow::anyhow!("relay address parse: {}", e))?;
    let gas_limit_str =
        relayer_proxy_gas_limit_for_signature(http, rpc, eoa, &factory_data).await;
    let digest = create_proxy_struct_hash(
        eoa,
        PROXY_FACTORY_POLYGON,
        &factory_data,
        0,
        0,
        &gas_limit_str,
        &rp.nonce,
        RELAY_HUB_POLYGON,
        relay_worker,
    );
    let sig = signer
        .sign_message_sync(digest.as_slice())
        .context("relayer proxy sign")?;
    let sig_hex = format!("0x{}", hex::encode(sig.as_bytes()));
    let body = json!({
        "from": format!("{:#x}", eoa),
        "to": format!("{:#x}", PROXY_FACTORY_POLYGON),
        "proxyWallet": format!("{:#x}", data_user),
        "data": format!("0x{}", hex::encode(&factory_data)),
        "nonce": rp.nonce,
        "signature": sig_hex,
        "signatureParams": {
            "gasPrice": "0",
            "gasLimit": gas_limit_str.clone(),
            "relayerFee": "0",
            "relayHub": format!("{:#x}", RELAY_HUB_POLYGON),
            "relay": format!("{:#x}", relay_worker),
        },
        "type": "PROXY",
        "metadata": metadata,
    });
    let resp = submit_proxy_tx(http, base, api_key, key_address, body).await?;
    poll_until_done(http, base, &resp.transaction_id).await
}

fn spenders_clob(cfg_std: &ContractConfig, cfg_neg: &ContractConfig) -> Vec<(&'static str, Address)> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (name, addr) in [
        ("CTF Exchange", cfg_std.exchange),
        ("Neg Risk CTF Exchange", cfg_neg.exchange),
    ] {
        if seen.insert(addr) {
            out.push((name, addr));
        }
    }
    if let Some(adapter) = cfg_neg.neg_risk_adapter {
        if seen.insert(adapter) {
            out.push(("Neg Risk Adapter", adapter));
        }
    }
    out
}

fn build_approval_proxy_calls(
    usdc: Address,
    ctf: Address,
    spenders: &[(&str, Address)],
) -> Vec<IProxyWalletFactory::ProxyCall> {
    let mut calls = Vec::with_capacity(spenders.len() * 2);
    for &(_name, spender) in spenders {
        calls.push(IProxyWalletFactory::ProxyCall {
            typeCode: PROXY_CALL_CALL,
            to: usdc,
            value: U256::ZERO,
            data: IERC20ApproveEncode::approveCall {
                spender,
                value: U256::MAX,
            }
            .abi_encode()
            .into(),
        });
        calls.push(IProxyWalletFactory::ProxyCall {
            typeCode: PROXY_CALL_CALL,
            to: ctf,
            value: U256::ZERO,
            data: IERC1155SetApprovalEncode::setApprovalForAllCall {
                operator: spender,
                approved: true,
            }
            .abi_encode()
            .into(),
        });
    }
    calls
}

async fn submit_relayer_approval_batch(
    http: &reqwest::Client,
    base: &str,
    rpc: &str,
    eoa: Address,
    signer: &PrivateKeySigner,
    data_user: polymarket_client_sdk_v2::types::Address,
    api_key: &str,
    key_address: &str,
) -> Result<()> {
    let cfg_std = contract_config(POLYGON, false).ok_or_else(|| anyhow::anyhow!("contract_config std"))?;
    let cfg_neg = contract_config(POLYGON, true).ok_or_else(|| anyhow::anyhow!("contract_config neg"))?;
    let spenders = spenders_clob(cfg_std, cfg_neg);
    let proxy_calls = build_approval_proxy_calls(
        cfg_std.collateral,
        cfg_std.conditional_tokens,
        &spenders,
    );
    let factory_data = IProxyWalletFactory::proxyCall {
        calls: proxy_calls,
    }
    .abi_encode();
    println!("   💳 PM Relayer：提交 pUSD/CTF 授权批次（{} 个 spender）…", spenders.len());
    let row = relay_submit_proxy(
        http,
        base,
        rpc,
        eoa,
        signer,
        data_user,
        api_key,
        key_address,
        factory_data,
        "desk clob approvals batch",
    )
    .await?;
    println!(
        "   ✓ PM Relayer 授权批次已确认 {} ({})",
        row.transaction_hash, row.state
    );
    Ok(())
}

struct RedeemRoundReport {
    ok: usize,
    fail: usize,
    skipped: usize,
}

async fn run_redeem_round(
    http: &reqwest::Client,
    base: &str,
    rpc: &str,
    eoa: Address,
    signer: &PrivateKeySigner,
    data_user: polymarket_client_sdk_v2::types::Address,
    api_key: &str,
    key_address: &str,
) -> Result<RedeemRoundReport> {
    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut skipped = 0usize;

    let ctf_std = contract_config(POLYGON, false)
        .map(|c| c.conditional_tokens)
        .ok_or_else(|| anyhow::anyhow!("CTF 配置"))?;
    let neg_adapter = contract_config(POLYGON, true)
        .and_then(|c| c.neg_risk_adapter)
        .ok_or_else(|| anyhow::anyhow!("NegRisk 配置"))?;

    let positions = fetch_all_positions(data_user).await?;
    let mut groups: BTreeMap<(B256, bool), Vec<Position>> = BTreeMap::new();
    for p in positions {
        let sz = p.size.to_string().parse::<f64>().unwrap_or(0.0);
        if sz.abs() < 1e-9 {
            continue;
        }
        groups.entry((p.condition_id, p.negative_risk)).or_default().push(p);
    }

    if groups.is_empty() {
        return Ok(RedeemRoundReport {
            ok: 0,
            fail: 0,
            skipped: 0,
        });
    }

    for ((condition_id, neg_risk), ps) in groups {
        let redeemable_any = ps.iter().any(|x| x.redeemable);

        if neg_risk {
            if !redeemable_any {
                skipped += 1;
                continue;
            }
        } else {
            let (eligible, _reason) = standard_ctf_eligible_for_redeem(
                http,
                rpc,
                ctf_std,
                data_user,
                condition_id,
            )
            .await
            .unwrap_or((false, "RPC 查询失败".into()));
            if !eligible {
                skipped += 1;
                continue;
            }
        }

        let inner_calldata: Vec<u8> = if neg_risk {
            if ps.iter().any(|p| p.outcome_index != 0 && p.outcome_index != 1) {
                skipped += 1;
                continue;
            }
            let mut yes = U256::ZERO;
            let mut no = U256::ZERO;
            for p in &ps {
                let raw = decimal_to_raw_micro(&p.size);
                match p.outcome_index {
                    0 => yes = yes.saturating_add(raw),
                    1 => no = no.saturating_add(raw),
                    _ => {}
                }
            }
            INegRiskRedeemEncode::redeemPositionsCall {
                conditionId: condition_id,
                amounts: vec![yes, no],
            }
            .abi_encode()
        } else {
            if !redeemable_any {
                skipped += 1;
                continue;
            }
            let req = polymarket_client_sdk_v2::ctf::types::RedeemPositionsRequest::for_binary_market(
                PUSD_POLYGON,
                condition_id,
            );
            ICTFRedeemEncode::redeemPositionsCall {
                collateralToken: req.collateral_token,
                parentCollectionId: req.parent_collection_id,
                conditionId: req.condition_id,
                indexSets: req.index_sets.clone(),
            }
            .abi_encode()
        };

        let target = if neg_risk {
            neg_adapter
        } else {
            ctf_std
        };

        let factory_data = IProxyWalletFactory::proxyCall {
            calls: vec![IProxyWalletFactory::ProxyCall {
                typeCode: PROXY_CALL_CALL,
                to: target,
                value: U256::ZERO,
                data: inner_calldata.into(),
            }],
        }
        .abi_encode();

        let meta = format!(
            "desk redeem {} {}",
            if neg_risk { "neg" } else { "ctf" },
            hex::encode(condition_id.as_slice())
        );
        match relay_submit_proxy(
            http,
            base,
            rpc,
            eoa,
            signer,
            data_user,
            api_key,
            key_address,
            factory_data,
            &meta,
        )
        .await
        {
            Ok(row) => {
                ok += 1;
                println!(
                    "   ✓ PM 赎回 {:#x} → {} ({})",
                    condition_id, row.transaction_hash, row.state
                );
            }
            Err(e) => {
                fail += 1;
                eprintln!("   ✗ PM 赎回 {:#x}: {}", condition_id, e);
            }
        }
    }

    Ok(RedeemRoundReport { ok, fail, skipped })
}

async fn refresh_clob_collateral_desk() -> Result<()> {
    let client = pm::connect().await?;
    let req = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    client
        .update_balance_allowance(req.clone())
        .await
        .map_err(|e| anyhow::anyhow!("CLOB update: {}", e))?;
    let row = client
        .balance_allowance(req)
        .await
        .map_err(|e| anyhow::anyhow!("CLOB balance: {}", e))?;
    println!(
        "   PM CLOB Collateral（已刷新）≈ ${:.2}",
        pm::clob_micro_to_usd(&row.balance)
    );
    Ok(())
}

/// `batch_approvals`：当前仅启动时调用，传 `true`（Relayer 批量授权 + 赎回 + 刷新 Collateral）。保留参数供将来若需「仅赎回不重授权」扩展。
pub async fn desk_hydrate_pm_proxy(
    http: &reqwest::Client,
    polygon_rpc: &str,
    relayer_url: &str,
    batch_approvals: bool,
) -> Result<PmDeskHydrateReport> {
    let mut report = PmDeskHydrateReport::default();
    let base = relayer_base(relayer_url);
    let rpc = polygon_rpc.trim_end_matches('/');

    if signature_type_u8() != 1 {
        report.note = Some("POLYMARKET_SIGNATURE_TYPE≠1，跳过 Relayer 赎回/授权".into());
        refresh_clob_collateral_desk().await?;
        report.clob_refresh_ok = true;
        return Ok(report);
    }

    report.poly_proxy_mode = true;

    let data_user = pm::polymarket_data_api_user_address()?;
    let hex = pm::read_polymarket_private_key_hex().context("PM 私钥")?;
    let signer = PrivateKeySigner::from_str(&hex)
        .context("PrivateKeySigner")?
        .with_chain_id(Some(POLYGON));
    let eoa = signer.address();
    let expected_proxy = derive_proxy_wallet(eoa, POLYGON);
    if expected_proxy != Some(data_user) {
        anyhow::bail!(
            "Proxy 地址不一致：Data API {:#x} ≠ derive {:#x?}",
            data_user,
            expected_proxy
        );
    }

    let creds = try_relayer_creds();
    report.relayer_env_ok = creds.is_some();

    if let Some((ref ak, ref addr)) = creds {
        if batch_approvals {
            if let Err(e) =
                submit_relayer_approval_batch(http, &base, rpc, eoa, &signer, data_user, ak, addr)
                    .await
            {
                eprintln!("   ⚠ PM Relayer 批量授权失败（赎回仍继续）: {}", e);
            } else {
                report.relayer_approvals_submitted = true;
            }
        }

        match run_redeem_round(
            http, &base, rpc, eoa, &signer, data_user, ak, addr,
        )
        .await
        {
            Ok(r) => {
                report.redeem_ok = r.ok;
                report.redeem_fail = r.fail;
                report.redeem_skipped = r.skipped;
            }
            Err(e) => {
                eprintln!("   ⚠ PM 赎回轮询失败: {}", e);
            }
        }
    } else {
        println!("   ℹ 未配置 Relayer API：跳过链上赎回/授权（请设 RELAYER_API_KEY 等）");
    }

    refresh_clob_collateral_desk().await?;
    report.clob_refresh_ok = true;
    Ok(report)
}

/// 将 Proxy 钱包中全部 USDC.e 通过 CollateralOnramp 换成 pUSD（Polymarket V2 抵押品迁移）。
/// 仅支持 `POLYMARKET_SIGNATURE_TYPE=1`（Proxy 钱包），通过 Relayer 提交链上交易。
/// 需要环境变量：`POLYMARKET_PRIVATE_KEY(_PATH)`、`RELAYER_API_KEY`、`RELAYER_API_KEY_ADDRESS`（或对应别名）。
pub async fn wrap_usdce_to_pusd(polygon_rpc: &str, relayer_url: &str) -> Result<()> {
    if signature_type_u8() != 1 {
        anyhow::bail!("此工具仅支持 POLYMARKET_SIGNATURE_TYPE=1（Proxy 钱包）；EOA 用户请直接调用 CollateralOnramp 合约。");
    }

    let http = crate::http_client_env::build_http_client(Duration::from_secs(30))
        .context("build http client")?;
    let base = relayer_base(relayer_url);
    let rpc = polygon_rpc.trim_end_matches('/');

    let data_user = pm::polymarket_data_api_user_address()?;
    let proxy: Address = data_user;
    let hex = pm::read_polymarket_private_key_hex().context("PM 私钥")?;
    let signer = PrivateKeySigner::from_str(&hex)
        .context("PrivateKeySigner")?
        .with_chain_id(Some(POLYGON));
    let eoa = signer.address();

    let ak = relayer_api_key()?;
    let addr = relayer_api_key_address()?;

    // 1. 查询 Proxy 钱包 USDC.e 余额
    let bal_call = IERC20Balance::balanceOfCall { account: proxy };
    let bal_raw = rpc_eth_call(&http, rpc, USDCE_POLYGON, bal_call.abi_encode())
        .await
        .context("查询 USDC.e 余额")?;
    if bal_raw.len() < 32 {
        anyhow::bail!("USDC.e balanceOf 返回数据不足");
    }
    let usdc_e_balance = decode_abi_u256_word(&bal_raw[bal_raw.len() - 32..])?;
    if usdc_e_balance.is_zero() {
        println!("   ℹ Proxy 钱包 USDC.e 余额为 0，无需 wrap。");
        return Ok(());
    }
    let balance_human = usdc_e_balance.to_string().parse::<u128>().unwrap_or(0) as f64 / 1_000_000.0;
    println!("   📋 Proxy 钱包 USDC.e 余额：{:.6} USDC.e（raw={}）", balance_human, usdc_e_balance);

    // 2. 构造两步 Proxy 调用：先 approve Onramp 消费 USDC.e，再调 wrap()
    let approve_call = IERC20ApproveEncode::approveCall {
        spender: ONRAMP_POLYGON,
        value: usdc_e_balance,
    };
    let wrap_call = ICollateralOnramp::wrapCall {
        _asset: USDCE_POLYGON,
        _to: proxy,
        _amount: usdc_e_balance,
    };

    let proxy_calls = vec![
        IProxyWalletFactory::ProxyCall {
            typeCode: PROXY_CALL_CALL,
            to: USDCE_POLYGON,
            value: U256::ZERO,
            data: approve_call.abi_encode().into(),
        },
        IProxyWalletFactory::ProxyCall {
            typeCode: PROXY_CALL_CALL,
            to: ONRAMP_POLYGON,
            value: U256::ZERO,
            data: wrap_call.abi_encode().into(),
        },
    ];
    let factory_data = IProxyWalletFactory::proxyCall { calls: proxy_calls }.abi_encode();

    println!("   🔄 提交 wrap 交易（approve USDC.e + wrap → pUSD）via Relayer…");
    let row = relay_submit_proxy(
        &http, &base, rpc, eoa, &signer, data_user, &ak, &addr, factory_data, "wrap usdce to pusd",
    )
    .await?;
    println!(
        "   ✅ wrap 已确认：{} ({})",
        row.transaction_hash, row.state
    );
    println!("   💰 {:.6} USDC.e → pUSD 转换完成。请重启主程序。", balance_human);
    Ok(())
}
