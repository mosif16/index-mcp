use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use candle_core::Device;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use once_cell::sync::Lazy;
use sentence_transformers_rs::sentence_transformer::{
    SentenceTransformer, SentenceTransformerBuilder, Which,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbeddingError {
    #[error("{0}")]
    Message(String),
}

impl From<anyhow::Error> for EmbeddingError {
    fn from(value: anyhow::Error) -> Self {
        EmbeddingError::Message(value.to_string())
    }
}

impl From<std::io::Error> for EmbeddingError {
    fn from(value: std::io::Error) -> Self {
        EmbeddingError::Message(value.to_string())
    }
}

impl From<String> for EmbeddingError {
    fn from(value: String) -> Self {
        EmbeddingError::Message(value)
    }
}

impl From<&str> for EmbeddingError {
    fn from(value: &str) -> Self {
        EmbeddingError::Message(value.to_string())
    }
}

pub type EmbeddingHandle = Arc<Mutex<Box<dyn EmbeddingRunner + Send>>>;

pub trait EmbeddingRunner {
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut embeddings = self.embed_batch(&[text.to_owned()])?;
        Ok(embeddings.pop().unwrap_or_default())
    }
}

#[derive(Debug, Clone)]
pub struct CandleModel {
    pub model_id: String,
}

#[derive(Debug, Clone)]
pub enum EmbeddingBackend {
    FastEmbed {
        model_variant: EmbeddingModel,
        quantized: bool,
    },
    Candle {
        model: CandleModel,
        batch_size: Option<usize>,
    },
}

impl EmbeddingBackend {
    pub fn metadata_label(&self) -> String {
        match self {
            EmbeddingBackend::FastEmbed { quantized, .. } => {
                if *quantized {
                    "onnx-quantized".to_string()
                } else {
                    "onnx".to_string()
                }
            }
            EmbeddingBackend::Candle { .. } => "candle".to_string(),
        }
    }

    pub fn cache_namespace(&self) -> &'static str {
        match self {
            EmbeddingBackend::FastEmbed { .. } => "fastembed",
            EmbeddingBackend::Candle { .. } => "candle",
        }
    }

    pub fn is_quantized(&self) -> bool {
        matches!(
            self,
            EmbeddingBackend::FastEmbed {
                quantized: true,
                ..
            }
        )
    }
}

struct FastEmbedRunner {
    inner: TextEmbedding,
    batch_size: Option<usize>,
}

impl EmbeddingRunner for FastEmbedRunner {
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let inputs: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        self.inner
            .embed(inputs, self.batch_size)
            .map_err(|error| EmbeddingError::Message(error.to_string()))
    }
}

struct CandleRunner {
    model: SentenceTransformer,
}

impl EmbeddingRunner for CandleRunner {
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let embeddings = self
            .model
            .embed(&refs)
            .map_err(|error| EmbeddingError::Message(error.to_string()))?;
        Ok(embeddings)
    }
}

static EMBEDDER_CACHE: Lazy<
    Mutex<HashMap<String, Arc<once_cell::sync::OnceCell<EmbeddingHandle>>>>,
> = Lazy::new(|| Mutex::new(HashMap::new()));

pub fn get_or_create_embedding_runner(
    backend: &EmbeddingBackend,
    model_name: &str,
    batch_size: Option<usize>,
) -> Result<EmbeddingHandle, EmbeddingError> {
    let cache_key = format!(
        "{}::{}::{}",
        backend.cache_namespace(),
        model_name.trim(),
        batch_size.unwrap_or(0)
    );

    let entry = {
        let mut cache = EMBEDDER_CACHE
            .lock()
            .map_err(|error| EmbeddingError::Message(error.to_string()))?;
        cache
            .entry(cache_key)
            .or_insert_with(|| Arc::new(once_cell::sync::OnceCell::new()))
            .clone()
    };

    let backend_clone = backend.clone();
    let handle = entry
        .get_or_try_init(|| {
            create_runner(&backend_clone, batch_size)
                .map(|runner| Arc::new(Mutex::new(runner)) as EmbeddingHandle)
        })
        .map_err(|error| EmbeddingError::Message(error.to_string()))?;

    Ok(handle.clone())
}

fn create_runner(
    backend: &EmbeddingBackend,
    batch_size: Option<usize>,
) -> Result<Box<dyn EmbeddingRunner + Send>, EmbeddingError> {
    match backend {
        EmbeddingBackend::FastEmbed {
            model_variant,
            quantized: _,
        } => {
            let options =
                TextInitOptions::new(model_variant.clone()).with_show_download_progress(false);
            let model = TextEmbedding::try_new(options)
                .map_err(|error| EmbeddingError::Message(error.to_string()))?;
            Ok(Box::new(FastEmbedRunner {
                inner: model,
                batch_size,
            }))
        }
        EmbeddingBackend::Candle {
            model,
            batch_size: candle_batch,
        } => {
            let device = Device::Cpu;
            let which = candle_which_for(&model.model_id)?;
            let builder = SentenceTransformerBuilder::with_sentence_transformer(&which)
                .with_device(&device)
                .batch_size(candle_batch.or(batch_size).unwrap_or(2048));
            let sentence_transformer = builder
                .build()
                .map_err(|error| EmbeddingError::Message(error.to_string()))?;
            Ok(Box::new(CandleRunner {
                model: sentence_transformer,
            }))
        }
    }
}

pub fn parse_fastembed_model(model: &str) -> Result<EmbeddingModel, EmbeddingError> {
    EmbeddingModel::from_str(model).map_err(|error| {
        EmbeddingError::Message(format!("Unknown embedding model '{model}': {error}"))
    })
}

pub fn is_quantized_model(model: &EmbeddingModel) -> bool {
    matches!(
        model,
        EmbeddingModel::AllMiniLML6V2Q
            | EmbeddingModel::AllMiniLML12V2Q
            | EmbeddingModel::BGEBaseENV15Q
            | EmbeddingModel::BGELargeENV15Q
            | EmbeddingModel::BGESmallENV15Q
            | EmbeddingModel::NomicEmbedTextV15Q
            | EmbeddingModel::ParaphraseMLMiniLML12V2Q
            | EmbeddingModel::MxbaiEmbedLargeV1Q
            | EmbeddingModel::GTEBaseENV15Q
            | EmbeddingModel::GTELargeENV15Q
    )
}

pub fn parse_candle_model(model: &str) -> Result<CandleModel, EmbeddingError> {
    candle_which_for(model)?;

    Ok(CandleModel {
        model_id: model.to_string(),
    })
}

fn candle_which_for(model: &str) -> Result<Which, EmbeddingError> {
    let which = match model {
        "sentence-transformers/all-MiniLM-L6-v2" => Which::AllMiniLML6v2,
        "sentence-transformers/all-MiniLM-L12-v2" => Which::AllMiniLML12v2,
        "sentence-transformers/paraphrase-MiniLM-L6-v2" => Which::ParaphraseMiniLML6v2,
        "sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2" => {
            Which::ParaphraseMultilingualMiniLML12v2
        }
        "sentence-transformers/LaBSE" => Which::LaBSE,
        "sentence-transformers/paraphrase-multilingual-mpnet-base-v2" => {
            Which::ParaphraseMultilingualMpnetBaseV2
        }
        "sentence-transformers/distiluse-base-multilingual-cased-v2" => {
            Which::DistiluseBaseMultilingualCasedV2
        }
        "BAAI/bge-small-en-v1.5" => Which::BgeSmallEnV1_5,
        "BAAI/bge-base-en-v1.5" => Which::BgeBaseEnV1_5,
        "intfloat/multilingual-e5-large" => Which::MultilingualE5Large,
        "intfloat/multilingual-e5-base" => Which::MultilingualE5Base,
        "intfloat/multilingual-e5-small" => Which::MultilingualE5Small,
        "sentence-transformers/all-mpnet-base-v2" => Which::AllMpnetBaseV2,
        "sentence-transformers/paraphrase-mpnet-base-v2" => Which::ParaphraseMpnetBaseV2,
        other => {
            return Err(EmbeddingError::Message(format!(
                "Unsupported Candle embedding model '{other}'"
            )))
        }
    };

    Ok(which)
}

pub fn build_fastembed_backend(model: &str) -> Result<EmbeddingBackend, EmbeddingError> {
    let variant = parse_fastembed_model(model)?;
    let quantized = is_quantized_model(&variant);
    Ok(EmbeddingBackend::FastEmbed {
        model_variant: variant,
        quantized,
    })
}

pub fn build_candle_backend(
    model: &str,
    batch_size: Option<usize>,
) -> Result<EmbeddingBackend, EmbeddingError> {
    let candle_model = parse_candle_model(model)?;
    Ok(EmbeddingBackend::Candle {
        model: candle_model,
        batch_size,
    })
}
