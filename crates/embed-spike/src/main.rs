//! Phase 3 spike: all-MiniLM-L6-v2 sentence embeddings via candle.
//!
//! Measures what the buildout plan needs to know before committing to the
//! embeddings design:
//!   1. model load time (app launch cost),
//!   2. per-sentence embedding latency (the <100ms-on-iPhone criterion),
//!   3. whether the embeddings are sane (cosine similarity ordering on a
//!      hand-picked corpus).
//!
//! Model files are NOT committed; fetch them first (see crate README).
//! Pass the directory containing model.safetensors / tokenizer.json /
//! config.json as argv[1], or default to `crates/embed-spike/model`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::Tokenizer;

struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    fn load(dir: &Path) -> Result<Self> {
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json")).context("reading config.json")?,
        )?;
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("loading tokenizer: {e}"))?;
        // SAFETY of mmap is candle's concern; the file is read-only here.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DTYPE, &device)?
        };
        let model = BertModel::load(vb, &config)?;
        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embed one sentence: tokenize, forward pass, attention-masked mean
    /// pooling over the token axis, L2 normalization. 384 dims out.
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow!("tokenizing: {e}"))?;
        let ids = Tensor::new(encoding.get_ids(), &self.device)?.unsqueeze(0)?;
        let type_ids = Tensor::new(encoding.get_type_ids(), &self.device)?.unsqueeze(0)?;
        let mask = Tensor::new(encoding.get_attention_mask(), &self.device)?.unsqueeze(0)?;

        let hidden = self.model.forward(&ids, &type_ids, Some(&mask))?;

        // Mean over real (unmasked) tokens only.
        let mask_f = mask.to_dtype(DTYPE)?.unsqueeze(2)?; // [1, seq, 1]
        let summed = hidden.broadcast_mul(&mask_f)?.sum(1)?; // [1, 384]
        let counts = mask_f.sum(1)?; // [1, 1]
        let mean = summed.broadcast_div(&counts)?;

        let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = mean.broadcast_div(&norm)?;
        Ok(normalized.squeeze(0)?.to_vec1()?)
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // Vectors are already L2-normalized, so the dot product is the cosine.
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn main() -> Result<()> {
    let dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("crates/embed-spike/model"), PathBuf::from);

    let load_start = Instant::now();
    let embedder = Embedder::load(&dir)?;
    println!("model load: {:?}", load_start.elapsed());

    // Warm-up: first inference pays one-time allocation costs.
    let warm_start = Instant::now();
    embedder.embed("warm up the kernels")?;
    println!("first embed (warm-up): {:?}", warm_start.elapsed());

    let sentences = [
        "remember to buy milk and eggs on the way home",
        "groceries: pick up dairy and bread tomorrow",
        "the harbor buoy light blinked twice before dawn",
        "ship navigation lights at sea during the night",
        "rust borrow checker fights back on self-referential structs",
    ];

    let mut vectors = Vec::new();
    for sentence in &sentences {
        let start = Instant::now();
        let vector = embedder.embed(sentence)?;
        println!(
            "embed {:?} chars={} -> {:?}",
            start.elapsed(),
            sentence.len(),
            &sentence[..28]
        );
        assert_eq!(vector.len(), 384, "expected 384-dim embedding");
        vectors.push(vector);
    }

    // Median-ish latency over a longer run for a stable number.
    let mut times = Vec::new();
    for i in 0..20 {
        let start = Instant::now();
        embedder.embed(sentences[i % sentences.len()])?;
        times.push(start.elapsed());
    }
    times.sort();
    println!("median embed over 20 runs: {:?}", times[times.len() / 2]);

    println!("\ncosine similarity sanity (expect related > unrelated):");
    println!(
        "  groceries vs groceries-paraphrase: {:.3}",
        cosine(&vectors[0], &vectors[1])
    );
    println!(
        "  buoy-light vs ship-lights:         {:.3}",
        cosine(&vectors[2], &vectors[3])
    );
    println!(
        "  groceries vs buoy-light:           {:.3}",
        cosine(&vectors[0], &vectors[2])
    );
    println!(
        "  groceries vs borrow-checker:       {:.3}",
        cosine(&vectors[0], &vectors[4])
    );

    let related = cosine(&vectors[0], &vectors[1]);
    let unrelated = cosine(&vectors[0], &vectors[2]);
    assert!(
        related > unrelated + 0.2,
        "embeddings failed sanity: related {related} not clearly above unrelated {unrelated}"
    );
    println!("\nspike PASS");
    Ok(())
}
