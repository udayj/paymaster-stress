use clap::{command, Parser, Subcommand};
use paymaster_rpc::client::Client;
use starknet::core::types::{Call, Felt};
use starknet::signers::SigningKey;
use std::fs;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time::{interval, Instant};
mod types;
use crate::types::*;
use paymaster_rpc::{
    BuildTransactionRequest, BuildTransactionResponse, ExecutableInvokeParameters,
    ExecutableTransactionParameters, ExecuteRequest, ExecutionParameters, FeeMode,
    InvokeParameters, TransactionParameters,
};

#[derive(Parser)]
#[command(name = "paymaster-stress")]
#[command(about = "Stress testing tool for paymaster service")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // Test Sending Increasing TPS to Paymaster
    // Only 1 command type supported for now
    Linear {
        #[arg(long, default_value = "http://localhost:12777")]
        endpoint: String,

        #[arg(long)]
        max_tps: u32,

        #[arg(long, default_value = "5")]
        duration: u32,

        #[arg(long, default_value = "5")]
        steps: u32,

        #[arg(long)]
        output: Option<PathBuf>,
    },
}

type TestError = Box<dyn std::error::Error>;

#[derive(Debug)]
enum TransactionError {
    Nonce,
    Timeout,
    Relayer,
    JsonRpc,
    Other,
}

#[tokio::main]
async fn main() -> Result<(), TestError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Linear {
            endpoint,
            max_tps,
            duration,
            steps,
            output,
        } => {
            let client = Client::new(&endpoint);
            let duration = Duration::from_secs(duration as u64);
            // Check if paymaster service is available
            if !client.is_available().await? {
                eprintln!("Paymaster service not available at {}", endpoint);
                exit(1);
            }

            println!("Starting single account stress test:");
            println!("  Endpoint: {}", endpoint);
            println!("  Max TPS: {}", max_tps);
            println!("  Duration for Full Test: {:?}", duration);
            println!("  Steps: {}", steps);
            println!();

            let config = envy::from_env::<Config>().unwrap();
            let private_key = config.private_key;
            let results = linear_ramp_test(client, private_key, max_tps, duration, steps).await?;

            if let Some(output_path) = output {
                fs::write(&output_path, serde_json::to_string_pretty(&results)?)?;
                println!("Results saved to: {}", output_path.display());
            } else {
                println!("{}", serde_json::to_string_pretty(&results)?);
            }
        }
    }

    Ok(())
}

// We divide the test duration by number of steps into equally sized duration for each sample tps
// For each such sub duration, we send the desired tps
// tps ramps up from 1 to target max tps
// We send txs asynchronously and wait for the results
// For each result we update the metrics and errors
// Finally we compile summary statistics
async fn linear_ramp_test(
    client: Client,
    private_key: String,
    max_tps: u32,
    duration: Duration,
    steps: u32,
) -> Result<StressTestResults, TestError> {
    let client = Arc::new(client);
    let mut results = Vec::new();
    let test_start = Instant::now();

    // Test account (hardcoded for simplicity)
    let user_address =
        Felt::from_hex("0x059e0eaf58972c3b7de923ad6a280476430295f7ea967b768bd381bf5d90d50b")?;
    let private_key =
        Felt::from_hex(private_key.as_str())?;
    let signing_key = SigningKey::from_secret_scalar(private_key);

    // Simple STRK transfer call
    let strk_token =
        Felt::from_hex("0x04718f5a0fc34cc1af16a1cdee98ffb20c31f5cd61d6ab07201858f4287c938d")?;
    let transfer_call = Call {
        to: strk_token,
        selector: Felt::from_hex(
            "0x83afd3f4caedc6eebf44246fe54e38c95e3179a5ec9ea81740eca5b482d12e",
        )?, // transfer selector
        calldata: vec![
            Felt::from_hex("0x03f27a34e5e5483bf91257a3232ba753cc94e5b4ca19f8e200e8387e4a2ce555")?, // to
            Felt::ONE,    // amount (low)
            Felt::ZERO,   // amount (high)
        ],
    };

    let step_duration = duration / steps;

    for step in 1..=steps {
        // Gradually increase tps on each run
        let target_tps = (max_tps * step) / steps;
        if target_tps == 0 {
            continue;
        }

        println!("Testing TPS: {}", target_tps);

        let mut task_set = JoinSet::new();
        // Start interval timer
        let mut ticker = interval(Duration::from_millis(1000 / target_tps as u64));
        let step_start = Instant::now();

        // Send transactions at target TPS for step_duration amount of time
        while step_start.elapsed() < step_duration {
            ticker.tick().await;

            let task_client = Arc::clone(&client);
            let task_call = transfer_call.clone();
            let task_key = signing_key.clone();
            task_set.spawn(async move {
                send_single_transaction(task_client, user_address, task_call, task_key, strk_token)
                    .await
            });
        }

        // Wait for all in-flight tasks to complete
        let mut metrics = Metrics::default();
        let mut errors = ErrorBreakdown::default();
        let mut latencies = Vec::new();

        while let Some(result) = task_set.join_next().await {
            match result? {
                Ok(latency) => {
                    metrics.successful_txs += 1;
                    latencies.push(latency);
                }
                Err(error_type) => {
                    metrics.failed_txs += 1;
                    match error_type {
                        TransactionError::Nonce => errors.nonce_conflicts += 1,
                        TransactionError::Timeout => errors.timeouts += 1,
                        TransactionError::Relayer => errors.relayer_exhaustion += 1,
                        TransactionError::JsonRpc => errors.json_rpc_errors += 1,
                        TransactionError::Other => errors.other += 1,
                    }
                }
            }
        }

        metrics.total_txs = metrics.successful_txs + metrics.failed_txs;
        metrics.avg_latency_ms = if !latencies.is_empty() {
            latencies.iter().sum::<f64>() / latencies.len() as f64
        } else {
            0.0
        };
        metrics.success_rate = if metrics.total_txs > 0 {
            metrics.successful_txs as f64 / metrics.total_txs as f64
        } else {
            0.0
        };
        results.push(TestResult {
            metrics,
            error_breakdown: errors,
        });
    }

    let total_successful: u32 = results.iter().map(|r| r.metrics.successful_txs).sum();
    let overall_success_rate =
        results.iter().map(|r| r.metrics.success_rate).sum::<f64>() / results.len() as f64;

    // We define sustainable tps as that at which tx success rate is more than 95%
    let max_sustainable_tps = results
        .iter()
        .filter(|r| r.metrics.success_rate > 0.95)
        .map(|r| r.metrics.target_tps)
        .max()
        .unwrap_or(0);

    Ok(StressTestResults {
        total_duration_secs: test_start.elapsed().as_secs(),
        results,
        summary: TestSummary {
            max_sustainable_tps,
            total_transactions: total_successful,
            overall_success_rate,
        },
    })
}

async fn send_single_transaction(
    client: Arc<Client>,
    user_address: Felt,
    transfer_call: Call,
    signing_key: SigningKey,
    eth_token: Felt,
) -> Result<f64, TransactionError> {
    let tx_start = Instant::now();

    // Build transaction
    let build_request = BuildTransactionRequest {
        transaction: TransactionParameters::Invoke {
            invoke: InvokeParameters {
                user_address,
                calls: vec![transfer_call],
            },
        },
        parameters: ExecutionParameters::V1 {
            fee_mode: FeeMode::Default {
                gas_token: eth_token,
            },
            time_bounds: None,
        },
    };

    let invoke_tx = match client.build_transaction(build_request).await {
        Ok(BuildTransactionResponse::Invoke(tx)) => tx,
        Err(_) => return Err(TransactionError::Other),
        _ => panic!("should not get this tx type"),
    };

    // Sign the transaction
    let message_hash = invoke_tx
        .typed_data
        .message_hash(user_address)
        .map_err(|_| TransactionError::Other)?;

    let signature = signing_key
        .sign(&message_hash)
        .map_err(|_| TransactionError::Other)?;

    // Execute transaction
    let execute_request = ExecuteRequest {
        transaction: ExecutableTransactionParameters::Invoke {
            invoke: ExecutableInvokeParameters {
                user_address,
                typed_data: invoke_tx.typed_data,
                signature: vec![signature.r, signature.s],
            },
        },
        parameters: ExecutionParameters::V1 {
            fee_mode: FeeMode::Default {
                gas_token: eth_token,
            },
            time_bounds: None,
        },
    };

    match client.execute_transaction(execute_request).await {
        Ok(_) => Ok(tx_start.elapsed().as_millis() as f64),
        Err(e) => {
            let error_str = e.to_string();
            if error_str.contains("nonce") {
                Err(TransactionError::Nonce)
            } else if error_str.contains("timeout") {
                Err(TransactionError::Timeout)
            } else if error_str.contains("relayer") || error_str.contains("unavailable") {
                Err(TransactionError::Relayer)
            } else if error_str.contains("JSON-RPC error") {
                Err(TransactionError::JsonRpc)
            } else {
                Err(TransactionError::Other)
            }
        }
    }
}
