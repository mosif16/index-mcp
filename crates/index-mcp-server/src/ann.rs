use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hnsw_rs::prelude::*;
use tempfile::NamedTempFile;
use uuid::Uuid;

pub const ANN_META_BASENAME_KEY: &str = "embedding_ann_basename";
pub const ANN_META_MAPPING_KEY: &str = "embedding_ann_mapping";

#[derive(Debug, Clone)]
pub struct AnnIndexInfo {
    pub basename: String,
    pub mapping_filename: String,
    pub dimension: usize,
}

#[derive(Debug, Clone)]
pub struct AnnSearchIndex {
    ann_dir: PathBuf,
    basename: String,
    pub id_lookup: Vec<String>,
}

pub fn sanitize_label(input: &str) -> String {
    let mut sanitized = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        "ann".to_string()
    } else {
        sanitized
    }
}

pub fn build_ann_index(
    ann_dir: &Path,
    base_prefix: &str,
    embeddings: &[(String, Vec<f32>)],
) -> Result<Option<AnnIndexInfo>> {
    if embeddings.is_empty() {
        return Ok(None);
    }

    let dimension = embeddings[0].1.len();
    if dimension == 0 {
        return Ok(None);
    }

    fs::create_dir_all(ann_dir)
        .with_context(|| format!("failed to create ann directory at {}", ann_dir.display()))?;

    let total = embeddings.len();
    let sanitized_prefix = sanitize_label(base_prefix);
    let requested_basename = format!("{}-{}", sanitized_prefix, Uuid::new_v4().simple());

    let max_nb_connection = 32usize;
    let ef_construction = 200usize;
    // hnsw_rs::hnsw::NB_LAYER_MAX is 16 and Description::dump expects exactly that value.
    // Using a smaller layer count triggers an "unexpected error" during file_dump.
    let nb_layer = 16usize;

    let mut hnsw = Hnsw::<f32, DistCosine>::new(
        max_nb_connection,
        total,
        nb_layer,
        ef_construction,
        DistCosine {},
    );
    hnsw.set_extend_candidates(true);
    hnsw.set_keeping_pruned(true);

    for (idx, (_, vector)) in embeddings.iter().enumerate() {
        if vector.len() != dimension {
            continue;
        }
        hnsw.insert((vector.as_slice(), idx));
    }

    let basename = hnsw
        .file_dump(ann_dir, &requested_basename)
        .with_context(|| {
            format!(
                "failed to persist ANN index to disk (dir=\"{}\", basename=\"{}\")",
                ann_dir.display(),
                requested_basename
            )
        })?;

    let mapping_filename = format!("{basename}.ids");
    let mapping_path = ann_dir.join(&mapping_filename);

    let mut temp = NamedTempFile::new_in(ann_dir)
        .context("failed to create temporary mapping file for ANN index")?;
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        for (chunk_id, _) in embeddings.iter() {
            writeln!(writer, "{}", chunk_id)
                .with_context(|| format!("failed to write mapping entry for {chunk_id}"))?;
        }
        writer.flush().context("failed to flush ANN mapping file")?;
    }
    temp.persist(&mapping_path).with_context(|| {
        format!(
            "failed to persist mapping file to {}",
            mapping_path.display()
        )
    })?;

    Ok(Some(AnnIndexInfo {
        basename,
        mapping_filename,
        dimension,
    }))
}

pub fn remove_ann_index(ann_dir: &Path, basename: &str) -> Result<()> {
    if basename.trim().is_empty() {
        return Ok(());
    }
    let graph_path = ann_dir.join(format!("{basename}.hnsw.graph"));
    let data_path = ann_dir.join(format!("{basename}.hnsw.data"));
    let ids_path = ann_dir.join(format!("{basename}.ids"));

    let _ = fs::remove_file(graph_path);
    let _ = fs::remove_file(data_path);
    let _ = fs::remove_file(ids_path);
    Ok(())
}

pub fn load_ann_index(ann_dir: &Path, basename: &str) -> Result<Option<AnnSearchIndex>> {
    if basename.trim().is_empty() {
        return Ok(None);
    }

    let graph_path = ann_dir.join(format!("{basename}.hnsw.graph"));
    let data_path = ann_dir.join(format!("{basename}.hnsw.data"));
    let ids_path = ann_dir.join(format!("{basename}.ids"));

    if !(graph_path.exists() && data_path.exists() && ids_path.exists()) {
        return Ok(None);
    }

    // Attempt to open the index once to ensure the files are readable before
    // returning a handle that will lazily reload on demand during search.
    {
        let loader = HnswIo::new(ann_dir, basename);
        let index = loader
            .load_hnsw_with_dist::<f32, DistCosine>(DistCosine {})
            .context("failed to load ANN graph")?;
        drop(index);
    }

    let file = fs::File::open(&ids_path)
        .with_context(|| format!("failed to open ANN mapping file {}", ids_path.display()))?;
    let reader = BufReader::new(file);
    let mut id_lookup = Vec::new();
    for line in reader.lines() {
        let value = line.context("failed to parse ANN mapping entry")?;
        if !value.is_empty() {
            id_lookup.push(value);
        }
    }

    Ok(Some(AnnSearchIndex {
        ann_dir: ann_dir.to_path_buf(),
        basename: basename.to_string(),
        id_lookup,
    }))
}

pub fn ann_base_prefix(database_path: &Path, model: &str, backend_label: &str) -> String {
    let stem = database_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("index");
    let sanitized_stem = sanitize_label(stem);
    let sanitized_model = sanitize_label(model);
    let sanitized_backend = sanitize_label(backend_label);
    format!(
        "{}-{}-{}",
        sanitized_stem, sanitized_model, sanitized_backend
    )
}

pub fn ann_directory(database_path: &Path) -> PathBuf {
    match database_path.parent() {
        Some(parent) => parent.join(".ann"),
        None => PathBuf::from(".ann"),
    }
}

impl AnnSearchIndex {
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
    ) -> Result<Vec<Neighbour>, anyhow::Error> {
        let loader = HnswIo::new(&self.ann_dir, &self.basename);
        let index = loader
            .load_hnsw_with_dist::<f32, DistCosine>(DistCosine {})
            .context("failed to load ANN graph")?;
        Ok(index.search(query, k, ef))
    }

    pub fn basename(&self) -> &str {
        &self.basename
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn sanitize_label_replaces_non_alphanumeric_chars() {
        assert_eq!(sanitize_label("foo"), "foo");
        assert_eq!(sanitize_label("foo/bar"), "foo_bar");
        assert_eq!(sanitize_label("a b@c"), "a_b_c");
        assert_eq!(sanitize_label(""), "ann");
    }

    #[test]
    fn ann_base_prefix_sanitizes_all_components() {
        let db_path = PathBuf::from("/tmp/.cache/index.sqlite");
        let prefix = ann_base_prefix(&db_path, "BAAI/bge-base-en-v1.5", "onnx-quantized");
        assert!(prefix.starts_with("index-"), "unexpected prefix: {prefix}");
        assert!(
            prefix.contains("BAAI_bge-base-en-v1.5"),
            "missing model: {prefix}"
        );
        assert!(
            prefix.ends_with("onnx-quantized"),
            "missing backend: {prefix}"
        );
        assert!(!prefix.contains('/'));
    }

    #[test]
    fn build_ann_index_round_trips() -> Result<()> {
        let temp = tempdir()?;
        let ann_dir = temp.path();
        let embeddings = vec![
            ("chunk-a".to_string(), vec![1.0_f32, 0.0, 0.0]),
            ("chunk-b".to_string(), vec![0.0_f32, 1.0, 0.0]),
            ("chunk-c".to_string(), vec![0.0_f32, 0.0, 1.0]),
        ];

        let info = build_ann_index(ann_dir, "my prefix", &embeddings)?
            .expect("ANN info should be returned");

        assert_eq!(info.dimension, 3);
        assert!(info.basename.starts_with("my_prefix"));

        let graph_path = ann_dir.join(format!("{}.hnsw.graph", info.basename));
        let data_path = ann_dir.join(format!("{}.hnsw.data", info.basename));
        let mapping_path = ann_dir.join(&info.mapping_filename);

        assert!(graph_path.exists(), "graph file missing");
        assert!(data_path.exists(), "data file missing");
        assert!(mapping_path.exists(), "mapping file missing");

        let index =
            load_ann_index(ann_dir, &info.basename)?.expect("loaded ANN index should exist");
        assert_eq!(index.id_lookup.len(), embeddings.len());

        let neighbours = index.search(&embeddings[0].1, 1, 64)?;
        assert_eq!(neighbours.len(), 1);
        let neighbour = &neighbours[0];
        let resolved_id = index
            .id_lookup
            .get(neighbour.d_id)
            .expect("neighbour id should map");
        assert_eq!(resolved_id, "chunk-a");

        remove_ann_index(ann_dir, &info.basename)?;
        assert!(!graph_path.exists(), "graph file should be removed");
        assert!(!data_path.exists(), "data file should be removed");
        assert!(!mapping_path.exists(), "mapping file should be removed");

        Ok(())
    }
}
