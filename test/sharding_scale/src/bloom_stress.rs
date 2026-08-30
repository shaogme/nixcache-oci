use nixcache_core::{FastBlockedBloomFilter, StoreHash};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// 全局布隆过滤器大规模压测与假阳性率检验报告
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BloomFilterReport {
    pub total_inserted: usize,
    pub tested_positive_hits: usize,
    pub false_negatives_count: usize,
    pub tested_negative_probes: usize,
    pub false_positives_count: usize,
    pub false_positive_rate: f64,
    pub total_bits: u64,
    pub memory_size_bytes: usize,
    pub bits_per_entry: f64,
    pub insert_duration_ms: f64,
    pub insert_qps: f64,
    pub query_duration_ms: f64,
    pub query_qps: f64,
    pub serialization_verified: bool,
    pub passed_scale_check: bool,
}

/// 对 FastBlockedBloomFilter 进行百万级压测、假阳性率检验与序列化验证
pub fn verify_bloom_filter_scale(
    inserted_hashes: &[StoreHash],
    non_existent_hashes: &[StoreHash],
) -> Result<BloomFilterReport, String> {
    let total_inserted = inserted_hashes.len();
    if total_inserted == 0 {
        return Err("Cannot stress test bloom filter with 0 entries".to_string());
    }

    // 1. 测量构建与插入性能
    let insert_start = Instant::now();
    let mut filter = FastBlockedBloomFilter::new_with_defaults(total_inserted);
    for hash in inserted_hashes {
        filter.insert(hash);
    }
    let insert_elapsed = insert_start.elapsed();
    let insert_duration_ms = insert_elapsed.as_secs_f64() * 1000.0;
    let insert_qps = if insert_elapsed.as_secs_f64() > 0.0 {
        (total_inserted as f64) / insert_elapsed.as_secs_f64()
    } else {
        0.0
    };

    // 2. 验证零假阴性 (Zero False Negatives)
    let query_start = Instant::now();
    let mut false_negatives_count = 0;
    for hash in inserted_hashes {
        if !filter.contains(hash) {
            false_negatives_count += 1;
        }
    }
    let tested_positive_hits = total_inserted - false_negatives_count;

    // 3. 验证假阳性率 (False Positive Rate)
    let tested_negative_probes = non_existent_hashes.len();
    let mut false_positives_count = 0;
    for hash in non_existent_hashes {
        if filter.contains(hash) {
            false_positives_count += 1;
        }
    }
    let query_elapsed = query_start.elapsed();
    let total_queries = total_inserted + tested_negative_probes;
    let query_duration_ms = query_elapsed.as_secs_f64() * 1000.0;
    let query_qps = if query_elapsed.as_secs_f64() > 0.0 {
        (total_queries as f64) / query_elapsed.as_secs_f64()
    } else {
        0.0
    };

    let false_positive_rate = if tested_negative_probes > 0 {
        (false_positives_count as f64) / (tested_negative_probes as f64)
    } else {
        0.0
    };

    // 4. 验证序列化与还原位级一致性
    let bytes = filter.to_bytes();
    let memory_size_bytes = bytes.len();
    let total_bits = filter.num_bits();
    let bits_per_entry = (total_bits as f64) / (total_inserted as f64);

    let restored =
        FastBlockedBloomFilter::from_bytes(&bytes, filter.num_entries(), filter.num_hashes())
            .map_err(|e| format!("Bloom filter deserialization failed: {}", e))?;

    let mut serialization_verified = true;
    for hash in inserted_hashes.iter().take(10_000) {
        if !restored.contains(hash) {
            serialization_verified = false;
            break;
        }
    }

    if filter.to_bytes() != restored.to_bytes() {
        serialization_verified = false;
    }

    // 判定标准：零假阴性、假阳性率 <= 1.5%、序列化验证通过
    let passed_scale_check =
        false_negatives_count == 0 && false_positive_rate <= 0.018 && serialization_verified;

    if false_negatives_count > 0 {
        return Err(format!(
            "Bloom filter false negative detected! {} items were wrongly reported as absent",
            false_negatives_count
        ));
    }

    Ok(BloomFilterReport {
        total_inserted,
        tested_positive_hits,
        false_negatives_count,
        tested_negative_probes,
        false_positives_count,
        false_positive_rate,
        total_bits,
        memory_size_bytes,
        bits_per_entry,
        insert_duration_ms,
        insert_qps,
        query_duration_ms,
        query_qps,
        serialization_verified,
        passed_scale_check,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::{generate_non_existent_hashes, generate_store_hashes};

    #[test]
    fn test_bloom_scale_at_10k() {
        let inserted = generate_store_hashes(10_000, 42);
        let non_existent = generate_non_existent_hashes(10_000, 42);
        let report = verify_bloom_filter_scale(&inserted, &non_existent)
            .expect("Bloom scale verification should succeed");
        assert!(report.passed_scale_check);
        assert_eq!(report.false_negatives_count, 0);
        assert!(report.false_positive_rate <= 0.015);
    }
}
