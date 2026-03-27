// src/vector_index.rs
//! 向量索引模块：在 L2 归一化 TF-IDF 向量上用 **精确余弦相似度**（等价于点积）检索。
//!
//! 与历史上基于 KD-Tree + 欧氏球半径的实现相比：在同一向量与同一阈值下，候选集合由
//! `dot(q, v) >= threshold` 直接定义，无近似近邻，构建阶段也不再逐点插入树，通常更快。

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use ndarray::{Array1, Array2};

/// K-D Tree 时代遗留的默认维度提示（实际维度以首条向量为准）
const DEFAULT_DIMENSION_HINT: usize = 100;

/// 向量索引项
#[derive(Debug, Clone)]
pub struct IndexItem {
    /// 唯一标识符（如事件ID）
    pub id: String,
    /// 原始向量（与 `TextVectorizer` 一致，已 L2 归一化时即为单位方向）
    pub vector: Array1<f64>,
    /// 附加数据（可选）
    pub data: Option<serde_json::Value>,
}

/// 向量索引：行堆叠矩阵 + 精确点积检索
pub struct VectorIndex {
    /// 类别名称
    pub category_name: String,
    /// ID 到行号的映射
    id_to_idx: HashMap<String, usize>,
    /// 索引到项的映射（保留 id / 元数据；向量与矩阵行一致）
    items: Vec<IndexItem>,
    /// 形状 (n, d)，每行一条索引向量，用于 `scores = data_matrix.dot(q)`
    data_matrix: Option<Array2<f64>>,
    /// 向量维度
    dimension: usize,
    /// 是否已构建（矩阵与 items 一致）
    built: bool,
}

impl VectorIndex {
    /// 创建新的空索引（指定类别）
    pub fn new(category_name: String, dimension: usize) -> Self {
        Self {
            category_name,
            id_to_idx: HashMap::new(),
            items: Vec::new(),
            data_matrix: None,
            dimension,
            built: false,
        }
    }

    /// 创建默认维度的索引
    pub fn default(category_name: String) -> Self {
        Self::new(category_name, DEFAULT_DIMENSION_HINT)
    }

    /// 从向量列表构建索引（堆叠为矩阵，无 KD 构建开销）
    pub fn build(&mut self, items: Vec<IndexItem>) -> Result<()> {
        if items.is_empty() {
            self.items.clear();
            self.id_to_idx.clear();
            self.data_matrix = None;
            self.built = true;
            return Ok(());
        }

        let total = items.len();
        println!("        构建索引: {} 个项", total);
        let start = std::time::Instant::now();

        self.dimension = items[0].vector.len();
        for (i, item) in items.iter().enumerate() {
            if item.vector.len() != self.dimension {
                return Err(anyhow!(
                    "类别 {} 向量维度不一致: 首条 dim={}, 第 {} 条 dim={}",
                    self.category_name,
                    self.dimension,
                    i,
                    item.vector.len()
                ));
            }
        }

        self.items = items;
        self.id_to_idx.clear();
        for (idx, item) in self.items.iter().enumerate() {
            self.id_to_idx.insert(item.id.clone(), idx);
        }

        println!("          堆叠相似度矩阵 ({} × {})...", total, self.dimension);
        let mut mat = Array2::<f64>::zeros((total, self.dimension));
        for (i, item) in self.items.iter().enumerate() {
            mat.row_mut(i).assign(&item.vector);
        }
        self.data_matrix = Some(mat);
        self.built = true;

        println!("        索引构建完成，耗时: {:?}", start.elapsed());
        Ok(())
    }

    /// 插入单个向量（仅追加列表；下次查询前需对整个类别重新 `build`，当前流程仅用批量 build）
    pub fn insert(&mut self, item: IndexItem) -> Result<()> {
        let idx = self.items.len();
        self.id_to_idx.insert(item.id.clone(), idx);
        self.items.push(item);
        self.built = false;
        self.data_matrix = None;
        Ok(())
    }

    /// 查找余弦相似度不低于阈值的结果（L2 归一化下 cosine = dot），按相似度降序，最多 `max_results` 条。
    ///
    /// 这是对索引中 **全体** 候选的精确打分，不做 ANN 近似。
    pub fn find_similar_with_threshold(
        &self,
        query_vector: &Array1<f64>,
        threshold: f64,
        max_results: usize,
    ) -> Vec<(IndexItem, f64)> {
        if !self.built || self.items.is_empty() {
            return Vec::new();
        }

        if query_vector.len() != self.dimension {
            return Vec::new();
        }

        let Some(ref mat) = self.data_matrix else {
            return Vec::new();
        };

        // (n, d) · (d,) -> (n,)
        let scores = mat.dot(query_vector);

        let mut hits: Vec<(usize, f64)> = scores
            .iter()
            .enumerate()
            .filter_map(|(i, &s)| {
                if s >= threshold {
                    Some((i, s))
                } else {
                    None
                }
            })
            .collect();

        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if max_results > 0 && hits.len() > max_results {
            hits.truncate(max_results);
        }

        hits.into_iter()
            .map(|(i, s)| (self.items[i].clone(), s))
            .collect()
    }

    /// 堆叠后的索引矩阵 `(n, d)`，与 `items` 行序一致；未 build 时为 `None`。
    pub fn data_matrix(&self) -> Option<&Array2<f64>> {
        self.data_matrix.as_ref()
    }

    /// 与矩阵行对齐的项列表。
    pub fn items(&self) -> &[IndexItem] {
        &self.items
    }

    /// 获取索引大小
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 检查索引是否为空
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 清理索引
    pub fn clear(&mut self) {
        self.id_to_idx.clear();
        self.items.clear();
        self.data_matrix = None;
        self.built = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_vec2(x: f64, y: f64) -> Array1<f64> {
        let mut a = Array1::from_vec(vec![x, y]);
        let n = a.dot(&a).sqrt();
        a.mapv_inplace(|v| v / n);
        a
    }

    #[test]
    fn exact_top_matches_brute_force() {
        let mut idx = VectorIndex::new("t".into(), 2);
        let items = vec![
            IndexItem {
                id: "a".into(),
                vector: unit_vec2(1.0, 0.0),
                data: None,
            },
            IndexItem {
                id: "b".into(),
                vector: unit_vec2(1.0, 1.0),
                data: None,
            },
            IndexItem {
                id: "c".into(),
                vector: unit_vec2(0.0, 1.0),
                data: None,
            },
        ];
        let ids_vecs: Vec<(String, Array1<f64>)> = items
            .iter()
            .map(|it| (it.id.clone(), it.vector.clone()))
            .collect();

        idx.build(items).unwrap();

        let q = unit_vec2(1.0, 0.1);
        let threshold = 0.5_f64;
        let max_results = 10;

        let got = idx.find_similar_with_threshold(&q, threshold, max_results);

        let mut brute: Vec<(String, f64)> = ids_vecs
            .iter()
            .map(|(id, v)| (id.clone(), q.dot(v)))
            .filter(|(_, s)| *s >= threshold)
            .collect();
        brute.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        assert_eq!(got.len(), brute.len());
        for ((g, gs), (bid, bs)) in got.iter().zip(brute.iter()) {
            assert_eq!(&g.id, bid);
            assert!((gs - bs).abs() < 1e-9);
        }
    }
}
