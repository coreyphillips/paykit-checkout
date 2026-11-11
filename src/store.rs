//! Durable invoice and receiving-address allocation for one regtest merchant.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use bitcoin::{
    Address, Amount, CompressedPublicKey, Network, NetworkKind,
    bip32::{ChildNumber, Xpub},
    secp256k1::Secp256k1,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params};

/// Payment instructions issued once for a merchant order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invoice {
    pub id: String,
    pub order_id: String,
    pub amount_sats: u64,
    pub address: String,
}

impl Invoice {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            order_id: row.get("order_id")?,
            amount_sats: row.get("amount_sats")?,
            address: row.get("address")?,
        })
    }
}

/// Pins one merchant account and reserves each receiving address exactly once.
pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).context("open invoice database")?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS merchant (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                account_xpub TEXT NOT NULL,
                next_index INTEGER NOT NULL DEFAULT 0 CHECK (next_index BETWEEN 0 AND 2147483648)
             );
             CREATE TABLE IF NOT EXISTS invoices (
                id TEXT PRIMARY KEY NOT NULL DEFAULT (lower(hex(randomblob(16)))),
                order_id TEXT NOT NULL UNIQUE,
                amount_sats INTEGER NOT NULL CHECK (amount_sats > 0),
                address TEXT NOT NULL UNIQUE,
                address_index INTEGER NOT NULL UNIQUE
             );",
        )?;
        Ok(Self { db })
    }

    /// Opens an existing invoice database for the guest checkout. SQLite
    /// enforces read-only access even if this handle enters a write path.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("open existing invoice database for reading")?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { db })
    }

    /// Retries of the same order return its original invoice. The wallet must
    /// export a dedicated BIP84 test-family account at m/84'/1'/account'.
    pub fn create(
        &mut self,
        order_id: &str,
        amount_sats: u64,
        account_xpub: &str,
    ) -> Result<Invoice> {
        ensure!(
            (1..=128).contains(&order_id.len())
                && order_id.trim() == order_id
                && !order_id.chars().any(char::is_control),
            "order ID must be 1 to 128 bytes with no control characters or surrounding whitespace"
        );
        ensure!(
            (1..=Amount::MAX_MONEY.to_sat()).contains(&amount_sats),
            "invoice amount must be positive and at most 21 million BTC"
        );
        let account = parse_account(account_xpub)?;

        // Acquire the write lock before reading the idempotency key or allocating an index.
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO merchant (singleton, account_xpub) VALUES (1, ?1)
             ON CONFLICT (singleton) DO NOTHING",
            [account_xpub],
        )?;
        let (merchant_xpub, index): (String, u32) = tx.query_row(
            "SELECT account_xpub, next_index FROM merchant WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            merchant_xpub == account_xpub,
            "invoice database belongs to another account"
        );
        let existing = tx
            .query_row(
                "SELECT id, order_id, amount_sats, address FROM invoices WHERE order_id = ?1",
                [order_id],
                Invoice::from_row,
            )
            .optional()?;
        if let Some(invoice) = existing {
            ensure!(
                invoice.amount_sats == amount_sats,
                "order already exists with a different amount"
            );
            tx.commit()?;
            return Ok(invoice);
        }

        let address = receiving_address(&account, index)?;
        let invoice = tx.query_row(
            "INSERT INTO invoices (order_id, amount_sats, address, address_index)
             VALUES (?1, ?2, ?3, ?4) RETURNING id, order_id, amount_sats, address",
            params![order_id, amount_sats, address.to_string(), index],
            Invoice::from_row,
        )?;
        tx.execute(
            "UPDATE merchant SET next_index = next_index + 1 WHERE singleton = 1",
            [],
        )?;
        tx.commit()?;
        Ok(invoice)
    }

    /// Looks up the invoice associated with an opaque checkout ID.
    pub fn get(&self, id: &str) -> Result<Option<Invoice>> {
        Ok(self
            .db
            .query_row(
                "SELECT id, order_id, amount_sats, address FROM invoices WHERE id = ?1",
                [id],
                Invoice::from_row,
            )
            .optional()?)
    }
}

fn parse_account(input: &str) -> Result<Xpub> {
    let account: Xpub = input
        .parse()
        .map_err(|_| anyhow!("expected a test-family account tpub"))?;
    ensure!(
        account.network == NetworkKind::Test,
        "expected a test-family tpub"
    );
    // An xpub records its own depth and child number, but cannot prove its ancestors.
    ensure!(
        account.depth == 3 && account.child_number.is_hardened(),
        "expected a hardened account at depth 3"
    );
    Ok(account)
}

fn receiving_address(account: &Xpub, index: u32) -> Result<Address> {
    let path = [
        ChildNumber::from_normal_idx(0)?,
        ChildNumber::from_normal_idx(index).context("account receiving-address range exhausted")?,
    ];
    let child = account.derive_pub(&Secp256k1::verification_only(), &path)?;
    Ok(Address::p2wpkh(
        &CompressedPublicKey(child.public_key),
        Network::Regtest,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::bip32::{DerivationPath, Xpriv};
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    fn key_at(network: Network, path: &str) -> Xpriv {
        Xpriv::new_master(network, &[71; 32])
            .unwrap()
            .derive_priv(&Secp256k1::new(), &path.parse::<DerivationPath>().unwrap())
            .unwrap()
    }

    fn account_xpub(account: u32) -> String {
        let key = key_at(Network::Regtest, &format!("m/84'/1'/{account}'"));
        Xpub::from_priv(&Secp256k1::new(), &key).to_string()
    }

    fn wallet_address(index: u32) -> String {
        let key = key_at(Network::Regtest, &format!("m/84'/1'/0'/0/{index}"));
        let public_key = key.to_priv().public_key(&Secp256k1::new());
        Address::p2wpkh(&CompressedPublicKey(public_key.inner), Network::Regtest).to_string()
    }

    #[test]
    fn retry_after_reopen_recovers_original_invoice_and_next_address() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("merchant.sqlite");
        let account = account_xpub(0);
        let first = Store::open(&path)
            .unwrap()
            .create("coffee", 500, &account)
            .unwrap();
        assert_eq!(first.address, wallet_address(0));
        assert_eq!(first.id.len(), 32);
        assert!(first.id.bytes().all(|byte| byte.is_ascii_hexdigit()));

        let mut reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get(&first.id).unwrap().as_ref(), Some(&first));
        assert_eq!(reopened.create("coffee", 500, &account).unwrap(), first);
        assert!(reopened.create("coffee", 501, &account).is_err());
        assert!(reopened.create("cake", 501, &account_xpub(1)).is_err());
        let second = reopened.create("cake", 501, &account).unwrap();
        assert_eq!(second.address, wallet_address(1));
        assert_ne!(first.id, second.id);
        assert!(reopened.get("missing").unwrap().is_none());
    }

    #[test]
    fn guest_handle_requires_an_existing_database_and_cannot_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("merchant.sqlite");
        assert!(Store::open_read_only(&path).is_err());
        let account = account_xpub(0);
        let mut merchant = Store::open(&path).unwrap();
        let issued = merchant.create("coffee", 500, &account).unwrap();
        let mut guest = Store::open_read_only(&path).unwrap();
        assert_eq!(guest.get(&issued.id).unwrap(), Some(issued));
        assert!(guest.create("tea", 400, &account).is_err());
        assert_eq!(
            merchant.create("tea", 400, &account).unwrap().address,
            wallet_address(1)
        );
    }

    #[test]
    fn rejects_private_mainnet_and_non_account_keys() {
        let private = key_at(Network::Regtest, "m/84'/1'/0'");
        assert!(parse_account(&private.to_string()).is_err());
        for (network, path) in [
            (Network::Bitcoin, "m/84'/0'/0'"),
            (Network::Regtest, "m/84'/1'"),
            (Network::Regtest, "m/84'/1'/0"),
        ] {
            let public = Xpub::from_priv(&Secp256k1::new(), &key_at(network, path));
            assert!(parse_account(&public.to_string()).is_err());
        }
    }

    #[test]
    fn invalid_order_and_amount_do_not_reserve_addresses() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let account = account_xpub(0);
        for order in ["", " ", " coffee", "a\nb", &"x".repeat(129)] {
            assert!(store.create(order, 1, &account).is_err());
        }
        for amount in [0, Amount::MAX_MONEY.to_sat() + 1, u64::MAX] {
            assert!(store.create("coffee", amount, &account).is_err());
        }
        assert_eq!(
            store.create("coffee", 1, &account).unwrap().address,
            wallet_address(0)
        );
    }

    #[test]
    fn concurrent_writers_allocate_once_per_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("merchant.sqlite");
        let barrier = Arc::new(Barrier::new(3));
        let mut writers = Vec::new();
        for order in ["coffee", "coffee", "tea"] {
            let mut connection = Store::open(&path).unwrap();
            let barrier = Arc::clone(&barrier);
            let account = account_xpub(0);
            writers.push(thread::spawn(move || {
                barrier.wait();
                connection.create(order, 500, &account).unwrap()
            }));
        }
        let invoices: Vec<_> = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect();
        assert_eq!(invoices[0], invoices[1]);
        assert_ne!(invoices[0].address, invoices[2].address);
        let mut addresses = [invoices[0].address.clone(), invoices[2].address.clone()];
        let mut expected = [wallet_address(0), wallet_address(1)];
        addresses.sort();
        expected.sort();
        assert_eq!(addresses, expected);
    }
}
