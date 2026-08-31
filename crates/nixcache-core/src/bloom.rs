use crate::{error::BloomError, types::StoreHash};

/// MurmurHash3 x64 128-bit 纯标准库实现 (零外部依赖，跨平台与 Wasm 兼容)
#[inline]
pub fn murmur3_x64_128(data: &[u8], seed: u64) -> (u64, u64) {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;

    let mut h1 = seed;
    let mut h2 = seed;

    let n_blocks = data.len() / 16;

    // 处理 16 字节完整块
    for i in 0..n_blocks {
        let chunk = &data[i * 16..(i + 1) * 16];
        let mut k1 = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(chunk[8..16].try_into().unwrap());

        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;

        h1 = h1.rotate_left(27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dc_e729);

        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;

        h2 = h2.rotate_left(31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }

    // 处理尾部剩余 1..15 字节
    let tail = &data[n_blocks * 16..];
    let mut k1 = 0u64;
    let mut k2 = 0u64;

    match tail.len() {
        15 => {
            k2 ^= (tail[14] as u64) << 48;
            k2 ^= (tail[13] as u64) << 40;
            k2 ^= (tail[12] as u64) << 32;
            k2 ^= (tail[11] as u64) << 24;
            k2 ^= (tail[10] as u64) << 16;
            k2 ^= (tail[9] as u64) << 8;
            k2 ^= tail[8] as u64;
            k2 = k2.wrapping_mul(C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(C1);
            h2 ^= k2;
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        14 => {
            k2 ^= (tail[13] as u64) << 40;
            k2 ^= (tail[12] as u64) << 32;
            k2 ^= (tail[11] as u64) << 24;
            k2 ^= (tail[10] as u64) << 16;
            k2 ^= (tail[9] as u64) << 8;
            k2 ^= tail[8] as u64;
            k2 = k2.wrapping_mul(C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(C1);
            h2 ^= k2;
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        13 => {
            k2 ^= (tail[12] as u64) << 32;
            k2 ^= (tail[11] as u64) << 24;
            k2 ^= (tail[10] as u64) << 16;
            k2 ^= (tail[9] as u64) << 8;
            k2 ^= tail[8] as u64;
            k2 = k2.wrapping_mul(C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(C1);
            h2 ^= k2;
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        12 => {
            k2 ^= (tail[11] as u64) << 24;
            k2 ^= (tail[10] as u64) << 16;
            k2 ^= (tail[9] as u64) << 8;
            k2 ^= tail[8] as u64;
            k2 = k2.wrapping_mul(C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(C1);
            h2 ^= k2;
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        11 => {
            k2 ^= (tail[10] as u64) << 16;
            k2 ^= (tail[9] as u64) << 8;
            k2 ^= tail[8] as u64;
            k2 = k2.wrapping_mul(C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(C1);
            h2 ^= k2;
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        10 => {
            k2 ^= (tail[9] as u64) << 8;
            k2 ^= tail[8] as u64;
            k2 = k2.wrapping_mul(C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(C1);
            h2 ^= k2;
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        9 => {
            k2 ^= tail[8] as u64;
            k2 = k2.wrapping_mul(C2);
            k2 = k2.rotate_left(33);
            k2 = k2.wrapping_mul(C1);
            h2 ^= k2;
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        8 => {
            k1 ^= (tail[7] as u64) << 56;
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        7 => {
            k1 ^= (tail[6] as u64) << 48;
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        6 => {
            k1 ^= (tail[5] as u64) << 40;
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        5 => {
            k1 ^= (tail[4] as u64) << 32;
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        4 => {
            k1 ^= (tail[3] as u64) << 24;
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        3 => {
            k1 ^= (tail[2] as u64) << 16;
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        2 => {
            k1 ^= (tail[1] as u64) << 8;
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        1 => {
            k1 ^= tail[0] as u64;
            k1 = k1.wrapping_mul(C1);
            k1 = k1.rotate_left(31);
            k1 = k1.wrapping_mul(C2);
            h1 ^= k1;
        }
        _ => {}
    }

    // 最终化雪崩混合 (Finalization mix)
    h1 ^= data.len() as u64;
    h2 ^= data.len() as u64;

    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);

    h1 = fmix64(h1);
    h2 = fmix64(h2);

    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);

    (h1, h2)
}

#[inline(always)]
fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

/// 块级紧凑布隆过滤器 (Fast Blocked Bloom Filter)
///
/// 每个块固定 512 位 (64 字节，对应 CPU L1 Cache Line)，单次查询仅命中单条 Cache Line，
/// 结合 MurmurHash3 双散列，提供极低延迟与超高吞吐，无内存读放大与分支预测惩罚。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastBlockedBloomFilter {
    /// 512 位块的连续位图，每块占 8 个 u64 (64 字节)
    bits: Vec<u64>,
    /// 块总数 (num_blocks >= 1)
    num_blocks: usize,
    /// 记录的条目总数
    num_entries: usize,
    /// 散列探测次数 (对于 10 bits/entry 推荐 7 次)
    num_hashes: u8,
}

pub type BloomFilter = FastBlockedBloomFilter;

impl FastBlockedBloomFilter {
    /// 默认假阳性率 p = 0.01 (1%)，单条目占用 10 bits，num_hashes = 7
    pub const DEFAULT_FALSE_POSITIVE_RATE: f64 = 0.01;
    pub const DEFAULT_BITS_PER_ENTRY: f64 = 10.0;
    pub const DEFAULT_NUM_HASHES: u8 = 7;
    pub const BLOCK_BITS: usize = 512;
    pub const WORDS_PER_BLOCK: usize = 8; // 512 / 64 = 8

    /// 根据预期条目数与假阳性率构建布隆过滤器
    pub fn new(expected_entries: usize, false_positive_rate: f64) -> Self {
        let p = false_positive_rate.clamp(0.00001, 0.5);
        let bits_per_entry = -(p.ln() / (2.0f64.ln().powi(2))) * 1.15;
        let num_hashes = ((bits_per_entry * 2.0f64.ln()).round() as u8).clamp(1, 30);

        let total_bits = ((expected_entries as f64) * bits_per_entry).ceil() as usize;
        let total_bits = total_bits.max(Self::BLOCK_BITS);
        let num_blocks = total_bits.div_ceil(Self::BLOCK_BITS);

        Self {
            bits: vec![0u64; num_blocks * Self::WORDS_PER_BLOCK],
            num_blocks,
            num_entries: 0,
            num_hashes,
        }
    }

    /// 使用默认参数 (1% 假阳性率) 创建
    pub fn new_with_defaults(expected_entries: usize) -> Self {
        Self::new(expected_entries, Self::DEFAULT_FALSE_POSITIVE_RATE)
    }

    /// 从可迭代集合批量构建布隆过滤器
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a StoreHash>) -> Self {
        let items: Vec<&'a StoreHash> = entries.into_iter().collect();
        let mut filter = Self::new_with_defaults(items.len());
        for hash in items {
            filter.insert(hash);
        }
        filter
    }

    /// 从原始字节流与元数据还原布隆过滤器
    pub fn from_bytes(
        bytes: &[u8],
        num_entries: usize,
        num_hashes: u8,
    ) -> Result<Self, BloomError> {
        if num_hashes == 0 {
            return Err(BloomError::ZeroHashCount(0));
        }
        if bytes.is_empty() || !bytes.len().is_multiple_of(64) {
            return Err(BloomError::InvalidByteLength {
                actual: bytes.len(),
            });
        }

        let num_blocks = bytes.len() / 64;
        let mut bits = Vec::with_capacity(num_blocks * Self::WORDS_PER_BLOCK);

        let (chunks, _) = bytes.as_chunks::<8>();
        for chunk in chunks {
            bits.push(u64::from_le_bytes(*chunk));
        }

        Ok(Self {
            bits,
            num_blocks,
            num_entries,
            num_hashes,
        })
    }

    /// 导出为紧凑二进制字节流 (小端对齐)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.bits.len() * 8);
        for word in &self.bits {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// 向布隆过滤器插入 StoreHash
    pub fn insert(&mut self, hash: &StoreHash) {
        self.insert_bytes(hash.as_bytes());
    }

    /// 向布隆过滤器插入原始字节切片
    pub fn insert_bytes(&mut self, key: &[u8]) {
        let (h1, h2) = murmur3_x64_128(key, 0);
        let block_idx = (h1 as usize) % self.num_blocks;
        let block_offset = block_idx * Self::WORDS_PER_BLOCK;

        let base = (h1 >> 32) ^ (h1 & 0xFFFF_FFFF);
        let step = h2 | 1;

        for i in 0..self.num_hashes {
            let probe = base.wrapping_add((i as u64).wrapping_mul(step));
            let bit_in_block = (probe & 511) as usize;
            let word_idx = bit_in_block >> 6;
            let bit_idx = bit_in_block & 63;
            self.bits[block_offset + word_idx] |= 1u64 << bit_idx;
        }

        self.num_entries += 1;
    }

    /// 探测 StoreHash 是否可能存在 (False Positive Rate ~ 1%，绝对无 False Negative)
    pub fn contains(&self, hash: &StoreHash) -> bool {
        self.contains_bytes(hash.as_bytes())
    }

    /// 探测原始字节切片是否可能存在
    pub fn contains_bytes(&self, key: &[u8]) -> bool {
        if self.num_entries == 0 {
            return false;
        }

        let (h1, h2) = murmur3_x64_128(key, 0);
        let block_idx = (h1 as usize) % self.num_blocks;
        let block_offset = block_idx * Self::WORDS_PER_BLOCK;

        let base = (h1 >> 32) ^ (h1 & 0xFFFF_FFFF);
        let step = h2 | 1;

        for i in 0..self.num_hashes {
            let probe = base.wrapping_add((i as u64).wrapping_mul(step));
            let bit_in_block = (probe & 511) as usize;
            let word_idx = bit_in_block >> 6;
            let bit_idx = bit_in_block & 63;
            if (self.bits[block_offset + word_idx] & (1u64 << bit_idx)) == 0 {
                return false;
            }
        }

        true
    }

    /// 获取已记录条目数
    pub fn num_entries(&self) -> usize {
        self.num_entries
    }

    /// 获取位图总位数
    pub fn num_bits(&self) -> u64 {
        (self.num_blocks * Self::BLOCK_BITS) as u64
    }

    /// 获取每个条目的散列次数
    pub fn num_hashes(&self) -> u8 {
        self.num_hashes
    }

    /// 获取块总数
    pub fn num_blocks(&self) -> usize {
        self.num_blocks
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.num_entries == 0
    }
}
