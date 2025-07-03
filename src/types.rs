use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug)]
pub struct Config {
    pub private_key: String,
}

#[derive(Serialize, Default)]
pub struct Metrics {
    pub successful_txs: u32,
    pub failed_txs: u32,
    pub total_txs: u32,
    pub target_tps: u32,
    pub success_rate: f64,
    pub avg_latency_ms: f64,
}
#[derive(Serialize)]
pub struct TestResult {
    pub metrics: Metrics,
    pub error_breakdown: ErrorBreakdown,
}

#[derive(Serialize, Default)]
pub struct ErrorBreakdown {
    pub nonce_conflicts: u32,
    pub timeouts: u32,
    pub relayer_exhaustion: u32,
    pub json_rpc_errors: u32,
    pub other: u32,
}

#[derive(Serialize)]
pub struct StressTestResults {
    pub total_duration_secs: u64,
    pub results: Vec<TestResult>,
    pub summary: TestSummary,
}

#[derive(Serialize)]
pub struct TestSummary {
    pub max_sustainable_tps: u32,
    pub total_transactions: u32,
    pub overall_success_rate: f64,
}
