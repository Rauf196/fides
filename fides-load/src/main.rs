use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use rand::prelude::*;
use rand::rngs::SmallRng;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;
use uuid::Uuid;

use fides_proto::ledger_service_client::LedgerServiceClient;
use fides_proto::{
    AuthorizeRequest, CaptureRequest, CreateAccountRequest, GetAccountRequest, GetBalanceRequest,
    GetEntriesRequest, TransferLeg, VoidRequest,
};

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
    captures: AtomicU64,
    voids: AtomicU64,
    balance_checks: AtomicU64,
    account_lookups: AtomicU64,
    entry_streams: AtomicU64,
    insufficient_funds: AtomicU64,
    idempotent_hits: AtomicU64,
    rejected: AtomicU64,
    errors: AtomicU64,
}

impl Stats {
    fn new() -> Self {
        Self {
            captures: AtomicU64::new(0),
            voids: AtomicU64::new(0),
            balance_checks: AtomicU64::new(0),
            account_lookups: AtomicU64::new(0),
            entry_streams: AtomicU64::new(0),
            insufficient_funds: AtomicU64::new(0),
            idempotent_hits: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    fn total_ops(&self) -> u64 {
        self.captures.load(Ordering::Relaxed)
            + self.voids.load(Ordering::Relaxed)
            + self.balance_checks.load(Ordering::Relaxed)
            + self.account_lookups.load(Ordering::Relaxed)
            + self.entry_streams.load(Ordering::Relaxed)
            + self.insufficient_funds.load(Ordering::Relaxed)
            + self.idempotent_hits.load(Ordering::Relaxed)
            + self.rejected.load(Ordering::Relaxed)
            + self.errors.load(Ordering::Relaxed)
    }
}

// weighted operation selection (out of 100)
const OP_CAPTURE: u8 = 40;
const OP_VOID: u8 = 55;
const OP_INSUF_FUNDS: u8 = 65;
const OP_IDEMPOTENT: u8 = 70;
const OP_GET_BALANCE: u8 = 85;
const OP_GET_ACCOUNT: u8 = 95;
// 95..100 = get_entries

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

    // create one unfunded asset+equity pair for insufficient funds tests
    let broke_asset = client
        .create_account(CreateAccountRequest {
            account_type: 1,
            asset_code: "USD".into(),
            asset_scale: 2,
        })
        .await?
        .into_inner()
        .account
        .unwrap()
        .id;
    let broke_equity = client
        .create_account(CreateAccountRequest {
            account_type: 3,
            asset_code: "USD".into(),
            asset_scale: 2,
        })
        .await?
        .into_inner()
        .account
        .unwrap()
        .id;

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
            let mut last_idempotency_key: Option<String> = None;
            let mut last_captured_tx: Option<i64> = None;

            while !stop.load(Ordering::Relaxed) {
                let roll: u8 = rng.gen_range(0..100);
                let asset = assets[rng.gen_range(0..assets.len())];
                let equity = equities[rng.gen_range(0..equities.len())];

                if roll < OP_CAPTURE {
                    // authorize + capture (~40%)
                    let op_start = Instant::now();
                    let key = Uuid::new_v4().to_string();
                    let auth = worker_client
                        .authorize(AuthorizeRequest {
                            idempotency_key: key.clone(),
                            legs: vec![
                                TransferLeg {
                                    account_id: asset,
                                    entry_type: 2,
                                    amount,
                                },
                                TransferLeg {
                                    account_id: equity,
                                    entry_type: 1,
                                    amount,
                                },
                            ],
                            metadata: String::new(),
                        })
                        .await;

                    match auth {
                        Ok(resp) => {
                            let tx_id = resp.into_inner().transaction.unwrap().id;
                            match worker_client
                                .capture(CaptureRequest {
                                    transaction_id: tx_id,
                                })
                                .await
                            {
                                Ok(_) => {
                                    let elapsed = op_start.elapsed().as_secs_f64() * 1000.0;
                                    stats.captures.fetch_add(1, Ordering::Relaxed);
                                    latencies.lock().await.push(elapsed);
                                    last_idempotency_key = Some(key);
                                    last_captured_tx = Some(tx_id);
                                }
                                Err(e) => classify_error(&e, &stats),
                            }
                        }
                        Err(e) => classify_error(&e, &stats),
                    }
                } else if roll < OP_VOID {
                    // authorize + void (~15%)
                    let op_start = Instant::now();
                    let auth = worker_client
                        .authorize(AuthorizeRequest {
                            idempotency_key: Uuid::new_v4().to_string(),
                            legs: vec![
                                TransferLeg {
                                    account_id: asset,
                                    entry_type: 2,
                                    amount,
                                },
                                TransferLeg {
                                    account_id: equity,
                                    entry_type: 1,
                                    amount,
                                },
                            ],
                            metadata: String::new(),
                        })
                        .await;

                    match auth {
                        Ok(resp) => {
                            let tx_id = resp.into_inner().transaction.unwrap().id;
                            match worker_client
                                .void(VoidRequest {
                                    transaction_id: tx_id,
                                })
                                .await
                            {
                                Ok(_) => {
                                    let elapsed = op_start.elapsed().as_secs_f64() * 1000.0;
                                    stats.voids.fetch_add(1, Ordering::Relaxed);
                                    latencies.lock().await.push(elapsed);
                                }
                                Err(e) => classify_error(&e, &stats),
                            }
                        }
                        Err(e) => classify_error(&e, &stats),
                    }
                } else if roll < OP_INSUF_FUNDS {
                    // insufficient funds (~10%) — withdraw from unfunded account
                    let _ = worker_client
                        .authorize(AuthorizeRequest {
                            idempotency_key: Uuid::new_v4().to_string(),
                            legs: vec![
                                TransferLeg {
                                    account_id: broke_asset,
                                    entry_type: 2, // credit (withdrawal)
                                    amount: 999_999,
                                },
                                TransferLeg {
                                    account_id: broke_equity,
                                    entry_type: 1,
                                    amount: 999_999,
                                },
                            ],
                            metadata: String::new(),
                        })
                        .await;
                    stats.insufficient_funds.fetch_add(1, Ordering::Relaxed);
                } else if roll < OP_IDEMPOTENT {
                    // idempotent retry (~5%) — reuse a previous key or re-capture
                    if let Some(ref key) = last_idempotency_key {
                        let _ = worker_client
                            .authorize(AuthorizeRequest {
                                idempotency_key: key.clone(),
                                legs: vec![
                                    TransferLeg {
                                        account_id: asset,
                                        entry_type: 2,
                                        amount,
                                    },
                                    TransferLeg {
                                        account_id: equity,
                                        entry_type: 1,
                                        amount,
                                    },
                                ],
                                metadata: String::new(),
                            })
                            .await;
                        stats.idempotent_hits.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(tx_id) = last_captured_tx {
                        let _ = worker_client
                            .capture(CaptureRequest {
                                transaction_id: tx_id,
                            })
                            .await;
                        stats.idempotent_hits.fetch_add(1, Ordering::Relaxed);
                    }
                } else if roll < OP_GET_BALANCE {
                    // get_balance (~15%)
                    let _ = worker_client
                        .get_balance(GetBalanceRequest { account_id: asset })
                        .await;
                    stats.balance_checks.fetch_add(1, Ordering::Relaxed);
                } else if roll < OP_GET_ACCOUNT {
                    // get_account (~10%)
                    let _ = worker_client
                        .get_account(GetAccountRequest { account_id: asset })
                        .await;
                    stats.account_lookups.fetch_add(1, Ordering::Relaxed);
                } else {
                    // get_entries (~5%)
                    if let Ok(resp) = worker_client
                        .get_entries(GetEntriesRequest { account_id: asset })
                        .await
                    {
                        let mut stream = resp.into_inner();
                        while let Some(_entry) = stream.next().await {}
                    }
                    stats.entry_streams.fetch_add(1, Ordering::Relaxed);
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
    let captures = stats.captures.load(Ordering::Relaxed);
    let voids = stats.voids.load(Ordering::Relaxed);
    let balance_checks = stats.balance_checks.load(Ordering::Relaxed);
    let account_lookups = stats.account_lookups.load(Ordering::Relaxed);
    let entry_streams = stats.entry_streams.load(Ordering::Relaxed);
    let insufficient_funds = stats.insufficient_funds.load(Ordering::Relaxed);
    let idempotent_hits = stats.idempotent_hits.load(Ordering::Relaxed);
    let rejected = stats.rejected.load(Ordering::Relaxed);
    let errors = stats.errors.load(Ordering::Relaxed);
    let total = stats.total_ops();

    // compute percentiles (authorize+capture/void latencies)
    let mut lats = latencies.lock().await;
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let p50 = percentile(&lats, 50.0);
    let p95 = percentile(&lats, 95.0);
    let p99 = percentile(&lats, 99.0);
    let max = lats.last().copied().unwrap_or(0.0);

    let throughput = if elapsed.as_secs_f64() > 0.0 {
        total as f64 / elapsed.as_secs_f64()
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
    println!();
    println!("breakdown:");
    println!("  captures:          {}", captures);
    println!("  voids:             {}", voids);
    println!("  balance checks:    {}", balance_checks);
    println!("  account lookups:   {}", account_lookups);
    println!("  entry streams:     {}", entry_streams);
    println!("  insuf. funds:      {}", insufficient_funds);
    println!("  idempotent hits:   {}", idempotent_hits);
    println!("  rejected (other):  {}", rejected);
    println!("  errors:            {}", errors);
    println!();
    println!("latency (authorize+capture/void):");
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
