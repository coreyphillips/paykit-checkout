//! Signed receiver-side Marketplace API, pinned in docs/paykit.md.

use std::{collections::BTreeMap, fmt, time::Duration};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::{Client, StatusCode, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use super::Binding;

const PREPARE: &str = "/marketplace/payment-requests/prepare";
const ACTIVATE: &str = "/marketplace/payment-requests/activate";
const STATUS: &str = "/marketplace/payment-requests/status";
const VOID: &str = "/marketplace/payment-requests/void";

#[derive(Clone)]
pub(super) struct Upstream {
    pub origin: String,
    client: Client,
    signer: SigningKey,
}

impl Upstream {
    pub fn origin(base: &str) -> Result<String> {
        let url = crate::config::service_url(base)?;
        ensure!(
            url.path() == "/" && url.query().is_none(),
            "configure a bare Paykit origin"
        );
        Ok(url.origin().ascii_serialization())
    }

    pub fn new(origin: String, signer: SigningKey) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(25))
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            origin,
            client,
            signer,
        })
    }

    pub async fn prepare(&self, binding: &Binding) -> Result<String> {
        let body = json!({
            "accepted_assets": ["BTC"], "amount": binding.amount.to_string(), "asset": "BTC",
            "creator": binding.creator, "operation_id": binding.reference,
            "payment_window_seconds": binding.window, "reader": binding.reader,
            "reference": binding.reference
        });
        let prepared: Acknowledgement = self.post(PREPARE, body).await?;
        prepared.check("prepared", binding.amount)?;
        ensure!(
            canonical_v4(&prepared.invoice_id),
            "invalid prepared Paykit invoice ID"
        );
        Ok(prepared.invoice_id)
    }

    pub async fn activate(&self, binding: &Binding, remote: &str) -> Result<()> {
        let payload = json!({"amount":binding.amount.to_string(),"asset":"BTC","creator":binding.creator,"invoice_id":remote});
        let active: Acknowledgement = self.post(ACTIVATE, payload).await?;
        active.check("active", binding.amount)?;
        ensure!(
            active.invoice_id == remote,
            "Paykit activation returned a different invoice"
        );
        Ok(())
    }

    pub async fn status(&self, binding: &Binding, remote: &str) -> Result<Status> {
        let status: Status = self
            .post(
                STATUS,
                json!({"creator":binding.creator,"invoice_id":remote}),
            )
            .await?;
        ensure!(
            status.invoice_id == remote && status.state == "active",
            "unexpected Paykit invoice status"
        );
        status.options.check(binding.amount)?;
        Ok(status)
    }

    pub async fn void(&self, binding: &Binding, remote: &str) -> Result<()> {
        let result: Value = self
            .post(VOID, json!({"creator":binding.creator,"invoice_id":remote}))
            .await?;
        ensure!(
            result["state"] == "voided" && result["invoice_id"] == remote,
            "unexpected Paykit void response"
        );
        Ok(())
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        let bytes = serde_json_canonicalizer::to_vec(&body)?;
        let signature = signed_header(&self.signer, path, &bytes);
        let mut response = self
            .client
            .post(format!("{}{path}", self.origin))
            .header("X-Paykit-Signature", signature)
            .header("Content-Type", "application/json")
            .body(bytes)
            .send()
            .await
            .context("Paykit request failed")?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                chunk.len() <= 64 * 1024 - body.len(),
                "oversize Paykit response"
            );
            body.extend_from_slice(&chunk);
        }
        if status != StatusCode::OK {
            let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let code = value
                .pointer("/error/code")
                .and_then(Value::as_str)
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 64
                        && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                })
                .unwrap_or("upstream_error")
                .to_owned();
            return Err(RemoteError { status, code }.into());
        }
        serde_json::from_slice(&body).context("invalid Paykit response")
    }
}

fn signed_header(key: &SigningKey, path: &str, bytes: &[u8]) -> String {
    let preimage = [
        b"paykit-http-signature-v1\0POST\0",
        path.as_bytes(),
        b"\0",
        bytes,
    ]
    .concat();
    URL_SAFE_NO_PAD.encode(key.sign(&preimage).to_bytes())
}

fn canonical_v4(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|uuid| {
        uuid.get_version_num() == 4
            && uuid.get_variant() == uuid::Variant::RFC4122
            && uuid.to_string() == id
    })
}

#[derive(Debug)]
pub(super) struct RemoteError {
    pub status: StatusCode,
    pub code: String,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Paykit returned {} ({})", self.status, self.code)
    }
}

impl std::error::Error for RemoteError {}

#[derive(Deserialize)]
struct Options {
    accepted_assets: Vec<String>,
    payment_amounts: BTreeMap<String, String>,
}

impl Options {
    fn check(&self, sats: u64) -> Result<()> {
        ensure!(
            self.accepted_assets == ["BTC"]
                && self.payment_amounts.len() == 1
                && self.payment_amounts.get("BTC") == Some(&sats.to_string()),
            "Paykit response changed the BTC terms"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
struct Acknowledgement {
    invoice_id: String,
    state: String,
    amount: String,
    asset: String,
    #[serde(flatten)]
    options: Options,
}

impl Acknowledgement {
    fn check(&self, state: &str, sats: u64) -> Result<()> {
        ensure!(
            self.state == state && self.asset == "BTC" && self.amount == sats.to_string(),
            "Paykit changed invoice terms or phase"
        );
        self.options.check(sats)
    }
}

#[derive(Deserialize)]
pub(super) struct Status {
    invoice_id: String,
    state: String,
    #[serde(flatten)]
    options: Options,
    pub proposal_delivery_state: String,
    pub request_state: Option<String>,
    pub payment_state: Option<String>,
    pub bitcoin: Option<BitcoinPayment>,
    pub paid_asset: Option<String>,
    payment_deadline: String,
}

impl Status {
    /// Compare at full upstream timestamp precision. Truncation could admit a late payment.
    pub fn deadline_within(&self, expires_at: i64) -> Result<bool> {
        Ok(OffsetDateTime::parse(&self.payment_deadline, &Rfc3339)?
            <= OffsetDateTime::from_unix_timestamp(expires_at)?)
    }
}

#[derive(Deserialize)]
pub(super) struct BitcoinPayment {
    pub txid: String,
    pub observed_sats: u64,
    pub confirmations: u32,
    pub amount_matched: bool,
    pub paid_on_time: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    #[test]
    fn signs_the_upstream_method_path_and_exact_canonical_bytes() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let bytes = serde_json_canonicalizer::to_vec(&json!({"z": 10,"a":"BTC"})).unwrap();
        assert_eq!(bytes, br#"{"a":"BTC","z":10}"#);
        let encoded = signed_header(&key, PREPARE, &bytes);
        let decoded = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        let signature = Signature::from_slice(&decoded).unwrap();
        key.verifying_key().verify(b"paykit-http-signature-v1\0POST\0/marketplace/payment-requests/prepare\0{\"a\":\"BTC\",\"z\":10}", &signature).unwrap();
    }
}
