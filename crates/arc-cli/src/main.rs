//! ARC Chain CLI - key management, queries, and transactions.
//!
//! Usage:
//!   arc keygen --scheme ed25519 --output my-key.json
//!   arc balance <address>
//!   arc transfer --from key.json --to <address> --amount 1000
//!   arc info
//!   arc block <height>
//!   arc tx <hash>
//!   arc faucet <address>

mod commands;
mod keygen;
mod rpc;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arc", version, about = "ARC Chain CLI")]
struct Cli {
    /// RPC node URL (or set ARC_RPC_URL env var)
    #[arg(long, global = true)]
    rpc: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate a new keypair and save to file
    Keygen {
        /// Signature scheme (ed25519, secp256k1, ml-dsa-65, falcon-512)
        #[arg(long, default_value = "ed25519")]
        scheme: String,
        /// Output keyfile path
        #[arg(long, default_value = "arc-key.json")]
        output: String,
        /// Migration only: read one protected legacy seed file and preserve its Ed25519 address
        #[arg(long, value_name = "PATH")]
        legacy_seed_file: Option<String>,
        /// Validate one private keyfile and print only its public ARC address
        #[arg(long, value_name = "PATH", conflicts_with = "legacy_seed_file")]
        verify_keyfile: Option<String>,
    },
    /// Query account balance
    Balance {
        /// Account address (64-char hex)
        address: String,
    },
    /// Send a transfer transaction
    Transfer {
        /// Path to sender's keyfile
        #[arg(long)]
        from: String,
        /// Recipient address (64-char hex)
        #[arg(long)]
        to: String,
        /// Amount in ARC
        #[arg(long)]
        amount: u64,
    },
    /// Query chain info
    Info,
    /// Validate node health JSON and print `ok` or `degraded`
    Health,
    /// Get block details
    Block {
        /// Block height
        height: u64,
    },
    /// Get transaction details
    Tx {
        /// Transaction hash (64-char hex)
        hash: String,
    },
    /// Request testnet tokens from faucet
    Faucet {
        /// Recipient address
        address: String,
        /// Faucet URL (or set ARC_FAUCET_URL env var)
        #[arg(long)]
        faucet_url: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let rpc_url = cli
        .rpc
        .or_else(|| std::env::var("ARC_RPC_URL").ok())
        .unwrap_or_else(|| "http://localhost:9944".to_string());
    let rpc_client = rpc::RpcClient::new(&rpc_url);

    let result = match cli.command {
        Commands::Keygen {
            scheme,
            output,
            legacy_seed_file,
            verify_keyfile,
        } => match (legacy_seed_file, verify_keyfile) {
            (Some(input), None) if scheme == "ed25519" => {
                keygen::run_legacy_seed_file(&input, &output)
            }
            (Some(_), None) => Err(anyhow::anyhow!(
                "--legacy-seed-file is migration-only and requires --scheme ed25519"
            )),
            (None, Some(path)) => keygen::run_verify_keyfile(&path),
            (None, None) => keygen::run(&scheme, &output),
            (Some(_), Some(_)) => unreachable!("clap rejects conflicting keygen modes"),
        },
        Commands::Balance { address } => commands::balance::run(&rpc_client, &address).await,
        Commands::Transfer { from, to, amount } => {
            commands::transfer::run(&rpc_client, &from, &to, amount).await
        }
        Commands::Info => commands::info::run(&rpc_client).await,
        Commands::Health => commands::health::run(&rpc_client).await,
        Commands::Block { height } => commands::block::run(&rpc_client, height).await,
        Commands::Tx { hash } => commands::tx::run(&rpc_client, &hash).await,
        Commands::Faucet {
            address,
            faucet_url,
        } => {
            let url = faucet_url
                .or_else(|| std::env::var("ARC_FAUCET_URL").ok())
                .unwrap_or_else(|| format!("{}/faucet", rpc_url));
            commands::faucet::run(&rpc_client, &address, &url).await
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
