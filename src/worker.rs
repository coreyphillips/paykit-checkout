use crate::{
    chain, config,
    ledger::{self, Observation},
    lightning::Lnd,
    paykit::Paykit,
    store::Store,
    webhooks::Webhook,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    time::Duration,
};

pub async fn run(
    path: PathBuf,
    network: bitcoin::Network,
    min_confirmations: u32,
    once: bool,
) -> Result<()> {
    ensure!(
        min_confirmations > 0,
        "at least one on-chain confirmation is required"
    );
    Store::open(&path)?.pin_network(network)?;
    let path = path.canonicalize()?;
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".worker.lock");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lease
        .try_lock()
        .context("another worker is using this database")?;

    let electrum = config::optional("CHECKOUT_ELECTRUM_URL")?
        .context("set CHECKOUT_ELECTRUM_URL for Bitcoin settlement")?;
    let lnd = Lnd::from_env()?;
    let paykit = Paykit::from_env()?;
    ensure!(
        paykit.is_none() || min_confirmations <= 6,
        "the configured Paykit receiver supports at most six confirmations"
    );
    let hook = Webhook::from_env()?;
    loop {
        let chain_path = path.clone();
        let endpoint = electrum.clone();
        let chain_job = tokio::task::spawn_blocking(move || {
            chain::sync(&Store::open(&chain_path)?, &endpoint, min_confirmations)
        });
        let paykit_job = async {
            if let Some(p) = &paykit {
                p.sync(&path, min_confirmations).await?;
            }
            anyhow::Ok(())
        };
        let lightning_job = async {
            if let Some(node) = &lnd {
                sync_lightning(node, &path).await?;
            }
            anyhow::Ok(())
        };
        let (chain_result, paykit_result, lightning_result) =
            tokio::join!(chain_job, paykit_job, lightning_job);
        let webhook_result = if let Some(hook) = &hook {
            hook.deliver(&path).await
        } else {
            Ok(())
        };
        let mut failed = false;
        for (name, result) in [
            ("Bitcoin", chain_result?.map(|_| ())),
            ("Paykit", paykit_result),
            ("Lightning", lightning_result),
            ("webhook", webhook_result),
        ] {
            if let Err(error) = result {
                eprintln!("{name} worker: {error}");
                failed = true;
            }
        }
        if once {
            ensure!(!failed, "one or more workers failed");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn sync_lightning(node: &Lnd, path: &Path) -> Result<()> {
    let store = Store::open(path)?;
    let mut failed = false;
    for invoice in store.all_invoices()? {
        let result = async {
            let id = &invoice.id;
            let mut obs = node
                .lookup(path, id, invoice.amount_sats, invoice.network)
                .await?;
            if obs.is_none()
                && invoice.expires_at > ledger::now()
                && ledger::status(store.connection(), id)?.state == "unpaid"
            {
                obs = Some(
                    node.start(
                        path,
                        id,
                        invoice.amount_sats,
                        invoice.network,
                        invoice.expires_at,
                    )
                    .await?,
                );
            }
            if let Some(payment) = obs
                && let (Some(msat), Some(time)) = (payment.settled_msat, payment.settled_at)
            {
                let sats = msat / 1000;
                ledger::record_observation_at(
                    store.connection(),
                    id,
                    "lightning",
                    Observation {
                        observed_sats: sats,
                        confirmed_sats: sats,
                    },
                    time,
                )?;
            }
            anyhow::Ok(())
        }
        .await;
        if let Err(err) = result {
            failed = true;
            eprintln!("Lightning invoice {}: {err}", invoice.id);
        }
    }
    ensure!(
        !failed,
        "one or more Lightning invoices could not be synchronized"
    );
    Ok(())
}
