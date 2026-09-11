use crate::{
    error::{CoreError, TypeError},
    types::{IndexEntry, NUM_SHARDS, NarDigest, ShardDescriptor, StoreHash},
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Nix RFC 4648 变体 Base32 编码字符集 (长度 32)
pub const NIX_BASE32_ALPHABET: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

const ENTRY_LEAF_DOMAIN: &str = "nixcache/merkle/v7/entry-leaf";
const SHARD_NODE_DOMAIN: &str = "nixcache/merkle/v7/shard-node";
const SHARD_ROOT_DOMAIN: &str = "nixcache/merkle/v7/shard-root";
const DESCRIPTOR_LEAF_DOMAIN: &str = "nixcache/merkle/v7/descriptor-leaf";
const ROOT_NODE_DOMAIN: &str = "nixcache/merkle/v7/root-node";
const ROOT_ROOT_DOMAIN: &str = "nixcache/merkle/v7/root-root";
const EMPTY_SUBTREE_DOMAIN: &str = "nixcache/merkle/v7/empty-subtree";

type DigestBytes = [u8; 32];

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

/// 计算单个分片内部所有条目的 v7 分层 Merkle 散列值。
///
/// 条目按 StoreHash 的 canonical 字典序排序。每个条目先生成叶子，之后
/// 使用带层级和基数标记的二叉树归并；最终再将 shard 身份包进 shard root。
pub fn compute_shard_merkle_hash(
    shard_id: u16,
    entries: &HashMap<StoreHash, IndexEntry>,
) -> Result<String, CoreError> {
    let root = compute_shard_root_bytes(shard_id, entries)?;
    Ok(format_digest(root))
}

fn compute_shard_root_bytes(
    shard_id: u16,
    entries: &HashMap<StoreHash, IndexEntry>,
) -> Result<DigestBytes, CoreError> {
    validate_shard_id(shard_id)?;

    let mut sorted_hashes: Vec<&StoreHash> = entries.keys().collect();
    sorted_hashes.sort();

    let mut leaves = Vec::with_capacity(sorted_hashes.len());
    for hash in sorted_hashes {
        let entry = entries.get(hash).expect("entry key collected from map");
        validate_entry_identity(hash, entry, shard_id)?;
        leaves.push(entry_leaf_hash(hash, entry)?);
    }

    let subtree_root = layered_root(&leaves, SHARD_NODE_DOMAIN);
    Ok(hash_with_encoder(SHARD_ROOT_DOMAIN, |encoder| {
        encoder.u16(shard_id);
        encoder.bytes(&shard_id_to_prefix_bytes(shard_id));
        encoder.u64(entries.len() as u64);
        encoder.digest(subtree_root);
    }))
}

fn entry_leaf_hash(hash: &StoreHash, entry: &IndexEntry) -> Result<DigestBytes, CoreError> {
    let nar_digest = canonical_digest_bytes(entry.nar_digest.as_str(), "entry nar_digest")?;
    Ok(hash_with_encoder(ENTRY_LEAF_DOMAIN, |encoder| {
        encoder.bytes(hash.as_bytes());
        encoder.string(&entry.name);
        encoder.option_string(entry.system.map(|system| system.as_str()));

        encoder.string(&entry.narinfo_meta.store_path);
        encoder.string(&entry.narinfo_meta.nar_basename);
        encoder.option_string(entry.narinfo_meta.compression.as_deref());
        encoder.option_string(entry.narinfo_meta.file_hash.as_deref());
        encoder.option_u64(entry.narinfo_meta.file_size);
        encoder.string(&entry.narinfo_meta.nar_hash);
        encoder.string_vec(&entry.narinfo_meta.references);
        encoder.option_string(entry.narinfo_meta.deriver.as_deref());
        encoder.string_vec(&entry.narinfo_meta.signatures);
        encoder.option_string(entry.narinfo_meta.ca.as_deref());

        encoder.digest(nar_digest);
        encoder.u64(entry.nar_size);
        encoder.string(&entry.added);
        encoder.option_string(entry.origin_job.as_deref());
    }))
}

/// 计算 1024 个分片的 v7 全局 Merkle Root Hash。
///
/// 输入必须恰好包含 ID 为 `0..1023` 的唯一描述符集合；输入顺序不参与结果。
pub fn compute_merkle_root(shards: &[ShardDescriptor]) -> Result<String, CoreError> {
    let ordered = index_shard_descriptors(shards)?;
    let leaves = ordered
        .into_iter()
        .map(descriptor_leaf_hash)
        .collect::<Result<Vec<_>, _>>()?;
    let descriptor_tree_root = layered_root(&leaves, ROOT_NODE_DOMAIN);
    let root = hash_with_encoder(ROOT_ROOT_DOMAIN, |encoder| {
        encoder.u64(NUM_SHARDS as u64);
        encoder.digest(descriptor_tree_root);
    });
    Ok(format_digest(root))
}

fn descriptor_leaf_hash(descriptor: &ShardDescriptor) -> Result<DigestBytes, CoreError> {
    let blob_digest = if descriptor.blob_digest.is_empty() {
        None
    } else {
        Some(canonical_digest_bytes(
            &descriptor.blob_digest,
            "descriptor blob_digest",
        )?)
    };
    let merkle_hash = canonical_digest_bytes(&descriptor.merkle_hash, "descriptor merkle_hash")?;

    Ok(hash_with_encoder(DESCRIPTOR_LEAF_DOMAIN, |encoder| {
        encoder.u16(descriptor.shard_id);
        encoder.bytes(descriptor.prefix.as_bytes());
        encoder.option_digest(blob_digest);
        encoder.u64(descriptor.compressed_size);
        encoder.u64(descriptor.uncompressed_size);
        encoder.u64(descriptor.entry_count as u64);
        encoder.digest(merkle_hash);
    }))
}

fn index_shard_descriptors(shards: &[ShardDescriptor]) -> Result<Vec<&ShardDescriptor>, CoreError> {
    if shards.len() != NUM_SHARDS {
        return Err(CoreError::InvalidShardSet {
            details: format!("expected exactly {NUM_SHARDS} shard descriptors"),
        });
    }

    let mut ordered: Vec<Option<&ShardDescriptor>> = vec![None; NUM_SHARDS];
    for descriptor in shards {
        validate_shard_id(descriptor.shard_id)?;
        let slot = &mut ordered[descriptor.shard_id as usize];
        if slot.is_some() {
            return Err(CoreError::InvalidShardSet {
                details: format!("duplicate shard id {}", descriptor.shard_id),
            });
        }
        descriptor.validate_structure()?;
        *slot = Some(descriptor);
    }

    ordered
        .into_iter()
        .enumerate()
        .map(|(id, descriptor)| {
            descriptor.ok_or_else(|| CoreError::InvalidShardSet {
                details: format!("missing shard id {id}"),
            })
        })
        .collect()
}

/// 比对两组完整的 ShardDescriptor 清单，精准返回发生变更的分片 ID 列表。
pub fn diff_shard_descriptors(
    old_shards: &[ShardDescriptor],
    new_shards: &[ShardDescriptor],
) -> Result<Vec<u16>, CoreError> {
    let old = index_shard_descriptors(old_shards)?;
    let new = index_shard_descriptors(new_shards)?;

    Ok(old
        .into_iter()
        .zip(new)
        .filter_map(|(old, new)| (old != new).then_some(new.shard_id))
        .collect())
}

fn validate_entry_identity(
    hash: &StoreHash,
    entry: &IndexEntry,
    shard_id: u16,
) -> Result<(), CoreError> {
    StoreHash::parse(hash.as_str()).map_err(|error| CoreError::InvalidEntry {
        details: format!("map key {hash} is not canonical: {error}"),
    })?;
    entry.validate_structure()?;
    if hash.shard_id() != shard_id {
        return Err(CoreError::InvalidMerkle {
            details: format!("entry {hash} belongs to another shard"),
        });
    }
    if entry.store_hash().as_ref() != Some(hash) {
        return Err(CoreError::InvalidMerkle {
            details: format!("entry {hash} StorePath does not match map key"),
        });
    }
    Ok(())
}

fn validate_shard_id(shard_id: u16) -> Result<(), CoreError> {
    if shard_id < NUM_SHARDS as u16 {
        Ok(())
    } else {
        Err(CoreError::InvalidMerkle {
            details: format!("shard id {shard_id} is out of range"),
        })
    }
}

fn layered_root(leaves: &[DigestBytes], node_domain: &str) -> DigestBytes {
    if leaves.is_empty() {
        return hash_with_encoder(EMPTY_SUBTREE_DOMAIN, |encoder| {
            encoder.u64(0);
            encoder.u8(0);
        });
    }
    if leaves.len() == 1 {
        return leaves[0];
    }

    let mut level = leaves.to_vec();
    let mut level_number = 1_u64;
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let (arity, left, right) = match pair {
                [left, right] => (2_u8, *left, Some(*right)),
                [left] => (1_u8, *left, None),
                _ => unreachable!("chunks(2) never produces an empty pair"),
            };
            next.push(hash_with_encoder(node_domain, |encoder| {
                encoder.u64(level_number);
                encoder.u8(arity);
                encoder.digest(left);
                if let Some(right) = right {
                    encoder.digest(right);
                }
            }));
        }
        level = next;
        level_number += 1;
    }
    level[0]
}

fn canonical_digest_bytes(value: &str, field: &'static str) -> Result<DigestBytes, CoreError> {
    let parsed = NarDigest::parse(value).map_err(|error| CoreError::InvalidCanonicalDigest {
        field,
        details: error.to_string(),
    })?;
    if parsed.as_str() != value {
        return Err(CoreError::InvalidCanonicalDigest {
            field,
            details: "digest must use the canonical lowercase sha256 form".to_string(),
        });
    }

    let mut bytes = [0_u8; 32];
    let hex = &value.as_bytes()[7..];
    for index in 0..32 {
        bytes[index] = (hex_value(hex[index * 2]) << 4) | hex_value(hex[index * 2 + 1]);
    }
    Ok(bytes)
}

#[inline]
fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("canonical digest was validated before decoding"),
    }
}

fn hash_with_encoder(domain: &str, encode: impl FnOnce(&mut CanonicalEncoder)) -> DigestBytes {
    let mut encoder = CanonicalEncoder::default();
    encoder.string(domain);
    encode(&mut encoder);
    Sha256::digest(encoder.finish()).into()
}

fn format_digest(digest: DigestBytes) -> String {
    let mut output = String::with_capacity(71);
    output.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

#[derive(Default)]
struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.raw(&value.to_be_bytes());
    }

    #[allow(dead_code)]
    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    #[allow(dead_code)]
    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.raw(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn digest(&mut self, value: DigestBytes) {
        self.raw(&value);
    }

    fn option_digest(&mut self, value: Option<DigestBytes>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.digest(value);
            }
            None => self.u8(0),
        }
    }

    fn option_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.string(value);
            }
            None => self.u8(0),
        }
    }

    fn option_u64(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.u64(value);
            }
            None => self.u8(0),
        }
    }

    fn string_vec(&mut self, values: &[String]) {
        self.u64(values.len() as u64);
        for value in values {
            self.string(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalEncoder, ENTRY_LEAF_DOMAIN, NUM_SHARDS, descriptor_leaf_hash, format_digest,
        layered_root,
    };
    use crate::{
        IndexEntry, NarDigest, NarInfoMeta, ShardDataPayload, ShardDescriptor,
        ShardedArchCacheIndexData, StoreHash, SystemArch, compute_merkle_root,
        compute_shard_merkle_hash, diff_shard_descriptors,
    };
    use std::collections::HashMap;

    #[test]
    fn canonical_encoder_distinguishes_length_boundaries() {
        let mut first = CanonicalEncoder::default();
        first.string("a");
        first.string("bc");

        let mut second = CanonicalEncoder::default();
        second.string("ab");
        second.string("c");

        assert_ne!(first.finish(), second.finish());

        let mut fixed = CanonicalEncoder::default();
        fixed.u16(0x0102);
        fixed.u32(0x03040506);
        fixed.u64(0x0708090a0b0c0d0e);
        fixed.bool(false);
        fixed.bool(true);
        assert_eq!(
            fixed.finish(),
            vec![
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
                0x00, 0x01,
            ]
        );

        let mut boundary = CanonicalEncoder::default();
        boundary.string(":,;\0雪");
        boundary.string("tail");
        let mut split = CanonicalEncoder::default();
        split.string(":,;");
        split.string("\0雪tail");
        assert_ne!(boundary.finish(), split.finish());
    }

    #[test]
    fn layered_tree_uses_unary_nodes_for_odd_levels() {
        let leaves = [[1_u8; 32], [2_u8; 32], [3_u8; 32]];
        let domain = "nixcache/merkle/v7/test-node";
        let root = layered_root(&leaves, domain);
        let reordered = [leaves[0], leaves[2], leaves[1]];

        assert_eq!(
            format_digest(layered_root(&[], domain)),
            "sha256:f9293ba1d29efd64af7c0ce424280445f304973a7bf16ac966b9ed176dc24b73"
        );
        assert_eq!(
            format_digest(layered_root(&leaves[..1], domain)),
            "sha256:0101010101010101010101010101010101010101010101010101010101010101"
        );
        assert_eq!(
            format_digest(layered_root(&leaves[..2], domain)),
            "sha256:d110298a12f0b046459648953b1c45a8ea4d4f2e36749a7ffdf5d64e4f17e213"
        );
        assert_eq!(
            format_digest(root),
            "sha256:2f2e862014e28dcd0019e0ab844f731c981f756216415976282841e021ef6350"
        );

        assert_ne!(root, layered_root(&reordered, domain));
        assert_ne!(root, layered_root(&[], ENTRY_LEAF_DOMAIN));
    }

    fn test_digest() -> NarDigest {
        NarDigest::new_sha256("0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0")
            .expect("test digest must be valid")
    }

    fn test_entry(hash: &StoreHash) -> IndexEntry {
        IndexEntry {
            name: "pkg".to_string(),
            system: Some(SystemArch::X86_64Linux),
            narinfo_meta: NarInfoMeta {
                store_path: format!("/nix/store/{hash}-pkg"),
                nar_basename: "pkg.nar.xz".to_string(),
                nar_hash: "sha256:0d1b50428e2194f481ad1cf387f3b8908861cf12674e1d743a6d9627fb2e2ff0"
                    .to_string(),
                ..Default::default()
            },
            nar_digest: test_digest(),
            nar_size: 100,
            added: "2026-08-29T00:00:00Z".to_string(),
            origin_job: None,
        }
    }

    fn test_hash() -> StoreHash {
        StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsw").expect("test hash must be valid")
    }

    #[test]
    fn entry_leaf_covers_every_persistent_field() {
        let hash = test_hash();
        let shard_id = hash.shard_id();
        let base = test_entry(&hash);
        let base_hash =
            compute_shard_merkle_hash(shard_id, &HashMap::from([(hash.clone(), base.clone())]))
                .expect("base entry must hash");

        macro_rules! assert_changes_hash {
            ($name:literal, $change:expr) => {{
                let mut entry = base.clone();
                $change(&mut entry);
                let changed =
                    compute_shard_merkle_hash(shard_id, &HashMap::from([(hash.clone(), entry)]))
                        .expect(concat!($name, " must hash"));
                assert_ne!(base_hash, changed, "{0} must affect the shard root", $name);
            }};
        }

        assert_changes_hash!("name", |entry: &mut IndexEntry| entry.name =
            "other".to_string());
        assert_changes_hash!("system", |entry: &mut IndexEntry| {
            entry.system = Some(SystemArch::Aarch64Linux)
        });
        assert_changes_hash!("store_path", |entry: &mut IndexEntry| {
            entry.narinfo_meta.store_path = format!("/nix/store/{hash}-other")
        });
        assert_changes_hash!("nar_basename", |entry: &mut IndexEntry| {
            entry.narinfo_meta.nar_basename = "other.nar.xz".to_string()
        });
        assert_changes_hash!("compression", |entry: &mut IndexEntry| {
            entry.narinfo_meta.compression = Some("xz".to_string())
        });
        assert_changes_hash!("file_hash", |entry: &mut IndexEntry| {
            entry.narinfo_meta.file_hash = Some(
                "sha256:0000000000000000000000000000000000000000000000000000000000000001"
                    .to_string(),
            )
        });
        assert_changes_hash!("file_size", |entry: &mut IndexEntry| {
            entry.narinfo_meta.file_size = Some(1)
        });
        assert_changes_hash!("nar_hash", |entry: &mut IndexEntry| {
            entry.narinfo_meta.nar_hash =
                "sha256:0000000000000000000000000000000000000000000000000000000000000001"
                    .to_string()
        });
        assert_changes_hash!("references", |entry: &mut IndexEntry| {
            entry.narinfo_meta.references.push(format!("{hash}-dep"))
        });
        assert_changes_hash!("deriver", |entry: &mut IndexEntry| {
            entry.narinfo_meta.deriver = Some(format!("{hash}-build.drv"))
        });
        assert_changes_hash!("signatures", |entry: &mut IndexEntry| {
            entry.narinfo_meta.signatures.push("cache:key".to_string())
        });
        assert_changes_hash!("ca", |entry: &mut IndexEntry| {
            entry.narinfo_meta.ca = Some("fixed:sha256:abc".to_string())
        });
        assert_changes_hash!("nar_digest", |entry: &mut IndexEntry| {
            entry.nar_digest = NarDigest::new_sha256(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .expect("test digest must be valid")
        });
        assert_changes_hash!("nar_size", |entry: &mut IndexEntry| entry.nar_size = 101);
        assert_changes_hash!("added", |entry: &mut IndexEntry| {
            entry.added = "2026-08-30T00:00:00Z".to_string()
        });
        assert_changes_hash!("origin_job", |entry: &mut IndexEntry| {
            entry.origin_job = Some("job-2".to_string())
        });

        let other_hash = StoreHash::parse("s66mzxpvicwk07gjbjfw9izjfa797vsa").unwrap();
        let other_entry = test_entry(&other_hash);
        let two_entries = HashMap::from([(hash.clone(), base), (other_hash.clone(), other_entry)]);
        let mut reversed = HashMap::new();
        reversed.insert(other_hash.clone(), test_entry(&other_hash));
        reversed.insert(hash.clone(), test_entry(&hash));
        assert_eq!(
            compute_shard_merkle_hash(shard_id, &two_entries).unwrap(),
            compute_shard_merkle_hash(shard_id, &reversed).unwrap()
        );
    }

    #[test]
    fn empty_shards_are_identity_bound_and_old_hashes_are_rejected() {
        let empty_zero = ShardDataPayload::new(0)
            .unwrap()
            .compute_merkle_hash()
            .unwrap();
        let empty_one = ShardDataPayload::new(1)
            .unwrap()
            .compute_merkle_hash()
            .unwrap();
        assert_ne!(empty_zero, empty_one);
        assert_ne!(
            empty_zero,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let hash = test_hash();
        let entries = HashMap::from([(hash.clone(), test_entry(&hash))]);
        assert!(compute_shard_merkle_hash(1024, &entries).is_err());
        assert!(compute_shard_merkle_hash(0, &entries).is_err());
        assert!(ShardDataPayload::new(NUM_SHARDS as u16).is_err());
        assert!(ShardDescriptor::empty(NUM_SHARDS as u16).is_err());
    }

    #[test]
    fn root_is_order_independent_but_requires_a_complete_set() {
        let root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "repo", "registry");
        let expected = compute_merkle_root(&root.shards).unwrap();
        let mut reversed = root.shards.clone();
        reversed.reverse();
        assert_eq!(expected, compute_merkle_root(&reversed).unwrap());

        assert!(compute_merkle_root(&reversed[..NUM_SHARDS - 1]).is_err());
        let mut duplicate = reversed;
        duplicate[0] = duplicate[1].clone();
        assert!(compute_merkle_root(&duplicate).is_err());

        let mut wrong_prefix = root.shards.clone();
        wrong_prefix[0].prefix = "01".to_string();
        assert!(compute_merkle_root(&wrong_prefix).is_err());

        let mut invalid_digest = root.shards.clone();
        invalid_digest[0].merkle_hash = "sha256:bad".to_string();
        assert!(compute_merkle_root(&invalid_digest).is_err());
    }

    #[test]
    fn descriptor_hash_covers_all_descriptor_fields() {
        let descriptor = ShardDescriptor::empty(0).unwrap();
        let base = descriptor_leaf_hash(&descriptor).unwrap();

        let mut changed = descriptor.clone();
        changed.blob_digest =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        changed.compressed_size = 1;
        changed.uncompressed_size = 1;
        changed.entry_count = 1;
        changed.merkle_hash =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        assert_ne!(base, descriptor_leaf_hash(&changed).unwrap());

        let mut prefix_changed = descriptor.clone();
        prefix_changed.prefix = "01".to_string();
        assert_ne!(base, descriptor_leaf_hash(&prefix_changed).unwrap());
        let mut id_changed = descriptor;
        id_changed.shard_id = 1;
        assert_ne!(base, descriptor_leaf_hash(&id_changed).unwrap());
        assert!(format_digest(base).starts_with("sha256:"));

        let mut invalid_digest = id_changed;
        invalid_digest.merkle_hash = "sha256:bad".to_string();
        assert!(descriptor_leaf_hash(&invalid_digest).is_err());
    }

    #[test]
    fn diff_compares_full_descriptors_by_id() {
        let root = ShardedArchCacheIndexData::new(SystemArch::X86_64Linux, "repo", "registry");
        let mut changed = root.shards.clone();
        changed[7].compressed_size = 1;
        changed[7].uncompressed_size = 1;
        changed[7].entry_count = 1;
        changed[7].blob_digest =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        changed[7].merkle_hash =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        assert_eq!(
            diff_shard_descriptors(&root.shards, &changed).unwrap(),
            vec![7]
        );
    }
}
