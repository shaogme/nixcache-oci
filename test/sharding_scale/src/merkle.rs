use nixcache_core::{
    IndexEntry, NUM_SHARDS, ShardDataPayload, ShardDescriptor, ShardedArchCacheIndexData,
    StoreHash, SystemArch, compute_merkle_root, compute_shard_merkle_hash, diff_shard_descriptors,
    partition_entries_by_shard,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 增量 Diff 与 Merkle 状态校验报告
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DiffVerificationReport {
    pub total_base_entries: usize,
    pub total_new_entries: usize,
    pub affected_shards_expected: Vec<u16>,
    pub affected_shards_detected: Vec<u16>,
    pub unchanged_shards_count: usize,
    pub diff_matches_exact: bool,
    pub old_merkle_root: String,
    pub new_merkle_root: String,
    pub roots_differ: bool,
}

/// 检验分片内部条目 Merkle 散列的绝对确定性 (乱序不变性)
pub fn verify_merkle_determinism(entries: &HashMap<StoreHash, IndexEntry>) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }

    let partitioned = partition_entries_by_shard(entries.clone());

    for (shard_id, shard_entries) in partitioned {
        if shard_entries.len() < 2 {
            continue;
        }

        // 1. 基准哈希计算
        let base_hash = compute_shard_merkle_hash(&shard_entries);

        // 2. 构造多种不同插入顺序的 HashMap (包含完全逆序与所有循环移位打乱)
        let entry_list: Vec<(StoreHash, IndexEntry)> = shard_entries.into_iter().collect();

        // 2.1 逆序
        let mut reversed_list = entry_list.clone();
        reversed_list.reverse();
        let map_reversed: HashMap<_, _> = reversed_list.into_iter().collect();
        let hash_reversed = compute_shard_merkle_hash(&map_reversed);
        if hash_reversed != base_hash {
            return Err(format!(
                "Determinism violation on shard {}: reversed hash '{}' != base '{}'",
                shard_id, hash_reversed, base_hash
            ));
        }

        // 2.2 循环移位打乱 (Rotate by 1..N)
        for rot in 1..entry_list.len().min(10) {
            let mut rotated = entry_list.clone();
            rotated.rotate_left(rot);
            let map_rotated: HashMap<_, _> = rotated.into_iter().collect();
            let hash_rotated = compute_shard_merkle_hash(&map_rotated);
            if hash_rotated != base_hash {
                return Err(format!(
                    "Determinism violation on shard {}: rotated ({}) hash '{}' != base '{}'",
                    shard_id, rot, hash_rotated, base_hash
                ));
            }
        }
    }

    Ok(())
}

/// 检验单条目突变或损坏时 Merkle Tree 状态检验的抗篡改敏感性 (雪崩效应)
pub fn verify_merkle_tamper_detection(
    entries: &HashMap<StoreHash, IndexEntry>,
    system: SystemArch,
) -> Result<(), String> {
    let mut root_index = ShardedArchCacheIndexData::new(system, "test/repo", "ghcr.io");
    let partitioned = partition_entries_by_shard(entries.clone());

    let mut payloads: Vec<ShardDataPayload> = Vec::with_capacity(NUM_SHARDS);
    for shard_id in 0..NUM_SHARDS as u16 {
        let shard_entries = partitioned.get(&shard_id).cloned().unwrap_or_default();
        let count = shard_entries.len();
        let payload = ShardDataPayload::with_entries(shard_id, shard_entries);
        let merkle = payload.compute_merkle_hash();
        let digest = format!("sha256:blob-fake-{}", shard_id);

        root_index.shards[shard_id as usize] = ShardDescriptor::new(
            shard_id,
            digest,
            (count * 50) as u64,
            (count * 200) as u64,
            count,
            merkle,
        );
        payloads.push(payload);
    }
    root_index.recalculate_merkle_root();

    let original_root = root_index.merkle_root.clone();

    // 找到一个非空分片并篡改其中 1 个条目
    let target_shard_id = payloads
        .iter()
        .position(|p| !p.entries.is_empty())
        .ok_or_else(|| "No non-empty shards found to test tampering".to_string())?
        as u16;

    let target_payload = &mut payloads[target_shard_id as usize];
    let first_hash = target_payload
        .entries
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| "Empty entries in target shard".to_string())?;

    let original_shard_hash = target_payload.compute_merkle_hash();

    // 篡改条目的 nar_size
    if let Some(entry) = target_payload.entries.get_mut(&first_hash) {
        entry.nar_size += 1;
    }

    let tampered_shard_hash = target_payload.compute_merkle_hash();
    if tampered_shard_hash == original_shard_hash {
        return Err(format!(
            "Tamper detection failed on shard {}: hash remained unchanged after modifying nar_size",
            target_shard_id
        ));
    }

    // 更新 root_index 对应分片并重算
    root_index.shards[target_shard_id as usize].merkle_hash = tampered_shard_hash;
    root_index.recalculate_merkle_root();

    if root_index.merkle_root == original_root {
        return Err(
            "Global Merkle Root tamper detection failed: root remained unchanged after single-item mutation"
                .to_string(),
        );
    }

    Ok(())
}

/// 验证在海量基线数据下，合入增量条目时 `diff_shard_descriptors` 的绝对精确性与零未变动分片开销
pub fn verify_incremental_diff_accuracy(
    initial_entries: &HashMap<StoreHash, IndexEntry>,
    new_entries: &HashMap<StoreHash, IndexEntry>,
    system: SystemArch,
) -> Result<DiffVerificationReport, String> {
    let mut root_index = ShardedArchCacheIndexData::new(system, "test/repo", "ghcr.io");
    let initial_partitioned = partition_entries_by_shard(initial_entries.clone());

    for shard_id in 0..NUM_SHARDS as u16 {
        let entries_in_shard = initial_partitioned
            .get(&shard_id)
            .cloned()
            .unwrap_or_default();
        let count = entries_in_shard.len();
        let payload = ShardDataPayload::with_entries(shard_id, entries_in_shard);
        let merkle = payload.compute_merkle_hash();
        let digest = format!("sha256:base-blob-{}", shard_id);

        root_index.shards[shard_id as usize] = ShardDescriptor::new(
            shard_id,
            digest,
            (count * 50) as u64,
            (count * 200) as u64,
            count,
            merkle,
        );
    }
    root_index.recalculate_merkle_root();
    let old_merkle_root = root_index.merkle_root.clone();
    let old_shards = root_index.shards.clone();

    // 确定预期受影响的分片 ID 集合
    let mut new_partitioned = partition_entries_by_shard(new_entries.clone());
    let mut expected_affected_shards: Vec<u16> = new_partitioned.keys().cloned().collect();
    expected_affected_shards.sort();

    // 模拟局部压实：仅对发生新增条目的分片更新 descriptor
    let mut updated_shards = old_shards.clone();
    for &shard_id in &expected_affected_shards {
        if let Some(incoming) = new_partitioned.remove(&shard_id) {
            let mut combined = initial_partitioned
                .get(&shard_id)
                .cloned()
                .unwrap_or_default();
            combined.extend(incoming);

            let new_count = combined.len();
            let new_payload = ShardDataPayload::with_entries(shard_id, combined);
            let new_merkle = new_payload.compute_merkle_hash();
            let new_digest = format!("sha256:updated-blob-{}", shard_id);

            updated_shards[shard_id as usize] = ShardDescriptor::new(
                shard_id,
                new_digest,
                (new_count * 50) as u64,
                (new_count * 200) as u64,
                new_count,
                new_merkle,
            );
        }
    }

    let detected_affected_shards = diff_shard_descriptors(&old_shards, &updated_shards);
    let diff_matches_exact = expected_affected_shards == detected_affected_shards;

    let new_merkle_root = compute_merkle_root(&updated_shards);
    let roots_differ = (old_merkle_root != new_merkle_root) || new_entries.is_empty();

    let unchanged_shards_count = NUM_SHARDS - detected_affected_shards.len();

    if !diff_matches_exact {
        return Err(format!(
            "Diff mismatch: expected {:?}, detected {:?}",
            expected_affected_shards, detected_affected_shards
        ));
    }

    Ok(DiffVerificationReport {
        total_base_entries: initial_entries.len(),
        total_new_entries: new_entries.len(),
        affected_shards_expected: expected_affected_shards,
        affected_shards_detected: detected_affected_shards,
        unchanged_shards_count,
        diff_matches_exact,
        old_merkle_root,
        new_merkle_root,
        roots_differ,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::generate_index_entries;

    #[test]
    fn test_merkle_verification_flow() {
        let base_entries = generate_index_entries(5000, 1001, SystemArch::X86_64Linux);
        if let Err(e) = verify_merkle_determinism(&base_entries) {
            panic!("verify_merkle_determinism failed: {}", e);
        }
        if let Err(e) = verify_merkle_tamper_detection(&base_entries, SystemArch::X86_64Linux) {
            panic!("verify_merkle_tamper_detection failed: {}", e);
        }

        let new_entries = generate_index_entries(20, 2002, SystemArch::X86_64Linux);
        let diff_report =
            verify_incremental_diff_accuracy(&base_entries, &new_entries, SystemArch::X86_64Linux)
                .expect("Diff verification should succeed");
        assert!(diff_report.diff_matches_exact);
        assert!(diff_report.unchanged_shards_count >= 1004);
    }
}
