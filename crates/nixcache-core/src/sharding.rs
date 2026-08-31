use crate::{
    error::TypeError,
    types::{IndexEntry, ShardDescriptor, StoreHash},
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Nix RFC 4648 变体 Base32 编码字符集 (长度 32)
pub const NIX_BASE32_ALPHABET: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// 空分片数据的标准规范 SHA-256 Merkle 散列
pub const EMPTY_SHARD_MERKLE_HASH: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 将 ASCII 字节转换为 Nix Base32 对应的值 (0..31)
#[inline(always)]
pub fn nix_base32_val(byte: u8) -> Result<u8, TypeError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'd' => Ok(byte - b'a' + 10),
        b'f'..=b'n' => Ok(byte - b'f' + 14),
        b'p'..=b's' => Ok(byte - b'p' + 23),
        b'v'..=b'z' => Ok(byte - b'v' + 27),
        _ => Err(TypeError::StoreHashInvalidChar {
            char: byte as char,
            index: 0,
        }),
    }
}

/// 将 Nix Base32 值 (0..31) 转换为 ASCII 字符
#[inline(always)]
pub fn nix_base32_char(val: u8) -> Result<u8, TypeError> {
    if (val as usize) < NIX_BASE32_ALPHABET.len() {
        Ok(NIX_BASE32_ALPHABET[val as usize])
    } else {
        Err(TypeError::StoreHashInvalidChar {
            char: val as char,
            index: 0,
        })
    }
}

/// 计算 StoreHash 对应的分片 ID (0..1023)
#[inline(always)]
pub fn calculate_shard_id(hash: &StoreHash) -> u16 {
    let bytes = hash.as_bytes();
    let c0 = nix_base32_val(bytes[0]).unwrap_or(0) as u16;
    let c1 = nix_base32_val(bytes[1]).unwrap_or(0) as u16;
    (c0 << 5) | c1
}

/// 从字符串前缀计算分片 ID (0..1023)
pub fn calculate_shard_id_from_str(s: &str) -> Result<u16, TypeError> {
    let trimmed = s.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 2 {
        return Err(TypeError::StoreHashInvalidLength {
            actual: bytes.len(),
        });
    }
    let c0 = nix_base32_val(bytes[0])? as u16;
    let c1 = nix_base32_val(bytes[1])? as u16;
    Ok((c0 << 5) | c1)
}

/// 将分片 ID (0..1023) 转换为 2 字符 Nix Base32 前缀字符串 (如 "00", "s6", "zz")
pub fn shard_id_to_prefix(shard_id: u16) -> String {
    let [b0, b1] = shard_id_to_prefix_bytes(shard_id);
    format!("{}{}", b0 as char, b1 as char)
}

/// 将分片 ID (0..1023) 转换为 2 字符 Nix Base32 字节数组
pub fn shard_id_to_prefix_bytes(shard_id: u16) -> [u8; 2] {
    let c0 = ((shard_id >> 5) & 0x1F) as usize;
    let c1 = (shard_id & 0x1F) as usize;
    [NIX_BASE32_ALPHABET[c0], NIX_BASE32_ALPHABET[c1]]
}

/// 将 IndexEntry 集合按 1024 个分片进行分组分桶
pub fn partition_entries_by_shard(
    entries: HashMap<StoreHash, IndexEntry>,
) -> HashMap<u16, HashMap<StoreHash, IndexEntry>> {
    let mut partitioned: HashMap<u16, HashMap<StoreHash, IndexEntry>> = HashMap::new();
    for (hash, entry) in entries {
        let shard_id = calculate_shard_id(&hash);
        partitioned.entry(shard_id).or_default().insert(hash, entry);
    }
    partitioned
}

/// 将 StoreHash 列表按 1024 个分片进行分组
pub fn partition_hashes_by_shard(hashes: &[StoreHash]) -> HashMap<u16, Vec<StoreHash>> {
    let mut partitioned: HashMap<u16, Vec<StoreHash>> = HashMap::new();
    for hash in hashes {
        let shard_id = calculate_shard_id(hash);
        partitioned.entry(shard_id).or_default().push(hash.clone());
    }
    partitioned
}

/// 计算单个分片内部所有条目的确定性 Merkle 散列值
///
/// 算法：先对条目按 StoreHash 字典序排序，依次哈希条目元数据，产出确定性 `sha256:<hex>`
pub fn compute_shard_merkle_hash(entries: &HashMap<StoreHash, IndexEntry>) -> String {
    if entries.is_empty() {
        return EMPTY_SHARD_MERKLE_HASH.to_string();
    }

    let mut sorted_hashes: Vec<&StoreHash> = entries.keys().collect();
    sorted_hashes.sort();

    let mut hasher = Sha256::new();
    for hash in sorted_hashes {
        if let Some(entry) = entries.get(hash) {
            hasher.update(hash.as_bytes());
            hasher.update(b":");
            hasher.update(entry.name.as_bytes());
            hasher.update(b":");
            hasher.update(entry.nar_digest.as_bytes());
            hasher.update(b":");
            hasher.update(entry.nar_size.to_be_bytes());
            hasher.update(b":");
            hasher.update(entry.narinfo_meta.nar_hash.as_bytes());
            hasher.update(b":");
            for dep in &entry.narinfo_meta.references {
                hasher.update(dep.as_bytes());
                hasher.update(b",");
            }
            hasher.update(b";");
        }
    }

    let result = hasher.finalize();
    format!("sha256:{}", bytes_to_hex(&result))
}

/// 计算 1024 个分片的全局 Merkle Root Hash
///
/// MerkleRoot = SHA-256( Shard_0.merkle_hash || Shard_1.merkle_hash || ... || Shard_1023.merkle_hash )
pub fn compute_merkle_root(shards: &[ShardDescriptor]) -> String {
    let mut hasher = Sha256::new();
    for shard in shards {
        hasher.update(shard.shard_id.to_be_bytes());
        hasher.update(shard.merkle_hash.as_bytes());
        hasher.update(shard.blob_digest.as_bytes());
    }
    let result = hasher.finalize();
    format!("sha256:{}", bytes_to_hex(&result))
}

#[inline]
fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    hex
}

/// 比对两组 ShardDescriptor 清单，精准返回发生变更的分片 ID 列表
pub fn diff_shard_descriptors(
    old_shards: &[ShardDescriptor],
    new_shards: &[ShardDescriptor],
) -> Vec<u16> {
    let mut changed = Vec::new();
    let max_len = old_shards.len().max(new_shards.len());

    for i in 0..max_len {
        let old_desc = old_shards.get(i);
        let new_desc = new_shards.get(i);

        match (old_desc, new_desc) {
            (Some(o), Some(n)) => {
                if o.merkle_hash != n.merkle_hash
                    || o.blob_digest != n.blob_digest
                    || o.entry_count != n.entry_count
                {
                    changed.push(n.shard_id);
                }
            }
            (None, Some(n)) => changed.push(n.shard_id),
            (Some(o), None) => changed.push(o.shard_id),
            (None, None) => {}
        }
    }

    changed.sort();
    changed.dedup();
    changed
}
