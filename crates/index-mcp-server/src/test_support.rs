use std::fs;
use std::path::Path;

use anyhow::Result;
use tempfile::TempDir;

use crate::bundle::{context_bundle, ContextBundleParams, ContextBundleResponse};
use crate::ingest::{ingest_codebase, EmbeddingParams, IngestParams, IngestResponse};
use crate::search::{semantic_search, SemanticSearchParams, SemanticSearchResponse, SummaryMode};

pub(crate) struct TestWorkspace {
    tempdir: TempDir,
    database_name: String,
}

impl TestWorkspace {
    pub fn new() -> Result<Self> {
        Ok(Self {
            tempdir: tempfile::tempdir()?,
            database_name: "test-index.sqlite".to_string(),
        })
    }

    pub fn root(&self) -> &Path {
        self.tempdir.path()
    }

    pub fn write_file(&self, relative: &str, contents: &str) -> Result<()> {
        let path = self.root().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    pub async fn ingest(&self, configure: impl FnOnce(&mut IngestParams)) -> IngestResponse {
        let mut params = IngestParams {
            root: Some(self.root().to_string_lossy().to_string()),
            include: None,
            exclude: None,
            database_name: Some(self.database_name.clone()),
            max_file_size_bytes: None,
            store_file_content: Some(true),
            paths: None,
            auto_evict: None,
            max_database_size_bytes: None,
            embedding: Some(EmbeddingParams {
                enabled: Some(true),
                backend: Some("mock".to_string()),
                model: None,
                chunk_size_tokens: None,
                chunk_overlap_tokens: None,
                batch_size: None,
            }),
        };
        configure(&mut params);
        ingest_codebase(params)
            .await
            .expect("ingest should succeed")
    }

    pub async fn semantic_search(
        &self,
        configure: impl FnOnce(&mut SemanticSearchParams),
    ) -> SemanticSearchResponse {
        let mut params = SemanticSearchParams {
            root: Some(self.root().to_string_lossy().to_string()),
            query: "".to_string(),
            database_name: Some(self.database_name.clone()),
            limit: Some(8),
            model: None,
            language: None,
            path_prefix: None,
            path_contains: None,
            classification: None,
            summary_mode: Some(SummaryMode::Brief),
            max_context_before: None,
            max_context_after: None,
            recent_hits: None,
        };
        configure(&mut params);
        semantic_search(params)
            .await
            .expect("semantic_search should succeed")
    }

    pub async fn bundle(
        &self,
        configure: impl FnOnce(&mut ContextBundleParams),
    ) -> ContextBundleResponse {
        let mut params = ContextBundleParams {
            root: Some(self.root().to_string_lossy().to_string()),
            database_name: Some(self.database_name.clone()),
            file: String::new(),
            symbol: None,
            ranges: None,
            focus_line: None,
            max_snippets: Some(8),
            max_neighbors: Some(8),
            budget_tokens: Some(800),
            query: None,
            context_goals: None,
        };
        configure(&mut params);
        context_bundle(params)
            .await
            .expect("context_bundle should succeed")
    }
}
