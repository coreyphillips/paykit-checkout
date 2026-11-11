//! Settlement facts and the transactional webhook outbox for one merchant.

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default)]
pub struct Observation {
    pub observed_sats: u64,
    pub confirmed_sats: u64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Rail {
    pub rail: String,
    pub observed_sats: u64,
    pub confirmed_sats: u64,
    pub full_seen_at: Option<i64>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Status {
    pub state: &'static str,
    pub rails: Vec<Rail>,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn init(db: &Connection) -> Result<()> {
    crate::identity::init(db)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS rail_observations (
             invoice_id TEXT NOT NULL, rail TEXT NOT NULL, observed_sats INTEGER NOT NULL,
             confirmed_sats INTEGER NOT NULL, full_seen_at INTEGER, PRIMARY KEY(invoice_id, rail)
         );
         CREATE TABLE IF NOT EXISTS payment_events (
             seq INTEGER PRIMARY KEY, event_id TEXT NOT NULL UNIQUE, invoice_id TEXT NOT NULL,
             kind TEXT NOT NULL, payload TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
             next_attempt INTEGER NOT NULL DEFAULT 0, delivered INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS pending_delivery ON payment_events(delivered,next_attempt);
         CREATE INDEX IF NOT EXISTS invoice_event ON payment_events(invoice_id,seq);
",
    )?;
    Ok(())
}

pub fn status(db: &Connection, invoice_id: &str) -> Result<Status> {
    let (amount, expiry): (u64, i64) = db.query_row(
        "SELECT amount_sats,expires_at FROM invoices WHERE id=?1",
        [invoice_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut query = db.prepare(
        "SELECT rail,observed_sats,confirmed_sats,full_seen_at
        FROM rail_observations WHERE invoice_id=?1 AND observed_sats>0 ORDER BY rail",
    )?;
    let rails = query
        .query_map([invoice_id], |r| {
            Ok(Rail {
                rail: r.get(0)?,
                observed_sats: r.get(1)?,
                confirmed_sats: r.get(2)?,
                full_seen_at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let full = rails
        .iter()
        .filter(|r| r.observed_sats >= amount)
        .collect::<Vec<_>>();
    let state = match full.as_slice() {
        [] if !rails.is_empty() => "partial",
        [] if now() >= expiry => "expired",
        [] => "unpaid",
        [rail] if rail.full_seen_at.is_none_or(|t| t > expiry) => "late",
        [rail] if rail.confirmed_sats < amount => "confirming",
        [rail] if rail.confirmed_sats > amount => "overpaid",
        [_] => "settled",
        _ => "multiple_payments",
    };
    Ok(Status { state, rails })
}

#[cfg(test)]
pub fn record_observation(
    db: &Connection,
    id: &str,
    rail: &str,
    observation: Observation,
) -> Result<()> {
    record_observation_at(db, id, rail, observation, now())
}

/// A backend with authenticated settlement time can recover an on-time payment after downtime.
pub fn record_observation_at(
    db: &Connection,
    id: &str,
    rail: &str,
    o: Observation,
    seen_at: i64,
) -> Result<()> {
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    record_authoritative_observation_in(&tx, id, rail, o, Some(seen_at))?;
    tx.commit()?;
    Ok(())
}

pub fn record_chain_observation_in(
    tx: &Transaction<'_>,
    id: &str,
    o: Observation,
    full_seen_at: Option<i64>,
) -> Result<()> {
    record_authoritative_observation_in(tx, id, "bitcoin", o, full_seen_at)
}

pub fn record_authoritative_observation_in(
    tx: &Transaction<'_>,
    id: &str,
    rail: &str,
    o: Observation,
    seen_at: Option<i64>,
) -> Result<()> {
    ensure!(
        matches!(rail, "bitcoin" | "lightning" | "paykit"),
        "unknown payment rail"
    );
    ensure!(
        o.confirmed_sats <= o.observed_sats && seen_at.is_none_or(|t| t > 0),
        "invalid settlement observation"
    );
    let before = status(tx, id)?;
    let (order, amount, network): (String, u64, String) = tx.query_row(
        "SELECT order_id,amount_sats,network FROM invoices WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let full_seen_at = seen_at.filter(|_| o.observed_sats >= amount);
    tx.execute(
        "INSERT INTO rail_observations(invoice_id,rail,observed_sats,confirmed_sats,full_seen_at)
         VALUES (?1,?2,?3,?4,?5) ON CONFLICT(invoice_id,rail) DO UPDATE SET
         observed_sats=excluded.observed_sats, confirmed_sats=excluded.confirmed_sats,
         full_seen_at=excluded.full_seen_at",
        params![id, rail, o.observed_sats, o.confirmed_sats, full_seen_at],
    )?;
    let after = status(tx, id)?;
    if before.state != after.state {
        let previous_settlement = tx.query_row(
            "SELECT 1 FROM payment_events WHERE invoice_id=?1 AND kind='invoice.settled' LIMIT 1",
            [id], |_| Ok(()),
        ).optional()?.is_some();
        let kind = match (before.state, after.state, previous_settlement) {
            (_, "settled", false) => "invoice.settled",
            (_, "settled", true) => "invoice.restored",
            ("settled", _, _) => "invoice.settlement_changed",
            _ => "invoice.updated",
        };
        let event_id = uuid::Uuid::new_v4().to_string();
        let payload = serde_json::to_string(&serde_json::json!({
            "event_id":event_id,"type":kind,"invoice_id":id,"order_id":order,
            "amount_sats":amount,"network":network,"state":after.state,"rails":after.rails
        }))?;
        tx.execute(
            "INSERT INTO payment_events(event_id,invoice_id,kind,payload) VALUES (?,?,?,?)",
            params![event_id, id, kind, payload],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_and_second_rail_never_emit_a_second_fulfillment() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE invoices(id TEXT,order_id TEXT,amount_sats INTEGER,network TEXT,expires_at INTEGER);
            INSERT INTO invoices VALUES ('id','order',100,'regtest',9999999999)")?;
        init(&db)?;
        let settled = Observation {
            observed_sats: 100,
            confirmed_sats: 100,
        };
        for observation in [settled, settled, Observation::default(), settled] {
            record_observation(&db, "id", "bitcoin", observation)?;
        }
        record_observation(&db, "id", "lightning", settled)?;
        assert_eq!(status(&db, "id")?.state, "multiple_payments");
        let kinds = db
            .prepare("SELECT kind FROM payment_events ORDER BY seq")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        assert_eq!(
            kinds,
            [
                "invoice.settled",
                "invoice.settlement_changed",
                "invoice.restored",
                "invoice.settlement_changed"
            ]
        );
        Ok(())
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn dust_on_another_rail_keeps_an_exact_payment_eligible_and_late_replacement_is_reviewed()
    -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE invoices(id TEXT, order_id TEXT, amount_sats INTEGER, network TEXT, expires_at INTEGER);
            INSERT INTO invoices VALUES ('id','shop-1',1000,'bitcoin',2000)")?;
        init(&db)?;
        record_observation_at(
            &db,
            "id",
            "bitcoin",
            Observation {
                observed_sats: 2,
                confirmed_sats: 2,
            },
            1500,
        )?;
        let full = Observation {
            observed_sats: 1000,
            confirmed_sats: 1000,
        };
        record_observation_at(&db, "id", "paykit", full, 1600)?;
        assert_eq!(status(&db, "id")?.state, "settled");
        record_observation_at(&db, "id", "paykit", full, 2001)?;
        assert_eq!(status(&db, "id")?.state, "late");
        Ok(())
    }
}
