//! An offline merchant signs an invoice using an independently held Pubky identity.
//! Buyers verify the signed terms against a merchant public key they already trust.

use crate::{
    ledger,
    store::{Invoice, derive_address},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitcoin::{Address, Network};
use data_encoding::Specification;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Terms {
    pub invoice_id: String,
    pub network: String,
    pub amount_sats: u64,
    pub address: String,
    pub expires_at: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    pub merchant: String,
    pub terms: Terms,
    pub signature: String,
}

pub fn init(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS attestations(invoice_id TEXT PRIMARY KEY, proof TEXT NOT NULL)",
    )?;
    Ok(())
}

impl Terms {
    fn message(&self) -> Result<Vec<u8>> {
        let mut message = b"paykit-checkout/onchain-invoice/v1\0".to_vec();
        message.extend(serde_json::to_vec(self)?);
        Ok(message)
    }

    pub fn for_invoice(invoice: &Invoice) -> Self {
        Self {
            invoice_id: invoice.id.clone(),
            network: invoice.network.to_string(),
            amount_sats: invoice.amount_sats,
            address: invoice.address.clone(),
            expires_at: invoice.expires_at,
        }
    }
}

fn pubky_encoding() -> Result<data_encoding::Encoding> {
    let mut spec = Specification::new();
    spec.symbols.push_str("ybndrfg8ejkmcpqxot1uwisza345h769");
    Ok(spec.encoding()?)
}

pub fn sign(terms: Terms, account: &str, index: u32, key_file: &Path) -> Result<Attestation> {
    let network = super::config::parse_network(&terms.network)?;
    let derived = derive_address(account, network, index)?;
    ensure!(
        terms.address == derived.to_string(),
        "invoice destination differs from the independently derived merchant address"
    );
    validate_terms(&terms, network)?;
    let key_bytes = URL_SAFE_NO_PAD
        .decode(fs::read_to_string(key_file)?.trim())
        .context("invalid signing key file")?;
    let seed = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected 32 signing key bytes"))?;
    let key = SigningKey::from_bytes(&seed);
    let merchant = format!(
        "pubky{}",
        pubky_encoding()?.encode(key.verifying_key().as_bytes())
    );
    let signature = URL_SAFE_NO_PAD.encode(key.sign(&terms.message()?).to_bytes());
    Ok(Attestation {
        merchant,
        terms,
        signature,
    })
}

fn validate_terms(terms: &Terms, network: Network) -> Result<()> {
    ensure!(
        terms.invoice_id.len() == 32 && terms.invoice_id.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid invoice ID"
    );
    ensure!(
        terms.network == network.to_string()
            && terms.amount_sats > 0
            && terms.amount_sats <= bitcoin::Amount::MAX_MONEY.to_sat(),
        "invalid invoice network or amount"
    );
    ensure!(terms.expires_at > ledger::now(), "invoice has expired");
    terms
        .address
        .parse::<Address<bitcoin::address::NetworkUnchecked>>()?
        .require_network(network)?;
    Ok(())
}

impl Attestation {
    /// The pin comes from the buyer's trusted merchant contact, outside the checkout response.
    pub fn verify(&self, pin: &str, network: Network, id: &str, amount_sats: u64) -> Result<()> {
        ensure!(
            self.merchant == pin
                && self.terms.invoice_id == id
                && self.terms.amount_sats == amount_sats,
            "merchant, order, or amount mismatch"
        );
        validate_terms(&self.terms, network)?;
        let raw = pubky_encoding()?.decode(
            pin.strip_prefix("pubky")
                .context("expected a Pubky public key")?
                .as_bytes(),
        )?;
        let pk = VerifyingKey::from_bytes(
            &raw.try_into()
                .map_err(|_| anyhow::anyhow!("invalid merchant key length"))?,
        )?;
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(&self.signature)?)?;
        pk.verify_strict(&self.terms.message()?, &signature)?;
        Ok(())
    }
}

pub fn attach(db: &Connection, invoice: &Invoice, pin: &str, proof: &Attestation) -> Result<()> {
    proof.verify(pin, invoice.network, &invoice.id, invoice.amount_sats)?;
    ensure!(
        proof.terms == Terms::for_invoice(invoice),
        "signed terms differ from the stored invoice"
    );
    db.execute("INSERT INTO attestations(invoice_id,proof) VALUES (?,?) ON CONFLICT(invoice_id) DO UPDATE SET proof=excluded.proof",
        params![invoice.id, serde_json::to_string(proof)?])?;
    Ok(())
}

pub fn get(db: &Connection, id: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT proof FROM attestations WHERE invoice_id=?",
            [id],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_independently_derived_and_pinned_terms_verify() -> Result<()> {
        let account = "tpubDC8msFGeGuwnKG9Upg7DM2b4DaRqg3CUZa5g8v2SRQ6K4NSkxUgd7HsL2XVWbVm39yBA4LAxysQAm397zwQSQoQgewGiYZqrA9DsP4zbQ1M";
        let dir = tempfile::tempdir()?;
        let key = dir.path().join("signer");
        fs::write(&key, URL_SAFE_NO_PAD.encode([33u8; 32]))?;
        let id = "1234567890abcdef1234567890abcdef";
        let terms = || Terms {
            invoice_id: id.into(),
            network: "regtest".into(),
            amount_sats: 20000,
            address: derive_address(account, Network::Regtest, 7)
                .unwrap()
                .to_string(),
            expires_at: ledger::now() + 3600,
        };
        assert!(sign(terms(), account, 0, &key).is_err());
        let mut proof = sign(terms(), account, 7, &key)?;
        proof.verify(&proof.merchant, Network::Regtest, id, 20000)?;
        assert!(
            proof
                .verify("pubkyunknown", Network::Regtest, id, 20000)
                .is_err()
        );
        assert!(
            proof
                .verify(&proof.merchant, Network::Bitcoin, id, 20000)
                .is_err()
        );
        assert!(
            proof
                .verify(&proof.merchant, Network::Regtest, id, 20001)
                .is_err()
        );
        proof.terms.address = derive_address(account, Network::Regtest, 2)?.to_string();
        assert!(
            proof
                .verify(&proof.merchant, Network::Regtest, id, 20000)
                .is_err()
        );
        Ok(())
    }
}
