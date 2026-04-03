// src/category_mapper.rs
//! 类别映射模块：负责将事件标题映射到预定义的类别
//! 
//! 从 categories.toml 加载配置，提供多类别判断功能

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use anyhow::{Result, Context};
use serde::{Deserialize, Serialize};
/// 类别配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CategoryConfig {
    /// 类别名称
    pub name: String,
    /// 关键词列表
    pub keywords: Vec<String>,
    /// 权重（预留，后续可用于加权）
    pub weight: f64,
    /// 描述
    pub description: Option<String>,
}

/// 类别映射器配置集合
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CategoryMapperConfig {
    /// 所有类别
    pub categories: Vec<CategoryConfig>,
}

/// 类别映射器
pub struct CategoryMapper {
    /// 类别配置
    config: CategoryMapperConfig,
    /// 关键词到类别的反向索引（用于快速查找）
    keyword_to_categories: HashMap<String, Vec<String>>,
    /// 类别名称集合
    category_names: HashSet<String>,
    /// 配置文件路径
    config_path: String,
    /// 最后修改时间（用于热加载）
    last_modified: Option<std::time::SystemTime>,
}

// 在结构体定义后添加 Clone 实现
impl Clone for CategoryMapper {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            keyword_to_categories: self.keyword_to_categories.clone(),
            category_names: self.category_names.clone(),
            config_path: self.config_path.clone(),
            last_modified: self.last_modified,
        }
    }
}




impl CategoryMapper {
    /// 从文件创建新的类别映射器
    pub fn default() -> Self {
        Self {
            config: CategoryMapperConfig::default(),
            keyword_to_categories: HashMap::new(),
            category_names: HashSet::new(),
            config_path: "".to_string(),
            last_modified: None,
        }
    }







    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .context(format!("读取类别配置文件失败: {:?}", path))?;
        
        let metadata = fs::metadata(path)?;
        let last_modified = metadata.modified().ok();
        
        let config: CategoryMapperConfig = if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            toml::from_str(&content)
                .context("解析 TOML 配置文件失败")?
        } else {
            // 默认尝试解析为 TOML
            toml::from_str(&content)
                .context("解析配置文件失败（仅支持 TOML 格式）")?
        };
        
        let mut mapper = Self {
            config,
            keyword_to_categories: HashMap::new(),
            category_names: HashSet::new(),
            config_path: path.to_string_lossy().to_string(),
            last_modified,
        };
        
        mapper.build_index();
        Ok(mapper)
    }
    
    /// 构建关键词反向索引
    fn build_index(&mut self) {
        self.keyword_to_categories.clear();
        self.category_names.clear();
        
        for category in &self.config.categories {
            self.category_names.insert(category.name.clone());
            
            for keyword in &category.keywords {
                let keyword_lower = keyword.to_lowercase();
                self.keyword_to_categories
                    .entry(keyword_lower)
                    .or_insert_with(Vec::new)
                    .push(category.name.clone());
            }
        }
    }
    
    /// 检查配置文件是否已更新（热加载）
    pub fn check_reload(&mut self) -> Result<bool> {
        let path = Path::new(&self.config_path);
        if !path.exists() {
            return Ok(false);
        }
        
        let metadata = fs::metadata(path)?;
        let current_modified = metadata.modified().ok();
        
        if current_modified != self.last_modified {
            // 文件已修改，重新加载
            let content = fs::read_to_string(path)?;
            let new_config: CategoryMapperConfig = toml::from_str(&content)?;
            
            self.config = new_config;
            self.last_modified = current_modified;
            self.build_index();
            
            println!("🔄 类别配置已热加载: {}", self.config_path);
            return Ok(true);
        }
        
        Ok(false)
    }
    
    /// 判断文本属于哪些类别
    pub fn classify(&self, text: &str) -> Vec<String> {
        let text_lower = text.to_lowercase();
        let mut matched_categories = HashSet::new();
        
        // 遍历所有关键词，检查是否出现在文本中
        for (keyword, categories) in &self.keyword_to_categories {
            if text_lower.contains(keyword) {
                for category in categories {
                    matched_categories.insert(category.clone());
                }
            }
        }
        
        let mut result: Vec<String> = matched_categories.into_iter().collect();
        result.sort(); // 保持结果稳定
        result
    }
    
    /// 获取所有类别名称
    pub fn get_all_categories(&self) -> &HashSet<String> {
        &self.category_names
    }
    
    /// 获取类别配置
    pub fn get_category_config(&self, name: &str) -> Option<&CategoryConfig> {
        self.config.categories.iter().find(|c| c.name == name)
    }
    
    /// 检查文本是否有任何类别匹配
    pub fn has_any_category(&self, text: &str) -> bool {
        !self.classify(text).is_empty()
    }
    
    /// 获取未分类的文本（返回原文本和提取的关键词）
    pub fn extract_keywords_for_log(&self, text: &str) -> Vec<String> {
        let text_lower = text.to_lowercase();
        let mut keywords = Vec::new();
        
        // 提取所有可能的关键词（长度>3的词）
        for word in text_lower.split_whitespace() {
            let clean_word = word.trim_matches(|c: char| !c.is_alphanumeric());
            if clean_word.len() > 3 && !self.keyword_to_categories.contains_key(clean_word) {
                keywords.push(clean_word.to_string());
            }
        }
        
        keywords.sort();
        keywords.dedup();
        keywords.truncate(10); // 最多保留10个关键词
        keywords
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_classify() {
        let mut config = CategoryMapperConfig::default();
        config.categories = vec![
            CategoryConfig {
                name: "religion".to_string(),
                keywords: vec!["jesus".to_string(), "christ".to_string(), "god".to_string()],
                weight: 1.0,
                description: Some("宗教相关".to_string()),
            },
            CategoryConfig {
                name: "gaming".to_string(),
                keywords: vec!["gta".to_string(), "game".to_string(), "playstation".to_string()],
                weight: 1.0,
                description: Some("游戏相关".to_string()),
            },
        ];
        
        let mut mapper = CategoryMapper {
            config,
            keyword_to_categories: HashMap::new(),
            category_names: HashSet::new(),
            config_path: "test.toml".to_string(),
            last_modified: None,
        };
        mapper.build_index();
        
        let categories = mapper.classify("Will Jesus Christ return before GTA VI?");
        assert_eq!(categories.len(), 2);
        assert!(categories.contains(&"religion".to_string()));
        assert!(categories.contains(&"gaming".to_string()));
    }
}