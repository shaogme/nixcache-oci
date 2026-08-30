pub mod bloom_stress;
pub mod concurrency;
pub mod distribution;
pub mod generator;
pub mod merkle;

use crate::{
    bloom_stress::{BloomFilterReport, verify_bloom_filter_scale},
    concurrency::{
        ConcurrencyReadReport, ConcurrencyWriteAndCompactionReport,
        simulate_concurrent_delta_and_compaction, simulate_concurrent_read_queries,
    },
    distribution::{
        DistributionReport, verify_entry_sharding_invariance, verify_sharding_distribution,
    },
    generator::{generate_index_entries, generate_non_existent_hashes},
    merkle::{
        DiffVerificationReport, verify_incremental_diff_accuracy, verify_merkle_determinism,
        verify_merkle_tamper_detection,
    },
};
use nixcache_core::{
    FastBlockedBloomFilter, ShardDataPayload, SystemArch, partition_entries_by_shard,
};
use scc::HashMap as SccHashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// 综合大规模分片压测与一致性检验全量报告
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FullScaleReport {
    pub scale_entries: usize,
    pub system: String,
    pub distribution: DistributionReport,
    pub merkle_determinism_passed: bool,
    pub merkle_tamper_detection_passed: bool,
    pub incremental_diff: DiffVerificationReport,
    pub bloom_filter: BloomFilterReport,
    pub concurrent_read: ConcurrencyReadReport,
    pub concurrent_write_compaction: ConcurrencyWriteAndCompactionReport,
    pub all_checks_passed: bool,
}

/// 执行指定条目规模下的完整分片一致性、Merkle Tree 状态与高并发压测仿真套件
pub async fn run_full_scale_simulation(
    entries_count: usize,
    concurrency: usize,
    system: SystemArch,
) -> Result<FullScaleReport, String> {
    info!(
        "=== 启动 Schema v5 分片索引海量规模 ({}) 自动化检验与高并发压测套件 ===",
        entries_count
    );

    // 1. 生成海量真实 Base32 StoreHash 与 IndexEntry 数据集
    info!(
        ">>> 步骤 1/5: 正在生成 {} 条大规模测试数据...",
        entries_count
    );
    let entries = generate_index_entries(entries_count, 1337, system);
    let hashes: Vec<_> = entries.keys().cloned().collect();
    let non_existent_hashes = generate_non_existent_hashes(entries_count.min(200_000), 1337);

    // 2. 分片离散均匀度与双向归属一致性检验
    info!(">>> 步骤 2/5: 执行 1024 分片离散均匀分布与双向归属一致性检验...");
    let distribution = verify_sharding_distribution(&hashes);
    if !distribution.passed_uniformity_check {
        return Err(format!(
            "Sharding uniformity check failed: CV = {:.4}, empty shards = {}",
            distribution.coefficient_of_variation, distribution.empty_shards_count
        ));
    }
    verify_entry_sharding_invariance(&entries)?;

    // 3. Merkle Tree 状态检验、乱序确定性、抗篡改与增量 Diff 校验
    info!(">>> 步骤 3/5: 执行 Merkle Tree 确定性、雪崩抗篡改与增量 Diff 状态检验...");
    verify_merkle_determinism(&entries)?;
    verify_merkle_tamper_detection(&entries, system)?;

    let incremental_count = (entries_count / 200).clamp(10, 500);
    let new_entries = generate_index_entries(incremental_count, 9999, system);
    let incremental_diff = verify_incremental_diff_accuracy(&entries, &new_entries, system)?;
    if !incremental_diff.diff_matches_exact {
        return Err("Incremental shard diff accuracy check failed".to_string());
    }

    // 4. Fast Blocked Bloom Filter 百万级容量、零假阴性与假阳性率校验
    info!(">>> 步骤 4/5: 执行 Fast Blocked Bloom Filter 百万级容量与假阳性率压测...");
    let bloom_filter_report = verify_bloom_filter_scale(&hashes, &non_existent_hashes)?;
    if !bloom_filter_report.passed_scale_check {
        return Err(format!(
            "Bloom filter check failed: FPR = {:.4}%, False Negatives = {}",
            bloom_filter_report.false_positive_rate * 100.0,
            bloom_filter_report.false_negatives_count
        ));
    }

    // 5. 高并发压测仿真 (只读 Bloom 0ms 直通与分片检索、多 Worker 并发 WAL 与局部压实)
    info!(
        ">>> 步骤 5/5: 执行 {} 并发 Worker 只读查询压测与 Partial Compaction 性能仿真...",
        concurrency
    );
    let bf_arc = Arc::new(FastBlockedBloomFilter::from_entries(&hashes));
    let sm_arc = Arc::new(SccHashMap::new());

    // 预热分片表
    let partitioned = partition_entries_by_shard(entries.clone());
    for (sid, s_entries) in partitioned {
        let payload = ShardDataPayload::with_entries(sid, s_entries);
        let _ = sm_arc.insert_sync(sid, Arc::new(payload));
    }

    let queries_per_worker = (entries_count / concurrency).clamp(1000, 20_000);
    let concurrent_read = simulate_concurrent_read_queries(
        bf_arc,
        sm_arc,
        &hashes,
        &non_existent_hashes,
        concurrency,
        queries_per_worker,
    )
    .await?;

    let concurrent_builders = (concurrency / 2).clamp(4, 32);
    let entries_per_builder = 5;
    let concurrent_write_compaction = simulate_concurrent_delta_and_compaction(
        entries,
        concurrent_builders,
        entries_per_builder,
        system,
    )
    .await?;

    let all_checks_passed = distribution.passed_uniformity_check
        && incremental_diff.diff_matches_exact
        && bloom_filter_report.passed_scale_check
        && concurrent_write_compaction.compaction_passed;

    info!(
        "=== 全量检验完成！状态: {}, 读吞吐: {:.0} QPS, 写放大降低: {:.1}% ===",
        if all_checks_passed {
            "通过 (PASSED)"
        } else {
            "失败 (FAILED)"
        },
        concurrent_read.throughput_qps,
        concurrent_write_compaction.write_amplification_reduction_pct
    );

    Ok(FullScaleReport {
        scale_entries: entries_count,
        system: system.to_string(),
        distribution,
        merkle_determinism_passed: true,
        merkle_tamper_detection_passed: true,
        incremental_diff,
        bloom_filter: bloom_filter_report,
        concurrent_read,
        concurrent_write_compaction,
        all_checks_passed,
    })
}
