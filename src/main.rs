mod chain;
mod checkout;
mod config;
mod identity;
mod ledger;
mod lightning;
mod merchant;
mod paykit;
mod store;
mod webhooks;
mod worker;

use anyhow::{Context, Result};
use bitcoin::Network;
use clap::{Parser, Subcommand};
use std::{fs, path::PathBuf};
use store::Store;

#[derive(Parser)]
#[command(version, about = "Self-hosted Bitcoin merchant checkout")]
struct Cli {
    /// SQLite database dedicated to this merchant account and Bitcoin network.
    #[arg(long, global = true, default_value = "checkout.sqlite3")]
    database: PathBuf,
    /// Bitcoin network: mainnet or regtest.
    #[arg(long,global=true,default_value="regtest",value_parser=config::parse_network)]
    network: Network,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Issue an order or recover its existing checkout path on retry.
    Invoice {
        #[arg(long)]
        order_id: String,
        #[arg(long)]
        amount_sats: u64,
        /// Dedicated BIP84 public account key. An xpub on mainnet or tpub on regtest.
        #[arg(long)]
        account_xpub: String,
        /// Optional Pubky identity of the buyer receiving a private payment request.
        #[arg(long)]
        reader: Option<String>,
    },
    /// Serve checkout, authenticated store API, and merchant dashboard on loopback.
    Serve {
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
    /// Observe payments and deliver signed webhook notifications.
    Worker {
        #[arg(long, default_value_t = 2)]
        min_confirmations: u32,
        /// Complete one reconciliation cycle and exit.
        #[arg(long)]
        once: bool,
    },
    /// Inspect an order by its public checkout ID.
    Status { id: String },
    /// Sign on-chain terms on the merchant's independent signing machine.
    Sign {
        /// JSON terms with invoice_id, network, amount_sats, address, expires_at.
        #[arg(long)]
        terms: PathBuf,
        #[arg(long)]
        account_xpub: String,
        #[arg(long)]
        index: u32,
        #[arg(long)]
        identity_key_file: PathBuf,
    },
    /// Check a BOLT11 invoice against a trusted merchant node, amount, network and current time.
    VerifyLightning {
        #[arg(long)]
        invoice: String,
        #[arg(long)]
        node_pubkey: String,
        #[arg(long)]
        amount_sats: u64,
    },
    /// Verify on-chain instructions against an independently pinned merchant key.
    Verify {
        #[arg(long)]
        proof: PathBuf,
        #[arg(long)]
        merchant: String,
        #[arg(long)]
        invoice_id: String,
        #[arg(long)]
        amount_sats: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Invoice {
            order_id,
            amount_sats,
            account_xpub,
            reader,
        } => {
            let reader = reader.as_deref().map(paykit::Reader::parse).transpose()?;
            let inv = Store::open(&cli.database)?.create_with_reader(
                &order_id,
                amount_sats,
                &account_xpub,
                cli.network,
                reader.as_ref(),
            )?;
            println!("/checkout/{}", inv.id);
        }
        Command::Serve { port } => checkout::serve(cli.database, port, cli.network).await?,
        Command::Worker {
            min_confirmations,
            once,
        } => worker::run(cli.database, cli.network, min_confirmations, once).await?,
        Command::Status { id } => {
            let db = Store::open_read_only(&cli.database)?;
            let invoice = db.get(&id)?.context("invoice not found")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "invoice_id":id,"order_id":invoice.order_id,"network":invoice.network.to_string(),
                    "amount_sats":invoice.amount_sats,"status":ledger::status(db.connection(),&id)?,
                    "paykit_delivery":paykit::delivery(&cli.database,&id)?
                }))?
            );
        }
        Command::Sign {
            terms,
            account_xpub,
            index,
            identity_key_file,
        } => {
            let terms: identity::Terms = serde_json::from_slice(&fs::read(terms)?)?;
            anyhow::ensure!(
                terms.network == cli.network.to_string(),
                "invoice network differs from selected network"
            );
            let proof = identity::sign(terms, &account_xpub, index, &identity_key_file)?;
            println!("{}", serde_json::to_string_pretty(&proof)?);
        }
        Command::VerifyLightning {
            invoice,
            node_pubkey,
            amount_sats,
        } => {
            lightning::verify_payment_request(
                &invoice,
                amount_sats,
                cli.network,
                &node_pubkey,
                ledger::now(),
            )?;
            println!("Verified Lightning invoice for {amount_sats} sats.");
        }
        Command::Verify {
            proof,
            merchant,
            invoice_id,
            amount_sats,
        } => {
            let proof: identity::Attestation = serde_json::from_slice(&fs::read(proof)?)?;
            proof.verify(&merchant, cli.network, &invoice_id, amount_sats)?;
            println!("{}", proof.terms.address);
        }
    }
    Ok(())
}
