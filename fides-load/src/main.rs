use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use rand::prelude::*;
use rand::rngs::SmallRng;
use tonic::transport::Channel;
use tonic::Code;
use uuid::Uuid;

use fides_proto::ledger_service_client::LedgerServiceClient;
use fides_proto::{AuthorizeRequest, CaptureRequest, CreateAccountRequest, TransferLeg};

#[derive(Parser)]
#[command(name = "fides-load", about = "load tester for fides ledger")]
struct Args {
    /// gRPC target address
    #[arg(long, default_value = "http://localhost:50051")]
    target: String,

    /// number of accounts to create (half asset, half equity)
    #[arg(long, default_value = "100")]
    accounts: usize,

    /// number of concurrent worker tasks
    #[arg(long, default_value = "10")]
    concurrency: usize,

    /// test duration in seconds
    #[arg(long, default_value = "30")]
    duration: u64,

    /// amount per transaction (smallest unit)
    #[arg(long, default_value = "100")]
    amount: i64,
}

struct Stats {
    success: AtomicU64,
    rejected: AtomicU64,
    errors: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            success: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    eprintln!("connecting to {}...", args.target);
    let mut client = LedgerServiceClient::connect(args.target.clone()).await?;

    // setup: create N/2 asset + N/2 equity accounts
    let half = args.accounts / 2;
    if half == 0 {
        eprintln!("need at least 2 accounts");
        std::process::exit(1);
    }

    eprintln!(
        "creating {} accounts ({} asset + {} equity)...",
        args.accounts, half, half
    );
    let mut assets = Vec::with_capacity(half);
    let mut equities = Vec::with_capacity(half);

    for _ in 0..half {
        let resp = client
            .create_account(CreateAccountRequest {
                account_type: 1, // asset
                asset_code: "USD".into(),
                asset_scale: 2,
            })
            .await?
            .into_inner();
        assets.push(resp.account.unwrap().id);
    }

    for _ in 0..half {
        let resp = client
            .create_account(CreateAccountRequest {
                account_type: 3, // equity
                asset_code: "USD".into(),
                asset_scale: 2,
            })
            .await?
            .into_inner();
        equities.push(resp.account.unwrap().id);
    }

    // fund each asset account from a paired equity
    let fund_amount = 1_000_000i64; // large enough for many transactions
    eprintln!("funding {} asset accounts with {}...", half, fund_amount);
    for (i, &asset_id) in assets.iter().enumerate().take(half) {
        let equity_idx = i % equities.len();
        let resp = client
            .authorize(AuthorizeRequest {
                idempotency_key: Uuid::new_v4().to_string(),
                legs: vec![
                    TransferLeg {
                        account_id: asset_id,
                        entry_type: 1, // debit (increases asset)
                        amount: fund_amount,
                    },
                    TransferLeg {
                        account_id: equities[equity_idx],
                        entry_type: 2, // credit (increases equity)
                        amount: fund_amount,
                    },
                ],
                metadata: String::new(),
            })
            .await?
            .into_inner();

        let tx_id = resp.transaction.unwrap().id;
        client
            .capture(CaptureRequest {
                transaction_id: tx_id,
            })
            .await?;
    }

    eprintln!("setup complete. starting load test...\n");

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::new());
    let latencies = Arc::new(tokio::sync::Mutex::new(Vec::<f64>::new()));

    let duration = Duration::from_secs(args.duration);
    let start = Instant::now();

    // spawn workers
    let mut handles = Vec::new();
    for _ in 0..args.concurrency {
        let channel = Channel::from_shared(args.target.clone())
            .unwrap()
            .connect()
            .await?;
        let mut worker_client = LedgerServiceClient::new(channel);
        let assets = assets.clone();
        let equities = equities.clone();
        let stop = stop.clone();
        let stats = stats.clone();
        let latencies = latencies.clone();
        let amount = args.amount;

        handles.push(tokio::spawn(async move {
            let mut rng = SmallRng::from_entropy();

            while !stop.load(Ordering::Relaxed) {
                let asset = assets[rng.gen_range(0..assets.len())];
                let equity = equities[rng.gen_range(0..equities.len())];

                let op_start = Instant::now();

                // authorize: credit asset (withdrawal), debit equity
                let auth_result = worker_client
                    .authorize(AuthorizeRequest {
                        idempotency_key: Uuid::new_v4().to_string(),
                        legs: vec![
                            TransferLeg {
                                account_id: asset,
                                entry_type: 2, // credit (withdrawal from asset)
                                amount,
                            },
                            TransferLeg {
                                account_id: equity,
                                entry_type: 1, // debit (withdrawal from equity)
                                amount,
                            },
                        ],
                        metadata: String::new(),
                    })
                    .await;

                match auth_result {
                    Ok(resp) => {
                        let tx_id = resp.into_inner().transaction.unwrap().id;

                        // capture
                        match worker_client
                            .capture(CaptureRequest {
                                transaction_id: tx_id,
                            })
                            .await
                        {
                            Ok(_) => {
                                let elapsed = op_start.elapsed().as_secs_f64() * 1000.0;
                                stats.success.fetch_add(1, Ordering::Relaxed);
                                latencies.lock().await.push(elapsed);
                            }
                            Err(e) => {
                                classify_error(&e, &stats);
                            }
                        }
                    }
                    Err(e) => {
                        classify_error(&e, &stats);
                    }
                }
            }
        }));
    }

    // wait for duration
    tokio::time::sleep(duration).await;
    stop.store(true, Ordering::Relaxed);

    // wait for workers to finish
    for handle in handles {
        let _ = handle.await;
    }

    let elapsed = start.elapsed();
    let success = stats.success.load(Ordering::Relaxed);
    let rejected = stats.rejected.load(Ordering::Relaxed);
    let errors = stats.errors.load(Ordering::Relaxed);
    let total = success + rejected + errors;

    // compute percentiles
    let mut lats = latencies.lock().await;
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let p50 = percentile(&lats, 50.0);
    let p95 = percentile(&lats, 95.0);
    let p99 = percentile(&lats, 99.0);
    let max = lats.last().copied().unwrap_or(0.0);

    let throughput = if elapsed.as_secs_f64() > 0.0 {
        success as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    let rejected_pct = if total > 0 {
        rejected as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    let error_pct = if total > 0 {
        errors as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    println!("--- fides-load results ---");
    println!("target:       {}", args.target);
    println!("duration:     {:.1}s", elapsed.as_secs_f64());
    println!("concurrency:  {}", args.concurrency);
    println!();
    println!("operations:   {}", total);
    println!("throughput:   {:.0} ops/sec", throughput);
    println!("rejected:     {} ({:.1}%)", rejected, rejected_pct);
    println!("errors:       {} ({:.1}%)", errors, error_pct);
    println!();
    println!("latency (authorize+capture):");
    println!("  p50:   {:.1}ms", p50);
    println!("  p95:   {:.1}ms", p95);
    println!("  p99:   {:.1}ms", p99);
    println!("  max:   {:.1}ms", max);

    Ok(())
}

fn classify_error(e: &tonic::Status, stats: &Stats) {
    match e.code() {
        Code::FailedPrecondition | Code::Aborted | Code::AlreadyExists => {
            stats.rejected.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            stats.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (pct / 100.0 * sorted.len() as f64) as usize;
    let idx = idx.min(sorted.len() - 1);
    sorted[idx]
}
