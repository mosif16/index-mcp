use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use dirs::home_dir;
use rmcp::schemars::JsonSchema;
use serde::Serialize;
use serde_json::{json, Map, Value};

/// Level for environment diagnostics.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticLevel {
    Warn,
    Error,
}

/// Source for the resolved model cache directory.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Hash)]
pub enum ModelCacheSource {
    #[serde(rename = "FASTEMBED_CACHE_DIR")]
    FastembedCacheDir,
    #[serde(rename = "INDEX_MCP_MODEL_CACHE_DIR")]
    IndexMcpModelCacheDir,
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "tmp")]
    Tmp,
}

impl ModelCacheSource {
    fn as_str(&self) -> &'static str {
        match self {
            ModelCacheSource::FastembedCacheDir => "FASTEMBED_CACHE_DIR",
            ModelCacheSource::IndexMcpModelCacheDir => "INDEX_MCP_MODEL_CACHE_DIR",
            ModelCacheSource::Default => "default",
            ModelCacheSource::Tmp => "tmp",
        }
    }
}

/// Diagnostic describing configuration or filesystem issues discovered while
/// preparing the embedding model cache directory.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentDiagnostic {
    pub level: DiagnosticLevel,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Map<String, Value>>,
}

/// Snapshot of the runtime environment relevant to embedding model caching.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelCacheInfo {
    pub directory: Option<String>,
    pub source: Option<ModelCacheSource>,
    pub diagnostics: Vec<EnvironmentDiagnostic>,
}

struct DirectoryCandidate {
    path: PathBuf,
    source: ModelCacheSource,
}

/// Resolve and prepare the directory used for caching embedding models. The
/// behaviour mirrors the legacy Node server so cached models remain
/// interchangeable between runtimes.
pub fn collect_model_cache_info() -> ModelCacheInfo {
    let mut diagnostics = Vec::new();
    let mut recorded_failures = HashSet::new();

    let mut resolved_candidate: Option<DirectoryCandidate> = None;

    if let Some(path) = std::env::var("FASTEMBED_CACHE_DIR")
        .ok()
        .and_then(normalize_non_empty)
    {
        let candidate = DirectoryCandidate {
            path: to_absolute_path(&path),
            source: ModelCacheSource::FastembedCacheDir,
        };
        if prepare_directory(&candidate, &mut diagnostics, &mut recorded_failures) {
            resolved_candidate = Some(candidate);
        }
    }

    if resolved_candidate.is_none() {
        for candidate in default_candidates() {
            if prepare_directory(&candidate, &mut diagnostics, &mut recorded_failures) {
                resolved_candidate = Some(candidate);
                break;
            }
        }
    }

    if let Some(candidate) = resolved_candidate {
        let directory = candidate.path.display().to_string();
        std::env::set_var("FASTEMBED_CACHE_DIR", &directory);
        if std::env::var("INDEX_MCP_MODEL_CACHE_DIR")
            .ok()
            .and_then(normalize_non_empty)
            .is_none()
        {
            std::env::set_var("INDEX_MCP_MODEL_CACHE_DIR", &directory);
        }

        ModelCacheInfo {
            directory: Some(directory),
            source: Some(candidate.source),
            diagnostics,
        }
    } else {
        diagnostics.push(EnvironmentDiagnostic {
            level: DiagnosticLevel::Error,
            code: "model_cache_unavailable".to_string(),
            message: "[index-mcp] Unable to configure a writable embedding model cache directory."
                .to_string(),
            context: None,
        });

        ModelCacheInfo {
            directory: None,
            source: None,
            diagnostics,
        }
    }
}

fn normalize_non_empty(value: String) -> Option<String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn default_candidates() -> Vec<DirectoryCandidate> {
    let mut candidates = Vec::new();

    if let Some(path) = std::env::var("INDEX_MCP_MODEL_CACHE_DIR")
        .ok()
        .and_then(normalize_non_empty)
    {
        candidates.push(DirectoryCandidate {
            path: to_absolute_path(&path),
            source: ModelCacheSource::IndexMcpModelCacheDir,
        });
    }

    if let Some(home) = home_dir() {
        let candidate = home.join(".index-mcp").join("models");
        candidates.push(DirectoryCandidate {
            path: candidate,
            source: ModelCacheSource::Default,
        });
    }

    let tmp = std::env::temp_dir().join("index-mcp").join("models");
    candidates.push(DirectoryCandidate {
        path: tmp,
        source: ModelCacheSource::Tmp,
    });

    candidates
}

fn to_absolute_path(candidate: &str) -> PathBuf {
    let path = PathBuf::from(candidate);
    if path.is_absolute() {
        path
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path
    }
}

fn prepare_directory(
    candidate: &DirectoryCandidate,
    diagnostics: &mut Vec<EnvironmentDiagnostic>,
    recorded_failures: &mut HashSet<String>,
) -> bool {
    if ensure_directory(candidate, diagnostics, recorded_failures).is_err() {
        return false;
    }

    if let Err(err) = check_writable(&candidate.path) {
        record_diagnostic(
            diagnostics,
            recorded_failures,
            EnvironmentDiagnostic {
                level: DiagnosticLevel::Error,
                code: "model_cache_unwritable".to_string(),
                message: format!(
                    "[index-mcp] Model cache directory {} is not writable.",
                    candidate.path.display()
                ),
                context: Some(diagnostic_context(candidate, Some(&err))),
            },
        );
        return false;
    }

    true
}

fn ensure_directory(
    candidate: &DirectoryCandidate,
    diagnostics: &mut Vec<EnvironmentDiagnostic>,
    recorded_failures: &mut HashSet<String>,
) -> Result<(), io::Error> {
    match fs::metadata(&candidate.path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                record_diagnostic(
                    diagnostics,
                    recorded_failures,
                    EnvironmentDiagnostic {
                        level: DiagnosticLevel::Error,
                        code: "model_cache_not_directory".to_string(),
                        message: format!(
                            "[index-mcp] Model cache path {} exists but is not a directory.",
                            candidate.path.display()
                        ),
                        context: Some(diagnostic_context(candidate, None)),
                    },
                );
                return Err(io::Error::new(io::ErrorKind::Other, "not a directory"));
            }
        }
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                record_diagnostic(
                    diagnostics,
                    recorded_failures,
                    EnvironmentDiagnostic {
                        level: DiagnosticLevel::Error,
                        code: "model_cache_stat_failed".to_string(),
                        message: format!(
                            "[index-mcp] Unable to inspect model cache directory {}.",
                            candidate.path.display()
                        ),
                        context: Some(diagnostic_context(candidate, Some(&err))),
                    },
                );
                return Err(err);
            }
        }
    }

    if let Err(err) = fs::create_dir_all(&candidate.path) {
        record_diagnostic(
            diagnostics,
            recorded_failures,
            EnvironmentDiagnostic {
                level: DiagnosticLevel::Error,
                code: "model_cache_creation_failed".to_string(),
                message: format!(
                    "[index-mcp] Unable to create model cache directory {}.",
                    candidate.path.display()
                ),
                context: Some(diagnostic_context(candidate, Some(&err))),
            },
        );
        return Err(err);
    }

    Ok(())
}

fn check_writable(path: &Path) -> Result<(), io::Error> {
    let probe_path = path.join(".mcp-write-test");
    match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&probe_path)
    {
        Ok(_) => {
            let _ = fs::remove_file(probe_path);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn diagnostic_context(
    candidate: &DirectoryCandidate,
    error: Option<&io::Error>,
) -> Map<String, Value> {
    let mut context = Map::new();
    context.insert(
        "source".to_string(),
        Value::String(candidate.source.as_str().to_string()),
    );
    context.insert(
        "path".to_string(),
        Value::String(candidate.path.display().to_string()),
    );

    if let Some(err) = error {
        context.insert("error".to_string(), Value::String(err.to_string()));
        context.insert(
            "code".to_string(),
            Value::String(format!("{:?}", err.kind())),
        );
    }

    context
}

fn record_diagnostic(
    diagnostics: &mut Vec<EnvironmentDiagnostic>,
    recorded_failures: &mut HashSet<String>,
    diagnostic: EnvironmentDiagnostic,
) {
    if diagnostic.level == DiagnosticLevel::Error {
        let key = json!({
            "code": diagnostic.code,
            "source": diagnostic
                .context
                .as_ref()
                .and_then(|ctx| ctx.get("source"))
                .cloned(),
            "path": diagnostic
                .context
                .as_ref()
                .and_then(|ctx| ctx.get("path"))
                .cloned(),
        })
        .to_string();

        if !recorded_failures.insert(key) {
            return;
        }
    }

    diagnostics.push(diagnostic);
}
