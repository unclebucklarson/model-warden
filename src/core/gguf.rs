//! Minimal GGUF header reader — metadata only, never the tensors.
//!
//! A GGUF file opens with a fixed header and a run of key/value metadata
//! pairs; the (multi-gigabyte) tensor data comes after. Everything the
//! inventory needs — architecture, context length, quantization — lives in
//! those KVs, so this reads a few megabytes at most and works identically on
//! a 20 GB shelf file or an Ollama blob.
//!
//! Spec: https://github.com/ggml-org/ggml/blob/master/docs/gguf.md (v2/v3).
//!
//! Harvested verbatim from llamacppCodeConf (src/core/gguf.rs) per PLAN.md.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufReader, Read};
use std::path::Path;

/// What the inventory shows per model file. (De)serializable because it is
/// recorded in per-root manifests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GgufMeta {
    /// `general.architecture` — "llama", "qwen3", …
    pub architecture: Option<String>,
    /// `general.name` — the model's self-declared name.
    pub name: Option<String>,
    /// `<arch>.context_length` — the training context window.
    pub context_length: Option<u64>,
    /// Human name for `general.file_type` (Q4_K_M, Q5_K_XL, …).
    pub quantization: Option<String>,
    /// `general.size_label` when present ("27B", "8x7B", …).
    pub size_label: Option<String>,
    /// `<arch>.block_count` — transformer layers. This and the four
    /// fields below are what a KV-cache fit calculation needs (modellab,
    /// 2026-09-01), read the same way `context_length` is. All are
    /// `None` on headers that lack them and on projector files.
    #[serde(default)]
    pub block_count: Option<u64>,
    /// `<arch>.embedding_length`.
    #[serde(default)]
    pub embedding_length: Option<u64>,
    /// `<arch>.attention.head_count`. `None` when the header holds a
    /// per-layer array (Laguna): a single number would be a guess, and
    /// `read_fields` hands back the array.
    #[serde(default)]
    pub head_count: Option<u64>,
    /// `<arch>.attention.head_count_kv`; `None` when per-layer (Gemma 4,
    /// Ollama's Qwen3.5 exports), as above.
    #[serde(default)]
    pub head_count_kv: Option<u64>,
    /// Head dimension: `<arch>.attention.key_length` when present, else
    /// embedding ÷ heads (llama.cpp's own fallback).
    #[serde(default)]
    pub head_dim: Option<u64>,
}

/// Bumped when `read_meta` learns to fill fields it did not before, so a
/// catalog record an older reader wrote re-reads its header once
/// (`manifest::build_root_manifest`) instead of carrying the gaps forever.
pub const READER_VERSION: u32 = 1;

/// One metadata value, typed as the file holds it. Arrays come back
/// whole — a per-layer head count is a per-layer head count. Nested
/// arrays do not occur in real files and are refused.
#[derive(Debug, Clone, PartialEq)]
pub enum GgufValue {
    Uint(u64),
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Array(Vec<GgufValue>),
}

impl GgufValue {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            GgufValue::Uint(v) => Some(*v),
            GgufValue::Int(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            GgufValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[GgufValue]> {
        match self {
            GgufValue::Array(v) => Some(v),
            _ => None,
        }
    }
}

/// Hard ceiling on how much header we're willing to read. Real metadata
/// (including 150k-entry tokenizer arrays) fits comfortably; a corrupt
/// length field must not make us slurp the tensor blob.
const MAX_HEADER_BYTES: u64 = 256 * 1024 * 1024;

/// The `general.*` keys and the `<arch>.` suffixes the inventory shows.
const META_GENERAL: [&str; 4] = [
    "general.architecture",
    "general.name",
    "general.size_label",
    "general.file_type",
];
const META_SUFFIXES: [&str; 6] = [
    ".context_length",
    ".block_count",
    ".embedding_length",
    ".attention.head_count",
    ".attention.head_count_kv",
    ".attention.key_length",
];

pub fn read_meta(path: &Path) -> Result<GgufMeta> {
    let fields = read_fields(path, &|k| {
        META_GENERAL.contains(&k) || META_SUFFIXES.iter().any(|s| k.ends_with(s))
    })?;
    Ok(GgufMeta::from_fields(&fields))
}

impl GgufMeta {
    fn from_fields(fields: &BTreeMap<String, GgufValue>) -> GgufMeta {
        let string = |k: &str| fields.get(k).and_then(GgufValue::as_str).map(str::to_owned);
        let architecture = string("general.architecture");
        // `<declared-arch>.<suffix>` when the file declares one, else the
        // first `<word>.<suffix>` — one-word prefixes only, so a vision or
        // audio sub-block (`gemma4.vision.block_count`) never answers for
        // the model.
        let pick = |suffix: &str| -> Option<&GgufValue> {
            if let Some(arch) = &architecture
                && let Some(v) = fields.get(&format!("{arch}{suffix}"))
            {
                return Some(v);
            }
            fields
                .iter()
                .find(|(k, _)| {
                    k.strip_suffix(suffix)
                        .is_some_and(|prefix| !prefix.is_empty() && !prefix.contains('.'))
                })
                .map(|(_, v)| v)
        };
        let number = |suffix: &str| pick(suffix).and_then(GgufValue::as_u64);
        let embedding_length = number(".embedding_length");
        let head_count = number(".attention.head_count");
        let head_dim = number(".attention.key_length").or(match (embedding_length, head_count) {
            (Some(e), Some(h)) if h > 0 && e % h == 0 => Some(e / h),
            _ => None,
        });
        let context_length = number(".context_length");
        let block_count = number(".block_count");
        let head_count_kv = number(".attention.head_count_kv");
        GgufMeta {
            architecture,
            name: string("general.name"),
            context_length,
            quantization: fields
                .get("general.file_type")
                .and_then(GgufValue::as_u64)
                .map(file_type_name),
            size_label: string("general.size_label"),
            block_count,
            embedding_length,
            head_count,
            head_count_kv,
            head_dim,
        }
    }
}

/// Read the header once and hand back every key `wanted` accepts, typed.
///
/// This is the one GGUF parser in the family: a consumer that needs keys
/// the inventory does not carry (modellab's fit arithmetic reads
/// per-layer head counts, sliding-window patterns, SSM sizes) asks here
/// rather than parsing on its own side. Declined keys are skipped without
/// allocation, so asking for a few keys costs what `read_meta` costs;
/// asking for `tokenizer.ggml.tokens` costs the vocabulary.
pub fn read_fields(
    path: &Path,
    wanted: &dyn Fn(&str) -> bool,
) -> Result<BTreeMap<String, GgufValue>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut r = CountingReader {
        inner: BufReader::new(file),
        read: 0,
    };

    let magic = read_u32(&mut r)?;
    if magic != 0x4655_4747 {
        bail!("not a GGUF file (bad magic)");
    }
    let version = read_u32(&mut r)?;
    if !(2..=3).contains(&version) {
        bail!("unsupported GGUF version {version}");
    }
    let _tensor_count = read_u64(&mut r)?;
    let kv_count = read_u64(&mut r)?;
    if kv_count > 100_000 {
        bail!("implausible metadata count {kv_count}");
    }

    let mut out = BTreeMap::new();
    for _ in 0..kv_count {
        let key = read_string(&mut r)?;
        let ty = read_u32(&mut r)?;
        if wanted(&key) {
            out.insert(key, read_value(&mut r, ty)?);
        } else {
            skip_value(&mut r, ty)?;
        }
        if r.read > MAX_HEADER_BYTES {
            bail!("metadata exceeded {MAX_HEADER_BYTES} bytes; refusing to read further");
        }
    }
    Ok(out)
}

struct CountingReader<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<()> {
    r.read_exact(buf).context("truncated GGUF header")
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    read_exact(r, &mut b)?;
    Ok(b[0])
}

fn read_u16<R: Read>(r: &mut R) -> Result<u16> {
    let mut b = [0u8; 2];
    read_exact(r, &mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    read_exact(r, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64> {
    let mut b = [0u8; 8];
    read_exact(r, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_string<R: Read>(r: &mut R) -> Result<String> {
    let len = read_u64(r)?;
    if len > 64 * 1024 * 1024 {
        bail!("implausible string length {len}");
    }
    // Grown as bytes arrive, never allocated up front: a 40-byte file
    // declaring a 64 MiB string used to cost 64 MiB of zeroed memory
    // before the read failed at EOF, and a header may hold up to
    // kv_count of them.
    let mut buf = Vec::new();
    let read = r.take(len).read_to_end(&mut buf)?;
    if read as u64 != len {
        bail!("truncated string: wanted {len} bytes, file held {read}");
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// GGUF metadata value types.
const T_U8: u32 = 0;
const T_I8: u32 = 1;
const T_U16: u32 = 2;
const T_I16: u32 = 3;
const T_U32: u32 = 4;
const T_I32: u32 = 5;
const T_F32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_U64: u32 = 10;
const T_I64: u32 = 11;
const T_F64: u32 = 12;

fn fixed_size(ty: u32) -> Option<u64> {
    match ty {
        T_U8 | T_I8 | T_BOOL => Some(1),
        T_U16 | T_I16 => Some(2),
        T_U32 | T_I32 | T_F32 => Some(4),
        T_U64 | T_I64 | T_F64 => Some(8),
        _ => None,
    }
}

fn skip_bytes<R: Read>(r: &mut R, mut n: u64) -> Result<()> {
    let mut buf = [0u8; 8192];
    while n > 0 {
        let take = n.min(buf.len() as u64) as usize;
        read_exact(r, &mut buf[..take])?;
        n -= take as u64;
    }
    Ok(())
}

fn skip_value<R: Read>(r: &mut R, ty: u32) -> Result<()> {
    if let Some(sz) = fixed_size(ty) {
        return skip_bytes(r, sz);
    }
    match ty {
        T_STRING => {
            let len = read_u64(r)?;
            skip_bytes(r, len)
        }
        T_ARRAY => {
            let elem_ty = read_u32(r)?;
            let count = read_u64(r)?;
            if let Some(sz) = fixed_size(elem_ty) {
                skip_bytes(r, count.saturating_mul(sz))
            } else if elem_ty == T_STRING {
                for _ in 0..count {
                    let len = read_u64(r)?;
                    skip_bytes(r, len)?;
                }
                Ok(())
            } else {
                // Nested arrays don't occur in real files; refuse rather
                // than mis-skip and misread everything after.
                bail!("unsupported array element type {elem_ty}")
            }
        }
        other => bail!("unknown GGUF value type {other}"),
    }
}

/// Longest array `read_fields` will materialize: vocabularies run to a
/// few hundred thousand entries; a corrupt count must not become a
/// giant allocation (the header byte ceiling still applies underneath).
const MAX_ARRAY_LEN: u64 = 1 << 22;

fn read_value<R: Read>(r: &mut R, ty: u32) -> Result<GgufValue> {
    Ok(match ty {
        T_U8 => GgufValue::Uint(read_u8(r)? as u64),
        T_I8 => GgufValue::Int(read_u8(r)? as i8 as i64),
        T_U16 => GgufValue::Uint(read_u16(r)? as u64),
        T_I16 => GgufValue::Int(read_u16(r)? as i16 as i64),
        T_U32 => GgufValue::Uint(read_u32(r)? as u64),
        T_I32 => GgufValue::Int(read_u32(r)? as i32 as i64),
        T_F32 => GgufValue::Float(f32::from_le_bytes(read_u32(r)?.to_le_bytes()) as f64),
        T_BOOL => GgufValue::Bool(read_u8(r)? != 0),
        T_STRING => GgufValue::Str(read_string(r)?),
        T_U64 => GgufValue::Uint(read_u64(r)?),
        T_I64 => GgufValue::Int(read_u64(r)? as i64),
        T_F64 => GgufValue::Float(f64::from_le_bytes(read_u64(r)?.to_le_bytes())),
        T_ARRAY => {
            let elem_ty = read_u32(r)?;
            let count = read_u64(r)?;
            if elem_ty == T_ARRAY {
                bail!("unsupported nested array");
            }
            if count > MAX_ARRAY_LEN {
                bail!("implausible array length {count}");
            }
            // Grown as elements arrive, never sized up front (same rule
            // as read_string): a bad count must not cost memory.
            let mut items = Vec::new();
            for _ in 0..count {
                items.push(read_value(r, elem_ty)?);
            }
            GgufValue::Array(items)
        }
        other => bail!("unknown GGUF value type {other}"),
    })
}

/// `general.file_type` enum → the quant name users know. Partial by design:
/// unknown values render as `type N` rather than guessing.
fn file_type_name(n: u64) -> String {
    match n {
        0 => "F32".into(),
        1 => "F16".into(),
        2 => "Q4_0".into(),
        3 => "Q4_1".into(),
        7 => "Q8_0".into(),
        8 => "Q5_0".into(),
        9 => "Q5_1".into(),
        10 => "Q2_K".into(),
        11 => "Q3_K_S".into(),
        12 => "Q3_K_M".into(),
        13 => "Q3_K_L".into(),
        14 => "Q4_K_S".into(),
        15 => "Q4_K_M".into(),
        16 => "Q5_K_S".into(),
        17 => "Q5_K_M".into(),
        18 => "Q6_K".into(),
        19 => "IQ2_XXS".into(),
        20 => "IQ2_XS".into(),
        24 => "IQ4_NL".into(),
        25 => "IQ4_XS".into(),
        30 => "BF16".into(),
        other => format!("type {other}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tiny synthetic GGUF header for tests.
    pub(crate) fn synthetic_gguf(arch: &str, ctx: u64, file_type: u64) -> Vec<u8> {
        synthetic_gguf_with(arch, ctx, file_type, &[])
    }

    /// A typed metadata value for `synthetic_gguf_with`.
    pub(crate) enum Kv {
        U32(u32),
        U32s(Vec<u32>),
        Bools(Vec<bool>),
    }

    /// `synthetic_gguf` plus any extra keys, in the order given, after
    /// the five standard ones — real files hold hundreds, and the
    /// KV-shape keys (per-layer arrays included) live among them.
    pub(crate) fn synthetic_gguf_with(
        arch: &str,
        ctx: u64,
        file_type: u64,
        extra: &[(&str, Kv)],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(0x4655_4747u32.to_le_bytes()); // "GGUF"
        b.extend(3u32.to_le_bytes()); // version
        b.extend(0u64.to_le_bytes()); // tensor count
        b.extend((5 + extra.len() as u64).to_le_bytes()); // kv count

        let kv_str = |b: &mut Vec<u8>, k: &str, v: &str| {
            b.extend((k.len() as u64).to_le_bytes());
            b.extend(k.as_bytes());
            b.extend(T_STRING.to_le_bytes());
            b.extend((v.len() as u64).to_le_bytes());
            b.extend(v.as_bytes());
        };
        let kv_u32 = |b: &mut Vec<u8>, k: &str, v: u32| {
            b.extend((k.len() as u64).to_le_bytes());
            b.extend(k.as_bytes());
            b.extend(T_U32.to_le_bytes());
            b.extend(v.to_le_bytes());
        };

        kv_str(&mut b, "general.architecture", arch);
        kv_str(&mut b, "general.name", "Synthetic Test Model");
        // A string array to prove the skipper walks composite values.
        let key = "tokenizer.ggml.tokens";
        b.extend((key.len() as u64).to_le_bytes());
        b.extend(key.as_bytes());
        b.extend(T_ARRAY.to_le_bytes());
        b.extend(T_STRING.to_le_bytes());
        b.extend(3u64.to_le_bytes());
        for tok in ["<s>", "hello", "world"] {
            b.extend((tok.len() as u64).to_le_bytes());
            b.extend(tok.as_bytes());
        }
        kv_u32(&mut b, &format!("{arch}.context_length"), ctx as u32);
        kv_u32(&mut b, "general.file_type", file_type as u32);
        for (k, v) in extra {
            match v {
                Kv::U32(n) => kv_u32(&mut b, k, *n),
                Kv::U32s(ns) => {
                    b.extend((k.len() as u64).to_le_bytes());
                    b.extend(k.as_bytes());
                    b.extend(T_ARRAY.to_le_bytes());
                    b.extend(T_U32.to_le_bytes());
                    b.extend((ns.len() as u64).to_le_bytes());
                    for n in ns {
                        b.extend(n.to_le_bytes());
                    }
                }
                Kv::Bools(bs) => {
                    b.extend((k.len() as u64).to_le_bytes());
                    b.extend(k.as_bytes());
                    b.extend(T_ARRAY.to_le_bytes());
                    b.extend(T_BOOL.to_le_bytes());
                    b.extend((bs.len() as u64).to_le_bytes());
                    for x in bs {
                        b.push(u8::from(*x));
                    }
                }
            }
        }
        b
    }

    fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.gguf");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        (dir, path)
    }

    #[test]
    fn reads_the_fields_the_inventory_needs() {
        let (_d, path) = write_temp(&synthetic_gguf("qwen3", 262_144, 17));
        let meta = read_meta(&path).unwrap();
        assert_eq!(meta.architecture.as_deref(), Some("qwen3"));
        assert_eq!(meta.name.as_deref(), Some("Synthetic Test Model"));
        assert_eq!(meta.context_length, Some(262_144));
        assert_eq!(meta.quantization.as_deref(), Some("Q5_K_M"));
    }

    #[test]
    fn rejects_non_gguf() {
        let (_d, path) = write_temp(b"definitely not a gguf file");
        let err = read_meta(&path).unwrap_err();
        assert!(format!("{err}").contains("bad magic"));
    }

    #[test]
    fn truncated_header_errors_cleanly() {
        let full = synthetic_gguf("llama", 8192, 15);
        let (_d, path) = write_temp(&full[..full.len() / 2]);
        assert!(read_meta(&path).is_err());
    }

    #[test]
    fn unknown_file_type_is_reported_not_guessed() {
        let (_d, path) = write_temp(&synthetic_gguf("llama", 4096, 999));
        let meta = read_meta(&path).unwrap();
        assert_eq!(meta.quantization.as_deref(), Some("type 999"));
    }

    // ---- KV-shape fields (modellab prerequisite, 2026-09-01) ----------

    #[test]
    fn reads_the_kv_shape_fields_a_fit_calculation_needs() {
        // Layers, embedding width, heads, KV heads, head dimension — read
        // the same way context_length is, so a consumer can size a KV
        // cache from the inventory without opening the file.
        let (_d, path) = write_temp(&synthetic_gguf_with(
            "qwen3",
            262_144,
            17,
            &[
                ("qwen3.block_count", Kv::U32(64)),
                ("qwen3.embedding_length", Kv::U32(5120)),
                ("qwen3.attention.head_count", Kv::U32(24)),
                ("qwen3.attention.head_count_kv", Kv::U32(4)),
                ("qwen3.attention.key_length", Kv::U32(256)),
                // A vision sub-block must never answer for the model.
                ("qwen3.vision.block_count", Kv::U32(27)),
            ],
        ));
        let meta = read_meta(&path).unwrap();
        assert_eq!(meta.block_count, Some(64));
        assert_eq!(meta.embedding_length, Some(5120));
        assert_eq!(meta.head_count, Some(24));
        assert_eq!(meta.head_count_kv, Some(4));
        assert_eq!(meta.head_dim, Some(256));
    }

    #[test]
    fn head_dim_falls_back_to_embedding_over_heads_or_stays_unknown() {
        // Older exports omit key_length; llama.cpp derives it the same way.
        let (_d, path) = write_temp(&synthetic_gguf_with(
            "llama",
            8192,
            15,
            &[
                ("llama.embedding_length", Kv::U32(4096)),
                ("llama.attention.head_count", Kv::U32(32)),
            ],
        ));
        assert_eq!(read_meta(&path).unwrap().head_dim, Some(128));
        // With neither, it is unknown — never a guess.
        let (_d, bare) = write_temp(&synthetic_gguf("llama", 8192, 15));
        let meta = read_meta(&bare).unwrap();
        assert_eq!(meta.head_dim, None);
        assert_eq!(meta.block_count, None);
    }

    #[test]
    fn per_layer_head_counts_are_not_flattened_to_one_number() {
        // Gemma 4 and Ollama's Qwen3.5 exports hold head_count_kv per
        // layer. Any single number would be a guess, so the inventory
        // says unknown and read_fields hands back the whole array.
        let (_d, path) = write_temp(&synthetic_gguf_with(
            "gemma4",
            131_072,
            15,
            &[
                ("gemma4.block_count", Kv::U32(3)),
                ("gemma4.attention.head_count_kv", Kv::U32s(vec![16, 16, 4])),
            ],
        ));
        let meta = read_meta(&path).unwrap();
        assert_eq!(meta.block_count, Some(3));
        assert_eq!(meta.head_count_kv, None);
        let fields = read_fields(&path, &|k| k.ends_with(".head_count_kv")).unwrap();
        assert_eq!(
            fields.get("gemma4.attention.head_count_kv"),
            Some(&GgufValue::Array(vec![
                GgufValue::Uint(16),
                GgufValue::Uint(16),
                GgufValue::Uint(4)
            ]))
        );
    }

    #[test]
    fn read_fields_returns_only_what_was_asked_typed_as_stored() {
        // The one GGUF parser in the family: a consumer that needs keys
        // the inventory does not carry asks for them here instead of
        // parsing on its own side. Declined keys are skipped, not read.
        let (_d, path) = write_temp(&synthetic_gguf_with(
            "gemma4",
            131_072,
            15,
            &[
                ("gemma4.attention.sliding_window", Kv::U32(1024)),
                (
                    "gemma4.attention.sliding_window_pattern",
                    Kv::Bools(vec![true, true, false]),
                ),
            ],
        ));
        let fields = read_fields(&path, &|k| {
            k.starts_with("gemma4.attention.") || k == "general.architecture"
        })
        .unwrap();
        assert_eq!(
            fields.get("general.architecture"),
            Some(&GgufValue::Str("gemma4".into()))
        );
        assert_eq!(
            fields.get("gemma4.attention.sliding_window"),
            Some(&GgufValue::Uint(1024))
        );
        assert_eq!(
            fields.get("gemma4.attention.sliding_window_pattern"),
            Some(&GgufValue::Array(vec![
                GgufValue::Bool(true),
                GgufValue::Bool(true),
                GgufValue::Bool(false)
            ]))
        );
        assert!(!fields.contains_key("general.name"));
        assert!(!fields.contains_key("tokenizer.ggml.tokens"));
        // A key the file lacks is simply absent, not an error.
        assert!(
            read_fields(&path, &|k| k == "nope.block_count")
                .unwrap()
                .is_empty()
        );
        // Junk is still refused with an error, not a panic.
        let (_d, junk) = write_temp(b"not a gguf");
        assert!(read_fields(&junk, &|_| true).is_err());
    }

    #[test]
    fn a_projector_header_keeps_the_shape_fields_absent() {
        // mmproj files carry clip.vision.* only; nothing there sizes a
        // KV cache, and nothing must pretend to.
        let (_d, path) = write_temp(&synthetic_gguf_with(
            "clip",
            0,
            1,
            &[
                ("clip.vision.block_count", Kv::U32(27)),
                ("clip.vision.embedding_length", Kv::U32(1152)),
            ],
        ));
        let meta = read_meta(&path).unwrap();
        assert_eq!(meta.block_count, None);
        assert_eq!(meta.embedding_length, None);
    }
}
