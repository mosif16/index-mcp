use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use napi::{Error, Result};
use napi_derive::napi;
use once_cell::sync::Lazy;

const DEFAULT_MODEL: &str = "Xenova/bge-small-en-v1.5";

static EMBEDDING_CACHE: Lazy<Mutex<HashMap<String, Arc<Mutex<TextEmbedding>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
struct CachePoisoned;

impl From<CachePoisoned> for Error {
    fn from(_: CachePoisoned) -> Self {
        Error::from_reason("Embedding cache lock was poisoned")
    }
}

fn lock_cache() -> std::result::Result<
    MutexGuard<'static, HashMap<String, Arc<Mutex<TextEmbedding>>>>,
    CachePoisoned,
> {
    EMBEDDING_CACHE.lock().map_err(|_| CachePoisoned)
}

fn normalize_model_name(raw: Option<String>) -> (String, Option<String>) {
    match raw
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        Some(explicit) => (explicit.clone(), Some(explicit)),
        None => (DEFAULT_MODEL.to_string(), None),
    }
}

fn create_embedder(options: &TextInitOptions) -> Result<TextEmbedding> {
    TextEmbedding::try_new(options.clone()).map_err(|error| {
        Error::from_reason(format!("Failed to initialize embedding model: {error}"))
    })
}

fn parse_model(model: Option<String>) -> Result<(String, TextInitOptions)> {
    let (resolved_name, explicit) = normalize_model_name(model);

    let options = if let Some(model_name) = explicit {
        let parsed = EmbeddingModel::from_str(&model_name).map_err(|reason| {
            Error::from_reason(format!("Unknown embedding model '{model_name}': {reason}"))
        })?;
        TextInitOptions::new(parsed)
    } else {
        TextInitOptions::default()
    }
    .with_show_download_progress(false);

    Ok((resolved_name, options))
}

fn get_or_create_embedder(
    model_name: &str,
    options: &TextInitOptions,
) -> Result<Arc<Mutex<TextEmbedding>>> {
    {
        let cache = lock_cache()?;
        if let Some(existing) = cache.get(model_name) {
            return Ok(existing.clone());
        }
    }

    let embedder = Arc::new(Mutex::new(create_embedder(options)?));

    let mut cache = lock_cache()?;
    let entry = cache
        .entry(model_name.to_string())
        .or_insert_with(|| embedder.clone());
    Ok(entry.clone())
}

#[napi(object)]
pub struct NativeEmbeddingRequest {
    pub texts: Vec<String>,
    pub model: Option<String>,
    pub batch_size: Option<u32>,
}

#[napi]
pub fn generate_embeddings(request: NativeEmbeddingRequest) -> Result<Vec<Vec<f32>>> {
    let NativeEmbeddingRequest {
        texts,
        model,
        batch_size,
    } = request;

    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let (model_name, options) = parse_model(model)?;

    let embedder = get_or_create_embedder(&model_name, &options)?;
    let mut embedder_guard = embedder
        .lock()
        .map_err(|_| Error::from_reason("Embedding model lock was poisoned"))?;

    let batch_size = batch_size.map(|value| value as usize);
    let embeddings = embedder_guard
        .embed(texts, batch_size)
        .map_err(|error| Error::from_reason(format!("Embedding generation failed: {error}")))?;

    Ok(embeddings)
}

#[napi]
pub fn clear_embedding_cache() -> Result<()> {
    let mut cache = lock_cache()?;
    cache.clear();
    Ok(())
}
