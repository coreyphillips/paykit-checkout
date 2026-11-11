//! Durable invoice and receiving-address allocation for one merchant account on one Bitcoin network.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, anyhow, ensure};
use bitcoin::{
    Address, Amount, CompressedPublicKey, Network, NetworkKind,
    bip32::{ChildNumber, Xpub},
    secp256k1::Secp256k1,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};

/// Payment instructions issued once for a merchant order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invoice {
    pub id: String,
    pub order_id: String,
    pub amount_sats: u64,
    pub address: String,
    pub network: Network,
    pub expires_at: i64,
}

impl Invoice {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let network_column = row.as_ref().column_index("network")?;
        let network = row
            .get::<_, String>(network_column)?
            .parse()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    network_column,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
        Ok(Self {
            id: row.get("id")?,
            order_id: row.get("order_id")?,
            amount_sats: row.get("amount_sats")?,
            address: row.get("address")?,
            network,
            expires_at: row.get("expires_at")?,
        })
    }
}

/// Pins one merchant account and reserves each receiving address exactly once.
pub struct Store {
    db: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut db = Connection::open(path).context("open invoice database")?;
        db.busy_timeout(Duration::from_secs(5))?;
        let schema = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        schema.execute_batch(
            "CREATE TABLE IF NOT EXISTS deployment (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1), network TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS merchant (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                account_xpub TEXT NOT NULL,
                network TEXT NOT NULL,
                next_index INTEGER NOT NULL DEFAULT 0 CHECK (next_index BETWEEN 0 AND 2147483648)
             );
             CREATE TABLE IF NOT EXISTS invoices (
                id TEXT PRIMARY KEY NOT NULL DEFAULT (lower(hex(randomblob(16)))),
                order_id TEXT NOT NULL UNIQUE,
                amount_sats INTEGER NOT NULL CHECK (amount_sats > 0),
                address TEXT NOT NULL UNIQUE,
                address_index INTEGER NOT NULL UNIQUE,
                network TEXT NOT NULL,
                expires_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS chain_receipts (
                invoice_id TEXT NOT NULL, txid TEXT NOT NULL, vout INTEGER NOT NULL,
                first_seen_at INTEGER NOT NULL,
                PRIMARY KEY (invoice_id, txid, vout)
             );",
        )?;
        // Original stores were regtest-only. Retain their key, next index and checkout IDs.
        let has_network: bool = schema.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('merchant') WHERE name = 'network')",
            [],
            |row| row.get(0),
        )?;
        if !has_network {
            schema.execute_batch(
                "ALTER TABLE merchant ADD COLUMN network TEXT NOT NULL DEFAULT 'regtest';
                 ALTER TABLE invoices ADD COLUMN network TEXT NOT NULL DEFAULT 'regtest';
                 ALTER TABLE invoices ADD COLUMN expires_at INTEGER NOT NULL DEFAULT 0;
                 UPDATE invoices SET expires_at = unixepoch() + 3600;",
            )?;
        }
        schema.execute(
            "INSERT OR IGNORE INTO deployment (singleton, network)
             SELECT singleton, network FROM merchant",
            [],
        )?;
        schema.commit()?;
        crate::ledger::init(&db)?;
        crate::lightning::init(&db)?;
        crate::paykit::init(&db)?;
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

    /// Commits the selected network even before the merchant issues an invoice.
    pub fn pin_network(&self, network: Network) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        pin_network_in(&tx, network)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn create(
        &mut self,
        order_id: &str,
        amount_sats: u64,
        account_xpub: &str,
    ) -> Result<Invoice> {
        self.create_on_network(order_id, amount_sats, account_xpub, Network::Regtest)
    }

    /// Retries return the original invoice. Supply a dedicated BIP84 account public key.
    #[cfg(test)]
    pub fn create_on_network(
        &mut self,
        order_id: &str,
        amount_sats: u64,
        account_xpub: &str,
        network: Network,
    ) -> Result<Invoice> {
        self.create_with_reader(order_id, amount_sats, account_xpub, network, None)
    }

    /// Commits the receiving address and optional private request recipient together.
    pub fn create_with_reader(
        &mut self,
        order_id: &str,
        amount_sats: u64,
        account_xpub: &str,
        network: Network,
        reader: Option<&crate::paykit::Reader>,
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
        let account = parse_account(account_xpub, network)?;

        // Acquire the write lock before reading the idempotency key or allocating an index.
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        pin_network_in(&tx, network)?;
        tx.execute(
            "INSERT INTO merchant (singleton, account_xpub, network) VALUES (1, ?1, ?2)
             ON CONFLICT (singleton) DO NOTHING",
            params![account_xpub, network.to_string()],
        )?;
        let (merchant_xpub, merchant_network, index): (String, String, u32) = tx.query_row(
            "SELECT account_xpub, network, next_index FROM merchant WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(
            merchant_xpub == account_xpub,
            "invoice database belongs to another account"
        );
        ensure!(
            merchant_network == network.to_string(),
            "invoice database belongs to another Bitcoin network"
        );
        let existing = tx
            .query_row(
                "SELECT * FROM invoices WHERE order_id = ?1",
                [order_id],
                Invoice::from_row,
            )
            .optional()?;
        let invoice = if let Some(invoice) = existing {
            ensure!(
                invoice.amount_sats == amount_sats,
                "order already exists with a different amount"
            );
            invoice
        } else {
            let address = receiving_address(&account, index, network)?;
            let invoice = tx.query_row(
            "INSERT INTO invoices (order_id, amount_sats, address, address_index, network, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch() + 3600) RETURNING *",
            params![order_id, amount_sats, address.to_string(), index, network.to_string()],
            Invoice::from_row,
        )?;
            tx.execute(
                "UPDATE merchant SET next_index = next_index + 1 WHERE singleton = 1",
                [],
            )?;
            invoice
        };
        if let Some(reader) = reader {
            crate::paykit::bind_in(&tx, &invoice, reader)?;
        }
        tx.commit()?;
        Ok(invoice)
    }

    pub fn connection(&self) -> &Connection {
        &self.db
    }

    /// Includes expired invoices because payments can arrive late or be reorganized.
    pub fn all_invoices(&self) -> Result<Vec<Invoice>> {
        let mut rows = self
            .db
            .prepare("SELECT * FROM invoices ORDER BY address_index")?;
        Ok(rows
            .query_map([], Invoice::from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Looks up the invoice associated with an opaque checkout ID.
    pub fn get(&self, id: &str) -> Result<Option<Invoice>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM invoices WHERE id = ?1",
                [id],
                Invoice::from_row,
            )
            .optional()?)
    }
}

fn pin_network_in(tx: &Transaction<'_>, network: Network) -> Result<()> {
    let name = network.to_string();
    tx.execute(
        "INSERT INTO deployment (singleton,network) VALUES (1,?1) ON CONFLICT(singleton) DO NOTHING", [&name])?;
    let matches: bool = tx.query_row(
        "SELECT network=?1 AND NOT EXISTS(SELECT 1 FROM merchant WHERE network<>?1)
         FROM deployment WHERE singleton=1",
        [&name],
        |row| row.get(0),
    )?;
    ensure!(
        matches,
        "invoice database belongs to another Bitcoin network"
    );
    Ok(())
}

/// Derives a native SegWit address at the public account's external branch /0/index.
pub fn derive_address(account_xpub: &str, network: Network, index: u32) -> Result<Address> {
    receiving_address(&parse_account(account_xpub, network)?, index, network)
}

fn parse_account(input: &str, network: Network) -> Result<Xpub> {
    let account: Xpub = input
        .parse()
        .map_err(|_| anyhow!("expected a BIP32 account extended public key"))?;
    ensure!(
        account.network == NetworkKind::from(network),
        "account public key has the wrong version for {network}"
    );
    // An xpub records its own depth and child number, but cannot prove its ancestors.
    ensure!(
        account.depth == 3 && account.child_number.is_hardened(),
        "expected a hardened account at depth 3"
    );
    Ok(account)
}

fn receiving_address(account: &Xpub, index: u32, network: Network) -> Result<Address> {
    let path = [
        ChildNumber::from_normal_idx(0)?,
        ChildNumber::from_normal_idx(index).context("account receiving-address range exhausted")?,
    ];
    let child = account.derive_pub(&Secp256k1::verification_only(), &path)?;
    Ok(Address::p2wpkh(
        &CompressedPublicKey(child.public_key),
        network,
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
        assert!(parse_account(&private.to_string(), Network::Regtest).is_err());
        for (network, path) in [
            (Network::Bitcoin, "m/84'/0'/0'"),
            (Network::Regtest, "m/84'/1'"),
            (Network::Regtest, "m/84'/1'/0"),
        ] {
            let mut public = Xpub::from_priv(&Secp256k1::new(), &key_at(Network::Regtest, path));
            public.network = NetworkKind::from(network);
            assert!(parse_account(&public.to_string(), Network::Regtest).is_err());
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
    fn migrates_regtest_orders_without_changing_payment_details_or_reusing_addresses() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("merchant.sqlite");
        let account = account_xpub(0);
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE merchant (singleton INTEGER PRIMARY KEY, account_xpub TEXT NOT NULL, next_index INTEGER DEFAULT 0);
                 CREATE TABLE invoices (id TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(16)))),
                    order_id TEXT UNIQUE, amount_sats INTEGER, address TEXT UNIQUE, address_index INTEGER UNIQUE);"
            ).unwrap();
            db.execute("INSERT INTO merchant VALUES (1, ?1, 3)", [&account])
                .unwrap();
            db.execute(
                "INSERT INTO invoices VALUES ('legacy', 'coffee', 99, ?1, 2)",
                [wallet_address(2)],
            )
            .unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let original = store.get("legacy").unwrap().unwrap();
        assert_eq!(original.network, Network::Regtest);
        assert!(original.expires_at > 0);
        assert_eq!(original, store.create("coffee", 99, &account).unwrap());
        assert_eq!(
            wallet_address(3),
            store.create("cake", 52, &account).unwrap().address
        );
        assert!(
            store
                .create_on_network("coffee", 99, &account, Network::Signet)
                .is_err()
        );
        assert_eq!(store.all_invoices().unwrap().len(), 2);
    }

    #[test]
    fn empty_deployment_pin_survives_reopen_and_limits_invoice_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shop.db");
        Store::open(&path)
            .unwrap()
            .pin_network(Network::Bitcoin)
            .unwrap();
        let mut reopened = Store::open(&path).unwrap();
        assert!(reopened.pin_network(Network::Regtest).is_err());
        assert!(reopened.create("coffee", 1, &account_xpub(0)).is_err());
    }

    #[test]
    fn mainnet_wallet_receives_on_mainnet_and_rejects_test_keys() {
        let mut public =
            Xpub::from_priv(&Secp256k1::new(), &key_at(Network::Regtest, "m/84'/1'/0'"));
        public.network = NetworkKind::Main;
        let public = public.to_string();
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        assert!(
            store
                .create_on_network("order", 500, &account_xpub(0), Network::Bitcoin)
                .is_err()
        );
        let invoice = store
            .create_on_network("order", 500, &public, Network::Bitcoin)
            .unwrap();
        assert_eq!(invoice.network, Network::Bitcoin);
        let expected = key_at(Network::Regtest, "m/84'/1'/0'/0/0")
            .to_priv()
            .public_key(&Secp256k1::new());
        assert_eq!(
            invoice.address,
            Address::p2wpkh(&CompressedPublicKey(expected.inner), Network::Bitcoin).to_string()
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
