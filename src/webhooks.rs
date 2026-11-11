//! Ordered at-least-once delivery. Merchants deduplicate using the stable event ID.

use crate::{config, ledger};
use anyhow::{Context, Result, ensure};
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::Sha256;
use std::{path::Path, time::Duration};

pub struct Webhook {
    endpoint: Url,
    secret: String,
    client: Client,
}

impl Webhook {
    pub fn from_env() -> Result<Option<Self>> {
        let url = config::optional("CHECKOUT_WEBHOOK_URL")?;
        let file = config::optional("CHECKOUT_WEBHOOK_SECRET_FILE")?;
        if url.is_none() && file.is_none() {
            return Ok(None);
        }
        let endpoint = config::service_url(&url.context("missing CHECKOUT_WEBHOOK_URL")?)?;
        let secret = config::secret(Path::new(
            &file.context("missing CHECKOUT_WEBHOOK_SECRET_FILE")?,
        ))?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Some(Self {
            endpoint,
            secret,
            client,
        }))
    }

    pub async fn deliver(&self, path: &Path) -> Result<()> {
        let db = Connection::open(path)?;
        // A failed event blocks later events for that invoice only.
        for _ in 0..50 {
            let event = db
                .query_row(
                    "SELECT seq,event_id,payload,attempts FROM payment_events e
                 WHERE delivered=0 AND next_attempt<=unixepoch()
                 AND NOT EXISTS(SELECT 1 FROM payment_events p WHERE p.invoice_id=e.invoice_id
                     AND p.delivered=0 AND p.seq<e.seq) ORDER BY seq LIMIT 1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, u32>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((seq, id, body, attempts)) = event else {
                break;
            };
            let timestamp = ledger::now().to_string();
            let signature = sign(&self.secret, &timestamp, &body)?;
            let success = self
                .client
                .post(self.endpoint.clone())
                .header("content-type", "application/json")
                .header("x-checkout-event", id)
                .header("x-checkout-timestamp", timestamp)
                .header("x-checkout-signature", signature)
                .body(body)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            let delay = (5_i64 << attempts.min(10)).min(3600);
            db.execute(
                "UPDATE payment_events SET delivered=?1,attempts=attempts+1,
                 next_attempt=unixepoch()+?2 WHERE seq=?3",
                params![success, delay, seq],
            )?;
            if !success {
                eprintln!("Webhook delivery {seq} failed. Retrying in {delay}s.");
            }
        }
        Ok(())
    }
}

fn sign(secret: &str, timestamp: &str, body: &str) -> Result<String> {
    ensure!(!secret.is_empty(), "empty webhook key");
    let mut hmac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())?;
    hmac.update(timestamp.as_bytes());
    hmac.update(b".");
    hmac.update(body.as_bytes());
    Ok(format!("v1={}", hex::encode(hmac.finalize().into_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn failed_delivery_retries_same_signed_event_before_its_successor() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("outbox.sqlite");
        let db = Connection::open(&path)?;
        ledger::init(&db)?;
        for id in ["event-a", "event-b"] {
            db.execute("INSERT INTO payment_events(event_id,invoice_id,kind,payload) VALUES (?,'order','invoice.updated',?)",[id,id])?;
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let url = Url::parse(&format!("http://{}/events", listener.local_addr()?))?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        let app = Router::new().route(
            "/events",
            post(move |headers: HeaderMap, body: String| {
                let sink = sink.clone();
                async move {
                    let timestamp = headers["x-checkout-timestamp"].to_str().unwrap();
                    assert_eq!(
                        headers["x-checkout-signature"],
                        sign("test-secret", timestamp, &body).unwrap()
                    );
                    assert_eq!(headers["x-checkout-event"], body.as_str());
                    let mut calls = sink.lock().unwrap();
                    calls.push(body);
                    if calls.len() == 1 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let hook = Webhook {
            endpoint: url,
            secret: "test-secret".into(),
            client: Client::new(),
        };
        hook.deliver(&path).await?;
        assert_eq!(*received.lock().unwrap(), ["event-a"]);
        db.execute("UPDATE payment_events SET next_attempt=0", [])?;
        hook.deliver(&path).await?;
        hook.deliver(&path).await?;
        assert_eq!(*received.lock().unwrap(), ["event-a", "event-a", "event-b"]);
        server.abort();
        Ok(())
    }
}
