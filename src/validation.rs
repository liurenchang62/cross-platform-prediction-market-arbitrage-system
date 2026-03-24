// src/validation.rs
//! 二筛模块：对向量匹配结果进行精确验证

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// 州缩写-选区号（如 ga-14、wv-02），用于选举语境
static ELECTORAL_STATE_DISTRICT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b[a-z]{2}-\d{1,2}\b").expect("ELECTORAL_STATE_DISTRICT_RE")
});
static ELECTORAL_NTH_PLACE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b\d+(?:st|nd|rd|th)\s+place\b").expect("ELECTORAL_NTH_PLACE_RE")
});

/// 安全词列表（单方有日期时放行）
const SAFE_WORDS: [&str; 6] = [
    "next", "upcoming", "today", "tonight", "future", "current"
];

/// 体育比分关键词
const SPORTS_KEYWORDS: [&str; 23] = [
    "points", "goals", "runs", "o/u", "over/under", "over", "under",
    "winner", "win", "tie", "draw", "spread", "moneyline", "total",
    "vs", "versus", "score", "scored", "mvp", "championship", "points",
    "rebounds", "assists"
];

/// 体育垃圾市场关键词
const GARBAGE_KEYWORDS: [&str; 10] = [
    "o/u", "rounds", "sets", "games", "maps", "upsets",
    "quarters", "halves", "periods", "wins"
];

/// 统计数据类型（必须互斥）
const STAT_TYPES: [&str; 8] = [
    "points", "rebounds", "assists", "steals",
    "blocks", "threes", "double", "triple"
];

/// 月份名称映射
const MONTH_MAP: [(&str, u32); 24] = [
    ("jan", 1), ("january", 1),
    ("feb", 2), ("february", 2),
    ("mar", 3), ("march", 3),
    ("apr", 4), ("april", 4),
    ("may", 5), ("may", 5),
    ("jun", 6), ("june", 6),
    ("jul", 7), ("july", 7),
    ("aug", 8), ("august", 8),
    ("sep", 9), ("september", 9),
    ("oct", 10), ("october", 10),
    ("nov", 11), ("november", 11),
    ("dec", 12), ("december", 12),
];

/// 匹配结果（带方向信息）
#[derive(Debug, Clone)]
pub struct MatchInfo {
    pub pm_title: String,
    pub kalshi_title: String,
    pub similarity: f64,
    pub category: String,
    pub pm_side: String,      // "YES" 或 "NO"
    pub kalshi_side: String,  // "YES" 或 "NO"
    pub needs_inversion: bool, // 是否需要颠倒 Y/N 含义
}

/// 提取的日期信息
#[derive(Debug, Clone, PartialEq)]
pub struct DateInfo {
    pub month: u32,
    pub day: u32,
    pub has_year: bool,
    pub year: Option<u32>,
}

/// 提取的数值信息
#[derive(Debug, Clone)]
pub struct NumberInfo {
    pub value: f64,
    pub context: String,
    pub is_year: bool,
}

/// 留存样本信息
#[derive(Debug, Clone)]
pub struct RetainedSample {
    pub pm_title: String,
    pub kalshi_title: String,
    pub similarity: f64,
    pub category: String,
    pub pm_side: String,
    pub kalshi_side: String,
    pub needs_inversion: bool,
}

/// ==================== 工具函数 ====================
pub fn extract_number(text: &str) -> Option<f64> {
    let re = Regex::new(r"(\d+\.?\d*)").ok()?;
    re.captures(text)?.get(1)?.as_str().parse::<f64>().ok()
}

pub fn extract_first_team(title: &str) -> String {
    if title.contains(" vs ") {
        title.split(" vs ").next().unwrap_or("").trim().to_string()
    } else if title.contains(" vs. ") {
        title.split(" vs. ").next().unwrap_or("").trim().to_string()
    } else {
        String::new()
    }
}

fn extract_teams(title: &str) -> Option<(String, String)> {
    if title.contains(" vs ") {
        let mut parts = title.splitn(2, " vs ");
        let team1 = parts.next()?.trim().to_string();
        let team2 = parts.next()?.trim().to_string();
        if !team1.is_empty() && !team2.is_empty() {
            return Some((team1, team2));
        }
    } else if title.contains(" vs. ") {
        let mut parts = title.splitn(2, " vs. ");
        let team1 = parts.next()?.trim().to_string();
        let team2 = parts.next()?.trim().to_string();
        if !team1.is_empty() && !team2.is_empty() {
            return Some((team1, team2));
        }
    }
    None
}

pub fn extract_winner(title: &str) -> String {
    if title.contains(" - ") {
        title.split(" - ").last().unwrap_or("").trim().to_string()
    } else {
        String::new()
    }
}

/// 去掉队名前的赛事前缀，如 "Miami Open: Ignacio Buse" → "Ignacio Buse"
fn strip_team_event_prefix(team: &str) -> String {
    let t = team.trim();
    if let Some(i) = t.rfind(':') {
        let after = t[i + 1..].trim();
        if !after.is_empty() {
            return after.to_string();
        }
    }
    t.to_string()
}

/// Kalshi 胜负盘标题中「对阵双方」：支持 `X at Y Winner?`、`Will … win the A vs B : …`、`A vs B Winner?`
fn extract_kalshi_moneyline_pair(title: &str) -> Option<(String, String)> {
    let main = title.split(" - ").next()?.trim();
    if main.is_empty() {
        return None;
    }
    let lower = main.to_lowercase();

    // 「Will X win map N in the TeamA vs. TeamB match?」类：必须在泛型 `vs.` 切分之前处理，
    // 否则会把整句问句切成 (Will…JD Gaming, LYON match?)，导致两队校验失败并落入默认 1Y1N。
    if let Some(idx) = lower.find(" in the ") {
        let after_in = main[idx + 8..].trim();
        let l_after = after_in.to_lowercase();
        if let Some(end_m) = l_after.rfind(" match") {
            let mid = after_in[..end_m].trim().trim_end_matches('?').trim();
            if let Some(vs_dot) = mid.to_lowercase().find(" vs. ") {
                let t1 = mid[..vs_dot].trim();
                let t2 = mid[vs_dot + 5..].trim().trim_end_matches('?').trim();
                if !t1.is_empty() && !t2.is_empty() {
                    return Some((t1.to_string(), t2.to_string()));
                }
            }
            if let Some(vs_s) = mid.to_lowercase().find(" vs ") {
                let t1 = mid[..vs_s].trim();
                let t2 = mid[vs_s + 4..].trim().trim_end_matches('?').trim();
                if !t1.is_empty() && !t2.is_empty() {
                    return Some((t1.to_string(), t2.to_string()));
                }
            }
        }
    }

    // Texas at BYU Winner?（无 "win the"）
    if lower.contains(" at ") {
        let before_winner = if let Some(i) = lower.find(" winner") {
            main[..i].trim()
        } else {
            main.split('?').next().unwrap_or(main).trim()
        };
        if let Some(pos) = before_winner.to_lowercase().find(" at ") {
            let left = before_winner[..pos].trim();
            let right = before_winner[pos + 4..].trim();
            if !left.is_empty() && !right.is_empty() {
                return Some((left.to_string(), right.to_string()));
            }
        }
    }

    // "Will X win the Foo vs Bar : …" 或 "Lakers vs Celtics Winner?"
    let segment = if let Some(wt) = lower.find("win the ") {
        let after = &main[wt + 8..];
        let l_after = after.to_lowercase();
        let end = [after.find(':'), l_after.find(" round"), l_after.find(" match")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(after.len());
        after[..end].trim()
    } else {
        let end = lower.find(" winner").unwrap_or(main.len());
        main[..end].trim()
    };

    if let Some(vs_pos) = segment.to_lowercase().find(" vs ") {
        let t1 = segment[..vs_pos].trim();
        let rest = segment[vs_pos + 4..].trim();
        let t2 = rest.split(':').next().unwrap_or(rest).trim();
        if !t1.is_empty() && !t2.is_empty() {
            return Some((t1.to_string(), t2.to_string()));
        }
    }
    if let Some(vs_pos) = segment.to_lowercase().find(" vs.") {
        let t1 = segment[..vs_pos].trim();
        let rest = segment[vs_pos + 5..].trim();
        let t2 = rest.split(':').next().unwrap_or(rest).trim();
        if !t1.is_empty() && !t2.is_empty() {
            return Some((t1.to_string(), t2.to_string()));
        }
    }

    None
}

/// 若 Kalshi 明显是「两队胜负盘」但未能解析出双方，应拒配以免漏网
fn kalshi_head_to_head_pair_required(title: &str) -> bool {
    let l = title.to_lowercase();
    (l.contains("win the ") && (l.contains(" vs") || l.contains("vs.")))
        || (l.contains(" at ") && l.contains("winner"))
        || ((l.contains(" vs ") || l.contains(" vs.")) && l.contains("winner"))
}

fn two_team_sets_consistent(pm_a: &str, pm_b: &str, ks_a: &str, ks_b: &str) -> bool {
    // 先去掉「 - VCL EMEA: Group D」等尾部元数据，再 strip「Valorant:」类前缀。
    // 否则 PM 队2 含 `... - Foo: Bar` 时 `rfind(':')` 会误把队名收成 `Bar`（如 Group D），
    // 导致与 Kalshi 两队校验失败并落入默认 1Y1N，颠倒盘无法识别。
    let pm_a = FinalsConsistencyValidator::trim_team_suffix(pm_a);
    let pm_b = FinalsConsistencyValidator::trim_team_suffix(pm_b);
    let ks_a = FinalsConsistencyValidator::trim_team_suffix(ks_a);
    let ks_b = FinalsConsistencyValidator::trim_team_suffix(ks_b);
    let p1 = strip_team_event_prefix(&pm_a);
    let p2 = strip_team_event_prefix(&pm_b);
    let k1 = strip_team_event_prefix(&ks_a);
    let k2 = strip_team_event_prefix(&ks_b);
    (names_match(&p1, &k1) && names_match(&p2, &k2)) || (names_match(&p1, &k2) && names_match(&p2, &k1))
}

/// NCAA 等「打进某轮」命题 vs 单场 A at B Winner
pub struct BracketAdvanceVsSingleGameValidator;

impl BracketAdvanceVsSingleGameValidator {
    fn is_bracket_advance_proposition(title: &str) -> bool {
        let l = title.to_lowercase();
        let progress = l.contains("advance to")
            || l.contains("advance into")
            || l.contains("reach the")
            || l.contains("make the")
            || l.contains("make it to");
        let round = l.contains("sweet sixteen")
            || l.contains("sweet 16")
            || l.contains("final four")
            || l.contains("elite eight")
            || l.contains("elite 8")
            || l.contains("national championship");
        progress && round
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_adv = Self::is_bracket_advance_proposition(pm_title);
        let ks_adv = Self::is_bracket_advance_proposition(kalshi_title);
        if pm_adv && SportsSingleVsFinalsValidator::is_single_game_format(kalshi_title) {
            return false;
        }
        if ks_adv && SportsSingleVsFinalsValidator::is_single_game_format(pm_title) {
            return false;
        }
        true
    }
}

fn normalize_entity_name(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut last_space = false;

    for ch in text.to_lowercase().chars() {
        if ch.is_alphanumeric() {
            normalized.push(ch);
            last_space = false;
        } else if ch.is_whitespace() || ch == '-' || ch == '_' || ch == '\'' || ch == '’' {
            if !last_space {
                normalized.push(' ');
                last_space = true;
            }
        }
    }

    normalized.trim().to_string()
}

fn names_match(a: &str, b: &str) -> bool {
    let na = normalize_entity_name(a);
    let nb = normalize_entity_name(b);

    if na.is_empty() || nb.is_empty() {
        return false;
    }

    if na == nb {
        return true;
    }

    // Poly/Kalshi 队名差一个空格：「TheMongolz」vs「The Mongolz」归一后应一致
    let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let ca = compact(&na);
    let cb = compact(&nb);
    if ca.len() >= 4 && cb.len() >= 4 {
        if ca == cb {
            return true;
        }
        if ca.len() >= 5 && cb.len() >= 5 && (ca.contains(&cb) || cb.contains(&ca)) {
            return true;
        }
    }

    // 允许一个是另一个的完整子串（>=3 以支持 BYU 等简称）
    if na.len() >= 4 && nb.contains(&na) {
        return true;
    }
    if nb.len() >= 3 && na.contains(&nb) {
        return true;
    }

    false
}

/// ==================== 天气/温度市场验证器 ====================
/// 温度市场必须地区一致：maximum（无地区）不能与 特定地区（如 Hong Kong）匹配
pub struct WeatherValidator;

impl WeatherValidator {
    /// 是否像温度/天气市场
    pub fn is_temperature_market(title: &str) -> bool {
        let lower = title.to_lowercase();
        (lower.contains("temperature") || lower.contains("°") || lower.contains("°c") || lower.contains("°f"))
            && (lower.contains("highest") || lower.contains("maximum") || lower.contains("minimum")
                || lower.contains("high") || lower.contains("low"))
    }

    /// 提取标题中的地区（如 "in Hong Kong" -> "hong kong"）
    pub fn extract_region(title: &str) -> Option<String> {
        // 匹配 "in X" 或 "at X"，X 为地点名（字母、空格、连字符）
        let re = Regex::new(r"(?i)(?:in|at)\s+([A-Za-z][A-Za-z\s\-']{2,}?)(?:\s+(?:be|on|,|or|\?)|$)").ok()?;
        if let Some(caps) = re.captures(title) {
            let region = caps.get(1)?.as_str().trim().to_lowercase();
            if region.len() >= 2 && !region.chars().all(|c| c == ' ' || c == '-') {
                return Some(region);
            }
        }
        // 匹配 "X temperature" 中 X 为地名（句首）
        let re2 = Regex::new(r"^(?:Will\s+(?:the\s+)?(?:highest|maximum|minimum|high|low)\s+temperature\s+in\s+)([A-Za-z][A-Za-z\s\-']+?)\s+be").ok()?;
        if let Some(caps) = re2.captures(title) {
            let region = caps.get(1)?.as_str().trim().to_lowercase();
            if region.len() >= 2 {
                return Some(region);
            }
        }
        None
    }

    /// 地区一致才通过：一方有地区一方无，或两地不同则过滤
    pub fn regions_match(pm_title: &str, kalshi_title: &str) -> bool {
        if !Self::is_temperature_market(pm_title) || !Self::is_temperature_market(kalshi_title) {
            return true; // 非温度市场，不在此处过滤
        }
        let pm_region = Self::extract_region(pm_title).map(|s| normalize_entity_name(&s));
        let ks_region = Self::extract_region(kalshi_title).map(|s| normalize_entity_name(&s));
        match (&pm_region, &ks_region) {
            (Some(p), Some(k)) => names_match(p.as_str(), k.as_str()),
            (Some(_), None) | (None, Some(_)) => false, // 一方有地区一方无，不能匹配
            (None, None) => true,                        // 都无地区（generic）则放行
        }
    }

    /// 提取「N°F or below」或「N° or below」中的整数档位（高温上限类命题）。
    fn extract_or_below_fahrenheit_threshold(title: &str) -> Option<i32> {
        let re_f = Regex::new(r"(?i)(\d+)\s*°\s*F\s*or\s+below").ok()?;
        if let Some(c) = re_f.captures(title) {
            if let Ok(n) = c.get(1)?.as_str().parse::<i32>() {
                if (20..=120).contains(&n) {
                    return Some(n);
                }
            }
        }
        let re_plain = Regex::new(r"(?i)(\d+)\s*°\s*or\s+below").ok()?;
        if let Some(c) = re_plain.captures(title) {
            if let Ok(n) = c.get(1)?.as_str().parse::<i32>() {
                if (20..=120).contains(&n) {
                    return Some(n);
                }
            }
        }
        None
    }

    /// 两侧都能解析出「or below」华氏档位时，阈值必须相同（避免 PM 37°F 档 vs Kalshi 49°F 档）。
    pub fn fahrenheit_or_below_buckets_match(pm_title: &str, kalshi_title: &str) -> bool {
        if !Self::is_temperature_market(pm_title) || !Self::is_temperature_market(kalshi_title) {
            return true;
        }
        let pm = Self::extract_or_below_fahrenheit_threshold(pm_title);
        let ks = Self::extract_or_below_fahrenheit_threshold(kalshi_title);
        match (pm, ks) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

/// ==================== 电竞局数验证器 ====================
/// 电竞类比赛（LoL、CS 等）需匹配 Game N / Map N / Match N，局数一致才能过二筛
pub struct EsportsGameValidator;

impl EsportsGameValidator {
    /// 提取标题中的局数：Game 4, Map 2, Match 3 等
    pub fn extract_game_number(title: &str) -> Option<u32> {
        let re = Regex::new(r"(?i)(?:game|map|match)\s*(\d+)").ok()?;
        re.captures(title)?.get(1)?.as_str().parse::<u32>().ok()
    }

    /// 是否看起来像电竞对局（有 vs 且包含 game/map/match 局数）
    pub fn is_esports_style_match(title: &str) -> bool {
        let has_vs = title.contains(" vs ") || title.contains(" vs. ");
        let has_game_keyword = Regex::new(r"(?i)(?:game|map|match)\s*\d+")
            .map(|re| re.is_match(title))
            .unwrap_or(false);
        has_vs && has_game_keyword
    }

    /// 当两边都是电竞对局时，局数必须一致才通过
    pub fn game_numbers_match(pm_title: &str, kalshi_title: &str) -> bool {
        if !Self::is_esports_style_match(pm_title) || !Self::is_esports_style_match(kalshi_title) {
            return true; // 非电竞对局，不在此处过滤
        }
        let pm_num = Self::extract_game_number(pm_title);
        let ks_num = Self::extract_game_number(kalshi_title);
        match (pm_num, ks_num) {
            (Some(p), Some(k)) => p == k,
            (Some(_), None) | (None, Some(_)) => false, // 一方有局数一方无，无法确认
            (None, None) => true,                         // 都无局数则放行
        }
    }

    /// 单局胜者（Game 3 Winner / Will X win map 3）与总局数（over 4.5 maps）不能匹配
    pub fn is_single_game_winner(title: &str) -> bool {
        let has_vs = title.contains(" vs ") || title.contains(" vs. ");
        let has_winner = Regex::new(r"(?i)(?:game|map|match)\s*\d+\s*winner")
            .map(|re| re.is_match(title))
            .unwrap_or(false);
        let has_win_map = Regex::new(r"(?i)win\s+(?:map|game|match)\s+\d+|(?:map|game|match)\s+\d+.*win")
            .map(|re| re.is_match(title))
            .unwrap_or(false);
        has_vs && (has_winner || has_win_map)
    }

    /// 总局数市场（over X maps, under X maps, X.5 maps be played）
    pub fn is_total_maps_market(title: &str) -> bool {
        let lower = title.to_lowercase();
        lower.contains("maps")
            && (lower.contains("over") || lower.contains("under"))
            && Regex::new(r"\d+\.?\d*").map(|re| re.is_match(title)).unwrap_or(false)
    }

    /// 单局胜者与总局数市场不能一起匹配
    pub fn single_vs_total_match(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_single = Self::is_single_game_winner(pm_title);
        let pm_total = Self::is_total_maps_market(pm_title);
        let ks_single = Self::is_single_game_winner(kalshi_title);
        let ks_total = Self::is_total_maps_market(kalshi_title);
        // 若一方是单局胜者、另一方是总局数 -> 不匹配
        if (pm_single && ks_total) || (pm_total && ks_single) {
            return false;
        }
        // 网球：Total Sets O/U 与 win set N 不能匹配
        let pm_total_sets = Self::is_total_sets_market(pm_title);
        let ks_total_sets = Self::is_total_sets_market(kalshi_title);
        let pm_single_set = Self::is_single_set_winner(pm_title);
        let ks_single_set = Self::is_single_set_winner(kalshi_title);
        if (pm_total_sets && ks_single_set) || (pm_single_set && ks_total_sets) {
            return false;
        }
        true
    }

    fn is_handicap_style_title(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("handicap")
            || l.contains("让分")
            || l.contains("spread")
            || l.contains("map handicap")
    }

    /// Map/Game 让分盘 与 「总局 maps over/under」不能匹配
    pub fn handicap_vs_total_maps_match(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_h = Self::is_handicap_style_title(pm_title);
        let ks_h = Self::is_handicap_style_title(kalshi_title);
        let pm_total = Self::is_total_maps_market(pm_title);
        let ks_total = Self::is_total_maps_market(kalshi_title);
        if (pm_h && ks_total) || (ks_h && pm_total) {
            return false;
        }
        true
    }

    fn is_total_sets_market(title: &str) -> bool {
        let lower = title.to_lowercase();
        lower.contains("total sets") && (lower.contains("o/u") || lower.contains("over") || lower.contains("under"))
            && Regex::new(r"\d+\.?\d*").map(|re| re.is_match(title)).unwrap_or(false)
    }

    fn is_single_set_winner(title: &str) -> bool {
        Regex::new(r"(?i)win\s+set\s+\d+")
            .map(|re| re.is_match(title))
            .unwrap_or(false)
    }

    /// 系列/BO赛果（BO5、First Stand、Group）与单局胜者不能匹配
    pub fn is_series_or_bo_market(title: &str) -> bool {
        let lower = title.to_lowercase();
        let has_vs = title.contains(" vs ") || title.contains(" vs. ");
        let has_bo = Regex::new(r"(?i)\bbo\d+\b").map(|re| re.is_match(title)).unwrap_or(false)
            || lower.contains("(bo5)") || lower.contains("(bo3)");
        let has_series = lower.contains("first stand") || lower.contains("group ")
            || lower.contains("group a") || lower.contains("group b");
        has_vs && (has_bo || has_series)
    }

    /// 单局胜者与 BO5/系列赛果 不能匹配
    pub fn single_vs_series_match(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_single = Self::is_single_game_winner(pm_title);
        let pm_series = Self::is_series_or_bo_market(pm_title);
        let ks_single = Self::is_single_game_winner(kalshi_title);
        let ks_series = Self::is_series_or_bo_market(kalshi_title);
        if (pm_single && ks_series) || (pm_series && ks_single) {
            return false;
        }
        true
    }

    /// 是否单局/某局胜者（含 "Will X win map N" 格式，可无 vs）
    pub fn is_single_map_winner(title: &str) -> bool {
        let has_win_map = Regex::new(r"(?i)win\s+(?:map|game|match)\s+\d+")
            .map(|re| re.is_match(title))
            .unwrap_or(false);
        let has_map_winner = Regex::new(r"(?i)(?:game|map|match)\s*\d+\s*winner")
            .map(|re| re.is_match(title))
            .unwrap_or(false);
        has_win_map || has_map_winner
    }

    /// 是否「整场对局谁赢」（无 Map/Game 局号），Kalshi 常见句式：Will X win the ... match
    fn is_whole_match_winner(title: &str) -> bool {
        let lower = title.to_lowercase();
        if !(lower.contains(" vs ") || lower.contains(" vs.")) {
            return false;
        }
        if Self::is_single_map_winner(title) || Self::is_single_game_winner(title) {
            return false;
        }
        Regex::new(r"(?i)\bwin\b[^?]{0,160}\bmatch\b")
            .map(|re| re.is_match(title))
            .unwrap_or(false)
    }

    /// Map/Game N 胜者 与 整场赛果胜者 不能匹配（Kalshi 一侧常无局号，不会触发 game_numbers_match）
    pub fn map_winner_vs_whole_match_match(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_round = Self::is_single_map_winner(pm_title) || Self::is_single_game_winner(pm_title);
        let ks_round = Self::is_single_map_winner(kalshi_title) || Self::is_single_game_winner(kalshi_title);
        let pm_whole = Self::is_whole_match_winner(pm_title);
        let ks_whole = Self::is_whole_match_winner(kalshi_title);
        if (pm_round && ks_whole) || (ks_round && pm_whole) {
            return false;
        }
        true
    }
}

/// ==================== 平局 vs 胜负盘验证器 ====================
/// 任一侧为平局/和局命题，另一侧为 A vs B Winner 类胜负盘则剔除（避免落入默认数值路径误配）
pub struct DrawVsWinnerValidator;

/// 抛硬币 / Who wins the toss 与 正常胜负盘不能混配
pub struct TossVsMatchMarketValidator;

impl TossVsMatchMarketValidator {
    fn is_toss_proposition(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("who wins the toss")
            || l.contains("win the toss")
            || l.contains("wins the toss")
            || l.contains("coin toss")
            || l.contains("toss winner")
            || l.contains("winner of the toss")
            || (l.contains(" toss") && l.contains(" who "))
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_t = Self::is_toss_proposition(pm_title);
        let ks_t = Self::is_toss_proposition(kalshi_title);
        pm_t == ks_t
    }
}

impl DrawVsWinnerValidator {
    fn is_draw_or_tie_proposition(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("draw")
            || l.contains("end in a draw")
            || l.contains(" tie ")
            || l.contains("finish in a tie")
            || l.starts_with("tie ")
    }

    fn is_vs_winner_moneyline(title: &str) -> bool {
        let l = title.to_lowercase();
        (l.contains("winner") || title.contains("Winner"))
            && (title.contains(" vs ") || title.contains(" vs.") || l.contains(" at "))
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        if Self::is_draw_or_tie_proposition(pm_title) && Self::is_vs_winner_moneyline(kalshi_title) {
            return false;
        }
        if Self::is_draw_or_tie_proposition(kalshi_title) && Self::is_vs_winner_moneyline(pm_title) {
            return false;
        }
        true
    }
}

/// ==================== 确切比分 vs 总进球数 ====================
/// 「Exact Score 0-1」类与「Totals Over X goals」不是同一可对冲命题
pub struct ExactScoreVsGoalsTotalsValidator;

impl ExactScoreVsGoalsTotalsValidator {
    fn is_exact_score_market(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("exact score") || l.contains("correct score")
    }

    /// 足球等全场进球 Totals / O-U（显式含 goals）；排除电竞 maps 盘
    fn is_goals_totals_line(title: &str) -> bool {
        let l = title.to_lowercase();
        if l.contains("maps") || l.contains(" map ") {
            return false;
        }
        if !l.contains("goal") {
            return false;
        }
        let has_ou = l.contains("over ")
            || l.contains("under ")
            || l.contains("o/u")
            || l.contains("totals")
            || l.contains("total ");
        has_ou && Regex::new(r"\d").map(|re| re.is_match(title)).unwrap_or(false)
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_ex = Self::is_exact_score_market(pm_title);
        let ks_ex = Self::is_exact_score_market(kalshi_title);
        let pm_go = Self::is_goals_totals_line(pm_title);
        let ks_go = Self::is_goals_totals_line(kalshi_title);
        !((pm_ex && ks_go) || (ks_ex && pm_go))
    }
}

/// ==================== 电竞「Will X win 大赛」vs 传统体育进球/让分盘 ====================
/// 例：PM「Will Cloud9 New York win DreamHack Major 2?」误配 Kalshi「… over 2.5 goals?」或「… wins by over 1.5 Points?」
/// 仅靠队名/城市相似会高分，命题不同。要求 Will-win 侧含明确电竞语境，另一侧为 goals O/U 或净胜分 Points 盘，才拦截。
pub struct EsportsTournamentWinnerVsSportsGoalsValidator;

impl EsportsTournamentWinnerVsSportsGoalsValidator {
    /// Polymarket 常见：Will \<team\> win \<赛事\>…（无 vs 头对头主结构）
    fn is_will_side_win_event_proposition(title: &str) -> bool {
        let main = title.split(" - ").next().unwrap_or(title).trim();
        let lm = main.to_lowercase();
        if lm.contains(" vs ") || lm.contains(" vs.") {
            return false;
        }
        Regex::new(r"(?i)will\s+.+\s+win\s+")
            .map(|re| re.is_match(main))
            .unwrap_or(false)
    }

    fn has_esports_series_anchor(title: &str) -> bool {
        let l = title.to_lowercase();
        const ANCHORS: &[&str] = &[
            "dreamhack",
            "esl one",
            "esl pro",
            "esl challenge",
            "esl ",
            "blast",
            "iem ",
            " iem",
            " vct",
            "vct ",
            "valorant champions",
            "lck",
            "lec",
            "lcs",
            "lol worlds",
            "league of legends",
            " worlds 20",
            "the international",
            "dota 2",
            "dota2",
            "dota ",
            "counter-strike",
            "counter strike",
            "cs2",
            "cs:go",
            "cs go",
            "faceit",
            "pgl ",
            " pgl",
            "six invitational",
            "rainbow six",
            "rlcs",
            "rocket league",
            "overwatch",
            "fortnite",
            "pubg",
            "starcraft",
            "evo 20",
            "capcom cup",
            "tekken",
            "smash bros",
            "fighting games",
            "esports",
            "e-sports",
        ];
        ANCHORS.iter().any(|a| l.contains(a))
    }

    /// 传统体育净胜分/让分表述：「wins by over/under X.X Points」（与 Major/系列赛夺冠不同命题）
    fn is_wins_by_points_margin_line(title: &str) -> bool {
        Regex::new(r"(?i)wins?\s+by\s+(over|under)\s+\d+\.?\d*\s*points")
            .map(|re| re.is_match(title))
            .unwrap_or(false)
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        for (a, b) in [(pm_title, kalshi_title), (kalshi_title, pm_title)] {
            if Self::is_will_side_win_event_proposition(a)
                && Self::has_esports_series_anchor(a)
                && (ExactScoreVsGoalsTotalsValidator::is_goals_totals_line(b)
                    || Self::is_wins_by_points_margin_line(b))
            {
                return false;
            }
        }
        true
    }
}

/// ==================== 公开赛总冠军 vs 单场对阵 ====================
/// 「Will X win the WTA Miami Open?」与「Miami Open: A vs B」不能匹配
pub struct TournamentOutrightVsMatchValidator;

impl TournamentOutrightVsMatchValidator {
    /// Will … win the … ？且无对阵双方 vs；非「赢本场/该局」
    fn is_tournament_outright_winner(title: &str) -> bool {
        let main = title.split(" - ").next().unwrap_or(title).trim();
        let l = main.to_lowercase();
        if l.contains(" vs ") || l.contains(" vs.") {
            return false;
        }
        let win_the = Regex::new(r"(?i)will\s+.+\s+win\s+the\s+")
            .map(|re| re.is_match(main))
            .unwrap_or(false);
        if !win_the {
            return false;
        }
        if l.contains("win the match")
            || l.contains("win the game")
            || l.contains("win map ")
        {
            return false;
        }
        l.contains(" open")
            || l.contains("open?")
            || l.contains("wta ")
            || l.contains("atp ")
            || l.contains("masters")
            || l.contains("grand slam")
            || l.contains("indian wells")
            || l.contains("wimbledon")
            || l.contains("roland")
            || l.contains("french open")
            || l.contains("australian open")
            || l.contains("us open")
            || l.contains("miami open")
    }

    /// 带赛事前缀的单场对阵：「Miami Open: A vs B」「WTA …: A vs B」
    fn is_event_head_to_head_match(title: &str) -> bool {
        if !(title.contains(" vs ") || title.contains(" vs.")) {
            return false;
        }
        let l = title.to_lowercase();
        l.contains("open:")
            || l.contains("masters:")
            || Regex::new(r"(?i)\b(wta|atp)\b[^:]{0,120}:")
                .map(|re| re.is_match(title))
                .unwrap_or(false)
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_out = Self::is_tournament_outright_winner(pm_title);
        let ks_out = Self::is_tournament_outright_winner(kalshi_title);
        let pm_h2h = Self::is_event_head_to_head_match(pm_title);
        let ks_h2h = Self::is_event_head_to_head_match(kalshi_title);
        !((pm_h2h && ks_out) || (ks_h2h && pm_out))
    }
}

/// Team Top Batter / Top Batsman 等与「全场 A vs B 谁赢」不是同一命题
pub struct TeamSidePropVsMatchWinnerValidator;

impl TeamSidePropVsMatchWinnerValidator {
    fn is_top_batter_or_team_scorer_prop(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("top batter")
            || l.contains("top batsman")
            || l.contains("top bowler")
            || (l.contains("team top") && (l.contains("batter") || l.contains("batsman")))
    }

    /// 仅「对阵谁赢」类 Winner，已排除击球员道具
    fn is_plain_head_to_head_winner(title: &str) -> bool {
        let l = title.to_lowercase();
        if !l.contains("winner") {
            return false;
        }
        if Self::is_top_batter_or_team_scorer_prop(title) {
            return false;
        }
        l.contains(" vs ") || l.contains(" vs.") || l.contains(" at ")
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_prop = Self::is_top_batter_or_team_scorer_prop(pm_title);
        let ks_prop = Self::is_top_batter_or_team_scorer_prop(kalshi_title);
        let pm_plain = Self::is_plain_head_to_head_winner(pm_title);
        let ks_plain = Self::is_plain_head_to_head_winner(kalshi_title);
        !((pm_prop && ks_plain) || (ks_prop && pm_plain))
    }
}

/// ==================== 娱乐榜单验证器 ====================
/// Billboard #1 与 Top 10 等不同档位不能匹配
pub struct EntertainmentChartValidator;

impl EntertainmentChartValidator {
    fn looks_like_billboard_chart(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("billboard") || l.contains("hot 100") || l.contains("hot100")
    }

    fn looks_like_spotify_chart(title: &str) -> bool {
        title.to_lowercase().contains("spotify")
    }

    fn has_number_one_rank(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("#1")
            || l.contains("# 1")
            || l.contains("number one")
            || l.contains("no. 1")
            || l.contains("no 1 ")
    }

    fn has_top_ten_rank(title: &str) -> bool {
        let l = title.to_lowercase();
        l.contains("top 10") || l.contains("top10")
    }

    /// Billboard/Hot100 与 Spotify 混配（用于二筛提示语）
    pub(crate) fn is_billboard_spotify_cross(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_bb = Self::looks_like_billboard_chart(pm_title);
        let ks_bb = Self::looks_like_billboard_chart(kalshi_title);
        let pm_spotify = Self::looks_like_spotify_chart(pm_title);
        let ks_spotify = Self::looks_like_spotify_chart(kalshi_title);
        (pm_spotify && ks_bb) || (pm_bb && ks_spotify)
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_bb = Self::looks_like_billboard_chart(pm_title);
        let ks_bb = Self::looks_like_billboard_chart(kalshi_title);
        let pm_spotify = Self::looks_like_spotify_chart(pm_title);
        let ks_spotify = Self::looks_like_spotify_chart(kalshi_title);
        // 榜单数据源须一致：Spotify #1 与 Billboard Hot 100 等不能互配
        if (pm_spotify && ks_bb) || (pm_bb && ks_spotify) {
            return false;
        }

        if !pm_bb || !ks_bb {
            return true;
        }
        let pm_one = Self::has_number_one_rank(pm_title);
        let ks_one = Self::has_number_one_rank(kalshi_title);
        let pm_t10 = Self::has_top_ten_rank(pm_title);
        let ks_t10 = Self::has_top_ten_rank(kalshi_title);
        if (pm_one && ks_t10) || (ks_one && pm_t10) {
            return false;
        }
        true
    }
}

/// ==================== 让分 vs 单局胜者验证器 ====================
/// 让分盘口（Handicap）与单纯的某局胜者（win map N）不能匹配
pub struct HandicapVsSingleWinnerValidator;

impl HandicapVsSingleWinnerValidator {
    pub fn is_handicap_market(title: &str) -> bool {
        let lower = title.to_lowercase();
        lower.contains("handicap") || lower.contains("让分") || lower.contains("spread")
    }

    /// 让分盘对面的「整场 moneyline」：`A at B Winner?` 或 `Will X win … vs … match?`（可无 Winner 字眼）
    fn is_moneyline_winner_proposition(title: &str) -> bool {
        let l = title.to_lowercase();
        if l.contains(" at ") && (l.contains("winner") || title.contains("Winner")) {
            return true;
        }
        Regex::new(r"(?i)\bwin\b[^?]{0,160}\bmatch\b")
            .map(|re| re.is_match(title))
            .unwrap_or(false)
    }

    pub fn handicap_vs_single_winner_match(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_handicap = Self::is_handicap_market(pm_title);
        let ks_handicap = Self::is_handicap_market(kalshi_title);
        let pm_single = EsportsGameValidator::is_single_map_winner(pm_title);
        let ks_single = EsportsGameValidator::is_single_map_winner(kalshi_title);
        if (pm_handicap && ks_single) || (ks_handicap && pm_single) {
            return false;
        }
        // 让分 vs 单场胜负盘（非仅 Map 局胜）
        let pm_ml = Self::is_moneyline_winner_proposition(pm_title);
        let ks_ml = Self::is_moneyline_winner_proposition(kalshi_title);
        if (pm_handicap && ks_ml) || (ks_handicap && pm_ml) {
            return false;
        }
        true
    }
}

/// ==================== 决赛一致性验证器 ====================
/// 一方有 Finals 另一方没有时，必须双方都包含相同两队才允许匹配，否则剔除
pub struct FinalsConsistencyValidator;

impl FinalsConsistencyValidator {
    fn has_finals_keyword(title: &str) -> bool {
        let lower = title.to_lowercase();
        lower.contains("finals") || lower.contains("championship") || lower.contains("conference finals")
    }

    pub(crate) fn trim_team_suffix(s: &str) -> String {
        let s = if let Some(i) = s.find(" Winner") { s[..i].trim_end() } else { s };
        let s = if let Some(i) = s.find(" - ") { s[..i].trim_end() } else { s };
        s.trim().to_string()
    }

    /// 从 "A vs B" 格式提取两队，去掉 " Winner? - X"、"- NBA Finals" 等后缀
    fn extract_teams_cleaned(title: &str) -> Option<(String, String)> {
        let (t1, t2) = extract_teams(title)?;
        Some((Self::trim_team_suffix(&t1), Self::trim_team_suffix(&t2)))
    }

    pub fn finals_consistency_match(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_finals = Self::has_finals_keyword(pm_title);
        let ks_finals = Self::has_finals_keyword(kalshi_title);
        if pm_finals == ks_finals {
            return true;
        }
        // 一方有 Finals 一方没有：必须双方都有 " vs " 且两队一致
        let pm_teams = match Self::extract_teams_cleaned(pm_title) {
            Some(t) => t,
            None => return false,
        };
        let ks_teams = match Self::extract_teams_cleaned(kalshi_title) {
            Some(t) => t,
            None => return false,
        };
        let (pm_a, pm_b) = pm_teams;
        let (ks_a, ks_b) = ks_teams;
        (names_match(&pm_a, &ks_a) && names_match(&pm_b, &ks_b))
            || (names_match(&pm_a, &ks_b) && names_match(&pm_b, &ks_a))
    }
}

/// ==================== 体育单场 vs 决赛验证器 ====================
/// 某场比赛（X at Y）与某决赛（X win Finals）不能匹配，除非确定决赛就是这两队
pub struct SportsSingleVsFinalsValidator;

impl SportsSingleVsFinalsValidator {
    /// 是否像单场比赛（A at B 格式，通常为常规赛单场）
    pub fn is_single_game_format(title: &str) -> bool {
        let lower = title.to_lowercase();
        (lower.contains(" at ") && (lower.contains("winner") || lower.contains(" - ")))
            || Regex::new(r"(?i)^[A-Za-z0-9\s]+at\s+[A-Za-z0-9\s]+")
                .map(|re| re.is_match(title))
                .unwrap_or(false)
    }

    /// 是否像决赛/系列赛（Finals, Championship, Conference Finals）
    pub fn is_finals_format(title: &str) -> bool {
        let lower = title.to_lowercase();
        lower.contains("finals") || lower.contains("championship") || lower.contains("conference finals")
    }

    /// 单场比赛与决赛不能匹配（除非能确认决赛就是这两队，此处保守过滤）
    pub fn single_vs_finals_match(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_single = Self::is_single_game_format(pm_title);
        let pm_finals = Self::is_finals_format(pm_title);
        let ks_single = Self::is_single_game_format(kalshi_title);
        let ks_finals = Self::is_finals_format(kalshi_title);
        if (pm_single && ks_finals) || (pm_finals && ks_single) {
            return false;
        }
        true
    }
}

/// ==================== 垃圾市场检测 ====================
pub struct GarbageMarketDetector;

impl GarbageMarketDetector {
    pub fn is_garbage_sports_market(title: &str) -> bool {
        let lower = title.to_lowercase();
        
        // 硬规则: 无对阵双方的「O/U … Rounds」裸盘一律垃圾（不靠大写数量放行）
        if lower.contains("o/u") && lower.contains("rounds") {
            let has_matchup = lower.contains(" vs ")
                || lower.contains(" vs.")
                || lower.contains(" at ");
            if !has_matchup {
                return true;
            }
        }
        
        // 检查垃圾关键词
        let has_garbage = GARBAGE_KEYWORDS.iter().any(|&kw| lower.contains(kw));
        if has_garbage {
            let upper_count = title.chars().filter(|c| c.is_uppercase()).count();
            let has_specific = lower.contains(" vs ") || 
                               lower.contains(" at ") ||
                               lower.contains(" - ") ||
                               upper_count > 1;
            
            if !has_specific {
                let numbers: Vec<f64> = NumberComparator::extract_numbers(title)
                    .into_iter()
                    .map(|n| n.value)
                    .collect();
                
                if !numbers.is_empty() {
                    return true;
                }
            }
        }
        
        false
    }
}

/// ==================== 胜负市场验证器 ====================
pub struct WinnerMarketValidator;

impl WinnerMarketValidator {
    pub fn validate(pm_title: &str, kalshi_title: &str) -> Option<(String, String, bool)> {
        // 总进球数 O/U 与胜负 Winner 不能匹配
        if pm_title.to_lowercase().contains("o/u") {
            return None;
        }
        // 平手 Draw/Tie 与胜负 Winner 不能匹配
        let pm_lower = pm_title.to_lowercase();
        if pm_lower.contains("draw") || pm_lower.contains("end in a draw") || pm_lower.contains(" tie ") {
            return None;
        }
        // 检查是否都是胜负市场
        let pm_is_winner = pm_title.contains(" vs ") || pm_title.contains(" vs. ");
        let ks_is_winner = kalshi_title.contains("Winner") || kalshi_title.contains(" - ");
        
        if !pm_is_winner || !ks_is_winner {
            return None;
        }
        
        let (pm_team1, pm_team2) = extract_teams(pm_title)?;
        let ks_winner = extract_winner(kalshi_title);
        
        if ks_winner.is_empty() {
            return None;
        }

        // Kalshi 若含明确两队对阵，必须与 PM 的双方一致（防「同选手、不同场次」）
        match extract_kalshi_moneyline_pair(kalshi_title) {
            Some((k1, k2)) => {
                if !two_team_sets_consistent(&pm_team1, &pm_team2, &k1, &k2) {
                    return None;
                }
            }
            None => {
                if kalshi_head_to_head_pair_required(kalshi_title) {
                    return None;
                }
            }
        }
        
        // 套利必须覆盖两队。颠倒=2Y/2N，非颠倒=1Y1N（队名须先 trim 尾部「 - 分组」再去赛事前缀）
        let pt1 = strip_team_event_prefix(&FinalsConsistencyValidator::trim_team_suffix(&pm_team1));
        let pt2 = strip_team_event_prefix(&FinalsConsistencyValidator::trim_team_suffix(&pm_team2));
        if names_match(&pt1, &ks_winner) {
            // Kalshi 问队1胜：PM Yes(队1) + Kalshi No(队2)，1Y1N 无颠倒
            Some(("YES".to_string(), "NO".to_string(), false))
        } else if names_match(&pt2, &ks_winner) {
            // Kalshi 问队2胜：PM Yes(队2) + Kalshi Yes(队2)，2Y 为颠倒
            Some(("YES".to_string(), "YES".to_string(), true))
        } else {
            None
        }
    }
}

/// ==================== 技术统计市场验证器 ====================
/// 体育球员技术统计（得分/助攻/篮板/三分）必须类型一致
/// O/U 5.5 Over 等价于 6+：同向，1Y1N，无颠倒
/// O/U 6.5 Over 与 6- 为颠倒市场：PM Y=>=7，Kalshi Y=<=6，需 (YES,YES)
pub struct StatMarketValidator;

/// 统计类型归一化（points/rebounds/assists/threes 互斥）
fn normalize_stat_type(s: &str) -> Option<&'static str> {
    let lower = s.to_lowercase();
    if lower.contains("points") && !lower.contains("rebounds") && !lower.contains("assists") {
        return Some("points");
    }
    if lower.contains("rebounds") {
        return Some("rebounds");
    }
    if lower.contains("assists") {
        return Some("assists");
    }
    if lower.contains("three") || lower.contains("threes") {
        return Some("threes");
    }
    None
}

impl StatMarketValidator {
    /// PM 格式: "Player: Rebounds O/U 4.5" 或 "Player: Assists O/U 5.5" 或 "Points Under 20.5"
    fn extract_pm_stat(title: &str) -> Option<(&'static str, f64, bool)> {
        let lower = title.to_lowercase();
        let has_ou = title.contains("O/U") || lower.contains(" over ") || lower.contains(" under ");
        if !has_ou {
            return None;
        }
        let stat = normalize_stat_type(title)?;
        let num = extract_number(title)?;
        let is_over = !lower.contains("under");
        Some((stat, num, is_over))
    }

    /// Kalshi 格式: "Player: 4+ assists" 或 "Player: 6+ rebounds - Player: 6+"
    fn extract_ks_stat(title: &str) -> Option<(&'static str, f64, bool)> {
        let re_plus = Regex::new(r"(?i)(\d+\.?\d*)\s*\+\s*([a-z]+)").ok()?;
        if let Some(caps) = re_plus.captures(title) {
            let num: f64 = caps.get(1)?.as_str().parse().ok()?;
            let word = caps.get(2)?.as_str();
            let stat = normalize_stat_type(word).or_else(|| normalize_stat_type(title))?;
            return Some((stat, num, true));
        }
        let re_minus = Regex::new(r"(?i)(\d+\.?\d*)\s*-\s*([a-z]+)").ok()?;
        if let Some(caps) = re_minus.captures(title) {
            let num: f64 = caps.get(1)?.as_str().parse().ok()?;
            let word = caps.get(2)?.as_str();
            let stat = normalize_stat_type(word).or_else(|| normalize_stat_type(title))?;
            return Some((stat, num, false));
        }
        None
    }

    /// 双方均为技术统计市场
    pub fn is_stat_market_pair(pm_title: &str, kalshi_title: &str) -> bool {
        Self::extract_pm_stat(pm_title).is_some() && Self::extract_ks_stat(kalshi_title).is_some()
    }

    /// 验证：统计类型一致且阈值匹配
    /// 同向（Over+Plus 或 Under+Minus）：等价市场，1Y1N 无颠倒
    /// 颠倒（Over+Minus 或 Under+Plus）：相反市场，需 (YES,YES)
    pub fn validate(pm_title: &str, kalshi_title: &str) -> Option<(String, String, bool)> {
        let (pm_stat, pm_num, pm_is_over) = Self::extract_pm_stat(pm_title)?;
        let (ks_stat, ks_num, ks_is_plus) = Self::extract_ks_stat(kalshi_title)?;

        if pm_stat != ks_stat {
            return None;
        }

        let pm_threshold = if pm_is_over { pm_num.ceil() as i32 } else { pm_num.floor() as i32 };
        let ks_ceil = ks_num.ceil() as i32;
        let ks_floor = ks_num.floor() as i32;

        if ks_is_plus {
            // Kalshi N+ = Over
            if pm_is_over {
                // 同向：O/U 5.5 Over ↔ 6+，等价，1Y1N 无颠倒
                if pm_threshold == ks_ceil {
                    return Some(("YES".to_string(), "NO".to_string(), false));
                }
            } else {
                // 颠倒：PM Under(<=5) + Kalshi 6+(>=6)，Y/Y 覆盖
                if pm_threshold + 1 == ks_ceil {
                    return Some(("YES".to_string(), "YES".to_string(), true));
                }
            }
        } else {
            // Kalshi N- = Under
            if !pm_is_over {
                // 同向：O/U 6.5 Under ↔ 6-，等价，1Y1N 无颠倒
                if pm_threshold == ks_floor {
                    return Some(("YES".to_string(), "NO".to_string(), false));
                }
            } else {
                // 颠倒：PM Over(>=7) + Kalshi 6-(<=6)，Y/Y 覆盖
                if pm_threshold == ks_floor + 1 {
                    return Some(("YES".to_string(), "YES".to_string(), true));
                }
            }
        }
        None
    }
}

/// ==================== 得分市场验证器 ====================
pub struct ScoreMarketValidator;

impl ScoreMarketValidator {
    /// 仅处理 Points 得分市场；Rebounds/Assists/Threes 等由 StatMarketValidator 处理
    pub fn validate(pm_title: &str, kalshi_title: &str) -> Option<(String, String, bool)> {
        // 若双方均为技术统计市场，交给 StatMarketValidator（避免 rebounds 与 assists 误配）
        if StatMarketValidator::is_stat_market_pair(pm_title, kalshi_title) {
            return None;
        }
        // 检查是否都是得分市场（Points）
        let pm_is_score = pm_title.contains("O/U") || pm_title.contains("Points");
        let ks_is_score = kalshi_title.contains('+') || kalshi_title.contains('-') ||
                          kalshi_title.contains("points");

        if !pm_is_score || !ks_is_score {
            return None;
        }
        
        let pm_num = match extract_number(pm_title) {
            Some(n) => n,
            None => return None,
        };
        
        let ks_num = match extract_number(kalshi_title) {
            Some(n) => n,
            None => return None,
        };
        
        // 判断方向：同向等价无颠倒，颠倒需 (YES,YES)
        let pm_is_over = !pm_title.to_lowercase().contains("under");
        
        let ks_is_plus = Regex::new(r"\b\d+(\.\d+)?\s*\+").ok()?.is_match(kalshi_title);
        let ks_is_minus = Regex::new(r"\b\d+(\.\d+)?\s*-").ok()?.is_match(kalshi_title);
        
        let pm_threshold = if pm_is_over { pm_num.ceil() as i32 } else { pm_num.floor() as i32 };
        let ks_ceil = ks_num.ceil() as i32;
        let ks_floor = ks_num.floor() as i32;

        if ks_is_plus {
            if pm_is_over {
                // 同向：Over + N+，等价，1Y1N 无颠倒
                if pm_threshold == ks_ceil {
                    return Some(("YES".to_string(), "NO".to_string(), false));
                }
            } else {
                // 颠倒：Under + N+，Y/Y 覆盖
                if pm_threshold + 1 == ks_ceil {
                    return Some(("YES".to_string(), "YES".to_string(), true));
                }
            }
        } else if ks_is_minus {
            if !pm_is_over {
                // 同向：Under + N-，等价，1Y1N 无颠倒
                if pm_threshold == ks_floor {
                    return Some(("YES".to_string(), "NO".to_string(), false));
                }
            } else {
                // 颠倒：Over + N-，Y/Y 覆盖
                if pm_threshold == ks_floor + 1 {
                    return Some(("YES".to_string(), "YES".to_string(), true));
                }
            }
        } else {
            // 默认按同向处理
            let ks_threshold = if pm_is_over { ks_ceil } else { ks_floor };
            if pm_threshold == ks_threshold {
                return Some(("YES".to_string(), "NO".to_string(), false));
            }
        }
        
        None
    }
}

/// ==================== 日期验证器 ====================
pub struct DateValidator;

impl DateValidator {
    pub fn new() -> Self {
        Self
    }
    
    pub fn extract_date(text: &str) -> Option<DateInfo> {
        let re = Regex::new(r"(?i)(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec|january|february|march|april|may|june|july|august|september|october|november|december)\s+(\d{1,2})(?:,?\s*(\d{4}))?").ok()?;
        
        if let Some(caps) = re.captures(text) {
            let month_name = caps.get(1)?.as_str();
            let day = caps.get(2)?.as_str().parse::<u32>().ok()?;
            
            let month = MONTH_MAP.iter()
                .find(|(name, _)| month_name.to_lowercase().contains(name))
                .map(|(_, m)| *m)?;
            
            let year = caps.get(3).and_then(|y| y.as_str().parse::<u32>().ok());
            
            return Some(DateInfo {
                month,
                day,
                has_year: year.is_some(),
                year,
            });
        }
        
        let year_re = Regex::new(r"\b(20\d{2})\b").ok()?;
        if let Some(caps) = year_re.captures(text) {
            let year = caps.get(1)?.as_str().parse::<u32>().ok()?;
            return Some(DateInfo {
                month: 0,
                day: 0,
                has_year: true,
                year: Some(year),
            });
        }
        
        None
    }
    
    pub fn has_safe_word(text: &str) -> bool {
        let text_lower = text.to_lowercase();
        SAFE_WORDS.iter().any(|&w| text_lower.contains(w))
    }
    
    pub fn dates_match(d1: &DateInfo, d2: &DateInfo) -> bool {
        if d1.month > 0 && d1.day > 0 && d2.month > 0 && d2.day > 0 {
            return d1.month == d2.month && d1.day == d2.day;
        }
        false
    }
    
    pub fn validate(&self, pm_title: &str, kalshi_title: &str) -> bool {
        let pm_date = Self::extract_date(pm_title);
        let kalshi_date = Self::extract_date(kalshi_title);
        
        match (pm_date, kalshi_date) {
            (Some(pm), Some(ks)) => Self::dates_match(&pm, &ks),
            (Some(_), None) => Self::has_safe_word(pm_title),
            (None, Some(_)) => Self::has_safe_word(kalshi_title),
            (None, None) => true,
        }
    }
}

/// ==================== 体育比分识别器 ====================
pub struct SportsIdentifier;

impl SportsIdentifier {
    pub fn new() -> Self {
        Self
    }
    
    pub fn is_sports_market(title: &str) -> bool {
        let title_lower = title.to_lowercase();
        SPORTS_KEYWORDS.iter().any(|&kw| title_lower.contains(kw))
    }
}

/// ==================== 数值比较器 ====================
pub struct NumberComparator;

impl NumberComparator {
    pub fn new() -> Self {
        Self
    }
    
    pub fn extract_numbers(text: &str) -> Vec<NumberInfo> {
        let mut numbers = Vec::new();
        let re = Regex::new(r"(\d+\.?\d*)").unwrap();
        
        for cap in re.captures_iter(text) {
            if let Ok(value) = cap[1].parse::<f64>() {
                let is_year = value >= 2000.0 && value < 2100.0;
                let context = text.to_string();
                
                numbers.push(NumberInfo {
                    value,
                    context,
                    is_year,
                });
            }
        }
        
        numbers
    }
    
    pub fn compare_numbers(nums1: &[NumberInfo], nums2: &[NumberInfo]) -> bool {
        if nums1.is_empty() != nums2.is_empty() {
            return false;
        }
        
        if nums1.is_empty() && nums2.is_empty() {
            return true;
        }
        
        for n1 in nums1 {
            for n2 in nums2 {
                if n1.is_year != n2.is_year {
                    continue;
                }
                if (n1.value - n2.value).abs() <= 0.5 {
                    return true;
                }
            }
        }
        
        false
    }
}

/// ==================== 选举/政治命题：政党赢席 vs 候选人提名、名次 vs 获胜 ====================
/// 剔除向量相似但兑付条件不同的高误配（如 PM 大选党赢席 vs Kalshi 初选提名；「赢」vs「第二名」）。
pub struct ElectoralPropositionValidator;

impl ElectoralPropositionValidator {
    fn looks_political_election_context(l: &str) -> bool {
        l.contains("house seat")
            || l.contains("u.s. house")
            || l.contains("us house")
            || (l.contains("senate") && (l.contains("seat") || l.contains("race") || l.contains("election")))
            || l.contains("congressional district")
            || l.contains("congressional ")
            || l.contains("special election")
            || l.contains("governor")
            || l.contains("mayor")
            || l.contains("presidential")
            || l.contains("primary")
            || l.contains("nominee")
            || l.contains("nomination")
            || l.contains("democratic party")
            || l.contains("republican party")
            || l.contains("the gop")
            || l.contains(" gop ")
            || ELECTORAL_STATE_DISTRICT_RE.is_match(l)
    }

    /// 如：Democratic Party win the WV-02 House seat（非提名）
    fn is_party_wins_seat_proposition(l: &str) -> bool {
        if l.contains("nominee") || l.contains("nomination") {
            return false;
        }
        let has_party = l.contains("democratic party")
            || l.contains("republican party")
            || l.contains("the gop")
            || l.contains(" gop ");
        let has_seat = l.contains("house seat")
            || l.contains("congressional")
            || (l.contains("senate") && l.contains("seat"));
        let has_win = l.contains("win");
        has_party && has_seat && has_win
    }

    /// 如：Democratic nominee for WV-02
    fn is_candidate_nominee_proposition(l: &str) -> bool {
        l.contains("nominee") || l.contains("nomination for") || l.contains(" nomination")
    }

    /// 明确名次/第二名等（与单纯「获胜」不同兑付）
    fn has_explicit_placement_or_rank(l: &str) -> bool {
        if l.contains("finish 2nd")
            || l.contains("finish second")
            || l.contains("finishes 2nd")
            || l.contains("finishes second")
            || l.contains("2nd place")
            || l.contains("second place")
            || l.contains("finish 3rd")
            || l.contains("finish third")
            || l.contains("3rd place")
            || l.contains("third place")
            || l.contains("runner-up")
            || l.contains("runner up")
            || l.contains("comes in second")
            || l.contains("come in second")
        {
            return true;
        }
        ELECTORAL_NTH_PLACE_RE.is_match(l)
    }

    pub fn allows_pair(pm_title: &str, kalshi_title: &str) -> bool {
        let pm_l = pm_title.to_lowercase();
        let ks_l = kalshi_title.to_lowercase();

        if !Self::looks_political_election_context(&pm_l) || !Self::looks_political_election_context(&ks_l) {
            return true;
        }

        let pm_party_seat = Self::is_party_wins_seat_proposition(&pm_l);
        let ks_party_seat = Self::is_party_wins_seat_proposition(&ks_l);
        let pm_nom = Self::is_candidate_nominee_proposition(&pm_l);
        let ks_nom = Self::is_candidate_nominee_proposition(&ks_l);
        if (pm_party_seat && ks_nom) || (ks_party_seat && pm_nom) {
            return false;
        }

        let pm_rank = Self::has_explicit_placement_or_rank(&pm_l);
        let ks_rank = Self::has_explicit_placement_or_rank(&ks_l);
        if pm_rank != ks_rank {
            return false;
        }

        true
    }
}

/// ==================== 主验证管道 ====================
pub struct ValidationPipeline {
    date_validator: DateValidator,
    pub filtered_count: usize,
    pub filtered_samples: Vec<(String, String, String)>,
    pub retained_samples: HashMap<String, Vec<RetainedSample>>,
}

impl ValidationPipeline {
    pub fn new() -> Self {
        Self {
            date_validator: DateValidator::new(),
            filtered_count: 0,
            filtered_samples: Vec::new(),
            retained_samples: HashMap::new(),
        }
    }

    pub fn validate(&mut self, pm_title: &str, kalshi_title: &str, similarity: f64, category: &str) -> Option<MatchInfo> {
        // 0. 垃圾市场检测
        if GarbageMarketDetector::is_garbage_sports_market(pm_title) ||
           GarbageMarketDetector::is_garbage_sports_market(kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "垃圾市场");
            return None;
        }
        
        // 1. 日期验证
        if !self.date_validator.validate(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "日期不匹配");
            return None;
        }

        // 1.1 温度市场：地区必须一致，maximum（无地区）不能与 特定地区 匹配
        if !WeatherValidator::regions_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "天气地区不匹配");
            return None;
        }
        if !WeatherValidator::fahrenheit_or_below_buckets_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "温度档位(°For below)不一致");
            return None;
        }

        // 1.1b 娱乐榜单：Billboard #1 与 Top 10 不能匹配
        if EntertainmentChartValidator::is_billboard_spotify_cross(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "娱乐榜单来源不一致(Billboard与Spotify)");
            return None;
        }
        if !EntertainmentChartValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "娱乐榜单#1与Top10不能匹配");
            return None;
        }

        // 1.2 电竞对局：局数必须一致；单局胜者与总局数市场不能匹配
        if !EsportsGameValidator::game_numbers_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "电竞局数不匹配");
            return None;
        }
        if !EsportsGameValidator::single_vs_total_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "电竞单局与总局数不能匹配");
            return None;
        }
        if !EsportsGameValidator::handicap_vs_total_maps_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "让分盘与总局maps盘不能匹配");
            return None;
        }
        if !EsportsGameValidator::single_vs_series_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "电竞单局与BO5/系列赛不能匹配");
            return None;
        }
        if !EsportsGameValidator::map_winner_vs_whole_match_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "电竞Map局胜者与整场赛果不能匹配");
            return None;
        }

        // 1.3 体育：单场比赛与决赛不能匹配
        if !SportsSingleVsFinalsValidator::single_vs_finals_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "体育单场与决赛不能匹配");
            return None;
        }

        // 1.4 让分盘口与某局胜者不能匹配
        if !HandicapVsSingleWinnerValidator::handicap_vs_single_winner_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "让分盘口与某局胜者不能匹配");
            return None;
        }

        // 1.4b 确切比分 vs 总进球数 Totals
        if !ExactScoreVsGoalsTotalsValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "确切比分与总进球数不能匹配");
            return None;
        }

        // 1.4b2 电竞 Will X win 系列赛事 vs 传统体育进球 O/U（队名相似易误配）
        if !EsportsTournamentWinnerVsSportsGoalsValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(
                pm_title,
                kalshi_title,
                "电竞赛事夺冠命题与体育进球Totals/净胜分Points盘不能匹配",
            );
            return None;
        }

        // 1.4c 公开赛/巡回赛总冠军 vs 带赛事前缀的单场对阵
        if !TournamentOutrightVsMatchValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "公开赛总冠军与单场对阵不能匹配");
            return None;
        }

        // 1.4d Team Top Batter 等与全场胜负盘
        if !TeamSidePropVsMatchWinnerValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "队内最佳击球员等与全场胜负盘不能匹配");
            return None;
        }

        // 1.5 决赛一致性：一方有 Finals 另一方没有时，必须双方都含相同两队，否则剔除
        if !FinalsConsistencyValidator::finals_consistency_match(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "决赛不一致（一方有Finals另一方无且非同一两队）");
            return None;
        }

        // 1.6 平局与胜负盘不能匹配（避免落入默认数值比较误配）
        if !DrawVsWinnerValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "平局市场与胜负盘不能匹配");
            return None;
        }

        // 1.7 锦标赛「打进某轮」vs 单场 A at B Winner 不能匹配
        if !BracketAdvanceVsSingleGameValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "锦标赛晋级命题与单场胜负不能匹配");
            return None;
        }

        // 1.8 抛硬币/掷币 vs 非掷币赛果不能混配
        if !TossVsMatchMarketValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "抛硬币/掷币与赛果命题不一致");
            return None;
        }

        // 1.9 选举命题：政党赢席 vs 初选提名；名次(如第二名) vs 单纯获胜
        if !ElectoralPropositionValidator::allows_pair(pm_title, kalshi_title) {
            self.record_filter(pm_title, kalshi_title, "选举命题类型不一致(党席/提名或名次/获胜)");
            return None;
        }

        // 2. 尝试胜负市场匹配
        if let Some((pm_side, kalshi_side, needs_inversion)) = WinnerMarketValidator::validate(pm_title, kalshi_title) {
            let match_info = MatchInfo {
                pm_title: pm_title.to_string(),
                kalshi_title: kalshi_title.to_string(),
                similarity,
                category: category.to_string(),
                pm_side,
                kalshi_side,
                needs_inversion,
            };
            self.record_retained(&match_info);
            return Some(match_info);
        }
        
        // 3. 尝试得分市场匹配
        if let Some((pm_side, kalshi_side, needs_inversion)) = ScoreMarketValidator::validate(pm_title, kalshi_title) {
            let match_info = MatchInfo {
                pm_title: pm_title.to_string(),
                kalshi_title: kalshi_title.to_string(),
                similarity,
                category: category.to_string(),
                pm_side,
                kalshi_side,
                needs_inversion,
            };
            self.record_retained(&match_info);
            return Some(match_info);
        }

        // 3.5 技术统计市场：双方均为 Stat 时，必须类型一致且阈值严格匹配（O/U 5.5 Over ↔ 6+）
        if StatMarketValidator::is_stat_market_pair(pm_title, kalshi_title) {
            if let Some((pm_side, kalshi_side, needs_inversion)) = StatMarketValidator::validate(pm_title, kalshi_title) {
                let match_info = MatchInfo {
                    pm_title: pm_title.to_string(),
                    kalshi_title: kalshi_title.to_string(),
                    similarity,
                    category: category.to_string(),
                    pm_side,
                    kalshi_side,
                    needs_inversion,
                };
                self.record_retained(&match_info);
                return Some(match_info);
            } else {
                self.record_filter(pm_title, kalshi_title, "技术统计类型或阈值不匹配");
                return None;
            }
        }

        // 4. 默认数值比较（禁止：双方均为电竞 Map/局胜者却未通过胜负校验时走 1Y1N）
        let pm_map_winner_line = EsportsGameValidator::is_single_map_winner(pm_title)
            || EsportsGameValidator::is_single_game_winner(pm_title);
        let ks_map_winner_line = EsportsGameValidator::is_single_map_winner(kalshi_title);
        if pm_map_winner_line && ks_map_winner_line {
            self.record_filter(pm_title, kalshi_title, "电竞Map局胜者须通过胜负与两队校验");
            return None;
        }

        let pm_numbers = NumberComparator::extract_numbers(pm_title);
        let kalshi_numbers = NumberComparator::extract_numbers(kalshi_title);

        // 双侧都无数字锚点时不再放行默认 YES/NO（易误配晋级命题等）
        if pm_numbers.is_empty() && kalshi_numbers.is_empty() {
            self.record_filter(pm_title, kalshi_title, "默认路径需至少一侧有锚点数字");
            return None;
        }
        
        if !NumberComparator::compare_numbers(&pm_numbers, &kalshi_numbers) {
            self.record_filter(pm_title, kalshi_title, "数值不匹配");
            return None;
        }
        
        // 默认：无法确定时采用 1Y1N（同问法则买相反边），非颠倒市场不应出现双 YES
        let match_info = MatchInfo {
            pm_title: pm_title.to_string(),
            kalshi_title: kalshi_title.to_string(),
            similarity,
            category: category.to_string(),
            pm_side: "YES".to_string(),
            kalshi_side: "NO".to_string(),
            needs_inversion: false,
        };
        self.record_retained(&match_info);
        Some(match_info)
    }
    
    fn record_filter(&mut self, pm: &str, ks: &str, reason: &str) {
        self.filtered_count += 1;
        if self.filtered_count <= 3 {
            self.filtered_samples.push((pm.to_string(), ks.to_string(), reason.to_string()));
            println!("\n         🔍 二筛过滤 #{} [{}]:", self.filtered_count, reason);
            println!("            PM: {}", pm);
            println!("            Kalshi: {}", ks);
        }
    }
    
    fn record_retained(&mut self, info: &MatchInfo) {
        let sample = RetainedSample {
            pm_title: info.pm_title.clone(),
            kalshi_title: info.kalshi_title.clone(),
            similarity: info.similarity,
            category: info.category.clone(),
            pm_side: info.pm_side.clone(),
            kalshi_side: info.kalshi_side.clone(),
            needs_inversion: info.needs_inversion,
        };
        
        self.retained_samples
            .entry(info.category.clone())
            .or_insert_with(Vec::new)
            .push(sample);
    }
    
    pub fn reset_filtered_count(&mut self) {
        self.filtered_count = 0;
        self.filtered_samples.clear();
        self.retained_samples.clear();
    }
    
    pub fn print_retained_samples(&self) {
        println!("\n📊 二筛后各类别最高分样本 (每个类别最多3个):");
        
        let mut categories: Vec<_> = self.retained_samples.keys().collect();
        categories.sort();
        
        for category in categories.iter().take(5) {
            if let Some(samples) = self.retained_samples.get(*category) {
                let mut sorted = samples.clone();
                sorted.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
                
                println!("\n  类别 [{}]: {} 个留存", category, samples.len());
                for (i, sample) in sorted.iter().take(3).enumerate() {
                    let inversion_note = if sample.needs_inversion { " [Y/N颠倒]" } else { "" };
                    println!("    {}. 相似度: {:.3}{}", i+1, sample.similarity, inversion_note);
                    println!("       PM {}: {}", sample.pm_side, sample.pm_title);
                    println!("       Kalshi {}: {}", sample.kalshi_side, sample.kalshi_title);
                }
                if samples.len() > 3 {
                    println!("       ... 还有 {} 个", samples.len() - 3);
                }
            }
        }
        if categories.len() > 5 {
            println!("   ... 以及其他 {} 个类别", categories.len() - 5);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_winner_market() {
        // 队1：1Y1N 无颠倒
        let result = WinnerMarketValidator::validate(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Lakers"
        ).unwrap();
        assert_eq!(result.0, "YES");
        assert_eq!(result.1, "NO");
        assert!(!result.2);

        // 队2：2Y 为颠倒
        let result = WinnerMarketValidator::validate(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Celtics"
        ).unwrap();
        assert_eq!(result.0, "YES");
        assert_eq!(result.1, "YES");
        assert!(result.2);

        // 总进球数 O/U 与胜负 Winner 不能匹配
        assert!(WinnerMarketValidator::validate(
            "Mainz vs Olomouc: O/U 1.5",
            "Mainz vs Olomouc Winner? - Olomouc"
        ).is_none());

        // 平手 Draw 与胜负 Winner 不能匹配
        assert!(WinnerMarketValidator::validate(
            "Will Arsenal FC vs. Manchester City FC end in a draw?",
            "Arsenal vs Manchester City Winner? - Manchester City"
        ).is_none());

        // Texas at BYU：Kalshi 问 BYU(队2)，应 (YES,YES) 颠倒
        let result = WinnerMarketValidator::validate(
            "Texas Longhorns vs. BYU Cougars",
            "Texas at BYU Winner? - BYU"
        ).unwrap();
        assert_eq!(result.0, "YES");
        assert_eq!(result.1, "YES");
        assert!(result.2);
    }

    /// PM 队2 带「 - 联赛/分组」时，不能用 strip 冒号误伤；Kalshi 问第二队须判颠倒 (YES,YES)
    #[test]
    fn test_winner_valorant_pm_suffix_kalshi_second_team_inversion() {
        let r = WinnerMarketValidator::validate(
            "Valorant: S2G Esports vs Mandatory (BO3) - VCL EMEA: Group D",
            "Will Mandatory win the Mandatory vs. S2G Esports Valorant match? - Mandatory",
        )
        .expect("两队应一致且 Mandatory 为 PM 第二队，须走胜负校验");
        assert_eq!(r.0, "YES");
        assert_eq!(r.1, "YES");
        assert!(r.2, "Kalshi 问 PM 队2 胜应为 2Y 颠倒");
    }

    #[test]
    fn test_winner_tennis_wrong_opponent_rejected() {
        assert!(WinnerMarketValidator::validate(
            "Miami Open: Ignacio Buse vs Damir Dzumhur",
            "Will Damir Dzumhur win the Dzumhur vs Sinner : Round Of 64 match? - Damir Dzumhur"
        )
        .is_none());
    }

    #[test]
    fn test_bracket_advance_vs_single_game() {
        assert!(!BracketAdvanceVsSingleGameValidator::allows_pair(
            "Will Texas advance to the Sweet Sixteen?",
            "Texas at BYU Winner? - Texas"
        ));
        assert!(!BracketAdvanceVsSingleGameValidator::allows_pair(
            "Will Duke advance to the Final Four?",
            "TCU at Duke Winner? - Duke"
        ));
    }

    #[test]
    fn test_handicap_vs_whole_match_winner() {
        assert!(!HandicapVsSingleWinnerValidator::handicap_vs_single_winner_match(
            "Game Handicap: JDG (-2.5) vs LYON (+2.5)",
            "Will LYON win the JD Gaming vs. LYON League of Legends match? - LYON"
        ));
    }

    #[test]
    fn test_toss_vs_match_winner_rejected() {
        assert!(!TossVsMatchMarketValidator::allows_pair(
            "T20I Series NZ vs SA: New Zealand vs South Africa - Who wins the toss?",
            "New Zealand vs South Africa Winner? - South Africa"
        ));
        assert!(TossVsMatchMarketValidator::allows_pair(
            "Team A vs Team B - Who wins the toss?",
            "Team A vs Team B - Who wins the toss?"
        ));
    }

    /// 政党赢大选议席 vs 初选/某人获政党提名 — 不得互配
    #[test]
    fn test_electoral_party_seat_vs_nominee_rejected() {
        assert!(!ElectoralPropositionValidator::allows_pair(
            "Will the Democratic Party win the WV-02 House seat?",
            "Will Ace Parsi be the Democratic nominee for WV-02? - Yes",
        ));
        assert!(!ElectoralPropositionValidator::allows_pair(
            "Will Ace Parsi be the Democratic nominee for WV-02?",
            "Will the Republican Party win the WV-02 House seat? - Yes",
        ));
    }

    /// 单纯「获胜/特别选举」vs 「第二名/名次」— 不得互配
    #[test]
    fn test_electoral_win_vs_second_place_rejected() {
        assert!(!ElectoralPropositionValidator::allows_pair(
            "Will Clayton Fuller win the GA-14 special election?",
            "Will Clayton Fuller finish 2nd in the Georgia 14th congressional district Republican primary? - Yes",
        ));
    }

    /// 非选举语境不受本规则影响；同类型（均无名次）仍放行
    #[test]
    fn test_electoral_non_political_and_same_shape_allowed() {
        assert!(ElectoralPropositionValidator::allows_pair(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Lakers",
        ));
        assert!(ElectoralPropositionValidator::allows_pair(
            "Will Alice win the GA-14 special election?",
            "Will Bob win the GA-14 special election? - Bob",
        ));
    }

    #[test]
    fn test_handicap_vs_total_maps_rejected() {
        assert!(!EsportsGameValidator::handicap_vs_total_maps_match(
            "Map Handicap: FURIA (-1.5) vs Aurora Gaming (+1.5)",
            "Will over 2.5 maps be played in the FURIA vs. Aurora Gaming CS2 match? - Over 2.5 maps"
        ));
    }
    
    #[test]
    fn test_weather_region() {
        // 香港 vs 无地区（maximum temperature）-> 地区不匹配
        assert!(!WeatherValidator::regions_match(
            "Will the highest temperature in Hong Kong be 18°C or below on March 19?",
            "Will the maximum temperature be <81° on Mar 19, 2026? - 80° or below"
        ));
        // 同地区
        assert!(WeatherValidator::regions_match(
            "Will the highest temperature in Hong Kong be 18°C or below on March 19?",
            "Will the maximum temperature in Hong Kong be <20° on Mar 19, 2026?"
        ));
        // 都无地区
        assert!(WeatherValidator::regions_match(
            "Will the maximum temperature be 20°C or below?",
            "Will the maximum temperature be <81° on Mar 19? - 80° or below"
        ));
    }

    #[test]
    fn test_weather_fahrenheit_or_below_bucket() {
        assert!(!WeatherValidator::fahrenheit_or_below_buckets_match(
            "Will the highest temperature in Chicago be 37°F or below on March 24?",
            "Will the high temp in Chicago be <50° on Mar 24, 2026? - 49° or below"
        ));
        assert!(WeatherValidator::fahrenheit_or_below_buckets_match(
            "Will the highest temperature in Chicago be 37°F or below on March 24?",
            "Chicago high on Mar 24 - 37° or below"
        ));
        assert!(WeatherValidator::fahrenheit_or_below_buckets_match(
            "Some unrelated market 100 points",
            "Another market without temp or below"
        ));
    }

    #[test]
    fn test_esports_single_vs_series() {
        // BO5/First Stand vs 单局胜者 -> 不能匹配
        assert!(!EsportsGameValidator::single_vs_series_match(
            "LoL: G2 Esports vs BNK FEARX (BO5) - First Stand Group A",
            "Will G2 Esports win map 3 in the BNK FEARX vs. G2 Esports match? - G2 Esports"
        ));
    }

    #[test]
    fn test_sports_single_vs_finals() {
        // 决赛 vs 单场（at 格式）-> 不能匹配
        assert!(!SportsSingleVsFinalsValidator::single_vs_finals_match(
            "Will the Portland Trail Blazers win the NBA Western Conference Finals?",
            "Portland at Minnesota Winner? - Portland"
        ));
    }

    #[test]
    fn test_handicap_vs_single_winner() {
        // 让分盘口 vs 某局胜者 -> 不能匹配
        assert!(!HandicapVsSingleWinnerValidator::handicap_vs_single_winner_match(
            "Game Handicap: BFX (-1.5) vs G2 Esports (+1.5)",
            "Will G2 Esports win map 2 in the BNK FEARX vs. G2 Esports match? - G2 Esports"
        ));
        assert!(!HandicapVsSingleWinnerValidator::handicap_vs_single_winner_match(
            "Will BNK win map 3 in the BNK vs. G2 match? - BNK",
            "Game Handicap: BFX (-2.5) vs G2 Esports (+2.5)"
        ));
    }

    #[test]
    fn test_finals_consistency() {
        // PM 有 Finals 无 vs，Kalshi 无 Finals -> 剔除
        assert!(!FinalsConsistencyValidator::finals_consistency_match(
            "Will the New York Knicks win the NBA Eastern Conference Finals?",
            "Houston vs New York M Winner? - New York M"
        ));
        assert!(!FinalsConsistencyValidator::finals_consistency_match(
            "Will the New York Knicks win the NBA Eastern Conference Finals?",
            "New York Y vs Detroit Winner? - New York Y"
        ));
        // 双方都有 vs 且同两队、一方有 Finals -> 允许
        assert!(FinalsConsistencyValidator::finals_consistency_match(
            "Lakers vs Celtics - NBA Finals",
            "Lakers vs Celtics Winner? - Lakers"
        ));
        // 双方都无 Finals -> 允许
        assert!(FinalsConsistencyValidator::finals_consistency_match(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Lakers"
        ));
    }

    #[test]
    fn test_esports_single_vs_total() {
        // 单局胜者 vs 总局数 -> 不能匹配
        assert!(!EsportsGameValidator::single_vs_total_match(
            "LoL: G2 Esports vs BNK FEARX - Game 3 Winner",
            "Will over 4.5 maps be played in the BNK FEARX vs. G2 Esports League of Legends match? - Over 4.5 maps"
        ));
        // 单局 vs 单局 -> 可以
        assert!(EsportsGameValidator::single_vs_total_match(
            "LoL: G2 vs BNK - Game 3 Winner",
            "Will BNK win map 3 in the BNK vs. G2 match? - BNK"
        ));
        // 网球 Total Sets O/U vs win set N -> 不能匹配
        assert!(!EsportsGameValidator::single_vs_total_match(
            "Emilio Nava vs. Tomas Machac: Total Sets O/U 2.5",
            "Will Tomas Machac win set 2 in the Emilio Nava vs Tomas Machac match - Tomas Machac"
        ));
    }

    #[test]
    fn test_esports_game_number() {
        // Game 4 vs Map 2 -> 局数不匹配，应过滤
        assert!(!EsportsGameValidator::game_numbers_match(
            "LoL: G2 Esports vs BNK FEARX - Game 4 Winner",
            "Will BNK FEARX win map 2 in the BNK FEARX vs. G2 Esports match? - BNK FEARX"
        ));
        // Game 4 vs Map 4 -> 局数匹配
        assert!(EsportsGameValidator::game_numbers_match(
            "LoL: G2 Esports vs BNK FEARX - Game 4 Winner",
            "Will BNK FEARX win map 4 in the BNK FEARX vs. G2 Esports match? - BNK FEARX"
        ));
        // 非电竞对局（无 game/map）-> 放行
        assert!(EsportsGameValidator::game_numbers_match(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Lakers"
        ));
    }

    #[test]
    fn test_map_winner_vs_whole_match() {
        assert!(!EsportsGameValidator::map_winner_vs_whole_match_match(
            "Counter-Strike: ENCE Academy vs BIG Academy - Map 2 Winner",
            "Will ENCE Academy win the ENCE Academy vs. BIG Academy CS2 match? - ENCE Academy"
        ));
        // 两侧均为单局或均为整场 -> 不由本规则单独拦截
        assert!(EsportsGameValidator::map_winner_vs_whole_match_match(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Lakers"
        ));
    }

    #[test]
    fn test_draw_vs_winner() {
        assert!(!DrawVsWinnerValidator::allows_pair(
            "Will Arsenal FC vs. Manchester City FC end in a draw?",
            "Arsenal vs Manchester City Winner? - Arsenal"
        ));
        assert!(DrawVsWinnerValidator::allows_pair(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Lakers"
        ));
    }

    #[test]
    fn test_exact_score_vs_goals_totals_rejected() {
        assert!(!ExactScoreVsGoalsTotalsValidator::allows_pair(
            "Exact Score: 1. FSV Mainz 05 0 - 1 Eintracht Frankfurt?",
            "Frankfurt at Mainz: Totals - Over 4.5 goals scored"
        ));
        assert!(!ExactScoreVsGoalsTotalsValidator::allows_pair(
            "Exact Score: 1. FSV Mainz 05 0 - 0 Eintracht Frankfurt?",
            "Frankfurt at Mainz: Totals - Over 1.5 goals scored"
        ));
        // 两侧均为总进球盘 -> 仍由其它规则处理，本规则放行
        assert!(ExactScoreVsGoalsTotalsValidator::allows_pair(
            "Frankfurt at Mainz: Totals - Over 2.5 goals scored",
            "Mainz vs Frankfurt: Over 2.5 goals?"
        ));
    }

    #[test]
    fn test_esports_will_win_major_vs_sports_goals_rejected() {
        let pm = "Will Cloud9 New York win DreamHack Major 2?";
        let ks = "New York R wins by over 2.5 goals? - New York R wins by over 2.5 goals";
        assert!(!EsportsTournamentWinnerVsSportsGoalsValidator::allows_pair(pm, ks));

        let mut pipe = ValidationPipeline::new();
        assert!(
            pipe.validate(pm, ks, 0.95, "esports").is_none(),
            "DreamHack 夺冠盘不应与 NHL/足球进球盘配对"
        );

        // 对侧非 goals Totals：不拦截
        assert!(EsportsTournamentWinnerVsSportsGoalsValidator::allows_pair(
            "Will FaZe win IEM Cologne?",
            "FaZe vs NaVi Winner? - FaZe"
        ));
        // 无电竞语境的 Will win：不拦截（避免误伤足球等）
        assert!(EsportsTournamentWinnerVsSportsGoalsValidator::allows_pair(
            "Will Arsenal win the Premier League?",
            "Arsenal vs Chelsea: Over 2.5 goals?"
        ));

        // Kalshi「净胜分 Points」盘（非 goals）亦应拦截
        let ks_points = "New York wins by over 1.5 Points? - New York wins by over 1.5 Points";
        assert!(!EsportsTournamentWinnerVsSportsGoalsValidator::allows_pair(pm, ks_points));
        let mut pipe2 = ValidationPipeline::new();
        assert!(pipe2.validate(pm, ks_points, 0.95, "esports").is_none());
        // 电竞 Major vs 普通单场胜负：仍放行
        assert!(EsportsTournamentWinnerVsSportsGoalsValidator::allows_pair(
            "Will FaZe win IEM Cologne?",
            "FaZe vs NaVi Winner? - FaZe"
        ));
    }

    #[test]
    fn test_open_outright_vs_round_match_rejected() {
        assert!(!TournamentOutrightVsMatchValidator::allows_pair(
            "Miami Open: Elisabetta Cocciaretto vs Coco Gauff",
            "Will Coco Gauff win the WTA Miami Open? - Coco Gauff"
        ));
        // 无「赛事: vs」前缀的 Kalshi 单场胜负，不应被本规则拦截
        assert!(TournamentOutrightVsMatchValidator::allows_pair(
            "Miami Open: A vs B",
            "A vs B Winner? - A"
        ));
        // 非公开赛总冠军命题
        assert!(TournamentOutrightVsMatchValidator::allows_pair(
            "Lakers vs Celtics",
            "Lakers vs Celtics Winner? - Lakers"
        ));
    }

    #[test]
    fn test_cs_vitality_map_kalshi_inversion() {
        let r = WinnerMarketValidator::validate(
            "Counter-Strike: TheMongolz vs Vitality - Map 2 Winner",
            "Will Vitality win map 2 in the Vitality vs. The Mongolz match? - Vitality",
        )
        .expect("TheMongolz vs The Mongolz 应两队一致且判颠倒");
        assert_eq!(r.0, "YES");
        assert_eq!(r.1, "YES");
        assert!(r.2);
    }

    #[test]
    fn test_top_batter_vs_plain_match_winner_rejected() {
        assert!(!TeamSidePropVsMatchWinnerValidator::allows_pair(
            "T20 Series New Zealand vs South Africa: New Zealand vs South Africa - Team Top Batter South Africa Winner",
            "New Zealand vs South Africa Winner? - South Africa"
        ));
    }

    #[test]
    fn test_ou_rounds_bare_is_garbage() {
        assert!(GarbageMarketDetector::is_garbage_sports_market("O/U 1.5 Rounds"));
        assert!(!GarbageMarketDetector::is_garbage_sports_market(
            "Team A vs Team B: O/U 1.5 Rounds in match"
        ));
    }

    #[test]
    fn test_mismatched_map_winners_no_default_path() {
        let mut p = ValidationPipeline::new();
        assert!(p
            .validate(
                "Counter-Strike: MINLATE vs MANA eSports - Map 1 Winner",
                "Will MANA eSports win map 1 in the Rebels Gaming vs. MANA eSports match? - MANA eSports",
                0.9,
                "esports",
            )
            .is_none());
    }

    #[test]
    fn test_entertainment_chart_number_one_vs_top10() {
        assert!(!EntertainmentChartValidator::allows_pair(
            "Will \"Choosin' Texas\" by Ella Langley be the Billboard #1 song for the week of March 28?",
            "Will Choosin' Texas be Top 10 on the Billboard Hot 100 chart for the week of March 28th in 2026? - Choosin' Texas"
        ));
        assert!(EntertainmentChartValidator::allows_pair(
            "Will X be Billboard #1 for March 28?",
            "Will Y be Billboard #1 for March 28?"
        ));
    }

    #[test]
    fn test_entertainment_spotify_vs_billboard_rejected() {
        assert!(!EntertainmentChartValidator::allows_pair(
            "Will \"Choosin' Texas - Ella Langley\" be the #1 song on US Spotify this week?",
            "Will Choosin' Texas be the #1 song on the Billboard Hot 100 charts this week? - Choosin' Texas"
        ));
        assert!(EntertainmentChartValidator::is_billboard_spotify_cross(
            "Will X be #1 on US Spotify?",
            "Will Y be #1 on Billboard Hot 100? - Y"
        ));
    }

    #[test]
    fn test_lyon_jd_esports_map_kalshi_pair_yields_inversion() {
        let r = WinnerMarketValidator::validate(
            "LoL: LYON vs JD Gaming - Game 1 Winner",
            "Will JD Gaming win map 1 in the JD Gaming vs. LYON match? - JD Gaming",
        )
        .expect("Kalshi in the A vs. B match 应解析出两队并判为颠倒");
        assert_eq!(r.0, "YES");
        assert_eq!(r.1, "YES");
        assert!(r.2, "Kalshi 问 PM 队2胜 应为 2Y 颠倒");
    }
    
    #[test]
    fn test_score_market() {
        // 同向：O/U 19.5 Over 与 20+ 等价，1Y1N 无颠倒
        let result = StatMarketValidator::validate(
            "Points O/U 19.5",
            "20+ points"
        ).unwrap();
        assert_eq!(result.0, "YES");
        assert_eq!(result.1, "NO");
        assert!(!result.2);

        let result = StatMarketValidator::validate(
            "Points O/U 23.5",
            "25+ points"
        );
        assert!(result.is_none());

        // 同向：Under 20.5 与 20- 等价，1Y1N 无颠倒
        let result = StatMarketValidator::validate(
            "Points Under 20.5",
            "20- points"
        ).unwrap();
        assert_eq!(result.0, "YES");
        assert_eq!(result.1, "NO");
        assert!(!result.2);

        // 颠倒：O/U 6.5 Over(>=7) + 6-(<=6)，需 (YES,YES)
        let result = StatMarketValidator::validate(
            "Points O/U 6.5",
            "6- points"
        ).unwrap();
        assert_eq!(result.0, "YES");
        assert_eq!(result.1, "YES");
        assert!(result.2);

        // 颠倒：O/U 5.5 Under(<=5) + 6+(>=6)，需 (YES,YES)
        let result = StatMarketValidator::validate(
            "Points Under 5.5",
            "6+ points"
        ).unwrap();
        assert_eq!(result.0, "YES");
        assert_eq!(result.1, "YES");
        assert!(result.2);
    }

    #[test]
    fn test_stat_market_type_mismatch() {
        // Rebounds vs Assists -> 必须过滤
        assert!(StatMarketValidator::validate(
            "Ace Bailey: Rebounds O/U 4.5",
            "Ace Bailey: 4+ assists - Ace Bailey: 4+"
        ).is_none());
        assert!(StatMarketValidator::validate(
            "Paolo Banchero: Assists O/U 5.5",
            "Paolo Banchero: 5+ threes - Paolo Banchero: 5+"
        ).is_none());
    }

    #[test]
    fn test_stat_market_threshold_strict() {
        // O/U 5.5 Over 只与 6+ 匹配，不与 5+ 匹配
        assert!(StatMarketValidator::validate(
            "Tobias Harris: Rebounds O/U 5.5",
            "Tobias Harris: 5+ assists - Tobias Harris: 5+"
        ).is_none()); // 类型不同
        assert!(StatMarketValidator::validate(
            "Tobias Harris: Rebounds O/U 5.5",
            "Tobias Harris: 5+ rebounds - Tobias Harris: 5+"
        ).is_none()); // 类型相同但 5+ != 6+
        assert!(StatMarketValidator::validate(
            "Tobias Harris: Rebounds O/U 5.5",
            "Tobias Harris: 6+ rebounds - Tobias Harris: 6+"
        ).is_some()); // 正确：O/U 5.5 Over ↔ 6+
    }
}