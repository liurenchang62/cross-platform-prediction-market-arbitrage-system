//! LLM 二次验证模块：在二筛通过后，调用 DeepSeek API 进行最终确认
//!
//! ==================== 快速开始 ====================
//! 1. 购买 DeepSeek API：访问 https://platform.deepseek.com/
//! 2. 创建 API Key，充值余额（建议 20元起）
//! 3. 在 env/credentials.env 填入：
//!    LLM_PROVIDER=deepseek
//!    DEEPSEEK_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
//! 4. （可选）调整参数：
//!    LLM_MODEL=deepseek-chat          # 或 deepseek-reasoner
//!    LLM_TIMEOUT_MS=5000                # 超时时间
//!    LLM_MAX_TOKENS=150                 # 最大响应长度
//!    LLM_TEMPERATURE=0.0                # 创造性（0=最保守）
//!
//! ==================== 成本参考 ====================
//! - DeepSeek-V3 (deepseek-chat): 输入 1元/百万token, 输出 2元/百万token
//! - 单次验证约 500-800 token，成本约 0.001-0.002 元（1-2厘钱）
//!
//! ==================== 启用逻辑 ====================
//! - 二筛规则通过后，如果检测到 LLM_PROVIDER 设置，自动调用
//! - LLM 判断不同合约时，会打印 🚫 LLM拒绝 并阻断匹配
//! - LLM API 故障时会打印 ⚠️ LLM验证失败 并保守放行（回退到规则判断）

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::env;
use std::time::Duration;

/// LLM 提供商类型（当前仅支持 DeepSeek，保留扩展性）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LlmProvider {
    DeepSeek,
    #[allow(dead_code)]
    Moonshot, // 预留，暂未完整实现
}

impl LlmProvider {
    pub fn from_env() -> Option<Self> {
        match env::var("LLM_PROVIDER").ok()?.to_lowercase().as_str() {
            "deepseek" => Some(Self::DeepSeek),
            "moonshot" | "kimi" => Some(Self::Moonshot),
            _ => None,
        }
    }

    fn api_base(&self) -> &'static str {
        match self {
            Self::DeepSeek => "https://api.deepseek.com",
            Self::Moonshot => "https://api.moonshot.cn/v1",
        }
    }

    fn default_model(&self) -> &'static str {
        match self {
            // deepseek-chat: 性价比高，响应快，适合简单二分类任务
            // deepseek-reasoner: 推理能力强，适合复杂合约判断，但较慢较贵
            Self::DeepSeek => "deepseek-chat",
            Self::Moonshot => "moonshot-v1-8k",
        }
    }

    fn api_key_env(&self) -> &'static str {
        match self {
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            Self::Moonshot => "MOONSHOT_API_KEY",
        }
    }

    /// 显示名称
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::DeepSeek => "DeepSeek",
            Self::Moonshot => "Moonshot",
        }
    }
}

/// LLM 验证配置
pub struct LlmValidatorConfig {
    pub provider: LlmProvider,
    pub api_key: String,
    pub model: String,
    pub timeout_ms: u64,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl LlmValidatorConfig {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(provider) = LlmProvider::from_env() else {
            return Ok(None);
        };

        let api_key = env::var(provider.api_key_env())
            .with_context(|| format!("{} 未设置", provider.api_key_env()))?;

        if api_key.trim().is_empty() {
            anyhow::bail!("{} 为空", provider.api_key_env());
        }

        let model = env::var("LLM_MODEL")
            .unwrap_or_else(|_| provider.default_model().to_string());

        let timeout_ms = env::var("LLM_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000);

        let max_tokens = env::var("LLM_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(150);

        let temperature = env::var("LLM_TEMPERATURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        Ok(Some(Self {
            provider,
            api_key,
            model,
            timeout_ms,
            max_tokens,
            temperature,
        }))
    }
}

/// LLM 验证结果
#[derive(Debug, Clone)]
pub struct LlmValidationResult {
    pub is_same_contract: bool,
    pub raw_response: String,
    pub reasoning: String,
    pub provider: LlmProvider,
    pub latency_ms: u64,
}

/// LLM 验证器
pub struct LlmValidator {
    config: LlmValidatorConfig,
    client: Client,
}

impl LlmValidator {
    pub fn new(config: LlmValidatorConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .context("创建 HTTP 客户端失败")?;

        Ok(Self { config, client })
    }

    /// 验证两个市场标题是否指向同一合约
    pub async fn validate_match(
        &self,
        pm_title: &str,
        kalshi_title: &str,
        category: &str,
    ) -> Result<LlmValidationResult> {
        let start = std::time::Instant::now();

        let prompt = self.build_prompt(pm_title, kalshi_title, category);
        let response = self.call_api(&prompt).await?;
        let latency_ms = start.elapsed().as_millis() as u64;

        let (is_same, reasoning) = self.parse_response(&response);

        Ok(LlmValidationResult {
            is_same_contract: is_same,
            raw_response: response.clone(),
            reasoning,
            provider: self.config.provider,
            latency_ms,
        })
    }

    /// 构建提示词
    fn build_prompt(&self, pm_title: &str, kalshi_title: &str, category: &str) -> String {
        format!(
            "你是一名预测市场合约比对专家。请判断以下两个合约是否是同一标的的同一方向合约：

【Polymarket合约】
{pm_title}

【Kalshi合约】
{kalshi_title}

【类别】{category}

【判断标准】
1. 是否是同一事件/标的（如都是同一场比赛、同一选举）
2. 是否是同一方向（如都是看涨、都是看跌，或都是某队获胜）
3. 是否涉及相同的关键参数（如分数线、日期、队伍）

【注意】
- 如果一个是胜负盘，一个是总进球数盘，回答[否]
- 如果一个是单场比赛，一个是锦标赛冠军，回答[否]
- 如果日期/队伍明显不同，回答[否]

请严格按以下格式回答：
答案: <是/否>
原因: <不超过15字的简要说明>",
            pm_title = pm_title,
            kalshi_title = kalshi_title,
            category = category
        )
    }

    /// 调用 LLM API（DeepSeek 兼容 OpenAI 格式）
    async fn call_api(&self, prompt: &str) -> Result<String> {
        let url = format!("{}/chat/completions", self.config.provider.api_base());

        // DeepSeek 使用 OpenAI 兼容格式
        let body = json!({
            "model": self.config.model,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("LLM API 请求失败")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API 错误: HTTP {} - {}", status, text);
        }

        let json_resp: Value = resp
            .json()
            .await
            .context("解析 LLM 响应 JSON 失败")?;

        // 检查错误字段
        if let Some(error) = json_resp.get("error") {
            let msg = error.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误");
            anyhow::bail!("LLM API 返回错误: {}", msg);
        }

        let content = json_resp
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .context("LLM 响应格式异常，缺少 choices[0].message.content")?;

        Ok(content.to_string())
    }

    /// 解析响应
    fn parse_response(&self, response: &str) -> (bool, String) {
        let response_lower = response.to_lowercase();

        // 检查是否包含 "是" 或 "否"
        let is_same = if response_lower.contains("答案: 是") || response_lower.contains("答案：是") {
            true
        } else if response_lower.contains("答案: 否") || response_lower.contains("答案：否") {
            false
        } else {
            // 兜底：简单关键词判断
            response_lower.contains("是") && !response_lower.contains("否")
        };

        // 提取原因
        let reasoning = response
            .lines()
            .find(|l| l.contains("原因:"))
            .map(|l| l.split("原因:").nth(1).unwrap_or(l).trim().to_string())
            .unwrap_or_else(|| "-".to_string());

        (is_same, reasoning)
    }
}

/// 便捷函数：检查是否启用 LLM 验证
pub fn llm_validation_enabled() -> bool {
    LlmProvider::from_env().is_some()
}

/// 便捷函数：执行 LLM 验证（如果配置存在）
pub async fn maybe_validate_with_llm(
    pm_title: &str,
    kalshi_title: &str,
    category: &str,
) -> Result<Option<LlmValidationResult>> {
    let Some(config) = LlmValidatorConfig::from_env()? else {
        return Ok(None);
    };

    let validator = LlmValidator::new(config)?;
    let result = validator.validate_match(pm_title, kalshi_title, category).await?;

    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response_yes() {
        let validator = LlmValidator::new(LlmValidatorConfig {
            provider: LlmProvider::DeepSeek,
            api_key: "test".to_string(),
            model: "test".to_string(),
            timeout_ms: 1000,
            max_tokens: 150,
            temperature: 0.0,
        }).unwrap();

        let resp = "答案: 是\n原因: 同一场比赛胜负盘";
        let (is_same, reason) = validator.parse_response(resp);
        assert!(is_same);
        assert_eq!(reason, "同一场比赛胜负盘");
    }

    #[test]
    fn test_parse_response_no() {
        let validator = LlmValidator::new(LlmValidatorConfig {
            provider: LlmProvider::DeepSeek,
            api_key: "test".to_string(),
            model: "test".to_string(),
            timeout_ms: 1000,
            max_tokens: 150,
            temperature: 0.0,
        }).unwrap();

        let resp = "答案: 否\n原因: 一个是进球数一个是胜负";
        let (is_same, reason) = validator.parse_response(resp);
        assert!(!is_same);
        assert_eq!(reason, "一个是进球数一个是胜负");
    }
}
