//! LLM 二次验证模块：在二筛通过后，调用 DeepSeek API 进行最终确认
//!
//! ==================== 设计目标 ====================
//! 确保进入跟踪系统的套利对是 100% 匹配的，杜绝误匹配导致的交易风险。
//! 本模块放置在二筛之后、全量匹配之后，响应时间不影响实际交易速度。
//!
//! ==================== 快速开始 ====================
//! 1. 购买 DeepSeek API：访问 https://platform.deepseek.com/
//! 2. 创建 API Key，充值余额（建议 20元起）
//! 3. 在 env/credentials.env 填入：
//!    LLM_PROVIDER=deepseek
//!    DEEPSEEK_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
//! 4. （可选）调整参数：
//!    LLM_MODEL=deepseek-chat          # 或 deepseek-reasoner
//!    LLM_TIMEOUT_MS=10000             # 超时时间（默认10秒，详细验证需要）
//!    LLM_MAX_TOKENS=400                 # 最大响应长度（默认400，详细输出）
//!    LLM_TEMPERATURE=0.0                # 创造性（0=最保守）
//!
//! ==================== 成本参考 ====================
//! - DeepSeek-V3 (deepseek-chat): 输入 1元/百万token, 输出 2元/百万token
//! - 单次验证约 1000-1500 token（详细验证输出），成本约 0.002-0.004 元（2-4厘钱）
//! - 这个成本远低于一次错误匹配可能造成的损失
//!
//! ==================== 验证逻辑 ====================
//! - 检查项：事件同一性、标的同一性、命题类型一致性、关键参数完全匹配
//! - 支持互斥市场：方向相反（YES/NO对立）是合法的套利模式
//! - LLM 判断不匹配时，会打印 🚫 LLM拒绝 并阻断匹配
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

    /// API Key 文件路径环境变量名
    fn api_key_path_env(&self) -> &'static str {
        match self {
            Self::DeepSeek => "DEEPSEEK_API_KEY_PATH",
            Self::Moonshot => "MOONSHOT_API_KEY_PATH",
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
    /// 从环境变量或文件加载配置
    ///
    /// API Key 加载优先级：
    /// 1. 如果设置了 `DEEPSEEK_API_KEY_PATH`（或对应 provider 的 _PATH 变量），从该文件读取
    /// 2. 否则尝试直接读取 `DEEPSEEK_API_KEY` 环境变量
    pub fn from_env() -> Result<Option<Self>> {
        let Some(provider) = LlmProvider::from_env() else {
            return Ok(None);
        };

        // 尝试从文件或环境变量获取 API Key
        let api_key = Self::load_api_key(&provider)?;

        if api_key.trim().is_empty() {
            anyhow::bail!(
                "{} 和 {} 均未设置或为空",
                provider.api_key_path_env(),
                provider.api_key_env()
            );
        }

        let model = env::var("LLM_MODEL")
            .unwrap_or_else(|_| provider.default_model().to_string());

        let timeout_ms = env::var("LLM_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10000);  // 增加超时以适应详细验证

        let max_tokens = env::var("LLM_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(400);  // 增加以适应详细的验证输出

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

    /// 加载 API Key：优先从文件，其次环境变量
    fn load_api_key(provider: &LlmProvider) -> Result<String> {
        // 先检查是否有文件路径
        if let Ok(key_path) = env::var(provider.api_key_path_env()) {
            let trimmed = key_path.trim();
            if !trimmed.is_empty() {
                // 规范化路径（支持 Windows 路径）
                let normalized = Self::normalize_file_path(trimmed);
                return Self::read_key_from_file(&normalized, provider.api_key_path_env());
            }
        }

        // 退回到直接环境变量
        env::var(provider.api_key_env())
            .with_context(|| format!("{} 未设置", provider.api_key_env()))
    }

    /// 从文件读取 API Key（取第一行非空非注释内容）
    fn read_key_from_file(path: &str, env_name: &str) -> Result<String> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("{} 指向的文件无法读取: {}", env_name, path))?;

        // 提取第一行有效内容（跳过空行和 # 注释行）
        let key = contents
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .context(format!("{} 指向的文件没有有效内容: {}", env_name, path))?;

        Ok(key.to_string())
    }

    /// 规范化文件路径（处理 Windows 反斜杠等）
    fn normalize_file_path(path: &str) -> String {
        // 去除首尾引号（如果有）
        let trimmed = path.trim_matches('"').trim_matches('\'');
        // 统一使用系统路径分隔符
        std::path::PathBuf::from(trimmed)
            .to_string_lossy()
            .to_string()
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

    /// 构建提示词 - 严格版，确保100%匹配准确性
    ///
    /// 关键概念：
    /// - 这是跨平台套利交易场景，两个平台可以有完全相同的合约，也可以有互斥合约
    /// - 互斥合约 = 同一标的但方向相反（如一个押A赢YES，另一个押A输NO），这在套利中是合法的
    /// - 我们需要的是【同一标的】，不一定需要【同一方向】
    fn build_prompt(&self, pm_title: &str, kalshi_title: &str, category: &str) -> String {
        format!(
            "你是一个预测市场套利系统的合约匹配审核专家。你的任务是判断 Polymarket 和 Kalshi 两个平台的合约是否指向【完全相同的标的（事件/比赛/选举/ proposition）】。

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
【Polymarket 合约标题】
{pm_title}

【Kalshi 合约标题】
{kalshi_title}

【市场类别】{category}
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

【核心任务】
判断这两个合约是否描述的是【同一标的】。注意：方向（YES/NO）可以不同，这在套利中是正常的。重点是【标的是否完全一致】。

【必须通过的全部检查项】

1. 【事件同一性】是否是同一个具体事件？
   - 体育：同一场比赛（同两队、同日期、同赛事轮次）
   - 选举：同一个选举/候选人/选区/轮次
   - 金融：同一标的/同一到期日/同一行权价
   - 娱乐：同一颁奖/同一节目/同一赛季
   - 如果涉及不同赛季、不同日期、不同对阵 → [否]

2. 【标的同一性】指向的具体对象是否完全一致？
   - 球队/选手名称是否完全对应（注意简称vs全称、缩写vs完整名）
   - 候选人姓名是否同一人
   - 地区/选区是否一致
   - 如果一方提到具体选手，另一方提到的是比赛本身 → [否]

3. 【命题类型一致性】合约类型是否对应？
   ✓ 允许的类型对应：
     - 胜负盘(Winner) ↔ 胜负盘(Winner)：YES/NO可以相反
     - 统计盘口(Points/Assists/Rebounds) ↔ 统计盘口：O/U X.5 ↔ X+ / X-
     - 冠军盘(Champion/Win the tournament) ↔ 冠军盘
     - 晋级盘(Advance/Make playoffs) ↔ 晋级盘
     - 特定事件发生盘(Will X happen?) ↔ (Will X happen?)
   
   ✗ 严禁匹配的类型错配：
     - 胜负盘 ↔ 统计盘口（如 Winner vs Points O/U）→ [否]
     - 单场比赛 ↔ 锦标赛冠军 → [否]
     - 比赛结束盘(Completed match) ↔ 胜负盘(Winner) → [否]
     - 总进球数(Goals scored) ↔ 胜负盘 → [否]
     - 第一轮选举 ↔ 大选/决选 → [否]

4. 【关键参数完全匹配】所有数字和阈值必须一致：
   - 【统计阈值等价关系 - 非常重要】Polymarket 的 O/U X.5 和 Kalshi 的 X+/X- 是等价的：
     * O/U 9.5 Over（>9.5） = 10+（≥10）→ 两者是同一标的 ✓
     * O/U 9.5 Under（<9.5） = 9-（≤9）→ 两者是同一标的 ✓
     * O/U 8.5 Over = 9+，O/U 8.5 Under = 8-
     * O/U 4.5 Over = 5+，O/U 4.5 Under = 4-
     * O/U 7.5 Over = 8+，O/U 7.5 Under = 7-
     * 依此类推：O/U X.5 Over = (X+1)+，O/U X.5 Under = X-
   - 但如果两边都是 Kalshi（如 10+ vs 15+）或都是 O/U（如 O/U 9.5 vs O/U 10.5），阈值必须完全相同
   - 日期必须一致（日期不一致 → [否]）
   - 轮次必须一致（Round 1 vs Round 2 → [否]）

5. 【方向理解】（重要）
   - 两个合约可以方向相反（一个YES一个NO），这是套利的基础
   - 只要标的是同一事件、同一阈值、同一类型，方向相反是正常的
   - 例如：PM Lakers-vs-Celtics-Winner-Lakers YES vs KS Lakers-vs-Celtics-Winner NO 是合法的套利对 → [是]

【常见错误陷阱 - 必须避免】
- ❌ 只看标题相似度，忽略具体参数
- ❌ 忽略轮次/阶段差异（初选vs大选、第一轮vs第二轮）
- ❌ 将统计盘口与胜负盘混为一谈
- ❌ 忽略分数线差异（2.5 vs 3.5 是不同市场）
- ❌ 忽略队伍名称对应关系（简称vs全称）

【决策流程】
1. 先判断是否是同一事件/比赛/标的
2. 如果是，判断命题类型是否一致
3. 如果是，检查所有关键参数（分数线、日期、轮次）是否完全一致
4. 如果全部匹配，回答[是]（即使方向相反也是正常的）
5. 任一项不匹配，回答[否]

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
【输出格式 - 严格遵循】

答案: [是] 或 [否]
置信度: [高/中/低]
匹配理由: [一句话说明为什么匹配或不匹配]
关键验证点: [列出验证通过的关键点，如：1.同一场比赛✓ 2.都是胜负盘✓ 3.队伍名称一致✓]

示例输出1（匹配）：
答案: [是]
置信度: [高]
匹配理由: [同一场NBA比赛LAL vs BOS，都是胜负盘，队伍名称完全对应]
关键验证点: [1.事件：Lakers vs Celtics同一场✓ 2.类型：都是Winner盘✓ 3.标的：Lakers/Celtics对应✓ 4.日期一致✓]

示例输出2（不匹配）：
答案: [否]
置信度: [高]
匹配理由: [一个是总进球数盘口，一个是胜负盘，标的不同]
关键验证点: [1.事件：同一场比赛✓ 2.类型：Goals O/U vs Winner ✗ 类型不匹配]

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
现在请分析上面的两个合约标题，给出你的判断：",
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

    /// 解析响应 - 支持新的详细格式
    fn parse_response(&self, response: &str) -> (bool, String) {
        let response_lower = response.to_lowercase();

        // 提取答案 [是] 或 [否]
        let is_same = if response_lower.contains("答案: [是]") ||
                         response_lower.contains("答案：[是]") ||
                         response_lower.contains("答案:是") ||
                         response_lower.contains("答案：是") {
            true
        } else if response_lower.contains("答案: [否]") ||
                  response_lower.contains("答案：[否]") ||
                  response_lower.contains("答案:否") ||
                  response_lower.contains("答案：否") {
            false
        } else {
            // 兜底：按简单关键词判断
            let has_yes = response_lower.contains("[是]") ||
                         response_lower.contains("是") && !response_lower.contains("否");
            let has_no = response_lower.contains("[否]") ||
                        response_lower.contains("否");
            has_yes && !has_no
        };

        // 提取匹配理由（更详细的说明）
        let reasoning = response
            .lines()
            .find(|l| l.to_lowercase().contains("匹配理由:"))
            .map(|l| {
                let lower = l.to_lowercase();
                if let Some(idx) = lower.find("匹配理由:") {
                    l[idx + "匹配理由:".len()..].trim()
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_string()
                } else {
                    l.to_string()
                }
            })
            .or_else(|| {
                // 备选：尝试提取关键验证点
                response
                    .lines()
                    .find(|l| l.to_lowercase().contains("关键验证点:"))
                    .map(|l| l.trim().to_string())
            })
            .unwrap_or_else(|| {
                // 最终备选：简单判断
                if is_same { "LLM判断为匹配".to_string() } else { "LLM判断为不匹配".to_string() }
            });

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
