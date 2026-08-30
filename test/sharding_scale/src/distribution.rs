use nixcache_core::{
    IndexEntry, NUM_SHARDS, StoreHash, calculate_shard_id, partition_entries_by_shard,
    partition_hashes_by_shard, shard_id_to_prefix,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 分片离散均匀度与统计学特征检验报告
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DistributionReport {
    pub total_entries: usize,
    pub num_shards: usize,
    pub expected_mean: f64,
    pub actual_mean: f64,
    pub min_shard_entries: usize,
    pub max_shard_entries: usize,
    pub std_deviation: f64,
    pub coefficient_of_variation: f64,
    pub empty_shards_count: usize,
    pub non_empty_shards_count: usize,
    pub max_deviation_percentage: f64,
    pub passed_uniformity_check: bool,
}

/// 验证给定 StoreHash 集合在 1024 个分片中的离散均匀分布情况
pub fn verify_sharding_distribution(hashes: &[StoreHash]) -> DistributionReport {
    let total_entries = hashes.len();
    let num_shards = NUM_SHARDS;
    let expected_mean = (total_entries as f64) / (num_shards as f64);

    let mut shard_counts = vec![0usize; num_shards];
    for hash in hashes {
        let shard_id = calculate_shard_id(hash);
        shard_counts[shard_id as usize] += 1;
    }

    let min_shard_entries = *shard_counts.iter().min().unwrap_or(&0);
    let max_shard_entries = *shard_counts.iter().max().unwrap_or(&0);
    let empty_shards_count = shard_counts.iter().filter(|&&c| c == 0).count();
    let non_empty_shards_count = num_shards - empty_shards_count;

    let sum_entries: usize = shard_counts.iter().sum();
    let actual_mean = (sum_entries as f64) / (num_shards as f64);

    // 计算标准差: sqrt( sum( (x - mean)^2 ) / N )
    let variance: f64 = shard_counts
        .iter()
        .map(|&c| {
            let diff = (c as f64) - actual_mean;
            diff * diff
        })
        .sum::<f64>()
        / (num_shards as f64);
    let std_deviation = variance.sqrt();
    let coefficient_of_variation = if actual_mean > 0.0 {
        std_deviation / actual_mean
    } else {
        0.0
    };

    let max_deviation = (max_shard_entries as f64 - actual_mean)
        .abs()
        .max((actual_mean - min_shard_entries as f64).abs());
    let max_deviation_percentage = if actual_mean > 0.0 {
        (max_deviation / actual_mean) * 100.0
    } else {
        0.0
    };

    // 判定标准：对于 100k 条目 (平均 ~97)，CV 应 < 0.15；对于 1M 条目 (平均 ~976)，CV 应 < 0.05
    let cv_threshold = if total_entries >= 500_000 {
        0.06
    } else if total_entries >= 100_000 {
        0.15
    } else {
        0.35
    };

    let passed_uniformity_check = coefficient_of_variation <= cv_threshold
        && (total_entries < 100_000 || empty_shards_count <= 2)
        && (total_entries < 500_000 || empty_shards_count == 0);

    DistributionReport {
        total_entries,
        num_shards,
        expected_mean,
        actual_mean,
        min_shard_entries,
        max_shard_entries,
        std_deviation,
        coefficient_of_variation,
        empty_shards_count,
        non_empty_shards_count,
        max_deviation_percentage,
        passed_uniformity_check,
    }
}

/// 验证条目与 StoreHash 的分片双向一致性
pub fn verify_entry_sharding_invariance(
    entries: &HashMap<StoreHash, IndexEntry>,
) -> Result<(), String> {
    let partitioned = partition_entries_by_shard(entries.clone());
    let hashes: Vec<StoreHash> = entries.keys().cloned().collect();
    let partitioned_hashes = partition_hashes_by_shard(&hashes);

    let mut reconstructed_count = 0;

    for (shard_id, shard_entries) in &partitioned {
        reconstructed_count += shard_entries.len();
        let expected_prefix = shard_id_to_prefix(*shard_id);

        for hash in shard_entries.keys() {
            // 1. 验证哈希前两个字符与分片前缀完全吻合
            let hash_str = hash.as_str();
            if !hash_str.starts_with(&expected_prefix) {
                return Err(format!(
                    "Prefix mismatch for hash {}: expected prefix '{}', got '{}'",
                    hash_str,
                    expected_prefix,
                    &hash_str[..2]
                ));
            }

            // 2. 验证 calculate_shard_id 定位结果与所在分片 ID 完全一致
            let computed_id = calculate_shard_id(hash);
            if computed_id != *shard_id {
                return Err(format!(
                    "Shard ID mismatch for hash {}: in bucket {}, but computed {}",
                    hash_str, shard_id, computed_id
                ));
            }
        }

        // 3. 验证 partition_hashes_by_shard 与 partition_entries_by_shard 对应分片条目数一致
        let hash_count = partitioned_hashes
            .get(shard_id)
            .map(|v| v.len())
            .unwrap_or(0);
        if hash_count != shard_entries.len() {
            return Err(format!(
                "Shard count mismatch for bucket {}: entries has {}, hashes has {}",
                shard_id,
                shard_entries.len(),
                hash_count
            ));
        }
    }

    if reconstructed_count != entries.len() {
        return Err(format!(
            "Total count mismatch: original has {}, partitioned has {}",
            entries.len(),
            reconstructed_count
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::{generate_index_entries, generate_store_hashes};
    use nixcache_core::SystemArch;

    #[test]
    fn test_distribution_at_10k_and_100k() {
        let hashes_10k = generate_store_hashes(10_000, 12345);
        let report_10k = verify_sharding_distribution(&hashes_10k);
        assert!(report_10k.passed_uniformity_check);

        let hashes_100k = generate_store_hashes(100_000, 54321);
        let report_100k = verify_sharding_distribution(&hashes_100k);
        assert!(report_100k.passed_uniformity_check);
        assert_eq!(report_100k.empty_shards_count, 0);
        assert!(report_100k.coefficient_of_variation < 0.12);
    }

    #[test]
    fn test_invariance_verification() {
        let entries = generate_index_entries(2000, 777, SystemArch::X86_64Linux);
        assert!(verify_entry_sharding_invariance(&entries).is_ok());
    }
}
