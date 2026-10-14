//! The Pour index: one memory-mapped file holding every formula and cask.
//!
//! Lookups are a binary search over a sorted, zero-copy name table and a
//! single `postcard` decode of the one record wanted, so `pour info ffmpeg`
//! touches a few kilobytes of a file the kernel has usually already cached.
//! Search scans a compact lower-cased text region (name, description,
//! aliases) that is about a megabyte for the whole of Homebrew.
//!
//! Layout (little endian):
//!
//! ```text
//! 0   "POUR\x01IDX"            magic
//! 8   u64 generated_at         unix seconds
//! 16  u64 formulae block offset
//! 24  u64 casks block offset
//! 32  u64 meta JSON offset, u64 meta JSON length
//! 48  u64 file length
//! block = u32 n
//!         table names   (u32[n+1] offsets + bytes, sorted, lower-case)
//!         table records (u32[n+1] offsets + postcard bytes)
//!         table search  (u32[n+1] offsets + lower-case text)
//!         u32 m, table aliases (sorted), u32[m] alias -> record index
//! ```

use crate::model::{Cask, Formula, Kind, Pkg};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::Path;

/// File magic. The byte after `POUR` is the record schema version: it is
/// bumped whenever a record field changes, so an index built by an older
/// Pour is refused (and re-fetched) instead of being misread.
pub const MAGIC: &[u8; 8] = b"POUR\x02IDX";
const HEADER: usize = 56;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index file is not a Pour index (bad magic)")]
    BadMagic,
    #[error("index file is truncated or corrupt at {0}")]
    Corrupt(&'static str),
    #[error("record decode failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Record stored in the index: formula or cask, serialized with postcard.
#[derive(Serialize, Deserialize)]
enum Rec {
    F(Formula),
    C(Cask),
}

enum Bytes {
    Map(Mmap),
    Vec(Vec<u8>),
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Bytes::Map(m) => m,
            Bytes::Vec(v) => v,
        }
    }
}

pub struct Index {
    data: Bytes,
    formulae: Block,
    casks: Block,
    generated_at: u64,
    meta: (usize, usize),
}

/// Offsets of one block's tables inside the file.
#[derive(Clone, Copy, Debug)]
struct Block {
    n: usize,
    name_off: usize,
    name_base: usize,
    rec_off: usize,
    rec_base: usize,
    search_off: usize,
    search_base: usize,
    alias_n: usize,
    alias_off: usize,
    alias_base: usize,
    alias_idx: usize,
}

fn u32_at(d: &[u8], at: usize) -> Result<u32, IndexError> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(IndexError::Corrupt("u32"))
}

fn u64_at(d: &[u8], at: usize) -> Result<u64, IndexError> {
    d.get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        .ok_or(IndexError::Corrupt("u64"))
}

/// Parse a table header at `at`: returns (offsets position, bytes base, end).
fn table(d: &[u8], at: usize, n: usize) -> Result<(usize, usize, usize), IndexError> {
    let offs = at;
    let base = offs + 4 * (n + 1);
    let len = u32_at(d, offs + 4 * n)? as usize;
    let end = base + len;
    if end > d.len() {
        return Err(IndexError::Corrupt("table"));
    }
    Ok((offs, base, end))
}

fn parse_block(d: &[u8], at: usize) -> Result<Block, IndexError> {
    let n = u32_at(d, at)? as usize;
    let (name_off, name_base, end) = table(d, at + 4, n)?;
    let (rec_off, rec_base, end) = table(d, end, n)?;
    let (search_off, search_base, end) = table(d, end, n)?;
    let alias_n = u32_at(d, end)? as usize;
    let (alias_off, alias_base, end) = table(d, end + 4, alias_n)?;
    let alias_idx = end;
    if alias_idx + 4 * alias_n > d.len() {
        return Err(IndexError::Corrupt("alias index"));
    }
    Ok(Block {
        n,
        name_off,
        name_base,
        rec_off,
        rec_base,
        search_off,
        search_base,
        alias_n,
        alias_off,
        alias_base,
        alias_idx,
    })
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub kind: Kind,
    pub index: usize,
    pub name: String,
    pub desc: String,
    pub score: u32,
}

impl Index {
    pub fn open(path: &Path) -> Result<Index, IndexError> {
        let file = File::open(path)?;
        // SAFETY: the file is only ever replaced atomically by rename, never
        // written in place, so the mapping stays valid for our lifetime.
        let map = unsafe { Mmap::map(&file)? };
        Self::from_bytes(Bytes::Map(map))
    }

    pub fn from_vec(v: Vec<u8>) -> Result<Index, IndexError> {
        Self::from_bytes(Bytes::Vec(v))
    }

    fn from_bytes(data: Bytes) -> Result<Index, IndexError> {
        let d = data.as_ref();
        if d.len() < HEADER || &d[..8] != MAGIC {
            return Err(IndexError::BadMagic);
        }
        let generated_at = u64_at(d, 8)?;
        let f_off = u64_at(d, 16)? as usize;
        let c_off = u64_at(d, 24)? as usize;
        let m_off = u64_at(d, 32)? as usize;
        let m_len = u64_at(d, 40)? as usize;
        let total = u64_at(d, 48)? as usize;
        if total != d.len() || m_off + m_len > d.len() {
            return Err(IndexError::Corrupt("length"));
        }
        let formulae = parse_block(d, f_off)?;
        let casks = parse_block(d, c_off)?;
        Ok(Index {
            data,
            formulae,
            casks,
            generated_at,
            meta: (m_off, m_len),
        })
    }

    pub fn generated_at(&self) -> u64 {
        self.generated_at
    }

    pub fn meta(&self) -> serde_json::Value {
        let d = self.data.as_ref();
        serde_json::from_slice(&d[self.meta.0..self.meta.0 + self.meta.1])
            .unwrap_or(serde_json::Value::Null)
    }

    pub fn len(&self, kind: Kind) -> usize {
        self.block(kind).n
    }

    pub fn is_empty(&self) -> bool {
        self.formulae.n + self.casks.n == 0
    }

    fn block(&self, kind: Kind) -> &Block {
        match kind {
            Kind::Formula => &self.formulae,
            Kind::Cask => &self.casks,
        }
    }

    fn entry(&self, offs: usize, base: usize, i: usize) -> &[u8] {
        let d = self.data.as_ref();
        let s = u32_at(d, offs + 4 * i).unwrap_or(0) as usize;
        let e = u32_at(d, offs + 4 * (i + 1)).unwrap_or(0) as usize;
        d.get(base + s..base + e).unwrap_or(&[])
    }

    /// Lower-cased name of record `i`.
    pub fn name_at(&self, kind: Kind, i: usize) -> &str {
        let b = self.block(kind);
        std::str::from_utf8(self.entry(b.name_off, b.name_base, i)).unwrap_or("")
    }

    fn alias_at(&self, kind: Kind, i: usize) -> &str {
        let b = self.block(kind);
        std::str::from_utf8(self.entry(b.alias_off, b.alias_base, i)).unwrap_or("")
    }

    fn search_line(&self, kind: Kind, i: usize) -> &str {
        let b = self.block(kind);
        std::str::from_utf8(self.entry(b.search_off, b.search_base, i)).unwrap_or("")
    }

    /// All names of one kind, in sorted order.
    pub fn names(&self, kind: Kind) -> impl Iterator<Item = &str> + '_ {
        (0..self.len(kind)).map(move |i| self.name_at(kind, i))
    }

    /// Decode record `i`.
    pub fn get(&self, kind: Kind, i: usize) -> Result<Pkg, IndexError> {
        let b = self.block(kind);
        let bytes = self.entry(b.rec_off, b.rec_base, i);
        let rec: Rec =
            postcard::from_bytes(bytes).map_err(|e| IndexError::Decode(e.to_string()))?;
        Ok(match rec {
            Rec::F(f) => Pkg::Formula(f),
            Rec::C(c) => Pkg::Cask(c),
        })
    }

    fn bsearch(&self, kind: Kind, name: &str, aliases: bool) -> Option<usize> {
        let b = self.block(kind);
        let n = if aliases { b.alias_n } else { b.n };
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let cur = if aliases {
                self.alias_at(kind, mid)
            } else {
                self.name_at(kind, mid)
            };
            match cur.cmp(name) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Index of the record called `name` (exact, case-insensitive), or
    /// reached through an alias or old name.
    pub fn position(&self, kind: Kind, name: &str) -> Option<usize> {
        let q = name.to_ascii_lowercase();
        let q = q.rsplit('/').next().unwrap_or(&q); // accept tap/name
        if let Some(i) = self.bsearch(kind, q, false) {
            return Some(i);
        }
        let a = self.bsearch(kind, q, true)?;
        let b = self.block(kind);
        u32_at(self.data.as_ref(), b.alias_idx + 4 * a)
            .ok()
            .map(|x| x as usize)
    }

    pub fn find(&self, kind: Kind, name: &str) -> Option<Pkg> {
        self.position(kind, name)
            .and_then(|i| self.get(kind, i).ok())
    }

    /// Formula first, then cask (the Homebrew rule).
    pub fn find_any(&self, name: &str) -> Option<Pkg> {
        self.find(Kind::Formula, name)
            .or_else(|| self.find(Kind::Cask, name))
    }

    /// Full-text search over names, descriptions and aliases.
    /// Every word of `query` must occur; results are ranked.
    pub fn search(&self, query: &str, kind: Option<Kind>, limit: usize) -> Vec<Hit> {
        let q = query.trim().to_ascii_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<Hit> = Vec::new();
        let kinds: &[Kind] = match kind {
            Some(Kind::Formula) => &[Kind::Formula],
            Some(Kind::Cask) => &[Kind::Cask],
            None => &[Kind::Formula, Kind::Cask],
        };
        for &k in kinds {
            for i in 0..self.len(k) {
                let line = self.search_line(k, i);
                if !words.iter().all(|w| line.contains(w)) {
                    continue;
                }
                let mut parts = line.split('\x1f');
                let name = parts.next().unwrap_or("");
                let desc = parts.next().unwrap_or("");
                let aliases = parts.next().unwrap_or("");
                let mut score = 0u32;
                for w in &words {
                    score += if name == *w {
                        1000
                    } else if name.starts_with(w) {
                        600u32.saturating_sub((name.len() - w.len()) as u32)
                    } else if name.contains(w) {
                        300
                    } else if aliases.split(' ').any(|a| a == *w) {
                        250
                    } else if aliases.contains(w) {
                        150
                    } else {
                        100u32.saturating_sub(desc.len().min(90) as u32 / 3)
                    };
                }
                // The whole phrase, as typed, is worth more than its words.
                if words.len() > 1 {
                    if name.contains(q.as_str()) {
                        score += 800;
                    } else if desc.contains(q.as_str()) {
                        score += 400;
                    }
                }
                if k == Kind::Cask {
                    score = score.saturating_sub(1); // formulae first on ties
                }
                hits.push(Hit {
                    kind: k,
                    index: i,
                    name: name.to_string(),
                    desc: desc.to_string(),
                    score,
                });
            }
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        hits.truncate(limit);
        // The search text is lower-cased; show the real description.
        for h in &mut hits {
            if let Ok(p) = self.get(h.kind, h.index) {
                h.desc = p.desc().to_string();
                h.name = p.name().to_string();
            }
        }
        hits
    }

    /// Names within an edit distance of two: "did you mean".
    pub fn suggest(&self, name: &str, limit: usize) -> Vec<(Kind, String)> {
        let q = name.to_ascii_lowercase();
        let mut out: Vec<(usize, Kind, String)> = Vec::new();
        for k in [Kind::Formula, Kind::Cask] {
            for n in self.names(k) {
                if (n.len() as isize - q.len() as isize).abs() > 2 {
                    continue;
                }
                let d = levenshtein(n, &q);
                if d <= 2 {
                    out.push((d, k, n.to_string()));
                }
            }
        }
        out.sort();
        out.into_iter()
            .take(limit)
            .map(|(_, k, n)| (k, n))
            .collect()
    }
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Collects records and writes an index file.
#[derive(Default)]
pub struct IndexBuilder {
    formulae: Vec<Formula>,
    casks: Vec<Cask>,
    pub meta: serde_json::Value,
}

struct Table {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl Table {
    fn new() -> Table {
        Table {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }
    fn push(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
        self.offsets.push(self.bytes.len() as u32);
    }
    fn write(&self, out: &mut Vec<u8>) {
        for o in &self.offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&self.bytes);
    }
}

impl IndexBuilder {
    pub fn new() -> IndexBuilder {
        IndexBuilder::default()
    }

    pub fn add_formula(&mut self, f: Formula) {
        self.formulae.push(f);
    }

    pub fn add_cask(&mut self, c: Cask) {
        self.casks.push(c);
    }

    pub fn counts(&self) -> (usize, usize) {
        (self.formulae.len(), self.casks.len())
    }

    fn write_block<T, N, R, S, A>(
        out: &mut Vec<u8>,
        items: &mut [T],
        name: N,
        rec: R,
        search: S,
        aliases: A,
    ) -> Result<(), IndexError>
    where
        N: Fn(&T) -> String,
        R: Fn(&T) -> Rec,
        S: Fn(&T) -> String,
        A: Fn(&T) -> Vec<String>,
    {
        items.sort_by_key(|x| name(x).to_ascii_lowercase());
        // Names must be unique within a block: keep the last definition.
        let mut dedup: Vec<usize> = Vec::new();
        for i in 0..items.len() {
            if i + 1 < items.len() && name(&items[i]).eq_ignore_ascii_case(&name(&items[i + 1])) {
                continue;
            }
            dedup.push(i);
        }
        let mut names = Table::new();
        let mut recs = Table::new();
        let mut searches = Table::new();
        let mut alias_pairs: Vec<(String, u32)> = Vec::new();
        for (pos, &i) in dedup.iter().enumerate() {
            let it = &items[i];
            let n = name(it).to_ascii_lowercase();
            names.push(n.as_bytes());
            let bytes =
                postcard::to_allocvec(&rec(it)).map_err(|e| IndexError::Decode(e.to_string()))?;
            recs.push(&bytes);
            searches.push(search(it).to_ascii_lowercase().as_bytes());
            for a in aliases(it) {
                let a = a.to_ascii_lowercase();
                if !a.is_empty() && a != n {
                    alias_pairs.push((a, pos as u32));
                }
            }
        }
        alias_pairs.sort();
        alias_pairs.dedup_by(|a, b| a.0 == b.0);
        // Aliases that are also real names must not shadow them.
        let real: std::collections::HashSet<String> = dedup
            .iter()
            .map(|&i| name(&items[i]).to_ascii_lowercase())
            .collect();
        alias_pairs.retain(|(a, _)| !real.contains(a));
        let mut al = Table::new();
        let mut al_idx: Vec<u8> = Vec::new();
        for (a, idx) in &alias_pairs {
            al.push(a.as_bytes());
            al_idx.extend_from_slice(&idx.to_le_bytes());
        }
        out.extend_from_slice(&(dedup.len() as u32).to_le_bytes());
        names.write(out);
        recs.write(out);
        searches.write(out);
        out.extend_from_slice(&(alias_pairs.len() as u32).to_le_bytes());
        al.write(out);
        out.extend_from_slice(&al_idx);
        Ok(())
    }

    pub fn build(mut self) -> Result<Vec<u8>, IndexError> {
        let mut out = vec![0u8; HEADER];
        out[..8].copy_from_slice(MAGIC);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out[8..16].copy_from_slice(&now.to_le_bytes());
        let f_off = out.len() as u64;
        Self::write_block(
            &mut out,
            &mut self.formulae,
            |f| f.name.clone(),
            |f| Rec::F(f.clone()),
            |f| {
                let mut al = f.aliases.clone();
                al.extend(f.oldnames.iter().cloned());
                format!(
                    "{}\x1f{}\x1f{}\x1f{}",
                    f.name,
                    f.desc,
                    al.join(" "),
                    f.full_name
                )
            },
            |f| {
                let mut al = f.aliases.clone();
                al.extend(f.oldnames.iter().cloned());
                al.push(f.full_name.clone());
                al
            },
        )?;
        let c_off = out.len() as u64;
        Self::write_block(
            &mut out,
            &mut self.casks,
            |c| c.token.clone(),
            |c| Rec::C(c.clone()),
            |c| {
                format!(
                    "{}\x1f{}\x1f{}\x1f{}",
                    c.token,
                    c.desc,
                    c.old_tokens.join(" "),
                    c.names.join(" ")
                )
            },
            |c| {
                let mut al = c.old_tokens.clone();
                al.push(c.full_token.clone());
                al
            },
        )?;
        let m_off = out.len() as u64;
        let meta = serde_json::to_vec(&self.meta).unwrap_or_else(|_| b"{}".to_vec());
        out.extend_from_slice(&meta);
        let total = out.len() as u64;
        out[16..24].copy_from_slice(&f_off.to_le_bytes());
        out[24..32].copy_from_slice(&c_off.to_le_bytes());
        out[32..40].copy_from_slice(&m_off.to_le_bytes());
        out[40..48].copy_from_slice(&(meta.len() as u64).to_le_bytes());
        out[48..56].copy_from_slice(&total.to_le_bytes());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn formula(name: &str, desc: &str, aliases: &[&str]) -> Formula {
        Formula {
            name: name.into(),
            full_name: name.into(),
            tap: "homebrew/core".into(),
            desc: desc.into(),
            version: "1.0".into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn cask(token: &str, desc: &str) -> Cask {
        Cask {
            token: token.into(),
            full_token: token.into(),
            tap: "homebrew/cask".into(),
            desc: desc.into(),
            version: "2".into(),
            artifacts: "[]".into(),
            ..Default::default()
        }
    }

    fn sample() -> Index {
        let mut b = IndexBuilder::new();
        b.add_formula(formula("jq", "Lightweight JSON processor", &[]));
        b.add_formula(formula(
            "ffmpeg",
            "Play, record, convert audio and video",
            &[],
        ));
        b.add_formula(formula(
            "python@3.14",
            "Interpreted, interactive, object-oriented programming language",
            &["python3", "python"],
        ));
        b.add_formula(formula("aalib", "Portable ASCII art graphics library", &[]));
        b.add_cask(cask("firefox", "Web browser"));
        b.add_cask(cask("jq", "Not a real cask, same name as a formula"));
        b.meta = serde_json::json!({"source": "test"});
        Index::from_vec(b.build().unwrap()).unwrap()
    }

    #[test]
    fn exact_and_alias_lookup() {
        let idx = sample();
        assert_eq!(idx.len(Kind::Formula), 4);
        assert_eq!(idx.len(Kind::Cask), 2);
        assert_eq!(idx.find(Kind::Formula, "JQ").unwrap().name(), "jq");
        assert_eq!(
            idx.find(Kind::Formula, "python3").unwrap().name(),
            "python@3.14"
        );
        assert_eq!(
            idx.find(Kind::Formula, "homebrew/core/ffmpeg")
                .unwrap()
                .name(),
            "ffmpeg"
        );
        assert!(idx.find(Kind::Formula, "nope").is_none());
        assert_eq!(idx.find_any("jq").unwrap().kind(), Kind::Formula);
        assert_eq!(idx.find(Kind::Cask, "jq").unwrap().kind(), Kind::Cask);
        assert_eq!(idx.meta()["source"], "test");
    }

    #[test]
    fn search_ranks_names_before_descriptions() {
        let idx = sample();
        let hits = idx.search("json", None, 10);
        assert_eq!(hits[0].name, "jq");
        let hits = idx.search("ff", Some(Kind::Formula), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "ffmpeg");
        let hits = idx.search("python", None, 10);
        assert_eq!(hits[0].name, "python@3.14");
        assert!(idx
            .search("web browser", None, 10)
            .iter()
            .any(|h| h.name == "firefox"));
        assert!(idx.search("zzzz", None, 10).is_empty());
    }

    #[test]
    fn suggestions_use_edit_distance() {
        let idx = sample();
        let s = idx.suggest("ffmpg", 3);
        assert_eq!(s[0].1, "ffmpeg");
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            Index::from_vec(vec![0; 10]),
            Err(IndexError::BadMagic)
        ));
        let mut good = {
            let mut b = IndexBuilder::new();
            b.add_formula(formula("a", "b", &[]));
            b.build().unwrap()
        };
        good.truncate(good.len() - 3);
        assert!(Index::from_vec(good).is_err());
    }
}
