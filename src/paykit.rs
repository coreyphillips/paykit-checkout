//! Durable delivery of a merchant invoice to a specific Pubky Reader.

mod upstream;

use std::{env, path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitcoin::{Network, Txid};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::{
    ledger::{self, Observation},
    store::Invoice,
};
use upstream::{RemoteError, Status, Upstream};

/// Canonical, public Pubky identity accepted by the upstream reader endpoint.
pub struct Reader(String);

impl Reader {
    pub fn parse(input: &str) -> Result<Self> {
        let encoded = input
            .strip_prefix("pubky")
            .context("expected a pubky-prefixed identity")?;
        ensure!(encoded.len() == 52, "expected a 52-character Pubky key");
        let mut encoding = data_encoding::Specification::new();
        encoding
            .symbols
            .push_str("ybndrfg8ejkmcpqxot1uwisza345h769");
        let decoded: [u8; 32] = encoding
            .encoding()?
            .decode(encoded.as_bytes())?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid Pubky key length"))?;
        let key = VerifyingKey::from_bytes(&decoded)?;
        ensure!(!key.is_weak(), "invalid Pubky identity");
        Ok(Self(input.to_owned()))
    }
}

struct Authority {
    origin: String,
    creator: Reader,
    network: Network,
}

impl Authority {
    fn from_env() -> Result<Option<Self>> {
        let Some(base) = crate::config::optional("CHECKOUT_PAYKIT_URL")? else {
            return Ok(None);
        };
        let origin = Upstream::origin(&base)?;
        let creator = Reader::parse(
            &env::var("CHECKOUT_PAYKIT_CREATOR").context("set CHECKOUT_PAYKIT_CREATOR")?,
        )?;
        let network = crate::config::parse_network(
            &env::var("CHECKOUT_PAYKIT_NETWORK").context("set CHECKOUT_PAYKIT_NETWORK")?,
        )?;
        Ok(Some(Self {
            origin,
            creator,
            network,
        }))
    }
}

pub fn init(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS paykit_requests (
            invoice_id TEXT PRIMARY KEY REFERENCES invoices(id), creator TEXT NOT NULL,
            reader TEXT NOT NULL, origin TEXT NOT NULL, reference TEXT NOT NULL UNIQUE,
            window_seconds INTEGER NOT NULL, remote_id TEXT, revision INTEGER NOT NULL DEFAULT 0,
            phase TEXT NOT NULL DEFAULT 'pending', delivery TEXT NOT NULL DEFAULT 'queued',
            UNIQUE(origin, creator, remote_id)
        );
        CREATE TABLE IF NOT EXISTS paykit_timely_outputs (
            invoice_id TEXT NOT NULL REFERENCES invoices(id), txid TEXT NOT NULL,
            proven_by INTEGER NOT NULL, PRIMARY KEY(invoice_id, txid)
        );",
    )?;
    Ok(())
}

fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}

/// Reserve a Reader and immutable proposal terms before issuing a remote request.
pub fn bind(path: &Path, invoice: &Invoice, reader: &Reader) -> Result<()> {
    let mut db = open(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    bind_in(&tx, invoice, reader)?;
    tx.commit()?;
    Ok(())
}

/// Bind the Paykit Reader atomically with the merchant order and its guest address.
pub fn bind_in(tx: &rusqlite::Transaction<'_>, invoice: &Invoice, reader: &Reader) -> Result<()> {
    let authority = Authority::from_env()?.context("Paykit delivery is not configured")?;
    bind_to(tx, invoice, reader, &authority)
}

fn bind_to(
    tx: &rusqlite::Transaction<'_>,
    invoice: &Invoice,
    reader: &Reader,
    authority: &Authority,
) -> Result<()> {
    ensure!(
        invoice.network == authority.network,
        "Paykit network differs from invoice"
    );
    ensure!(
        authority.creator.0 != reader.0,
        "creator and reader must differ"
    );
    let existing = tx
        .query_row(
            "SELECT reader, creator, origin FROM paykit_requests WHERE invoice_id=?1",
            [&invoice.id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((bound_reader, creator, origin)) = existing {
        ensure!(
            bound_reader == reader.0
                && creator == authority.creator.0
                && origin == authority.origin,
            "invoice already bound to another Paykit identity or provider"
        );
        return Ok(());
    }
    let window = invoice.expires_at - ledger::now();
    ensure!(window > 0, "invoice expired before Paykit delivery");
    ensure!(
        ledger::status(tx, &invoice.id)?.state == "unpaid",
        "invoice already received a payment"
    );
    tx.execute(
        "INSERT INTO paykit_requests (invoice_id, creator, reader, origin, reference, window_seconds)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![invoice.id, authority.creator.0, reader.0, authority.origin, Uuid::new_v4().to_string(), window]
    )?;
    Ok(())
}

/// Last known delivery state. Payment and fulfillment are tracked by the shared ledger.
pub fn delivery(path: &Path, id: &str) -> Result<Option<String>> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(db
        .query_row(
            "SELECT delivery FROM paykit_requests WHERE invoice_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

pub struct Paykit {
    authority: Authority,
    upstream: Upstream,
}

struct Binding {
    id: String,
    creator: String,
    reader: String,
    reference: String,
    window: u64,
    amount: u64,
    expires_at: i64,
    remote: Option<String>,
    phase: String,
    revision: i64,
}

impl Paykit {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(authority) = Authority::from_env()? else {
            return Ok(None);
        };
        let filename = env::var_os("CHECKOUT_PAYKIT_SIGNING_KEY_FILE")
            .context("set CHECKOUT_PAYKIT_SIGNING_KEY_FILE")?;
        let encoded = crate::config::secret(Path::new(&filename))
            .context("read Paykit service signing key file")?;
        let seed: [u8; 32] = URL_SAFE_NO_PAD
            .decode(encoded.trim())
            .context("invalid Paykit key encoding")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected a 32-byte Paykit signing seed"))?;
        let upstream = Upstream::new(authority.origin.clone(), SigningKey::from_bytes(&seed))?;
        Ok(Some(Self {
            authority,
            upstream,
        }))
    }

    /// Resume durable proposals and reconcile current Bitcoin observations, including reorgs.
    pub async fn sync(&self, path: &Path, min_confirmations: u32) -> Result<()> {
        ensure!(
            (1..=6).contains(&min_confirmations),
            "Paykit requires 1 to 6 confirmations"
        );
        let jobs = self.bindings(path)?;
        let mut first_error = None;
        for binding in jobs {
            if let Err(err) = self.advance(path, &binding, min_confirmations).await {
                first_error.get_or_insert(err);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn bindings(&self, path: &Path) -> Result<Vec<Binding>> {
        let db = open(path)?;
        let mut query = db.prepare(
            "SELECT p.invoice_id,p.creator,p.reader,p.reference,p.window_seconds,i.amount_sats,
                    i.expires_at,p.remote_id,p.phase,p.revision
             FROM paykit_requests p JOIN invoices i ON i.id=p.invoice_id
             WHERE p.phase IN ('pending','prepared','active') AND p.origin=?1 AND p.creator=?2 AND i.network=?3"
        )?;
        let rows = query.query_map(
            params![
                self.authority.origin,
                self.authority.creator.0,
                self.authority.network.to_string()
            ],
            |r| {
                Ok(Binding {
                    id: r.get(0)?,
                    creator: r.get(1)?,
                    reader: r.get(2)?,
                    reference: r.get(3)?,
                    window: r.get(4)?,
                    amount: r.get(5)?,
                    expires_at: r.get(6)?,
                    remote: r.get(7)?,
                    phase: r.get(8)?,
                    revision: r.get(9)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    async fn advance(&self, path: &Path, binding: &Binding, min_confirmations: u32) -> Result<()> {
        if binding.phase == "active" {
            let remote = binding
                .remote
                .as_deref()
                .context("missing active Paykit invoice ID")?;
            let status = self.upstream.status(binding, remote).await?;
            return apply_status(path, binding, &status, min_confirmations);
        }
        let stop = stop_before_delivery(path, binding)?;
        if binding.phase == "pending" {
            if let Some(reason) = stop {
                return transition(path, binding, "stopped", reason, None);
            }
            return match self.upstream.prepare(binding).await {
                Ok(id) => transition(path, binding, "prepared", "prepared", Some(&id)),
                Err(e) if has_code(&e, "reader_not_payable") => {
                    transition(path, binding, "failed", "reader_not_payable", None)
                }
                Err(e) => Err(e),
            };
        }
        let remote = binding
            .remote
            .as_deref()
            .context("missing durable Paykit invoice ID")?;
        ensure!(
            binding.phase == "prepared",
            "unexpected Paykit request phase"
        );
        if let Some(reason) = stop {
            // Void serializes against activate upstream. If activation already won (including a
            // lost response), retain monitoring of the previously dispatched payment request.
            return match self.upstream.void(binding, remote).await {
                Ok(()) => transition(path, binding, "stopped", reason, None),
                Err(e) if has_code(&e, "invoice_active") => {
                    transition(path, binding, "active", "active", None)
                }
                Err(e) => Err(e),
            };
        }
        match self.upstream.activate(binding, remote).await {
            Ok(()) => transition(path, binding, "active", "active", None),
            Err(e) if has_code(&e, "prepare_expired") => renew_preparation(path, binding),
            Err(e) => Err(e),
        }
    }
}

fn stop_before_delivery(path: &Path, binding: &Binding) -> Result<Option<&'static str>> {
    let status = ledger::status(&open(path)?, &binding.id)?;
    if !status.rails.is_empty() {
        return Ok(Some("payment_received"));
    }
    Ok((ledger::now() >= binding.expires_at).then_some("expired"))
}

fn has_code(error: &anyhow::Error, code: &str) -> bool {
    error
        .downcast_ref::<RemoteError>()
        .is_some_and(|e| e.status == 409 && e.code == code)
}

fn transition(
    path: &Path,
    binding: &Binding,
    phase: &str,
    delivery: &str,
    remote: Option<&str>,
) -> Result<()> {
    open(path)?.execute(
        "UPDATE paykit_requests SET phase=?1,delivery=?2,remote_id=coalesce(?3,remote_id),
             revision=revision+1 WHERE invoice_id=?4 AND revision=?5",
        params![phase, delivery, remote, binding.id, binding.revision],
    )?;
    Ok(())
}

fn renew_preparation(path: &Path, binding: &Binding) -> Result<()> {
    // This exact error guarantees that activation failed before payment instructions were sent.
    let window = binding.expires_at - ledger::now();
    if window <= 0 {
        return transition(path, binding, "expired", "expired", None);
    }
    open(path)?.execute(
        "UPDATE paykit_requests SET phase='pending',delivery='queued',remote_id=NULL,
            reference=?1,window_seconds=?2,revision=revision+1 WHERE invoice_id=?3 AND revision=?4",
        params![
            Uuid::new_v4().to_string(),
            window,
            binding.id,
            binding.revision
        ],
    )?;
    Ok(())
}

fn apply_status(
    path: &Path,
    binding: &Binding,
    status: &Status,
    min_confirmations: u32,
) -> Result<()> {
    let now = ledger::now();
    let mut db = open(path)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE paykit_requests SET delivery=?1,revision=revision+1 WHERE invoice_id=?2 AND revision=?3",
        params![status.proposal_delivery_state, binding.id, binding.revision]
    )?;
    if changed == 0 {
        return Ok(());
    }
    let previous = if let Some(payment) = &status.bitcoin {
        tx.query_row(
            "SELECT proven_by FROM paykit_timely_outputs WHERE invoice_id=?1 AND txid=?2",
            params![binding.id, payment.txid],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    if let Some((observation, timely_at)) =
        evidence(binding, status, min_confirmations, now, previous)?
    {
        if let (Some(at), Some(payment)) = (timely_at, &status.bitcoin) {
            tx.execute("INSERT OR IGNORE INTO paykit_timely_outputs (invoice_id,txid,proven_by) VALUES (?1,?2,?3)",
                params![binding.id,payment.txid,at])?;
        }
        ledger::record_authoritative_observation_in(
            &tx,
            &binding.id,
            "paykit",
            observation,
            timely_at,
        )?;
    }
    tx.commit()?;
    Ok(())
}

type Evidence = (Observation, Option<i64>);

fn evidence(
    binding: &Binding,
    status: &Status,
    minimum: u32,
    now: i64,
    previous: Option<i64>,
) -> Result<Option<Evidence>> {
    let Some(state) = status.payment_state.as_deref() else {
        return Ok(None);
    };
    let zero = || Some((Observation::default(), None));
    match state {
        "undetected" if status.bitcoin.is_some() || status.request_state.is_some() => {
            return Ok(zero());
        }
        "undetected" => return Ok(None),
        "expired" if status.bitcoin.is_none() => return Ok(zero()),
        "confirmed" | "detected" | "expired" => {}
        _ => bail!("unknown Paykit payment state"),
    }
    let btc = status
        .bitcoin
        .as_ref()
        .context("Paykit has no Bitcoin observation")?;
    btc.txid
        .parse::<Txid>()
        .context("invalid Paykit Bitcoin transaction ID")?;
    ensure!(btc.observed_sats > 0, "invalid observed Paykit amount");
    // The upstream retains disappeared outputs. An expired record with no evidence of presence
    // cannot distinguish a missing output from a late unconfirmed partial payment.
    if state == "expired" && btc.confirmations == 0 && !btc.amount_matched {
        return Ok(zero());
    }
    let matched = btc.amount_matched
        && btc.paid_on_time
        && btc.observed_sats >= binding.amount
        && status.paid_asset.as_deref() == Some("BTC");
    let mut timely_at = None;
    if matched {
        timely_at = previous;
        if timely_at.is_none()
            && (now <= binding.expires_at || status.deadline_within(binding.expires_at)?)
        {
            // The conservative upper bound on when this output was already observed.
            timely_at = Some(now.min(binding.expires_at));
        }
    }
    let confirmed = state == "confirmed" && btc.confirmations >= minimum && timely_at.is_some();
    Ok(Some((
        Observation {
            observed_sats: btc.observed_sats,
            confirmed_sats: if confirmed { btc.observed_sats } else { 0 },
        },
        timely_at,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use bitcoin::{
        bip32::{DerivationPath, Xpriv, Xpub},
        secp256k1::Secp256k1,
    };

    fn guest_account() -> String {
        let secp = Secp256k1::new();
        let account = Xpriv::new_master(Network::Regtest, &[79; 32])
            .unwrap()
            .derive_priv(&secp, &"m/84'/1'/0'".parse::<DerivationPath>().unwrap())
            .unwrap();
        Xpub::from_priv(&secp, &account).to_string()
    }

    fn binding() -> Binding {
        Binding {
            id: "invoice".into(),
            creator: String::new(),
            reader: String::new(),
            reference: String::new(),
            window: 60,
            amount: 50000,
            expires_at: 1700000000,
            remote: None,
            phase: "active".into(),
            revision: 0,
        }
    }

    #[test]
    fn status_fixtures_distinguish_settlement_partial_payment_reorg_and_expiry() -> Result<()> {
        let binding = binding();
        for (raw, seen, confirmed) in [
            (
                include_str!("../tests/paykit-regtest/confirmed-matched.json"),
                50000,
                50000,
            ),
            (
                include_str!("../tests/paykit-regtest/confirmed-unmatched.json"),
                49999,
                0,
            ),
            (include_str!("../tests/paykit-regtest/reorged.json"), 0, 0),
            (
                include_str!("../tests/paykit-regtest/expired-late-match.json"),
                50000,
                0,
            ),
        ] {
            let status = serde_json::from_str(raw)?;
            let (facts, _) =
                evidence(&binding, &status, 3, binding.expires_at + 60, None)?.unwrap();
            assert_eq!(
                (facts.observed_sats, facts.confirmed_sats),
                (seen, confirmed)
            );
        }
        Ok(())
    }

    #[test]
    fn retries_preserve_request_and_reject_a_replacement_reader() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("merchant.db");
        let invoice = Store::open(&path)?.create_on_network(
            "order",
            50000,
            &guest_account(),
            Network::Regtest,
        )?;
        let creator = Reader::parse("pubkytkrq8zmwb8a3m9k15csu3q17qmfgqnp9dskbrg9uq1rydpyxp7qy")?;
        let reader = Reader::parse("pubky3f4khszd4gzg38bsazw88dmcfws36js9ct19s9dikb3a9ht3r84y")?;
        let other = Reader::parse("pubky7ir1ttte48bcp4zjychjyscicrwi1j34mtt91ptsafdbjmr8g9eo")?;
        let authority = Authority {
            origin: "https://example.org".into(),
            creator,
            network: Network::Regtest,
        };
        let reserve = |r: &Reader| -> Result<()> {
            let mut db = open(&path)?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            bind_to(&tx, &invoice, r, &authority)?;
            tx.commit()?;
            Ok(())
        };
        reserve(&reader)?;
        let reference =
            || -> Result<String> {
                Ok(open(&path)?
                    .query_row("SELECT reference FROM paykit_requests", [], |r| r.get(0))?)
            };
        let first = reference()?;
        reserve(&reader)?;
        assert!(reserve(&other).is_err());
        assert_eq!(reference()?, first);
        Ok(())
    }

    #[tokio::test]
    async fn a_guest_payment_stops_a_queued_proposal_before_remote_creation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("merchant.db");
        let invoice = Store::open(&path)?.create_on_network(
            "order",
            50_000,
            &guest_account(),
            Network::Regtest,
        )?;
        let reader = Reader::parse("pubky3f4khszd4gzg38bsazw88dmcfws36js9ct19s9dikb3a9ht3r84y")?;
        let creator = Reader::parse("pubkytkrq8zmwb8a3m9k15csu3q17qmfgqnp9dskbrg9uq1rydpyxp7qy")?;
        let origin = String::from("http://127.0.0.1:1");
        let authority = Authority {
            origin: origin.clone(),
            creator,
            network: Network::Regtest,
        };
        let mut db = open(&path)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        bind_to(&tx, &invoice, &reader, &authority)?;
        tx.commit()?;
        ledger::record_observation(
            &db,
            &invoice.id,
            "bitcoin",
            Observation {
                observed_sats: 1,
                confirmed_sats: 0,
            },
        )?;
        let upstream = Upstream::new(origin, SigningKey::from_bytes(&[11; 32]))?;
        Paykit {
            authority,
            upstream,
        }
        .sync(&path, 1)
        .await?;
        assert_eq!(
            delivery(&path, &invoice.id)?.as_deref(),
            Some("payment_received")
        );
        Ok(())
    }

    /// The pinned upstream harness supplies a real receiver, signer and SDK payer.
    #[tokio::test]
    #[ignore = "run with python3 tests/paykit-regtest/run.py"]
    async fn real_sdk_reader_pays_checkout() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("checkout.sqlite");
        let invoice = Store::open(&path)?.create_on_network(
            "regtest-order",
            50_000,
            &guest_account(),
            Network::Regtest,
        )?;
        let reader = Reader::parse(&env::var("CHECKOUT_REGTEST_READER")?)?;
        bind(&path, &invoice, &reader)?;
        bind(&path, &invoice, &reader)?;
        let reference: String = open(&path)?.query_row(
            "SELECT reference FROM paykit_requests WHERE invoice_id=?1",
            [&invoice.id],
            |r| r.get(0),
        )?;
        std::fs::write(env::var("CHECKOUT_REGTEST_REFERENCE_FILE")?, reference)?;
        for _ in 0..150 {
            // Reopen the client, key and SQLite file each time. The job must survive restart.
            let client = Paykit::from_env()?.context("regtest Paykit origin missing")?;
            client.sync(&path, 6).await?;
            let db = open(&path)?;
            let status = ledger::status(&db, &invoice.id)?;
            if status.state == "settled" {
                assert_eq!(status.rails.len(), 1);
                assert_eq!(status.rails[0].rail, "paykit");
                assert_eq!(status.rails[0].confirmed_sats, 50_000);
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM payment_events WHERE kind='invoice.settled'",
                        [],
                        |r| r.get::<_, u32>(0)
                    )?,
                    1
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
        bail!("Paykit checkout did not reach six regtest confirmations")
    }
}
