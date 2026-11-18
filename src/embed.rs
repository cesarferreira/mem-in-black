//! Local transformer embeddings for semantic search.
//!
//! Models run on the CPU with candle. Weights are downloaded once from a pinned
//! revision and every file is verified against a SHA-256 digest before use.

use anyhow::{Context, Result, anyhow, ensure};
use candle_core::{Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pooling {
    Cls,
    Mean,
}

pub struct Spec {
    pub name: &'static str,
    pub repo: &'static str,
    pub revision: &'static str,
    /// (file, SHA-256, size in bytes)
    pub files: [(&'static str, &'static str, u64); 3],
    pub pooling: Pooling,
    pub query_prefix: &'static str,
    pub dimensions: usize,
    /// Similarity that justifies unprompted recall on its own: the 99th
    /// percentile of top-1 scores for unrelated queries in `quietmem eval`.
    pub recall_floor: f32,
    /// Lower similarity (95th percentile of unrelated) accepted when the record
    /// also shares a keyword with the prompt.
    pub assisted_floor: f32,
}

const RETRIEVAL_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// Best hybrid retrieval in `quietmem eval` on real Claude Mem history.
pub const DEFAULT_MODEL: &str = "arctic-m";

pub const MODELS: &[Spec] = &[
    Spec {
        name: "minilm",
        repo: "sentence-transformers/all-MiniLM-L6-v2",
        revision: "1110a243fdf4706b3f48f1d95db1a4f5529b4d41",
        files: [
            (
                "config.json",
                "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
                612,
            ),
            (
                "tokenizer.json",
                "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
                466_247,
            ),
            (
                "model.safetensors",
                "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
                90_868_376,
            ),
        ],
        pooling: Pooling::Mean,
        query_prefix: "",
        dimensions: 384,
        recall_floor: 0.70,
        assisted_floor: 0.62,
    },
    Spec {
        name: "bge-small",
        repo: "BAAI/bge-small-en-v1.5",
        revision: "5c38ec7c405ec4b44b94cc5a9bb96e735b38267a",
        files: [
            (
                "config.json",
                "094f8e891b932f2000c92cfc663bac4c62069f5d8af5b5278c4306aef3084750",
                743,
            ),
            (
                "tokenizer.json",
                "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
                711_396,
            ),
            (
                "model.safetensors",
                "3c9f31665447c8911517620762200d2245a2518d6e7208acc78cd9db317e21ad",
                133_466_304,
            ),
        ],
        pooling: Pooling::Cls,
        query_prefix: RETRIEVAL_PREFIX,
        dimensions: 384,
        recall_floor: 0.82,
        assisted_floor: 0.79,
    },
    Spec {
        name: "bge-base",
        repo: "BAAI/bge-base-en-v1.5",
        revision: "a5beb1e3e68b9ab74eb54cfd186867f64f240e1a",
        files: [
            (
                "config.json",
                "bc00af31a4a31b74040d73370aa83b62da34c90b75eb77bfa7db039d90abd591",
                777,
            ),
            (
                "tokenizer.json",
                "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
                711_396,
            ),
            (
                "model.safetensors",
                "c7c1988aae201f80cf91a5dbbd5866409503b89dcaba877ca6dba7dd0a5167d7",
                437_955_512,
            ),
        ],
        pooling: Pooling::Cls,
        query_prefix: RETRIEVAL_PREFIX,
        dimensions: 768,
        recall_floor: 0.78,
        assisted_floor: 0.74,
    },
    Spec {
        name: "arctic-m",
        repo: "Snowflake/snowflake-arctic-embed-m-v1.5",
        revision: "e58a8f756156a1293d763f17e3aae643474e9b8a",
        files: [
            (
                "config.json",
                "d2dddc06af0aeeb7fd3289a3dee1ac76dce0c3df2467cadd26f5f731f8d7919b",
                772,
            ),
            (
                "tokenizer.json",
                "91f1def9b9391fdabe028cd3f3fcc4efd34e5d1f08c3bf2de513ebb5911a1854",
                711_649,
            ),
            (
                "model.safetensors",
                "463d27a3e88a748997316367c419bbdab4d809300a5f24b9b19ba9b2dc08ffdd",
                435_588_776,
            ),
        ],
        pooling: Pooling::Cls,
        query_prefix: RETRIEVAL_PREFIX,
        dimensions: 768,
        recall_floor: 0.65,
        assisted_floor: 0.60,
    },
];

pub fn spec(name: &str) -> Result<&'static Spec> {
    MODELS.iter().find(|spec| spec.name == name).ok_or_else(|| {
        let names: Vec<_> = MODELS.iter().map(|spec| spec.name).collect();
        anyhow!("unknown model {name}; choose one of {}", names.join(", "))
    })
}

pub fn model_dir(data: &Path, spec: &Spec) -> PathBuf {
    data.join("models").join(spec.name).join(spec.revision)
}

/// Download any missing model files and verify them. Returns the model directory.
/// With `show_progress`, a live progress line is drawn on stderr.
pub fn install(data: &Path, spec: &Spec, show_progress: bool) -> Result<PathBuf> {
    let dir = model_dir(data, spec);
    let total: u64 = spec.files.iter().map(|(_, _, size)| size).sum();
    let mut progress = Progress::new(total, show_progress);
    crate::store::prepare_dir(&dir)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .https_only(true)
        .build()
        .into();
    for (file, digest, size) in spec.files {
        let path = dir.join(file);
        if fs::metadata(&path).is_ok_and(|meta| meta.len() == size) {
            progress.advance(size, false);
            continue;
        }
        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{file}",
            spec.repo, spec.revision
        );
        let mut response = agent
            .get(&url)
            .call()
            .with_context(|| format!("downloading {url}"))?;
        let mut reader = response.body_mut().with_config().limit(size + 1).reader();
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        let mut written = 0u64;
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            temp.write_all(&buffer[..read])?;
            written += read as u64;
            progress.advance(read as u64, true);
        }
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        ensure!(
            written == size && actual == digest,
            "{file} from {} failed verification (expected {digest}, got {actual})",
            spec.repo
        );
        temp.persist(&path)?;
    }
    progress.finish();
    Ok(dir)
}

/// A single self-overwriting stderr line: downloaded / total, percent, speed.
struct Progress {
    total: u64,
    done: u64,
    fetched: u64,
    started: std::time::Instant,
    drawn: Option<std::time::Instant>,
    visible: bool,
}

impl Progress {
    fn new(total: u64, visible: bool) -> Self {
        Self {
            total,
            done: 0,
            fetched: 0,
            started: std::time::Instant::now(),
            drawn: None,
            visible,
        }
    }

    /// `fetched` separates bytes from the network from files already on disk,
    /// so the speed reflects the download only.
    fn advance(&mut self, bytes: u64, fetched: bool) {
        self.done += bytes;
        if fetched {
            self.fetched += bytes;
        }
        if self
            .drawn
            .is_none_or(|drawn| drawn.elapsed() >= Duration::from_millis(100))
        {
            self.draw();
        }
    }

    fn draw(&mut self) {
        if !self.visible {
            return;
        }
        self.drawn = Some(std::time::Instant::now());
        let line = progress_line(self.done, self.total, self.fetched, self.started.elapsed());
        eprint!("\r\x1b[2K{line}");
    }

    fn finish(&mut self) {
        if self.visible && self.drawn.is_some() {
            self.draw();
            eprintln!();
        }
    }
}

fn progress_line(done: u64, total: u64, fetched: u64, elapsed: Duration) -> String {
    const MB: f64 = 1_000_000.0;
    let percent = (done as f64 / total.max(1) as f64 * 100.0).min(100.0);
    let width = 24;
    let filled = ((percent / 100.0) * width as f64).round() as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(width - filled));
    let seconds = elapsed.as_secs_f64();
    let speed = if seconds > 0.2 {
        format!("  {:.1} MB/s", fetched as f64 / MB / seconds)
    } else {
        String::new()
    };
    format!(
        "  {bar} {:>5.1} / {:.1} MB  {percent:>3.0}%{speed}",
        done as f64 / MB,
        total as f64 / MB
    )
}

pub fn installed(data: &Path, spec: &Spec) -> bool {
    let dir = model_dir(data, spec);
    spec.files
        .iter()
        .all(|(file, _, size)| fs::metadata(dir.join(file)).is_ok_and(|meta| meta.len() == *size))
}

pub struct Embedder {
    pub spec: &'static Spec,
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

const MAX_TOKENS: usize = 512;

impl Embedder {
    pub fn load(data: &Path, spec: &'static Spec) -> Result<Self> {
        ensure!(
            installed(data, spec),
            "model {} is not installed; run `quietmem embeddings enable --model {}`",
            spec.name,
            spec.name
        );
        let dir = model_dir(data, spec);
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(&fs::read_to_string(dir.join("config.json"))?)?;
        let mut tokenizer =
            Tokenizer::from_file(dir.join("tokenizer.json")).map_err(anyhow::Error::msg)?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(anyhow::Error::msg)?;
        // The weights were verified by SHA-256 at install time and are not modified afterwards.
        let weights = unsafe {
            VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DTYPE, &device)?
        };
        let model = BertModel::load(weights, &config)?;
        Ok(Self {
            spec,
            model,
            tokenizer,
            device,
        })
    }

    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let text = format!("{}{text}", self.spec.query_prefix);
        Ok(self.embed(&[text.as_str()])?.remove(0))
    }

    pub fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed(texts)
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // Group texts of similar length so short ones are not padded to long ones.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&index| texts[index].len());
        let mut vectors = vec![Vec::new(); texts.len()];
        for chunk in order.chunks(8) {
            let batch: Vec<_> = chunk.iter().map(|&index| texts[index]).collect();
            for (&index, vector) in chunk.iter().zip(self.embed_batch(&batch)?) {
                vectors[index] = vector;
            }
        }
        Ok(vectors)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(anyhow::Error::msg)?;
        let rows = encodings.len();
        let width = encodings[0].get_ids().len();
        let ids: Vec<u32> = encodings
            .iter()
            .flat_map(|e| e.get_ids().to_vec())
            .collect();
        let mask: Vec<u32> = encodings
            .iter()
            .flat_map(|e| e.get_attention_mask().to_vec())
            .collect();
        let ids = Tensor::from_vec(ids, (rows, width), &self.device)?;
        let mask = Tensor::from_vec(mask, (rows, width), &self.device)?;
        let types = ids.zeros_like()?;
        let output = self.model.forward(&ids, &types, Some(&mask))?;
        let pooled = match self.spec.pooling {
            Pooling::Cls => output.i((.., 0))?,
            Pooling::Mean => {
                let mask = mask.to_dtype(DTYPE)?.unsqueeze(2)?;
                let summed = output.broadcast_mul(&mask)?.sum(1)?;
                summed.broadcast_div(&mask.sum(1)?)?
            }
        };
        ensure!(
            pooled.dim(1)? == self.spec.dimensions,
            "{} produced {} dimensions, expected {}",
            self.spec.name,
            pooled.dim(1)?,
            self.spec.dimensions
        );
        let norms = pooled.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = pooled.broadcast_div(&norms)?;
        Ok(normalized.to_vec2::<f32>()?)
    }
}

pub enum Scope<'a> {
    All,
    Workspace(&'a str),
    Project(&'a str),
}

/// Brute-force cosine index over normalized vectors.
#[derive(Default)]
pub struct Index {
    rows: Vec<Row>,
}

struct Row {
    id: i64,
    workspace: String,
    project: String,
    vector: Vec<f32>,
}

impl Index {
    pub fn insert(&mut self, id: i64, workspace: String, project: String, vector: Vec<f32>) {
        self.rows.push(Row {
            id,
            workspace,
            project,
            vector,
        });
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn search(&self, query: &[f32], scope: &Scope<'_>, limit: usize) -> Vec<(i64, f32)> {
        let mut scored: Vec<_> = self
            .rows
            .iter()
            .filter(|row| match scope {
                Scope::All => true,
                Scope::Workspace(root) => row.workspace == *root,
                Scope::Project(project) => row.project == *project,
            })
            .map(|row| (row.id, dot(query, &row.vector)))
            .collect();
        let limit = limit.min(scored.len());
        if limit == 0 {
            return vec![];
        }
        scored.select_nth_unstable_by(limit - 1, |a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }
}

/// The configured model, from `embeddings.json` in the data directory.
pub fn configured(data: &Path) -> Result<Option<&'static Spec>> {
    let source = match fs::read_to_string(data.join("embeddings.json")) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: serde_json::Value = serde_json::from_str(&source)?;
    let name = value["model"]
        .as_str()
        .context("embeddings.json needs a model")?;
    Ok(Some(spec(name)?))
}

pub fn configure(data: &Path, model: Option<&Spec>) -> Result<()> {
    let path = data.join("embeddings.json");
    match model {
        Some(spec) => crate::install::atomic_write(
            &path,
            serde_json::json!({"model": spec.name})
                .to_string()
                .as_bytes(),
        ),
        None => match fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        },
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Reciprocal rank fusion of several ranked ID lists.
pub fn fuse(lists: &[Vec<i64>], limit: usize) -> Vec<i64> {
    const K: f64 = 60.0;
    let mut scores: Vec<(i64, f64)> = Vec::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            let score = 1.0 / (K + rank as f64 + 1.0);
            match scores.iter_mut().find(|(seen, _)| seen == id) {
                Some(entry) => entry.1 += score,
                None => scores.push((*id, score)),
            }
        }
    }
    scores.sort_by(|a, b| b.1.total_cmp(&a.1));
    scores.into_iter().take(limit).map(|(id, _)| id).collect()
}

pub fn to_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

pub fn from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_shows_size_percent_and_speed() {
        let line = progress_line(
            218_000_000,
            436_000_000,
            218_000_000,
            Duration::from_secs(10),
        );
        assert!(line.contains("218.0 / 436.0 MB"), "{line}");
        assert!(line.contains(" 50%"), "{line}");
        assert!(line.contains("21.8 MB/s"), "{line}");
        assert_eq!(line.matches('█').count(), 12);
    }

    #[test]
    fn fusion_rewards_agreement_between_rankings() {
        let fused = fuse(&[vec![1, 2, 3], vec![3, 1, 4]], 3);
        assert_eq!(fused, [1, 3, 2]);
    }

    #[test]
    fn index_returns_the_nearest_vectors_in_a_workspace() {
        let mut index = Index::default();
        index.insert(1, "a".into(), "p".into(), vec![1.0, 0.0]);
        index.insert(2, "a".into(), "p".into(), vec![0.6, 0.8]);
        index.insert(3, "b".into(), "p".into(), vec![1.0, 0.0]);
        let hits = index.search(&[0.0, 1.0], &Scope::Workspace("a"), 5);
        assert_eq!(index.search(&[0.0, 1.0], &Scope::Project("p"), 5).len(), 3);
        assert_eq!(hits.iter().map(|hit| hit.0).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(from_bytes(&to_bytes(&[0.25, -1.5])), [0.25, -1.5]);
    }
}
