//! Reconcile merchant invoice outputs against a trusted Electrum indexer.

use std::{collections::HashSet, fs::File, net::IpAddr, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use bitcoin::{
    Address, OutPoint, Script, Transaction, Txid,
    blockdata::constants::{COINBASE_MATURITY, genesis_block},
};
use electrum_client::{Client, ConfigBuilder, ElectrumApi, utils::validate_merkle_proof};
use rusqlite::{TransactionBehavior, params};

use crate::{
    ledger::{self, Observation},
    store::{Invoice, Store},
};

/// Reads each invoice's complete history, including spent outputs. A fresh scan
/// replaces the old totals so that reorgs and mempool eviction can undo a payment.
pub fn sync(store: &Store, electrum_url: &str, min_confirmations: u32) -> Result<usize> {
    check_endpoint(electrum_url)?;
    ensure!(
        min_confirmations > 0,
        "at least one on-chain confirmation is required"
    );
    // Serialize observers, including separate CLI processes. A slow scan must
    // never overwrite results from a newer scan. The OS releases this on exit.
    let path = store
        .connection()
        .path()
        .context("chain sync needs a database file")?;
    ensure!(!path.is_empty(), "chain sync needs a database file");
    let mut lock_path = Path::new(path).canonicalize()?.into_os_string();
    lock_path.push("-chain-sync.lock");
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.try_lock()
        .context("chain sync already running or database lock unavailable")?;

    let invoices = store.all_invoices()?;
    let Some(first) = invoices.first() else {
        return Ok(0);
    };
    let config = ConfigBuilder::new()
        .timeout(Some(Duration::from_secs(15)))
        .build();
    let client =
        Client::from_config(electrum_url, config).context("connect to merchant Electrum server")?;
    let genesis = client.block_header(0)?;
    ensure!(
        genesis.block_hash() == genesis_block(first.network).block_hash(),
        "Electrum server is on a different Bitcoin network"
    );
    let tip = client.block_headers_subscribe()?;

    let mut observations = Vec::with_capacity(invoices.len());
    for invoice in &invoices {
        ensure!(
            invoice.network == first.network,
            "invoice database contains multiple networks"
        );
        let observation = observe_invoice(&client, invoice, tip.height, min_confirmations)
            .with_context(|| format!("scan invoice {}", invoice.id))?;
        observations.push((invoice, observation, ledger::now()));
    }
    let latest = client.block_headers_subscribe()?;
    ensure!(
        latest.height >= tip.height,
        "Electrum tip lost height during scan, retry"
    );
    let anchor = if latest.height == tip.height {
        latest.header
    } else {
        client.block_header(tip.height)?
    };
    ensure!(
        anchor == tip.header,
        "Electrum chain reorganized during scan, retry"
    );
    let tx =
        rusqlite::Transaction::new_unchecked(store.connection(), TransactionBehavior::Immediate)?;
    for (invoice, receipts, observed_at) in observations {
        record_receipts(&tx, invoice, &receipts, observed_at)?;
    }
    tx.commit()?;
    Ok(invoices.len())
}

fn check_endpoint(url: &str) -> Result<()> {
    let (scheme, authority) = url
        .split_once("://")
        .context("specify an Electrum URL with ssl:// or tcp://")?;
    let (host, port) = authority
        .rsplit_once(':')
        .context("Electrum URL is missing its port")?;
    ensure!(
        !host.is_empty()
            && !host.contains(['/', '\\', '?', '#', '@'])
            && !host.chars().any(char::is_whitespace)
            && port.parse::<u16>().is_ok_and(|port| port > 0),
        "invalid Electrum host or port"
    );
    let ip = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let loopback = host == "localhost" || ip.parse::<IpAddr>().is_ok_and(|addr| addr.is_loopback());
    ensure!(
        scheme == "ssl" || (scheme == "tcp" && loopback),
        "use ssl:// for remote Electrum or tcp:// with a loopback address"
    );
    Ok(())
}

struct Receipt {
    outpoint: OutPoint,
    sats: u64,
    confirmed: bool,
}

fn observe_invoice(
    client: &Client,
    invoice: &Invoice,
    tip_height: usize,
    min_confirmations: u32,
) -> Result<Vec<Receipt>> {
    let address = invoice
        .address
        .parse::<Address<_>>()?
        .require_network(invoice.network)?;
    let script = address.script_pubkey();
    let mut receipts = Vec::new();
    let mut txids = HashSet::new();
    for entry in client.script_get_history(&script)? {
        ensure!(
            txids.insert(entry.tx_hash),
            "Electrum repeated a transaction in script history"
        );
        ensure!(entry.height >= -1, "invalid Electrum transaction height");
        let tx = client.transaction_get(&entry.tx_hash)?;
        ensure!(
            tx.compute_txid() == entry.tx_hash,
            "Electrum returned a different transaction"
        );
        let outputs = invoice_outputs(&tx, entry.tx_hash, &script)?;
        if outputs.is_empty() {
            continue;
        }
        let depth = if entry.height > 0 {
            let height = usize::try_from(entry.height)?;
            let header = client.block_header(height)?;
            let proof = client.transaction_get_merkle(&entry.tx_hash, height)?;
            ensure!(
                proof.block_height == height
                    && validate_merkle_proof(&entry.tx_hash, &header.merkle_root, &proof),
                "invalid Electrum transaction inclusion proof"
            );
            // A normal extension can confirm new payments while we scan other orders.
            // Credit confirmations only up to this pass's stable chain anchor.
            tip_height
                .checked_sub(height)
                .map_or(0, |distance| distance + 1)
        } else {
            0
        };
        let required = if tx.is_coinbase() {
            min_confirmations.max(COINBASE_MATURITY)
        } else {
            min_confirmations
        };
        for (outpoint, sats) in outputs {
            receipts.push(Receipt {
                outpoint,
                sats,
                confirmed: depth >= required as usize,
            });
        }
    }
    Ok(receipts)
}

fn invoice_outputs(tx: &Transaction, txid: Txid, script: &Script) -> Result<Vec<(OutPoint, u64)>> {
    tx.output
        .iter()
        .enumerate()
        .filter(|(_, output)| {
            output.script_pubkey.as_script() == script && output.value.to_sat() > 0
        })
        .map(|(index, output)| {
            Ok((
                OutPoint::new(txid, index.try_into()?),
                output.value.to_sat(),
            ))
        })
        .collect()
}

fn record_receipts(
    tx: &rusqlite::Transaction<'_>,
    invoice: &Invoice,
    receipts: &[Receipt],
    observed_at: i64,
) -> Result<()> {
    let mut totals = Observation::default();
    let mut timed = Vec::with_capacity(receipts.len());
    for receipt in receipts {
        let key = params![
            invoice.id,
            receipt.outpoint.txid.to_string(),
            receipt.outpoint.vout,
            observed_at
        ];
        let first_seen: i64 = tx.query_row(
            "INSERT INTO chain_receipts(invoice_id, txid, vout, first_seen_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(invoice_id,txid,vout) DO UPDATE SET first_seen_at = chain_receipts.first_seen_at
             RETURNING first_seen_at", key, |row| row.get(0))?;
        totals.observed_sats = totals
            .observed_sats
            .checked_add(receipt.sats)
            .context("invoice total overflow")?;
        if receipt.confirmed {
            totals.confirmed_sats = totals
                .confirmed_sats
                .checked_add(receipt.sats)
                .context("confirmed total overflow")?;
        }
        timed.push((first_seen, receipt));
    }
    // Once sufficient outputs have confirmed, base the timing decision on those
    // exact outputs. An evicted payment cannot lend its arrival time to an RBF replacement.
    let use_confirmed = totals.confirmed_sats >= invoice.amount_sats;
    timed.sort_unstable_by_key(|(first_seen, _)| *first_seen);
    let mut amount = 0;
    let mut full_seen_at = None;
    for (first_seen, receipt) in timed {
        if use_confirmed && !receipt.confirmed {
            continue;
        }
        amount += receipt.sats; // Bounded by the checked observed total above.
        if amount >= invoice.amount_sats {
            full_seen_at = Some(first_seen);
            break;
        }
    }
    ledger::record_chain_observation_in(tx, &invoice.id, totals, full_seen_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Amount, ScriptBuf, TxOut, absolute, transaction};

    #[test]
    fn replacement_after_expiry_does_not_inherit_the_original_outpoints_arrival() -> Result<()> {
        use bitcoin::hashes::Hash;
        let store = Store::open(Path::new(":memory:"))?;
        let db = store.connection();
        db.execute(
            "INSERT INTO invoices(id,order_id,amount_sats,address,address_index,network,expires_at)
            VALUES('invoice','order',100,'test-address',0,'regtest',20)",
            [],
        )?;
        let invoice = store.get("invoice")?.unwrap();
        let first = Receipt {
            outpoint: OutPoint::new(Txid::from_byte_array([1; 32]), 0),
            sats: 100,
            confirmed: false,
        };
        let replacement = Receipt {
            outpoint: OutPoint::new(Txid::from_byte_array([2; 32]), 1),
            sats: 100,
            confirmed: true,
        };
        let tx = db.unchecked_transaction()?;
        record_receipts(&tx, &invoice, &[first], 10)?;
        assert_eq!(ledger::status(&tx, &invoice.id)?.state, "confirming");
        record_receipts(&tx, &invoice, &[replacement], 30)?;
        assert_eq!(ledger::status(&tx, &invoice.id)?.state, "late");
        tx.commit()?;
        Ok(())
    }

    #[test]
    fn remote_indexers_require_tls_and_plaintext_is_limited_to_loopback() {
        for url in [
            "tcp://127.0.0.1:50001",
            "tcp://[::1]:60001",
            "ssl://electrum.example:50002",
        ] {
            check_endpoint(url).unwrap();
        }
        for url in [
            "tcp://192.168.1.5:50001",
            "electrum.example:50001",
            "tcp://localhost@example.org:50001",
            "https://example.org:443",
        ] {
            assert!(check_endpoint(url).is_err());
        }
    }

    #[test]
    fn counts_all_outputs_to_the_invoice_and_ignores_other_scripts() {
        let script = ScriptBuf::from_bytes(vec![0x51]);
        let outputs = [(13, &script), (500, &ScriptBuf::new()), (29, &script)];
        let mut tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![],
            output: outputs
                .iter()
                .map(|(sats, script)| TxOut {
                    value: Amount::from_sat(*sats),
                    script_pubkey: (*script).clone(),
                })
                .collect(),
        };
        let txid = tx.compute_txid();
        assert_eq!(
            invoice_outputs(&tx, txid, &script).unwrap(),
            [(OutPoint::new(txid, 0), 13), (OutPoint::new(txid, 2), 29)]
        );
        tx.output.retain(|out| out.script_pubkey != script);
        assert!(
            invoice_outputs(&tx, tx.compute_txid(), &script)
                .unwrap()
                .is_empty()
        );
    }
}
