mod checkout;
mod store;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use store::Store;

#[derive(Parser)]
#[command(version, about = "Create and serve regtest Bitcoin invoices")]
struct Cli {
    /// SQLite database for this merchant's regtest wallet.
    #[arg(long, global = true, default_value = "checkout.sqlite3")]
    database: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an invoice or recover its checkout path by replaying the same order.
    Invoice {
        /// Merchant's unique reference for this order.
        #[arg(long)]
        order_id: String,
        /// Price in whole satoshis.
        #[arg(long)]
        amount_sats: u64,
        /// Account public key from the merchant's regtest BIP84 wallet (tpub).
        #[arg(long)]
        account_xpub: String,
    },
    /// Serve existing guest checkout links on the loopback interface.
    Serve {
        #[arg(long, default_value_t = 8080)]
        port: u16,
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
        } => {
            let invoice =
                Store::open(&cli.database)?.create(&order_id, amount_sats, &account_xpub)?;
            println!("/checkout/{}", invoice.id);
            Ok(())
        }
        Command::Serve { port } => checkout::serve(cli.database, port).await,
    }
}
