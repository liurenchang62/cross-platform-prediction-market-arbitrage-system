// market_matcher.rs
//! 市场匹配器：TF-IDF（L2 归一化）+ **按类精确余弦 Top-K** 候选生成，再经二筛。

use crate::market::Market;
use crate::category_mapper::CategoryMapper;
use crate::system_params::{
    MATCH_MATMUL_CHUNK_ROWS, SIMILARITY_THRESHOLD, SIMILARITY_TOP_K,
};
use crate::category_vectorizer::{CategoryVectorizer, CategoryVectorizerManager};
use crate::text_vectorizer::{TextVectorizer, VectorizerConfig};
use crate::validation::{second_screen_try_match_with_reason, ValidationPipeline};
use rayon::prelude::*;
use tokio::task;

use ndarray::s;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use anyhow::Result;
use serde_json::Value;

/// 匹配结果置信度
#[derive(Debug, Clone)]
pub struct MatchConfidence {
    pub overall_score: f64,
    pub text_similarity: f64,
    pub date_match: bool,
    pub category_match: bool,
}

impl MatchConfidence {
    pub fn is_high_confidence(&self) -> bool {
        self.overall_score >= 0.75
    }

    pub fn is_medium_confidence(&self) -> bool {
        self.overall_score >= 0.50 && self.overall_score < 0.75
    }
}

/// 市场匹配器配置
#[derive(Debug, Clone)]
pub struct MarketMatcherConfig {
    pub similarity_threshold: f64,
    pub vectorizer_config: VectorizerConfig,
    pub use_date_boost: bool,
    pub use_category_boost: bool,
    pub date_boost_factor: f64,
    pub category_boost_factor: f64,
}

impl Default for MarketMatcherConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: SIMILARITY_THRESHOLD,
            vectorizer_config: VectorizerConfig::default(),
            use_date_boost: true,
            use_category_boost: true,
            date_boost_factor: 0.05,
            category_boost_factor: 0.03,
        }
    }
}

/// 市场匹配器
pub struct MarketMatcher {
    pub config: MarketMatcherConfig,
    pub category_mapper: CategoryMapper,
    pub kalshi_vectorizers: Arc<CategoryVectorizerManager>,
    pub polymarket_vectorizers: Arc<CategoryVectorizerManager>,
    pub fitted: bool,
    kalshi_market_cache: Arc<HashMap<String, Arc<Market>>>,
    polymarket_market_cache: Arc<HashMap<String, Arc<Market>>>,
    pub validation_pipeline: ValidationPipeline,
}

impl MarketMatcher {
    pub fn new(config: MarketMatcherConfig, category_mapper: CategoryMapper) -> Self {
        Self {
            config,
            category_mapper,
            kalshi_vectorizers: Arc::new(CategoryVectorizerManager::new()),
            polymarket_vectorizers: Arc::new(CategoryVectorizerManager::new()),
            fitted: false,
            kalshi_market_cache: Arc::new(HashMap::new()),
            polymarket_market_cache: Arc::new(HashMap::new()),
            validation_pipeline: ValidationPipeline::new(),
        }
    }

    /// 按类别并行建索引：与串行逐类 `add_markets_batch` 数学上等价，不改变近邻与匹配结果。
    fn parallel_build_category_indices(
        manager: &mut CategoryVectorizerManager,
        by_category: HashMap<String, Vec<(String, String, Option<Value>)>>,
    ) -> Result<(), anyhow::Error> {
        let n_cat = by_category.len();
        if n_cat == 0 {
            return Ok(());
        }
        println!("      并行构建 {} 个类别索引 (rayon)...", n_cat);

        let mut tasks = Vec::new();
        for (category, items) in by_category {
            let Some(cv) = manager.get(&category) else {
                continue;
            };
            if !cv.fitted {
                continue;
            }
            tasks.push((category, items, cv.vectorizer.clone()));
        }

        let built: Vec<(String, CategoryVectorizer)> = tasks
            .into_par_iter()
            .map(|(category, items, vz)| -> anyhow::Result<(String, CategoryVectorizer)> {
                let mut cv = CategoryVectorizer::with_fitted_vectorizer(category.clone(), vz);
                cv.add_markets_batch(items)?;
                Ok((category, cv))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        for (cat, cv) in built {
            manager.insert_built_category(cat, cv);
        }
        Ok(())
    }

    pub fn fit_vectorizer(&mut self, kalshi_markets: &[Market], polymarket_markets: &[Market]) -> Result<()> {
        println!("📚 按类别训练向量化器（每类合并 Poly + Kalshi 标题语料，词表与维度一致）...");
        
        let mut kalshi_by_category: HashMap<String, Vec<String>> = HashMap::new();
        for market in kalshi_markets {
            let categories = self.category_mapper.classify(&market.title);
            for cat in categories {
                kalshi_by_category
                    .entry(cat)
                    .or_insert_with(Vec::new)
                    .push(market.title.clone());
            }
        }
        
        let mut polymarket_by_category: HashMap<String, Vec<String>> = HashMap::new();
        for market in polymarket_markets {
            let categories = self.category_mapper.classify(&market.title);
            for cat in categories {
                polymarket_by_category
                    .entry(cat)
                    .or_insert_with(Vec::new)
                    .push(market.title.clone());
            }
        }
        
        let mut all_categories: HashSet<String> = kalshi_by_category.keys().cloned().collect();
        for c in polymarket_by_category.keys() {
            all_categories.insert(c.clone());
        }

        let mut pairs: Vec<(String, Vec<String>)> = Vec::new();
        for cat in all_categories {
            let mut docs = kalshi_by_category.get(&cat).cloned().unwrap_or_default();
            docs.extend(polymarket_by_category.get(&cat).cloned().unwrap_or_default());
            if docs.is_empty() {
                continue;
            }
            pairs.push((cat, docs));
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let kalshi_vec = Arc::get_mut(&mut self.kalshi_vectorizers).unwrap();
        let polymarket_vec = Arc::get_mut(&mut self.polymarket_vectorizers).unwrap();
        kalshi_vec.clear();
        polymarket_vec.clear();

        let n_cat = pairs.len();
        if n_cat == 0 {
            self.fitted = true;
            return Ok(());
        }

        println!("   📊 并行拟合 {} 个类别（合并语料 TF-IDF）...", n_cat);
        let vcfg = self.config.vectorizer_config.clone();
        let fitted: Vec<(String, TextVectorizer)> = pairs
            .into_par_iter()
            .map(|(category, titles)| {
                let mut vz = TextVectorizer::new(vcfg.clone());
                vz.fit(&titles);
                (category, vz)
            })
            .collect();

        for (category, vz) in fitted {
            let cv_k = CategoryVectorizer::with_fitted_vectorizer(category.clone(), vz.clone());
            let cv_p = CategoryVectorizer::with_fitted_vectorizer(category.clone(), vz);
            kalshi_vec.insert_built_category(category.clone(), cv_k);
            polymarket_vec.insert_built_category(category, cv_p);
        }
        
        self.fitted = true;
        Ok(())
    }

    pub fn build_kalshi_index(&mut self, markets: &[Market]) -> Result<(), anyhow::Error> {
        if markets.is_empty() {
            return Ok(());
        }

        println!("📊 构建 Kalshi 市场索引...");
        
        let mut by_category: HashMap<String, Vec<(String, String, Option<Value>)>> = HashMap::new();
        let mut cache = HashMap::new();
        
        for market in markets {
            let market_id = format!("{}:{}", market.platform, market.market_id);
            cache.insert(market_id.clone(), Arc::new(market.clone()));
            
            let categories = self.category_mapper.classify(&market.title);
            let data = Some(serde_json::json!({
                "title": market.title,
                "platform": market.platform,
            }));
            
            if categories.is_empty() {
                by_category
                    .entry("unclassified".to_string())
                    .or_insert_with(Vec::new)
                    .push((market_id, market.title.clone(), data));
            } else {
                for cat in categories {
                    by_category
                        .entry(cat)
                        .or_insert_with(Vec::new)
                        .push((market_id.clone(), market.title.clone(), data.clone()));
                }
            }
        }
        
        // 注意：必须分别缓存两边市场，否则后一次 build_* 会覆盖，导致某个方向 0 匹配
        self.kalshi_market_cache = Arc::new(cache);
        
        let kalshi_vec = Arc::get_mut(&mut self.kalshi_vectorizers).unwrap();
        Self::parallel_build_category_indices(kalshi_vec, by_category)?;
        
        println!("   ✅ Kalshi 索引构建完成，总市场数: {}", kalshi_vec.total_size());
        Ok(())
    }

    pub fn build_polymarket_index(&mut self, markets: &[Market]) -> Result<(), anyhow::Error> {
        if markets.is_empty() {
            return Ok(());
        }

        println!("📊 构建 Polymarket 市场索引...");
        
        let mut by_category: HashMap<String, Vec<(String, String, Option<Value>)>> = HashMap::new();
        let mut cache = HashMap::new();
        
        for market in markets {
            let market_id = format!("{}:{}", market.platform, market.market_id);
            cache.insert(market_id.clone(), Arc::new(market.clone()));
            
            let categories = self.category_mapper.classify(&market.title);
            let data = Some(serde_json::json!({
                "title": market.title,
                "platform": market.platform,
            }));
            
            if categories.is_empty() {
                by_category
                    .entry("unclassified".to_string())
                    .or_insert_with(Vec::new)
                    .push((market_id, market.title.clone(), data));
            } else {
                for cat in categories {
                    by_category
                        .entry(cat)
                        .or_insert_with(Vec::new)
                        .push((market_id.clone(), market.title.clone(), data.clone()));
                }
            }
        }
        
        self.polymarket_market_cache = Arc::new(cache);
        
        let polymarket_vec = Arc::get_mut(&mut self.polymarket_vectorizers).unwrap();
        Self::parallel_build_category_indices(polymarket_vec, by_category)?;
        
        println!("   ✅ Polymarket 索引构建完成，总市场数: {}", polymarket_vec.total_size());
        Ok(())
    }









    pub async fn find_matches_bidirectional(
        &mut self,
        _pm_markets: &[Market],
        _kalshi_markets: &[Market],
    ) -> Vec<(Market, Market, f64, String, String, bool, String)> {
        if !self.fitted {
            println!("⚠️ 索引未构建");
            return Vec::new();
        }

        self.validation_pipeline.reset_filtered_count();

        println!("\n🔍 双向匹配：按大类 P·Kᵀ / K·Pᵀ 初筛（类内并行）→ 全类统一二筛");

        let kalshi_vec = self.kalshi_vectorizers.clone();
        let polymarket_vec = self.polymarket_vectorizers.clone();
        let config = self.config.clone();
        let market_cache_ks = self.kalshi_market_cache.clone();
        let market_cache_pm = self.polymarket_market_cache.clone();

        let start_time = std::time::Instant::now();

        let handle = task::spawn_blocking(move || {
            Self::find_matches_batched_sync(
                polymarket_vec.as_ref(),
                kalshi_vec.as_ref(),
                &config,
                market_cache_pm.as_ref(),
                market_cache_ks.as_ref(),
            )
        });

        let (mut all_raw, pipeline) = handle.await.unwrap();

        self.validation_pipeline = pipeline;

        let initial_count = all_raw.len();
        all_raw.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        let mut deduped: Vec<(Market, Market, f64, String, String, bool, String)> = Vec::new();
        let mut seen_m = HashSet::new();
        for t in all_raw {
            let key = (t.0.market_id.clone(), t.1.market_id.clone());
            if seen_m.insert(key) {
                deduped.push(t);
            }
        }

        // 二筛 + market_id 去重后的对数；启用 LLM 时仅保留「LLM 成功返回且 is_same_contract」的对；API/解析失败一律剔除
        let pairs_after_dedup_before_llm = deduped.len();

        // ===== 写入全量候选对日志（每次全量匹配覆盖写，供人工分析二筛/LLM 逻辑）=====
        {
            let log_path = "logs/match_candidates_latest.jsonl";
            let mut lines = Vec::with_capacity(deduped.len());
            for (pm, ks, sim, p_side, k_side, inv, cat) in &deduped {
                let obj = serde_json::json!({
                    "pm_id":    pm.market_id,
                    "ks_id":    ks.market_id,
                    "pm_title": pm.title,
                    "ks_title": ks.title,
                    "similarity": sim,
                    "pm_side":  p_side,
                    "ks_side":  k_side,
                    "inversion": inv,
                    "category": cat,
                    "pm_resolution": pm.resolution_date.map(|d| d.to_string()),
                    "ks_resolution": ks.resolution_date.map(|d| d.to_string()),
                });
                lines.push(obj.to_string());
            }
            match std::fs::write(log_path, lines.join("\n") + "\n") {
                Ok(_) => println!("   📝 已写入全量候选对 {} 条 → {}", deduped.len(), log_path),
                Err(e) => eprintln!("   ⚠️ 写入候选对日志失败: {}", e),
            }
        }

        // ===== LLM 二次验证（如果启用）=====
        // 【高并发优化】默认 32 并发，200 对约 12-20 秒完成（假设单请求 2-3 秒）
        let (deduped, llm_rejected_total, llm_error_total) = if crate::llm_validator::llm_validation_enabled() {
            let llm_concurrency: usize = std::env::var("LLM_CONCURRENCY")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(32); // 默认 32 并发，兼顾速度与稳定性

            let llm_total = deduped.len();
            println!("\n🤖 LLM 二次验证启动（{} 对候选，并发 {}）...", llm_total, llm_concurrency);
            let llm_start = std::time::Instant::now();

            // 准备并发输入数据
            let inputs: Vec<(String, String, String, (Market, Market, f64, String, String, bool, String))> = deduped
                .into_iter()
                .map(|(pm, ks, sim, p_side, k_side, inv, cat)| {
                    (pm.title.clone(), ks.title.clone(), cat.clone(), (pm, ks, sim, p_side, k_side, inv, cat))
                })
                .collect();

            // 执行高并发批量验证；整批 Err 则本批候选全部剔除（不混用二筛）
            let (results, llm_fatal_batch) =
                match crate::llm_validator::validate_matches_concurrent(inputs, llm_concurrency).await {
                    Ok(r) => (r, false),
                    Err(e) => {
                        eprintln!("⚠️ LLM 并发验证失败，本批 {} 对全部剔除: {}", llm_total, e);
                        (Vec::new(), true)
                    }
                };

            // 分离通过/拒绝：仅 is_same_contract==true 进入下游；API/解析失败（None）→ 剔除
            let mut llm_passed = Vec::new();
            let mut llm_rejected_pairs: Vec<serde_json::Value> = Vec::new();
            let mut llm_rejected = 0usize;
            let mut llm_errors = 0usize;

            for (result, (pm, ks, sim, p_side, k_side, inv, cat)) in results {
                match result {
                    Some(validation) if validation.is_same_contract => {
                        llm_passed.push((pm, ks, sim, p_side, k_side, inv, cat));
                    }
                    Some(validation) => {
                        println!("   ✗ LLM拒绝 [{}] {}ms | {}",
                            validation.provider.display_name(),
                            validation.latency_ms,
                            validation.reasoning.chars().take(60).collect::<String>()
                        );
                        llm_rejected_pairs.push(serde_json::json!({
                            "pm_id":    pm.market_id,
                            "ks_id":    ks.market_id,
                            "pm_title": pm.title,
                            "ks_title": ks.title,
                            "similarity": sim,
                            "pm_side":  p_side,
                            "ks_side":  k_side,
                            "inversion": inv,
                            "category": cat,
                            "pm_resolution": pm.resolution_date.map(|d| d.to_string()),
                            "ks_resolution": ks.resolution_date.map(|d| d.to_string()),
                            "llm_reject_reason": validation.reasoning,
                            "llm_provider": validation.provider.display_name(),
                            "llm_latency_ms": validation.latency_ms,
                        }));
                        llm_rejected += 1;
                    }
                    None => {
                        llm_errors += 1;
                        println!(
                            "   ✗ LLM剔除（无有效结论）| PM:{}… / KS:{}…",
                            pm.title.chars().take(40).collect::<String>(),
                            ks.title.chars().take(40).collect::<String>()
                        );
                        llm_rejected_pairs.push(serde_json::json!({
                            "pm_id":    pm.market_id,
                            "ks_id":    ks.market_id,
                            "pm_title": pm.title,
                            "ks_title": ks.title,
                            "similarity": sim,
                            "pm_side":  p_side,
                            "ks_side":  k_side,
                            "inversion": inv,
                            "category": cat,
                            "pm_resolution": pm.resolution_date.map(|d| d.to_string()),
                            "ks_resolution": ks.resolution_date.map(|d| d.to_string()),
                            "llm_reject_reason": null,
                            "llm_provider": null,
                            "llm_latency_ms": null,
                        }));
                    }
                }
            }

            // 写入 LLM 拒绝对日志（每次覆盖）
            {
                let log_path = "logs/arb_pairs_llm_rejected.jsonl";
                let lines: Vec<String> = llm_rejected_pairs.iter().map(|v| v.to_string()).collect();
                match std::fs::write(log_path, lines.join("\n") + "\n") {
                    Ok(_) => println!("   📝 LLM拒绝对 {} 条 → {}", llm_rejected_pairs.len(), log_path),
                    Err(e) => eprintln!("   ⚠️ 写入LLM拒绝对日志失败: {}", e),
                }
            }

            if llm_fatal_batch {
                llm_errors = llm_total;
            }

            let llm_elapsed = llm_start.elapsed();
            let avg_time = if llm_total > 0 {
                llm_elapsed.as_millis() as f64 / llm_total as f64
            } else {
                0.0
            };
            println!(
                "🤖 LLM 验证完成：通过 {} 对，拒绝 {} 对，无有效结论剔除 {} 对{}，并发 {}，总耗时 {:?}，平均每对 {:.1}ms",
                llm_passed.len(),
                llm_rejected,
                llm_errors,
                if llm_fatal_batch { "（含整批失败）" } else { "" },
                llm_concurrency,
                llm_elapsed,
                avg_time
            );
            (llm_passed, llm_rejected, llm_errors)
        } else {
            (deduped, 0usize, 0usize)
        };

        let final_count = deduped.len();
        let merge_deduped = initial_count.saturating_sub(final_count);

        // ===== 写入 LLM 后全量套利对日志（每次覆盖，供人工比对误杀）=====
        {
            let log_path = "logs/arb_pairs_latest.jsonl";
            let mut lines = Vec::with_capacity(deduped.len());
            for (pm, ks, sim, p_side, k_side, inv, cat) in &deduped {
                let obj = serde_json::json!({
                    "pm_id":    pm.market_id,
                    "ks_id":    ks.market_id,
                    "pm_title": pm.title,
                    "ks_title": ks.title,
                    "similarity": sim,
                    "pm_side":  p_side,
                    "ks_side":  k_side,
                    "inversion": inv,
                    "category": cat,
                    "pm_resolution": pm.resolution_date.map(|d| d.to_string()),
                    "ks_resolution": ks.resolution_date.map(|d| d.to_string()),
                });
                lines.push(obj.to_string());
            }
            match std::fs::write(log_path, lines.join("\n") + "\n") {
                Ok(_) => println!("   📝 LLM后套利对 {} 条 → {}", deduped.len(), log_path),
                Err(e) => eprintln!("   ⚠️ 写入套利对日志失败: {}", e),
            }
        }

        let elapsed = start_time.elapsed();
        if crate::llm_validator::llm_validation_enabled() {
            println!(
                "\n📊 匹配：{:?} · 初筛二筛原始 {} 条 · 二筛+去重后 {} 对 · 【LLM 后保留 {} 对】（LLM 判否 {} · 无有效结论剔除 {}）· 相对初筛压掉 {}",
                elapsed,
                initial_count,
                pairs_after_dedup_before_llm,
                final_count,
                llm_rejected_total,
                llm_error_total,
                merge_deduped
            );
        } else {
            println!(
                "\n📊 匹配：{:?} · 二筛后列表 {} 条 · 按 market_id 去重后 {} · 列表合并压掉 {}",
                elapsed, initial_count, final_count, merge_deduped
            );
        }

        self.validation_pipeline.print_retained_samples();

        deduped
    }







    /// 类内对每个 `chunk` 行块的 `dot` 用 rayon 并行；块结果按 `r0` 排序后展平，再接 `K·Pᵀ` 侧——与同顺序串行双层循环在 `seen_pair` 上行为一致。
    fn sweep_pm_to_ks_candidates_ordered(
        p_mat: &ndarray::Array2<f64>,
        k_mat: &ndarray::Array2<f64>,
        pm_items: &[crate::vector_index::IndexItem],
        ks_items: &[crate::vector_index::IndexItem],
        chunk: usize,
        threshold: f64,
        top_k: usize,
    ) -> Vec<(String, String, f64)> {
        let n_p = p_mat.nrows();
        if n_p == 0 {
            return Vec::new();
        }
        let k_t = k_mat.t();
        let chunk_starts: Vec<usize> = (0..n_p).step_by(chunk).collect();
        let mut parts: Vec<(usize, Vec<(String, String, f64)>)> = chunk_starts
            .par_iter()
            .map(|&r0| {
                let r1 = (r0 + chunk).min(n_p);
                let p_sl = p_mat.slice(s![r0..r1, ..]);
                let scores = p_sl.dot(&k_t);
                let mut out = Vec::new();
                for local in 0..scores.nrows() {
                    let row_idx = r0 + local;
                    let pm_id = pm_items[row_idx].id.clone();
                    let hits = Self::top_k_similarities_for_row(
                        scores.row(local),
                        threshold,
                        top_k,
                    );
                    for (j, sim) in hits {
                        let ks_id = ks_items[j].id.clone();
                        out.push((pm_id.clone(), ks_id, sim));
                    }
                }
                (r0, out)
            })
            .collect();
        parts.sort_by_key(|(r0, _)| *r0);
        parts.into_iter().flat_map(|(_, v)| v).collect()
    }

    fn sweep_ks_to_pm_candidates_ordered(
        p_mat: &ndarray::Array2<f64>,
        k_mat: &ndarray::Array2<f64>,
        pm_items: &[crate::vector_index::IndexItem],
        ks_items: &[crate::vector_index::IndexItem],
        chunk: usize,
        threshold: f64,
        top_k: usize,
    ) -> Vec<(String, String, f64)> {
        let n_k = k_mat.nrows();
        if n_k == 0 {
            return Vec::new();
        }
        let p_t = p_mat.t();
        let chunk_starts: Vec<usize> = (0..n_k).step_by(chunk).collect();
        let mut parts: Vec<(usize, Vec<(String, String, f64)>)> = chunk_starts
            .par_iter()
            .map(|&r0| {
                let r1 = (r0 + chunk).min(n_k);
                let k_sl = k_mat.slice(s![r0..r1, ..]);
                let scores = k_sl.dot(&p_t);
                let mut out = Vec::new();
                for local in 0..scores.nrows() {
                    let row_idx = r0 + local;
                    let ks_id = ks_items[row_idx].id.clone();
                    let hits = Self::top_k_similarities_for_row(
                        scores.row(local),
                        threshold,
                        top_k,
                    );
                    for (j, sim) in hits {
                        let pm_id = pm_items[j].id.clone();
                        out.push((pm_id, ks_id.clone(), sim));
                    }
                }
                (r0, out)
            })
            .collect();
        parts.sort_by_key(|(r0, _)| *r0);
        parts.into_iter().flat_map(|(_, v)| v).collect()
    }

    fn fold_candidates_into_best(
        best: &mut HashMap<(String, String), (f64, String)>,
        pm: String,
        ks: String,
        sim: f64,
        cat: &str,
    ) {
        best.entry((pm, ks))
            .and_modify(|e| {
                if sim > e.0 {
                    *e = (sim, cat.to_string());
                }
            })
            .or_insert_with(|| (sim, cat.to_string()));
    }

    /// 对每个大类：仅做矩阵初筛并把命中折叠进全局 `(pm,ks)→max(sim)`；全部类别结束后对去重结果统一跑二筛。
    fn find_matches_batched_sync(
        polymarket_vec: &CategoryVectorizerManager,
        kalshi_vec: &CategoryVectorizerManager,
        config: &MarketMatcherConfig,
        polymarket_cache: &HashMap<String, Arc<Market>>,
        kalshi_cache: &HashMap<String, Arc<Market>>,
    ) -> (
        Vec<(Market, Market, f64, String, String, bool, String)>,
        ValidationPipeline,
    ) {
        let start_time = std::time::Instant::now();
        let mut validation_pipeline = ValidationPipeline::new();
        let mut all_arc: Vec<(Arc<Market>, Arc<Market>, f64, String, String, bool, String)> = Vec::new();

        let mut categories_used = 0usize;
        let chunk = MATCH_MATMUL_CHUNK_ROWS.max(1);
        let mut best_by_pair: HashMap<(String, String), (f64, String)> = HashMap::new();
        let mut cumulative_raw_hits = 0usize;

        let cats = polymarket_vec.get_all_categories();
        for cat in cats {
            let Some(pm_cv) = polymarket_vec.get(&cat) else {
                continue;
            };
            let Some(ks_cv) = kalshi_vec.get(&cat) else {
                continue;
            };
            let Some(p_mat) = pm_cv.index.data_matrix() else {
                continue;
            };
            let Some(k_mat) = ks_cv.index.data_matrix() else {
                continue;
            };
            if p_mat.nrows() == 0 || k_mat.nrows() == 0 {
                continue;
            }
            if p_mat.ncols() != k_mat.ncols() {
                eprintln!(
                    "   ⚠️  类别 {:?} Poly 与 Kalshi 向量维不一致 ({} vs {})，跳过",
                    cat,
                    p_mat.ncols(),
                    k_mat.ncols()
                );
                continue;
            }

            categories_used += 1;
            let pm_items = pm_cv.index.items();
            let ks_items = ks_cv.index.items();
            let n_p = p_mat.nrows();
            let n_k = k_mat.nrows();
            let cat_started = std::time::Instant::now();

            let cand_pk = Self::sweep_pm_to_ks_candidates_ordered(
                p_mat,
                k_mat,
                pm_items,
                ks_items,
                chunk,
                config.similarity_threshold,
                SIMILARITY_TOP_K,
            );
            let cand_kp = Self::sweep_ks_to_pm_candidates_ordered(
                p_mat,
                k_mat,
                pm_items,
                ks_items,
                chunk,
                config.similarity_threshold,
                SIMILARITY_TOP_K,
            );
            let cat_raw = cand_pk.len() + cand_kp.len();
            cumulative_raw_hits += cat_raw;
            for (pm, ks, sim) in cand_pk {
                Self::fold_candidates_into_best(&mut best_by_pair, pm, ks, sim, &cat);
            }
            for (pm, ks, sim) in cand_kp {
                Self::fold_candidates_into_best(&mut best_by_pair, pm, ks, sim, &cat);
            }

            println!(
                "   ✓ #{} 「{}」Poly×Kalshi {}×{} · 本类初筛 {} 条 · 累计初筛 {} 条 · {:?}",
                categories_used,
                cat,
                n_p,
                n_k,
                cat_raw,
                cumulative_raw_hits,
                cat_started.elapsed()
            );
        }

        let n_unique_pairs = best_by_pair.len();
        let mut merged: Vec<(String, String, f64, String)> = best_by_pair
            .into_iter()
            .map(|((pm, ks), (sim, cat))| (pm, ks, sim, cat))
            .collect();
        merged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let merged_len = merged.len();

        // 二筛：并行执行，同时收集被拒绝对的诊断原因（每对都执行一次带原因版）
        let mut second_screen_outcomes: Vec<(usize, Option<Option<crate::validation::MatchInfo>>, Option<String>)> =
            (0..merged_len)
                .into_par_iter()
                .map(|k| {
                    let (pm_id, ks_id, sim, cat) = &merged[k];
                    let Some(pm_arc) = polymarket_cache.get(pm_id.as_str()) else {
                        return (k, None, None);
                    };
                    let Some(ks_arc) = kalshi_cache.get(ks_id.as_str()) else {
                        return (k, None, None);
                    };
                    let mut reason: Option<String> = Some(String::new()); // 触发原因记录
                    let mi = crate::validation::second_screen_try_match_with_reason(
                        pm_arc.title.as_str(),
                        ks_arc.title.as_str(),
                        *sim,
                        cat.as_str(),
                        &mut reason,
                    );
                    (k, Some(mi), reason)
                })
                .collect();
        second_screen_outcomes.sort_by_key(|(idx, _, _)| *idx);

        // 收集被拒绝样本（按原因分组，每组最多 3 个样本）
        let mut reject_by_reason: std::collections::BTreeMap<String, Vec<(String, String, f64)>> =
            std::collections::BTreeMap::new();

        let mut seen_accepted: HashSet<(String, String)> = HashSet::new();
        for k in 0..merged_len {
            let (pm_id, ks_id, sim, _cat) = &merged[k];
            let (_, outcome, reason) = &second_screen_outcomes[k];
            match outcome {
                None => {}
                Some(None) => {
                    validation_pipeline.filtered_count += 1;
                    // 记录拒绝原因样本
                    if let (Some(r), Some(pm_arc), Some(ks_arc)) = (
                        reason.as_ref(),
                        polymarket_cache.get(pm_id.as_str()),
                        kalshi_cache.get(ks_id.as_str()),
                    ) {
                        let key = r.clone();
                        let entry = reject_by_reason.entry(key).or_default();
                        if entry.len() < 3 {
                            entry.push((pm_arc.title.clone(), ks_arc.title.clone(), *sim));
                        }
                    }
                }
                Some(Some(match_info)) => {
                    let key = (pm_id.clone(), ks_id.clone());
                    if seen_accepted.contains(&key) {
                        continue;
                    }
                    let Some(pm_arc) = polymarket_cache.get(pm_id.as_str()) else {
                        continue;
                    };
                    let Some(ks_arc) = kalshi_cache.get(ks_id.as_str()) else {
                        continue;
                    };
                    validation_pipeline.record_retained(match_info);
                    let confidence = Self::calculate_confidence(
                        pm_arc.as_ref(),
                        ks_arc.as_ref(),
                        *sim,
                        config,
                    );
                    if confidence.overall_score >= config.similarity_threshold {
                        seen_accepted.insert(key);
                        all_arc.push((
                            Arc::clone(pm_arc),
                            Arc::clone(ks_arc),
                            confidence.overall_score,
                            match_info.pm_side.clone(),
                            match_info.kalshi_side.clone(),
                            match_info.needs_inversion,
                            match_info.category.clone(), // 加入 category 用于 LLM 验证
                        ));
                    } else {
                        // 二筛通过但 confidence 不够
                        let entry = reject_by_reason
                            .entry(format!("LowConfidence({:.3}<{:.3})", confidence.overall_score, config.similarity_threshold))
                            .or_default();
                        if entry.len() < 3 {
                            entry.push((pm_arc.title.clone(), ks_arc.title.clone(), confidence.overall_score));
                        }
                    }
                }
            }
        }

        let all_matches: Vec<(Market, Market, f64, String, String, bool, String)> = all_arc
            .into_iter()
            .map(|(qa, ta, s, ps, ks, ni, cat)| ((*qa).clone(), (*ta).clone(), s, ps, ks, ni, cat))
            .collect();

        println!(
            "   ✅ 大类 {} 个 · {:?} · 初筛原始命中 {} 条 · 去重 {} 对 · 统一二筛保留 {} 条",
            categories_used,
            start_time.elapsed(),
            cumulative_raw_hits,
            n_unique_pairs,
            all_matches.len()
        );

        // 打印二筛拒绝原因摘要（按出现次数降序，每种原因展示 ≤2 个样本）
        if !reject_by_reason.is_empty() {
            // 用完整计数：重新扫一次（样本最多3个，但计数要准确）
            let mut reason_counts: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for (_, outcome, reason) in &second_screen_outcomes {
                if matches!(outcome, Some(None)) {
                    if let Some(r) = reason.as_ref() {
                        *reason_counts.entry(r.clone()).or_default() += 1;
                    }
                }
            }
            // 加 LowConfidence
            for (reason, samples) in &reject_by_reason {
                if reason.starts_with("LowConfidence") {
                    reason_counts.entry(reason.clone()).or_insert(samples.len());
                }
            }
            let mut sorted_reasons: Vec<_> = reason_counts.iter().collect();
            sorted_reasons.sort_by(|a, b| b.1.cmp(a.1));
            println!("\n   🔎 二筛拒绝原因摘要（{} 种，共 {} 对被拒）：",
                sorted_reasons.len(), validation_pipeline.filtered_count);
            for (reason, count) in sorted_reasons.iter().take(15) {
                println!("      [{:>3}对] {}", count, reason);
                if let Some(samples) = reject_by_reason.get(*reason) {
                    for (pm, ks, sim) in samples.iter().take(2) {
                        println!("             sim={:.3} PM:「{}」", sim, pm);
                        println!("                    KS:「{}」", ks);
                    }
                }
            }
        }

        (all_matches, validation_pipeline)
    }

    fn top_k_similarities_for_row(
        row: ndarray::ArrayView1<f64>,
        threshold: f64,
        max_results: usize,
    ) -> Vec<(usize, f64)> {
        if max_results == 0 {
            return Vec::new();
        }
        let mut hits: Vec<(usize, f64)> = row
            .iter()
            .enumerate()
            .filter_map(|(j, &s)| (s >= threshold).then_some((j, s)))
            .collect();
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(max_results);
        hits
    }

    fn calculate_confidence(
        market1: &Market,
        market2: &Market,
        vector_similarity: f64,
        config: &MarketMatcherConfig,
    ) -> MatchConfidence {
        let mut final_score = vector_similarity;

        let date_match = if let (Some(d1), Some(d2)) = (market1.resolution_date, market2.resolution_date) {
            let diff = (d1 - d2).num_seconds().abs();
            let match_quality = if diff <= 86400 { 1.0 } else { 0.0 };
            
            if config.use_date_boost {
                final_score += config.date_boost_factor * match_quality;
            }
            match_quality > 0.0
        } else {
            false
        };

        let category_match = if let (Some(c1), Some(c2)) = (&market1.category, &market2.category) {
            let match_quality = if c1.to_lowercase() == c2.to_lowercase() { 1.0 } else { 0.0 };
            
            if config.use_category_boost {
                final_score += config.category_boost_factor * match_quality;
            }
            match_quality > 0.0
        } else {
            false
        };

        final_score = final_score.min(1.0);

        MatchConfidence {
            overall_score: final_score,
            text_similarity: vector_similarity,
            date_match,
            category_match,
        }
    }

    pub fn kalshi_index_size(&self) -> usize {
        self.kalshi_vectorizers.total_size()
    }

    pub fn polymarket_index_size(&self) -> usize {
        self.polymarket_vectorizers.total_size()
    }
}