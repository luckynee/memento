use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use candle_core::{DType, Device};
use fastembed::BertTextEmbedding;

pub const DIMENSIONS: usize = 384;
const INFERENCE_CHUNK: usize = 8;

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

pub struct CandleEmbedder {
    model: Arc<Mutex<BertTextEmbedding>>,
}

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
impl Embedder for CandleEmbedder {
    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    async fn embed_documents(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(INFERENCE_CHUNK) {
            let model = Arc::clone(&self.model);
            let chunk = chunk.to_vec();
            let embedded = tokio::task::spawn_blocking(move || {
                model
                    .lock()
                    .map_err(|_| anyhow!("embedding model lock poisoned"))?
                    .embed(&chunk)
                    .context("embedding documents with Candle")
            })
            .await
            .context("joining Candle embedding inference")??;
            vectors.extend(embedded);
        }
        Ok(vectors)
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
    async fn real_model_loads() {
        let embedder = CandleEmbedder::new().await.unwrap();
        assert_eq!(
            embedder.embed_query("test".into()).await.unwrap().len(),
            DIMENSIONS
        );
    }
}
