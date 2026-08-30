use nixcache_core::{
    DeltaPatchData, FastBlockedBloomFilter, IndexEntry, NUM_SHARDS, ShardDataPayload, StoreHash,
    SystemArch, calculate_shard_id, compute_shard_merkle_hash, partition_entries_by_shard,
};
use scc::HashMap as SccHashMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::task::JoinSet;

/// 高并发只读查询压测仿真报告
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ConcurrencyReadReport {
    pub total_queries: usize,
    pub concurrency_workers: usize,
    pub total_duration_ms: f64,
    pub throughput_qps: f64,
    pub negative_queries: usize,
    pub positive_queries: usize,
    pub bloom_bypass_rate: f64,
    pub avg_latency_ns: f64,
    pub p50_latency_ns: f64,
    pub p90_latency_ns: f64,
    pub p99_latency_ns: f64,
}

/// 高并发 Delta Patch WAL 并发写入与 Partial Compaction 仿真报告
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ConcurrencyWriteAndCompactionReport {
    pub total_base_entries: usize,
    pub concurrent_builders: usize,
    pub total_new_entries: usize,
    pub wal_generation_duration_ms: f64,
    pub partial_compaction_duration_ms: f64,
    pub full_compaction_duration_ms: f64,
    pub speedup_ratio: f64,
    pub affected_shards_count: usize,
    pub unchanged_shards_count: usize,
    pub write_amplification_reduction_pct: f64,
    pub compaction_passed: bool,
}

/// 仿真大规模多线程并发查询 (包括 Bloom Filter 0ms 旁路直通与分片查找)
pub async fn simulate_concurrent_read_queries(
    bloom_filter: Arc<FastBlockedBloomFilter>,
    shards_map: Arc<SccHashMap<u16, Arc<ShardDataPayload>>>,
    positive_hashes: &[StoreHash],
    negative_hashes: &[StoreHash],
    concurrency_workers: usize,
    queries_per_worker: usize,
) -> Result<ConcurrencyReadReport, String> {
    let positive_arc = Arc::new(positive_hashes.to_vec());
    let negative_arc = Arc::new(negative_hashes.to_vec());

    let total_queries = concurrency_workers * queries_per_worker;
    let counter_negative = Arc::new(AtomicUsize::new(0));
    let counter_positive = Arc::new(AtomicUsize::new(0));

    let start_time = Instant::now();
    let mut join_set = JoinSet::new();

    for worker_id in 0..concurrency_workers {
        let bf = Arc::clone(&bloom_filter);
        let sm = Arc::clone(&shards_map);
        let pos = Arc::clone(&positive_arc);
        let neg = Arc::clone(&negative_arc);
        let c_neg = Arc::clone(&counter_negative);
        let c_pos = Arc::clone(&counter_positive);

        join_set.spawn(async move {
            let mut latencies_ns = Vec::with_capacity(queries_per_worker);
            let pos_len = pos.len();
            let neg_len = neg.len();

            for i in 0..queries_per_worker {
                let is_negative = (i % 5) != 0; // 80% negative, 20% positive
                let t0 = Instant::now();

                if is_negative && neg_len > 0 {
                    let hash = &neg[(worker_id * 17 + i) % neg_len];
                    // Step 1: Bloom Filter 判定
                    if !bf.contains(hash) {
                        // 0ms 旁路直通 (命中布隆否定判断)
                        c_neg.fetch_add(1, Ordering::Relaxed);
                    } else {
                        // 罕见假阳性，定位分片进一步查找
                        let sid = calculate_shard_id(hash);
                        if let Some(payload) = sm.get_sync(&sid) {
                            let _ = payload.entries.get(hash);
                        }
                        c_neg.fetch_add(1, Ordering::Relaxed);
                    }
                } else if pos_len > 0 {
                    let hash = &pos[(worker_id * 17 + i) % pos_len];
                    // Step 1: Bloom Filter 判定
                    if bf.contains(hash) {
                        // Step 2: 定位分片并在分片内 O(1) 查找
                        let sid = calculate_shard_id(hash);
                        if let Some(payload) = sm.get_sync(&sid) {
                            let _ = payload.entries.get(hash);
                        }
                    }
                    c_pos.fetch_add(1, Ordering::Relaxed);
                }

                let elapsed_ns = t0.elapsed().as_nanos() as u64;
                latencies_ns.push(elapsed_ns);
            }

            latencies_ns
        });
    }

    let mut all_latencies = Vec::with_capacity(total_queries);
    while let Some(res) = join_set.join_next().await {
        let latencies = res.map_err(|e| format!("Worker task failed: {}", e))?;
        all_latencies.extend(latencies);
    }

    let total_duration = start_time.elapsed();
    let total_duration_ms = total_duration.as_secs_f64() * 1000.0;
    let throughput_qps = if total_duration.as_secs_f64() > 0.0 {
        (total_queries as f64) / total_duration.as_secs_f64()
    } else {
        0.0
    };

    all_latencies.sort_unstable();
    let len = all_latencies.len();
    let avg_latency_ns = if len > 0 {
        (all_latencies.iter().sum::<u64>() as f64) / (len as f64)
    } else {
        0.0
    };
    let p50_latency_ns = if len > 0 {
        all_latencies[len * 50 / 100] as f64
    } else {
        0.0
    };
    let p90_latency_ns = if len > 0 {
        all_latencies[len * 90 / 100] as f64
    } else {
        0.0
    };
    let p99_latency_ns = if len > 0 {
        all_latencies[len * 99 / 100] as f64
    } else {
        0.0
    };

    let negative_queries = counter_negative.load(Ordering::Relaxed);
    let positive_queries = counter_positive.load(Ordering::Relaxed);
    let bloom_bypass_rate = if total_queries > 0 {
        (negative_queries as f64) / (total_queries as f64) * 100.0
    } else {
        0.0
    };

    Ok(ConcurrencyReadReport {
        total_queries,
        concurrency_workers,
        total_duration_ms,
        throughput_qps,
        negative_queries,
        positive_queries,
        bloom_bypass_rate,
        avg_latency_ns,
        p50_latency_ns,
        p90_latency_ns,
        p99_latency_ns,
    })
}

/// 仿真多构建节点并发产出 Delta Patch WAL，并在大规模基线上进行 Partial Compaction
pub async fn simulate_concurrent_delta_and_compaction(
    base_entries: HashMap<StoreHash, IndexEntry>,
    concurrent_builders: usize,
    entries_per_builder: usize,
    system: SystemArch,
) -> Result<ConcurrencyWriteAndCompactionReport, String> {
    let total_base_entries = base_entries.len();
    let total_new_entries = concurrent_builders * entries_per_builder;

    // 1. 并发模拟 Matrix CI Runners 生成 Delta Patch
    let wal_start = Instant::now();
    let mut join_set = JoinSet::new();

    for builder_id in 0..concurrent_builders {
        join_set.spawn(async move {
            use crate::generator::generate_index_entries;
            let new_sub_entries =
                generate_index_entries(entries_per_builder, (builder_id as u64 + 1) * 9999, system);
            let active_roots: Vec<StoreHash> = new_sub_entries.keys().cloned().collect();
            DeltaPatchData::with_entries_and_roots(
                1001,
                format!("job-runner-{}", builder_id),
                system,
                new_sub_entries,
                active_roots,
            )
        });
    }

    let mut delta_patches = Vec::with_capacity(concurrent_builders);
    while let Some(res) = join_set.join_next().await {
        let delta = res.map_err(|e| format!("Builder delta generation failed: {}", e))?;
        delta_patches.push(delta);
    }
    let wal_generation_duration_ms = wal_start.elapsed().as_secs_f64() * 1000.0;

    // 2. 汇聚所有 Delta Patches 中的新增条目
    let mut all_incoming_entries: HashMap<StoreHash, IndexEntry> = HashMap::new();
    for delta in delta_patches {
        all_incoming_entries.extend(delta.new_entries);
    }

    let base_partitioned = partition_entries_by_shard(base_entries);
    let mut incoming_partitioned = partition_entries_by_shard(all_incoming_entries);

    let affected_shards: Vec<u16> = incoming_partitioned.keys().cloned().collect();
    let affected_shards_count = affected_shards.len();
    let unchanged_shards_count = NUM_SHARDS - affected_shards_count;

    // 3. 执行 Partial Shard Compaction (仅重新哈希并压实变动的分片)
    let partial_start = Instant::now();
    let mut partial_compacted_hashes: HashMap<u16, String> = HashMap::new();
    for (&shard_id, incoming_shard) in &incoming_partitioned {
        let mut shard_entries = base_partitioned.get(&shard_id).cloned().unwrap_or_default();
        shard_entries.extend(incoming_shard.clone());
        let hash = compute_shard_merkle_hash(&shard_entries);
        partial_compacted_hashes.insert(shard_id, hash);
    }
    let partial_compaction_duration_ms = partial_start.elapsed().as_secs_f64() * 1000.0;

    // 4. 对照组：执行 Full Compaction (无分片单体时代需全量 1024 个分片重新遍历压实)
    let full_start = Instant::now();
    for shard_id in 0..NUM_SHARDS as u16 {
        let mut shard_entries = base_partitioned.get(&shard_id).cloned().unwrap_or_default();
        if let Some(incoming_shard) = incoming_partitioned.remove(&shard_id) {
            shard_entries.extend(incoming_shard);
        }
        let _ = compute_shard_merkle_hash(&shard_entries);
    }
    let full_compaction_duration_ms = full_start.elapsed().as_secs_f64() * 1000.0;

    let speedup_ratio = if partial_compaction_duration_ms > 0.0 {
        full_compaction_duration_ms / partial_compaction_duration_ms
    } else {
        1.0
    };

    let write_amplification_reduction_pct =
        (unchanged_shards_count as f64) / (NUM_SHARDS as f64) * 100.0;

    Ok(ConcurrencyWriteAndCompactionReport {
        total_base_entries,
        concurrent_builders,
        total_new_entries,
        wal_generation_duration_ms,
        partial_compaction_duration_ms,
        full_compaction_duration_ms,
        speedup_ratio,
        affected_shards_count,
        unchanged_shards_count,
        write_amplification_reduction_pct,
        compaction_passed: affected_shards_count > 0 && write_amplification_reduction_pct > 50.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::{
        generate_index_entries, generate_non_existent_hashes, generate_store_hashes,
    };

    #[tokio::test]
    async fn test_concurrency_read_simulation_flow() {
        let pos_hashes = generate_store_hashes(5000, 111);
        let neg_hashes = generate_non_existent_hashes(5000, 111);

        let bf = Arc::new(FastBlockedBloomFilter::from_entries(&pos_hashes));
        let sm = Arc::new(SccHashMap::new());

        let report = simulate_concurrent_read_queries(bf, sm, &pos_hashes, &neg_hashes, 8, 500)
            .await
            .expect("Read simulation should pass");
        assert_eq!(report.total_queries, 4000);
        assert!(report.throughput_qps > 100_000.0);
    }

    #[tokio::test]
    async fn test_concurrency_delta_and_compaction_flow() {
        let base = generate_index_entries(2000, 333, SystemArch::X86_64Linux);
        let report = simulate_concurrent_delta_and_compaction(base, 4, 10, SystemArch::X86_64Linux)
            .await
            .expect("Compaction simulation should pass");
        assert!(report.compaction_passed);
        assert!(report.write_amplification_reduction_pct >= 90.0);
    }
}
