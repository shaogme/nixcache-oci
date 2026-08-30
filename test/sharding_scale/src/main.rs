use clap::Parser;
use nixcache_core::SystemArch;
use sharding_scale_sim::{FullScaleReport, run_full_scale_simulation};
use std::process::ExitCode;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(
    name = "sharding-scale-sim",
    about = "Schema v5 Sharded Merkle-Radix Index Scale & Concurrency Stress Test Suite",
    version = "0.1.0"
)]
struct Args {
    /// 压测条目规模 (100,000 ~ 1,000,000+)
    #[arg(short = 'n', long, default_value_t = 100_000)]
    entries: usize,

    /// 并发 Worker 线程数
    #[arg(short = 'c', long, default_value_t = 64)]
    concurrency: usize,

    /// 目标系统架构 (如 x86_64-linux, aarch64-linux)
    #[arg(short = 's', long, default_value = "x86_64-linux")]
    system: String,

    /// 以结构化 JSON 格式输出全量指标
    #[arg(long, default_value_t = false)]
    json: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    if !args.json {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        fmt().with_env_filter(filter).init();
    }

    let system = SystemArch::from(args.system.as_str());
    let sys = if system.is_known() {
        system
    } else {
        SystemArch::X86_64Linux
    };

    match run_full_scale_simulation(args.entries, args.concurrency, sys).await {
        Ok(report) => {
            if args.json {
                let json_output = serde_json::to_string_pretty(&report).unwrap_or_default();
                println!("{}", json_output);
            } else {
                print_human_report(&report);
            }

            if report.all_checks_passed {
                ExitCode::SUCCESS
            } else {
                eprintln!("❌ One or more scale stress checks failed!");
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("❌ Scale simulation encountered critical error: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn print_human_report(r: &FullScaleReport) {
    println!();
    println!(
        "=========================================================================================="
    );
    println!(
        "        NixCache Schema v5 Sharded Index Scale & Concurrency Verification Report           "
    );
    println!(
        "=========================================================================================="
    );
    println!(
        " 条目规模 (Entries): {:<12} | 架构 (Arch): {:<15} | 状态: {}",
        r.scale_entries,
        r.system,
        if r.all_checks_passed {
            "✅ ALL CHECKS PASSED"
        } else {
            "❌ FAILED"
        }
    );
    println!(
        "------------------------------------------------------------------------------------------"
    );
    println!(" [1] 1024 分片离散均匀度统计检验 (Radix Sharding Distribution):");
    println!(
        "     - 理论均值 / 实际均值 : {:.2} / {:.2} 条目/分片",
        r.distribution.expected_mean, r.distribution.actual_mean
    );
    println!(
        "     - 最小桶 / 最大桶     : {} / {} 条目",
        r.distribution.min_shard_entries, r.distribution.max_shard_entries
    );
    println!(
        "     - 样本标准差 (σ)      : {:.4} (变异系数 CV: {:.4})",
        r.distribution.std_deviation, r.distribution.coefficient_of_variation
    );
    println!(
        "     - 空分片数 (Empty)    : {} / {} (判定: {})",
        r.distribution.empty_shards_count,
        r.distribution.num_shards,
        if r.distribution.passed_uniformity_check {
            "✅ 严密均匀"
        } else {
            "❌ 异常倾斜"
        }
    );
    println!(
        "------------------------------------------------------------------------------------------"
    );
    println!(" [2] Merkle Tree 状态检验与抗篡改 (Merkle Tree State Verification):");
    println!(
        "     - 分片乱序插入确定性   : {}",
        if r.merkle_determinism_passed {
            "✅ 100% 确定性"
        } else {
            "❌ 失败"
        }
    );
    println!(
        "     - 单条目突变雪崩检测   : {}",
        if r.merkle_tamper_detection_passed {
            "✅ 极速敏感拦截"
        } else {
            "❌ 失败"
        }
    );
    println!(
        "     - 增量 Diff 精准定位   : {} (变更分片: {} 个, 未变分片: {} 个)",
        if r.incremental_diff.diff_matches_exact {
            "✅ 100% 精确"
        } else {
            "❌ 偏差"
        },
        r.incremental_diff.affected_shards_detected.len(),
        r.incremental_diff.unchanged_shards_count
    );
    println!(
        "     - Merkle Root 变更     : {} -> {}",
        &r.incremental_diff.old_merkle_root[..16],
        &r.incremental_diff.new_merkle_root[..16]
    );
    println!(
        "------------------------------------------------------------------------------------------"
    );
    println!(" [3] Fast Blocked Bloom Filter 百万级容量与假阳性检验 (Bloom Filter Summary):");
    println!(
        "     - 插入条目 / 漏报统计 : {} / {} (零假阴性保证: {})",
        r.bloom_filter.total_inserted,
        r.bloom_filter.false_negatives_count,
        if r.bloom_filter.false_negatives_count == 0 {
            "✅ 100% 命中"
        } else {
            "❌ 存在假阴性"
        }
    );
    println!(
        "     - 随机未命中探测      : {} 次 (误判: {} 次, 实测 FPR: {:.3}%)",
        r.bloom_filter.tested_negative_probes,
        r.bloom_filter.false_positives_count,
        r.bloom_filter.false_positive_rate * 100.0
    );
    println!(
        "     - 内存开销 / 单条位数 : {:.2} KB ({:.2} bits/entry)",
        (r.bloom_filter.memory_size_bytes as f64) / 1024.0,
        r.bloom_filter.bits_per_entry
    );
    println!(
        "     - 序列化还原校验      : {}",
        if r.bloom_filter.serialization_verified {
            "✅ 字节级完全一致"
        } else {
            "❌ 校验失败"
        }
    );
    println!(
        "------------------------------------------------------------------------------------------"
    );
    println!(" [4] 高并发读仿真 (Concurrent Read Query Simulation):");
    println!(
        "     - 并发 Worker / 总查询: {} 线程 / {} 次查询",
        r.concurrent_read.concurrency_workers, r.concurrent_read.total_queries
    );
    println!(
        "     - 读吞吐量 (QPS)      : {:.0} QPS (总耗时: {:.2} ms)",
        r.concurrent_read.throughput_qps, r.concurrent_read.total_duration_ms
    );
    println!(
        "     - 0ms 旁路直通比例     : {:.1}% (未命中穿透)",
        r.concurrent_read.bloom_bypass_rate
    );
    println!(
        "     - 延迟分布 (Latency)  : P50: {:.2} µs | P90: {:.2} µs | P99: {:.2} µs (均值: {:.2} µs)",
        r.concurrent_read.p50_latency_ns / 1000.0,
        r.concurrent_read.p90_latency_ns / 1000.0,
        r.concurrent_read.p99_latency_ns / 1000.0,
        r.concurrent_read.avg_latency_ns / 1000.0
    );
    println!(
        "------------------------------------------------------------------------------------------"
    );
    println!(" [5] 高并发增量 WAL 与 Partial Compaction 局部压实 (Write Amplification):");
    println!(
        "     - 并发构建者 (Runners): {} 个 Matrix Workers (产出 {} 条目)",
        r.concurrent_write_compaction.concurrent_builders,
        r.concurrent_write_compaction.total_new_entries
    );
    println!(
        "     - Partial Compaction  : {:.2} ms (仅压实 {} 个分片)",
        r.concurrent_write_compaction.partial_compaction_duration_ms,
        r.concurrent_write_compaction.affected_shards_count
    );
    println!(
        "     - 全量压实对照组耗时  : {:.2} ms (加速比: {:.1}x)",
        r.concurrent_write_compaction.full_compaction_duration_ms,
        r.concurrent_write_compaction.speedup_ratio
    );
    println!(
        "     - 写放大降低比例      : ✅ {:.2}% 零开销复用 (未变分片: {}/1024)",
        r.concurrent_write_compaction
            .write_amplification_reduction_pct,
        r.concurrent_write_compaction.unchanged_shards_count
    );
    println!(
        "=========================================================================================="
    );
    println!();
}
