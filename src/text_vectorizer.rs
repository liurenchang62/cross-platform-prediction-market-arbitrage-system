// text_vectorizer.rs
//! TF-IDF 文本向量化模块

use std::collections::{HashMap, HashSet};
use rust_stemmers::{Algorithm, Stemmer};
use ndarray::Array1;
use crate::system_params::MAX_VOCAB_SIZE;

/// 停用词集合（常见无意义词语）
fn get_stop_words() -> HashSet<&'static str> {
    let mut set = HashSet::new();
    
    for word in &[
        "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is",
        "it", "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there",
        "these", "they", "this", "to", "was", "will", "with", "would", "am", "been", "being",
        "did", "do", "does", "doing", "had", "has", "have", "having", "he", "her", "here",
        "hers", "herself", "him", "himself", "his", "how", "i", "me", "my", "myself",
        "our", "ours", "ourselves", "she", "should", "than", "that", "theirs", "them",
        "themselves", "there", "these", "they", "this", "those", "through", "too", "under",
        "until", "up", "very", "was", "we", "were", "what", "when", "where", "which", "while",
        "who", "whom", "why", "you", "your", "yours", "yourself", "yourselves",
        
        "will", "be", "the", "market", "price", "prediction", "event", "outcome",
        "contract", "share", "stock", "binary", "option", "trade", "trading",
        "buy", "sell", "yes", "no", "up", "down", "over", "under",
    ] {
        set.insert(*word);
    }
    
    set
}

/// 文本向量化器配置
#[derive(Debug, Clone)]
pub struct VectorizerConfig {
    pub use_stemming: bool,
    pub filter_stop_words: bool,
    pub min_word_length: usize,
    pub max_df_ratio: f64,
    pub min_df: usize,
    pub normalize: bool,
    pub custom_stop_words: HashSet<String>,
    pub max_features: Option<usize>,
}

impl Default for VectorizerConfig {
    fn default() -> Self {
        Self {
            use_stemming: true,
            filter_stop_words: true,
            min_word_length: 2,
            max_df_ratio: 0.8,
            min_df: 1,
            normalize: true,
            custom_stop_words: HashSet::new(),
            max_features: MAX_VOCAB_SIZE,
        }
    }
}

/// 包装 Stemmer，手动实现 Debug 和 Clone
struct StemmerWrapper(Stemmer);

impl StemmerWrapper {
    fn new(algorithm: Algorithm) -> Self {
        Self(Stemmer::create(algorithm))
    }
    
    fn stem(&self, word: &str) -> String {
        self.0.stem(word).to_string()
    }
}

// 手动实现 Debug
impl std::fmt::Debug for StemmerWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StemmerWrapper").finish()
    }
}

// 手动实现 Clone
impl Clone for StemmerWrapper {
    fn clone(&self) -> Self {
        Self(Stemmer::create(Algorithm::English))
    }
}

/// 文本向量化器
#[derive(Debug, Clone)]
pub struct TextVectorizer {
    config: VectorizerConfig,
    stemmer: Option<StemmerWrapper>,
    stop_words: HashSet<String>,
    vocabulary: HashMap<String, usize>,
    idf: Vec<f64>,
    n_docs: usize,
    fitted: bool,
}

impl TextVectorizer {
    pub fn new(config: VectorizerConfig) -> Self {
        let stemmer = if config.use_stemming {
            Some(StemmerWrapper::new(Algorithm::English))
        } else {
            None
        };
        
        let mut stop_words = get_stop_words()
            .into_iter()
            .map(String::from)
            .collect::<HashSet<_>>();
        
        for word in &config.custom_stop_words {
            stop_words.insert(word.clone());
        }
        
        Self {
            config,
            stemmer,
            stop_words,
            vocabulary: HashMap::new(),
            idf: Vec::new(),
            n_docs: 0,
            fitted: false,
        }
    }
    
    pub fn default() -> Self {
        Self::new(VectorizerConfig::default())
    }
    
    pub fn tokenize(&self, text: &str) -> Vec<String> {
        let text = text.to_lowercase();
        
        let words: Vec<String> = text
            .split(|c: char| !c.is_alphanumeric() && c != '-')
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect();
        
        let mut result = Vec::new();
        
        for word in words {
            if word.contains('-') {
                for part in word.split('-') {
                    if !part.is_empty() {
                        if let Some(processed) = self.process_token(part) {
                            result.push(processed);
                        }
                    }
                }
            } else {
                if let Some(processed) = self.process_token(&word) {
                    result.push(processed);
                }
            }
        }
        
        result
    }
    
    fn process_token(&self, token: &str) -> Option<String> {
        if token.chars().all(|c| c.is_ascii_digit()) {
            if token.len() == 4 && token.starts_with(|c: char| c == '1' || c == '2') {
                return Some(format!("YEAR_{}", token));
            }
            return None;
        }
        
        if token.len() < self.config.min_word_length {
            return None;
        }
        
        if self.config.filter_stop_words && self.stop_words.contains(token) {
            return None;
        }
        
        if let Some(stemmer) = &self.stemmer {
            Some(stemmer.stem(token))
        } else {
            Some(token.to_string())
        }
    }
    
    pub fn fit(&mut self, documents: &[String]) -> &mut Self {
        if documents.is_empty() {
            return self;
        }
        
        self.n_docs = documents.len();
        
        let mut all_tokens: Vec<Vec<String>> = Vec::with_capacity(documents.len());
        for doc in documents {
            all_tokens.push(self.tokenize(doc));
        }
        
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        
        for tokens in &all_tokens {
            let unique_tokens: HashSet<_> = tokens.iter().cloned().collect();
            for token in unique_tokens {
                *doc_freq.entry(token.clone()).or_insert(0) += 1;
            }
        }
        
        let max_df = (self.config.max_df_ratio * self.n_docs as f64).ceil() as usize;
        
        let mut vocab_with_freq: Vec<(String, usize)> = doc_freq
            .into_iter()
            .filter(|(_, df)| *df >= self.config.min_df && *df <= max_df)
            .collect();
        
        // 按文档频率降序排序
        vocab_with_freq.sort_by(|a, b| b.1.cmp(&a.1));
        
        // 如果有上限，截取前 max_features 个
        if let Some(max_features) = self.config.max_features {
            if vocab_with_freq.len() > max_features {
                vocab_with_freq.truncate(max_features);
            }
        }
        
        self.vocabulary = vocab_with_freq
            .into_iter()
            .enumerate()
            .map(|(i, (word, _))| (word, i))
            .collect();
        
        let vocab_size = self.vocabulary.len();
        self.idf = vec![0.0; vocab_size];
        
        let mut filtered_doc_freq = vec![0; vocab_size];
        
        for tokens in &all_tokens {
            let unique_tokens: HashSet<_> = tokens.iter().cloned().collect();
            for token in unique_tokens {
                if let Some(&idx) = self.vocabulary.get(&token) {
                    filtered_doc_freq[idx] += 1;
                }
            }
        }
        
        for (idx, &df) in filtered_doc_freq.iter().enumerate() {
            if df > 0 {
                self.idf[idx] = ((1.0 + self.n_docs as f64) / (1.0 + df as f64)).ln() + 1.0;
            } else {
                self.idf[idx] = 1.0;
            }
        }
        
        self.fitted = true;
        self
    }
    
    pub fn transform(&self, text: &str) -> Option<Array1<f64>> {
        if !self.fitted || self.vocabulary.is_empty() {
            return None;
        }
        
        let tokens = self.tokenize(text);
        let mut vector = vec![0.0; self.vocabulary.len()];
        
        for token in tokens {
            if let Some(&idx) = self.vocabulary.get(&token) {
                vector[idx] += 1.0;
            }
        }
        
        if vector.iter().all(|&x| x == 0.0) {
            return None;
        }
        
        for (idx, &idf_value) in self.idf.iter().enumerate() {
            if vector[idx] > 0.0 {
                vector[idx] *= idf_value;
            }
        }
        
        let mut array = Array1::from_vec(vector);
        
        if self.config.normalize {
            let norm = array.dot(&array).sqrt();
            if norm > 1e-12 {
                array.mapv_inplace(|x| x / norm);
            }
        }
        
        Some(array)
    }
    
    pub fn vocab_size(&self) -> usize {
        self.vocabulary.len()
    }
    
    pub fn is_fitted(&self) -> bool {
        self.fitted
    }
}