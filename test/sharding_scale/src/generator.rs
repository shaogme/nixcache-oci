use nixcache_core::{
    IndexEntry, NIX_BASE32_ALPHABET, NarDigest, NarInfoMeta, StoreHash, SystemArch,
};
use std::{collections::HashMap, str};

/// 高性能轻量级伪随机数生成器 (XorShift64，零外部依赖，极高吞吐)
#[derive(Clone, Debug)]
pub struct FastRng {
    state: u64,
}

impl FastRng {
    pub const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x853c_49e6_748f_ea9b
            } else {
                seed
            },
        }
    }

    #[inline(always)]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    #[inline(always)]
    pub fn next_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() as usize) % bound
        }
    }
}

/// 快速生成指定数量的合法 Nix RFC 4648 Base32 StoreHash
pub fn generate_store_hashes(count: usize, seed: u64) -> Vec<StoreHash> {
    let mut rng = FastRng::new(seed);
    let mut hashes = Vec::with_capacity(count);

    let mut buf = [0u8; 32];
    for _ in 0..count {
        for b in &mut buf {
            let idx = (rng.next_u32() & 0x1F) as usize;
            *b = NIX_BASE32_ALPHABET[idx];
        }
        let s = unsafe { str::from_utf8_unchecked(&buf) };
        hashes.push(StoreHash::new_unchecked(s));
    }

    hashes
}

/// 快速生成指定数量的 IndexEntry 映射表 (模拟大规模真实 Nix Store 产物)
pub fn generate_index_entries(
    count: usize,
    seed: u64,
    system: SystemArch,
) -> HashMap<StoreHash, IndexEntry> {
    let mut rng = FastRng::new(seed);
    let hashes = generate_store_hashes(count, seed);
    let mut entries = HashMap::with_capacity(count);

    let base_digest = "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0";

    for (i, hash) in hashes.into_iter().enumerate() {
        let pkg_name = format!("pkg-scale-{}-{}", (i % 1000), i);
        let store_path = format!("/nix/store/{}-{}", hash, pkg_name);
        let nar_basename = format!("{}.nar.xz", pkg_name);
        let nar_size = (rng.next_u32() % 50_000_000 + 1024) as u64;

        let narinfo_meta = NarInfoMeta {
            store_path,
            nar_basename,
            compression: Some("xz".to_string()),
            file_hash: Some(base_digest.to_string()),
            file_size: Some(nar_size / 3),
            nar_hash: base_digest.to_string(),
            references: Vec::new(),
            deriver: None,
            signatures: vec![
                "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_string(),
            ],
            ca: None,
        };

        let entry = IndexEntry {
            name: pkg_name,
            system: Some(system),
            narinfo_meta,
            nar_digest: NarDigest::new_unchecked(base_digest),
            nar_size,
            added: "2026-08-30T00:00:00Z".to_string(),
            origin_job: Some(format!("job-{}", (i % 128))),
        };

        entries.insert(hash, entry);
    }

    entries
}

/// 生成指定数量的不存在于给定集合的 StoreHash (用于假阳性与未命中测试)
pub fn generate_non_existent_hashes(count: usize, seed: u64) -> Vec<StoreHash> {
    let mut rng = FastRng::new(seed ^ 0xDEAD_BEEF_CAFE_BABE);
    let mut hashes = Vec::with_capacity(count);

    let mut buf = [0u8; 32];
    for _ in 0..count {
        for b in &mut buf {
            let idx = (rng.next_u32() & 0x1F) as usize;
            *b = NIX_BASE32_ALPHABET[idx];
        }
        let s = unsafe { str::from_utf8_unchecked(&buf) };
        hashes.push(StoreHash::new_unchecked(s));
    }

    hashes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generator_validity() {
        let hashes = generate_store_hashes(1000, 42);
        assert_eq!(hashes.len(), 1000);
        for h in &hashes {
            assert_eq!(h.len(), 32);
            assert!(StoreHash::parse(h.as_str()).is_ok());
        }

        let entries = generate_index_entries(100, 100, SystemArch::X86_64Linux);
        assert_eq!(entries.len(), 100);
        for (h, e) in &entries {
            assert_eq!(e.store_hash(), Some(h.clone()));
        }
    }
}
