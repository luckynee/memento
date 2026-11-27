use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
#[cfg(feature = "embed-candle")]
use candle_core::{DType, Device};
#[cfg(feature = "embed-candle")]
use fastembed::BertTextEmbedding;
#[cfg(feature = "embed-onnx")]
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

pub const DIMENSIONS: usize = 384;

#[async_trait]
pub trait Embedder: Send + Sync {
    fn dimensions(&self) -> usize;
    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;

    async fn embed_query(&self, text: String) -> Result<Vec<f32>> {
        self.embed_documents(vec![text])
            .await?
            .pop()
            .context("embedder returned no query vector")
    }
}

#[cfg(feature = "embed-onnx")]
pub struct FastEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
}

#[cfg(feature = "embed-onnx")]
impl FastEmbedder {
    pub async fn new(cache_dir: Option<std::path::PathBuf>) -> Result<Self> {
        let model = tokio::task::spawn_blocking(move || {
            let mut options = TextInitOptions::new(EmbeddingModel::BGESmallENV15);
            if let Some(cache_dir) = cache_dir {
                options = options.with_cache_dir(cache_dir);
            }
            TextEmbedding::try_new(options).context("initializing BGE embedding model")
        })
        .await
        .context("joining embedding initialization")??;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }
}

#[async_trait]
#[cfg(feature = "embed-onnx")]
impl Embedder for FastEmbedder {
    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            model
                .lock()
                .map_err(|_| anyhow!("embedding model lock poisoned"))?
                .embed(texts, None)
                .context("embedding documents")
        })
        .await
        .context("joining embedding inference")?
    }
}

#[cfg(feature = "embed-candle")]
pub struct CandleEmbedder {
    model: Arc<Mutex<BertTextEmbedding>>,
}

#[cfg(feature = "embed-candle")]
impl CandleEmbedder {
    pub async fn new() -> Result<Self> {
        let model = tokio::task::spawn_blocking(|| {
            BertTextEmbedding::from_hf("BAAI/bge-small-en-v1.5", &Device::Cpu, DType::F32, 512)
                .context("initializing Candle BGE embedding model")
        })
        .await
        .context("joining Candle embedding initialization")??;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }
}

#[async_trait]
#[cfg(feature = "embed-candle")]
impl Embedder for CandleEmbedder {
    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            model
                .lock()
                .map_err(|_| anyhow!("embedding model lock poisoned"))?
                .embed(&texts)
                .context("embedding documents with Candle")
        })
        .await
        .context("joining Candle embedding inference")?
    }
}

pub async fn create_embedder(
    selection: &str,
    cache_dir: Option<std::path::PathBuf>,
) -> Result<Arc<dyn Embedder>> {
    match selection {
        "onnx" => {
            #[cfg(feature = "embed-onnx")]
            {
                Ok(Arc::new(FastEmbedder::new(cache_dir).await?))
            }
            #[cfg(not(feature = "embed-onnx"))]
            {
                let _ = cache_dir;
                Err(anyhow!(
                    "ONNX embedder was not compiled in; enable embed-onnx"
                ))
            }
        }
        "candle" => {
            #[cfg(feature = "embed-candle")]
            {
                let _ = cache_dir;
                Ok(Arc::new(CandleEmbedder::new().await?))
            }
            #[cfg(not(feature = "embed-candle"))]
            {
                let _ = cache_dir;
                Err(anyhow!(
                    "Candle embedder was not compiled in; enable embed-candle"
                ))
            }
        }
        _ => Err(anyhow!("invalid MEMENTO_EMBEDDER: {selection}")),
    }
}

pub fn default_embedder() -> &'static str {
    if cfg!(feature = "embed-onnx") {
        "onnx"
    } else {
        "candle"
    }
}

#[derive(Default)]
pub struct FakeEmbedder;

#[async_trait]
impl Embedder for FakeEmbedder {
    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .into_iter()
            .map(|text| {
                let mut vector = vec![0.0; DIMENSIONS];
                for word in text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|word| !word.is_empty())
                {
                    let hash = word
                        .to_lowercase()
                        .bytes()
                        .fold(14695981039346656037u64, |hash, byte| {
                            (hash ^ u64::from(byte)).wrapping_mul(1099511628211)
                        });
                    vector[hash as usize % DIMENSIONS] += 1.0;
                }
                if vector.iter().all(|value| *value == 0.0) {
                    vector[0] = 1.0;
                }
                vector
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_vectors_are_deterministic() {
        let embedder = FakeEmbedder;
        let first = embedder.embed_query("segment.rs".into()).await.unwrap();
        assert_eq!(first.len(), DIMENSIONS);
        assert_eq!(
            first,
            embedder.embed_query("segment.rs".into()).await.unwrap()
        );
        assert_ne!(
            first,
            embedder.embed_query("different".into()).await.unwrap()
        );
    }

    #[tokio::test]
    #[ignore]
    #[cfg(feature = "embed-onnx")]
    async fn real_model_loads() {
        let embedder = FastEmbedder::new(None).await.unwrap();
        assert_eq!(
            embedder.embed_query("test".into()).await.unwrap().len(),
            DIMENSIONS
        );
    }
}
