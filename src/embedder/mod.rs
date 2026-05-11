//! Shared flat embedding store spanning curated + staging.
//!
//! Binary format (embeddings.bin):
//!   [u32 magic][u32 version][u32 dim][u32 count]
//!   per entry: [u8 source_tag][u32 verb_len][verb utf8][f32 * dim]

pub const MAGIC: u32 = 0x534D454D; // "SMEM"
pub const VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct EmbeddingEntry {
    pub verb: String,
    pub source: EmbeddingSource,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingSource {
    Curated,
    Staging,
    Wrapped,
}

impl EmbeddingSource {
    /// Score boost during similarity ranking.
    /// Wrapped procedures represent confirmed multi-step intent — reward them.
    pub fn boost(&self) -> f32 {
        match self {
            EmbeddingSource::Curated => 1.00,
            EmbeddingSource::Wrapped => 1.15,
            EmbeddingSource::Staging => 0.85,
        }
    }
    fn tag(&self) -> u8 { match self { Self::Curated => 0, Self::Staging => 1, Self::Wrapped => 2 } }
    fn from_tag(b: u8) -> Self { match b { 2 => Self::Wrapped, 1 => Self::Staging, _ => Self::Curated } }
}

pub struct EmbeddingStore {
    entries: Vec<EmbeddingEntry>,
    dim: usize,
    path: std::path::PathBuf,
}

impl EmbeddingStore {
    pub fn load_or_create(path: &std::path::Path, dim: usize) -> anyhow::Result<Self> {
        if path.exists() { Self::load(path) }
        else { Ok(Self { entries: vec![], dim, path: path.to_path_buf() }) }
    }

    pub fn len(&self) -> usize { self.entries.len() }

    pub fn has_verb(&self, verb: &str) -> bool {
        self.entries.iter().any(|e| e.verb == verb)
    }

    pub fn upsert(&mut self, verb: &str, source: EmbeddingSource, vector: Vec<f32>) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.verb == verb) {
            e.source = source; e.vector = vector;
        } else {
            self.entries.push(EmbeddingEntry { verb: verb.to_string(), source, vector });
        }
    }

    pub fn remove(&mut self, verb: &str) { self.entries.retain(|e| e.verb != verb); }

    /// Top-k search above threshold, returns (verb, boosted_score) desc.
    pub fn search(&self, query: &[f32], top_k: usize, threshold: f32) -> Vec<(String, f32)> {
        let mut scored: Vec<(String, f32)> = self.entries.iter()
            .map(|e| (e.verb.clone(), cosine(query, &e.vector) * e.source.boost()))
            .filter(|(_, s)| *s >= threshold)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        scored
    }

    pub fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;
        if let Some(p) = self.path.parent() { std::fs::create_dir_all(p)?; }
        let mut f = std::fs::File::create(&self.path)?;
        f.write_all(&MAGIC.to_le_bytes())?;
        f.write_all(&VERSION.to_le_bytes())?;
        f.write_all(&(self.dim as u32).to_le_bytes())?;
        f.write_all(&(self.entries.len() as u32).to_le_bytes())?;
        for e in &self.entries {
            f.write_all(&[e.source.tag()])?;
            f.write_all(&(e.verb.len() as u32).to_le_bytes())?;
            f.write_all(e.verb.as_bytes())?;
            for v in &e.vector { f.write_all(&v.to_le_bytes())?; }
        }
        Ok(())
    }

    fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        use std::io::Read;
        use anyhow::Context;
        let mut buf = vec![];
        std::fs::File::open(path)?.read_to_end(&mut buf)?;
        let mut p = 0;
        let magic = ru32(&buf, &mut p)?;
        if magic != MAGIC { anyhow::bail!("Bad magic in embeddings.bin"); }
        let _ver = ru32(&buf, &mut p)?;
        let dim = ru32(&buf, &mut p)? as usize;
        let count = ru32(&buf, &mut p)? as usize;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = *buf.get(p).context("EOF source tag")?; p += 1;
            let vlen = ru32(&buf, &mut p)? as usize;
            let verb = String::from_utf8(buf[p..p+vlen].to_vec())?; p += vlen;
            let mut vec = Vec::with_capacity(dim);
            for _ in 0..dim {
                let b: [u8;4] = buf[p..p+4].try_into()?; p += 4;
                vec.push(f32::from_le_bytes(b));
            }
            entries.push(EmbeddingEntry { verb, source: EmbeddingSource::from_tag(tag), vector: vec });
        }
        Ok(Self { entries, dim, path: path.to_path_buf() })
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let dot: f32 = a.iter().zip(b).map(|(x,y)| x*y).sum();
    let na: f32 = a.iter().map(|x| x*x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x*x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

fn ru32(buf: &[u8], p: &mut usize) -> anyhow::Result<u32> {
    let b: [u8;4] = buf.get(*p..*p+4)
        .ok_or_else(|| anyhow::anyhow!("EOF reading u32"))?
        .try_into()?;
    *p += 4;
    Ok(u32::from_le_bytes(b))
}

/// Bag-of-words embedding — bootstrap without ONNX.
/// Replace inner loop with fastembed call when toolchain allows.
pub fn embed_text_bow(text: &str, dim: usize) -> Vec<f32> {
    let mut vec = vec![0.0f32; dim];
    for token in text.split_whitespace() {
        let t = token.to_lowercase();
        let mut h: usize = 5381;
        for c in t.bytes() { h = h.wrapping_mul(33).wrapping_add(c as usize); }
        vec[h % dim] += 1.0;
    }
    let norm: f32 = vec.iter().map(|x| x*x).sum::<f32>().sqrt();
    if norm > 0.0 { vec.iter_mut().for_each(|x| *x /= norm); }
    vec
}
