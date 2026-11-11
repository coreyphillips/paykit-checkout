//! Receive payments through a separately operated LND node.
use std::{
    env, fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use bitcoin::{
    Network,
    hashes::{Hash, HashEngine, Hmac, HmacEngine, sha256},
};
use lightning_invoice::{Bolt11Invoice, Currency};
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderValue},
    redirect::Policy,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;

#[derive(Clone)]
pub struct Lnd {
    client: Client,
    url: Url,
    invoice_secret: [u8; 32],
}

pub struct Observation {
    pub settled_msat: Option<u64>,
    pub settled_at: Option<i64>,
}

pub fn init(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS lightning_invoices (
            invoice_id TEXT PRIMARY KEY NOT NULL REFERENCES invoices(id),
            payment_hash TEXT NOT NULL UNIQUE,
            node_pubkey TEXT NOT NULL,
            amount_sats INTEGER NOT NULL,
            network TEXT NOT NULL,
            deadline INTEGER NOT NULL,
            bolt11 TEXT,
            bolt11_expires_at INTEGER,
            state TEXT NOT NULL DEFAULT 'PENDING'
        )",
    )?;
    Ok(())
}

pub fn current_bolt11(db: &Connection, id: &str, now: i64) -> Result<Option<String>> {
    let row = db.query_row(
        "SELECT bolt11, amount_sats, network, node_pubkey FROM lightning_invoices WHERE invoice_id = ?1 AND state = 'OPEN'
         AND deadline > ?2 AND bolt11_expires_at > ?2 AND bolt11 IS NOT NULL",
        params![id, now], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
    ).optional()?;
    let Some((bolt11, amount, network, node)) = row else {
        return Ok(None);
    };
    let network = match network.as_str() {
        "mainnet" => Network::Bitcoin,
        "regtest" => Network::Regtest,
        _ => bail!("unsupported Lightning network in invoice database"),
    };
    verify_payment_request(&bolt11, amount, network, &node, now)?;
    Ok(Some(bolt11))
}

fn db(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

struct Reservation {
    hash: String,
    node: String,
    amount: u64,
    network: String,
    deadline: i64,
    issued: bool,
}

fn reservation(path: &Path, id: &str) -> Result<Option<Reservation>> {
    Ok(db(path)?
        .query_row(
            "SELECT payment_hash, node_pubkey, amount_sats, network, deadline, bolt11 IS NOT NULL
         FROM lightning_invoices WHERE invoice_id = ?1",
            [id],
            |row| {
                Ok(Reservation {
                    hash: row.get(0)?,
                    node: row.get(1)?,
                    amount: row.get(2)?,
                    network: row.get(3)?,
                    deadline: row.get(4)?,
                    issued: row.get(5)?,
                })
            },
        )
        .optional()?)
}

fn unix_now() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?)
}

fn chain(network: Network) -> Result<(&'static str, Currency)> {
    match network {
        Network::Bitcoin => Ok(("mainnet", Currency::Bitcoin)),
        Network::Regtest => Ok(("regtest", Currency::Regtest)),
        _ => bail!("LND checkout supports mainnet and regtest"),
    }
}

impl Lnd {
    /// Load an optional receiver. A partial configuration is an error.
    pub fn from_env() -> Result<Option<Self>> {
        let url = env::var_os("PAYKIT_LND_URL");
        let cert = env::var_os("PAYKIT_LND_TLS_CERT");
        let macaroon = env::var_os("PAYKIT_LND_MACAROON");
        let secret = env::var_os("PAYKIT_LND_INVOICE_SECRET");
        match (url, cert, macaroon, secret) {
            (None, None, None, None) => Ok(None),
            (Some(url), Some(cert), Some(macaroon), Some(secret)) => Ok(Some(Self::new(
                url.to_str().context("invalid LND URL")?,
                Path::new(&cert),
                Path::new(&macaroon),
                Path::new(&secret),
            )?)),
            _ => bail!(
                "set PAYKIT_LND_URL, PAYKIT_LND_TLS_CERT, PAYKIT_LND_MACAROON and PAYKIT_LND_INVOICE_SECRET together"
            ),
        }
    }

    pub fn new(url: &str, tls_cert: &Path, macaroon: &Path, invoice_secret: &Path) -> Result<Self> {
        let url = Url::parse(url).context("invalid LND REST URL")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "LND URL must be an HTTPS origin without credentials"
        );
        let certificate = reqwest::Certificate::from_pem(
            &fs::read(tls_cert).context("read LND TLS certificate")?,
        )?;
        let raw = fs::read(macaroon).context("read receive macaroon")?;
        ensure!(!raw.is_empty(), "empty receive macaroon");
        let mut token = HeaderValue::from_str(&hex::encode(raw))?;
        token.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("grpc-metadata-macaroon", token);
        let client = Client::builder()
            .use_native_tls()
            .https_only(true)
            .no_proxy()
            .redirect(Policy::none())
            .tls_built_in_root_certs(false)
            .add_root_certificate(certificate)
            .timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()?;
        let invoice_secret = fs::read(invoice_secret)
            .context("read invoice secret")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invoice secret must contain exactly 32 binary bytes"))?;
        Ok(Self {
            client,
            url,
            invoice_secret,
        })
    }

    fn preimage(&self, id: &str, network: &str) -> [u8; 32] {
        let mut hmac = HmacEngine::<sha256::Hash>::new(&self.invoice_secret);
        hmac.input(b"paykit-checkout/lnd/preimage/v1\0");
        hmac.input(network.as_bytes());
        hmac.input(&[0]);
        hmac.input(id.as_bytes());
        Hmac::<sha256::Hash>::from_engine(hmac).to_byte_array()
    }

    async fn node(&self, network: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Chain {
            chain: String,
            network: String,
        }
        #[derive(Deserialize)]
        struct Info {
            identity_pubkey: String,
            chains: Vec<Chain>,
        }
        let info: Info = self
            .client
            .get(self.url.join("v1/getinfo")?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            info.chains
                .iter()
                .any(|c| c.chain == "bitcoin" && c.network == network),
            "LND runs on a different network"
        );
        let node = info
            .identity_pubkey
            .parse::<bitcoin::secp256k1::PublicKey>()
            .context("invalid LND node identity")?;
        Ok(node.to_string())
    }

    /// Reserve a hash before calling LND. A retry uses the same preimage even
    /// if the first request succeeded while its response was lost.
    pub async fn start(
        &self,
        database: &Path,
        invoice_id: &str,
        amount_sats: u64,
        network: Network,
        expires_at: i64,
    ) -> Result<Observation> {
        let network_name = chain(network)?.0;
        let node = self.node(network_name).await?;
        let preimage = self.preimage(invoice_id, network_name);
        let hash = sha256::Hash::hash(&preimage).to_string();
        db(database)?.execute(
            "INSERT INTO lightning_invoices(invoice_id, payment_hash, node_pubkey, amount_sats, network, deadline)
             VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(invoice_id) DO NOTHING",
            params![invoice_id, hash, node, amount_sats, network_name, expires_at],
        )?;
        let saved = reservation(database, invoice_id)?.context("missing Lightning reservation")?;
        ensure!(
            saved.node == node
                && saved.hash == hash
                && saved.amount == amount_sats
                && saved.network == network_name
                && saved.deadline == expires_at,
            "Lightning receiver, secret or invoice terms differ from the original"
        );
        if !saved.issued {
            let remaining = expires_at.saturating_sub(unix_now()?);
            if remaining > 5 {
                // Reserve five seconds for transit. Never serve a BOLT11 whose
                // signed expiry exceeds the merchant's invoice deadline.
                let request = serde_json::json!({"r_preimage": STANDARD.encode(preimage),
                    "value": amount_sats.to_string(), "expiry": (remaining - 5).to_string()});
                let url = self.url.join("v1/invoices")?;
                let add = self
                    .client
                    .post(url)
                    .json(&request)
                    .send()
                    .await
                    .and_then(|response| response.error_for_status());
                // A duplicate or ambiguous response is recoverable through LookupInvoice.
                if let Err(error) = add {
                    return self
                        .observe(database, invoice_id, &saved, network)
                        .await?
                        .context("LND invoice was not found after AddInvoice")
                        .context(format!(
                            "LND AddInvoice did not return success ({error}); recovering by hash"
                        ));
                }
            }
        }
        self.observe(database, invoice_id, &saved, network)
            .await?
            .context("LND invoice was not found after issuance")
    }

    pub async fn lookup(
        &self,
        database: &Path,
        invoice_id: &str,
        amount_sats: u64,
        network: Network,
    ) -> Result<Option<Observation>> {
        let Some(saved) = reservation(database, invoice_id)? else {
            return Ok(None);
        };
        let network_name = chain(network)?.0;
        ensure!(
            saved.amount == amount_sats && saved.network == network_name,
            "Lightning invoice terms changed"
        );
        ensure!(
            self.node(network_name).await? == saved.node,
            "configured LND node changed"
        );
        self.observe(database, invoice_id, &saved, network).await
    }

    async fn observe(
        &self,
        database: &Path,
        id: &str,
        saved: &Reservation,
        network: Network,
    ) -> Result<Option<Observation>> {
        let url = self.url.join(&format!("v1/invoice/{}", saved.hash))?;
        let response = self.client.get(url).send().await?;
        // The worker reissues a pending reservation if the process stopped
        // after saving its hash but before LND received AddInvoice.
        if !saved.issued && response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let remote: RemoteInvoice = response.error_for_status()?.json().await?;
        let (expires, observation) = remote.check(saved, network)?;
        let changed = db(database)?.execute(
            "UPDATE lightning_invoices SET bolt11 = ?2, bolt11_expires_at = ?3,
             state = CASE WHEN state IN ('CANCELED','SETTLED') THEN state ELSE ?4 END
             WHERE invoice_id = ?1 AND (bolt11 IS NULL OR bolt11 = ?2)",
            params![id, remote.payment_request, expires, remote.state],
        )?;
        ensure!(changed == 1, "LND changed a previously issued invoice");
        Ok(Some(observation))
    }
}

#[derive(Deserialize)]
struct RemoteInvoice {
    r_hash: String,
    payment_request: String,
    value_msat: String,
    state: String,
    #[serde(default)]
    amt_paid_msat: String,
    #[serde(default)]
    settle_date: String,
}

fn signed_terms(bolt11: &str, amount: u64, network: Network, node: &str) -> Result<Bolt11Invoice> {
    let invoice: Bolt11Invoice = bolt11.parse().context("invalid signed BOLT11")?;
    ensure!(
        invoice.currency() == chain(network)?.1,
        "BOLT11 network mismatch"
    );
    let msats = amount
        .checked_mul(1_000)
        .context("invoice amount overflow")?;
    ensure!(
        amount > 0 && invoice.amount_milli_satoshis() == Some(msats),
        "BOLT11 amount mismatch"
    );
    ensure!(
        invoice.recover_payee_pub_key().to_string() == node,
        "unexpected BOLT11 signing node"
    );
    Ok(invoice)
}

impl RemoteInvoice {
    fn check(&self, saved: &Reservation, network: Network) -> Result<(i64, Observation)> {
        let bolt11 = signed_terms(&self.payment_request, saved.amount, network, &saved.node)?;
        ensure!(
            hex::encode(STANDARD.decode(&self.r_hash)?) == saved.hash
                && bolt11.payment_hash().to_string() == saved.hash,
            "LND payment hash mismatch"
        );
        ensure!(
            Some(self.value_msat.parse::<u64>()?) == bolt11.amount_milli_satoshis(),
            "LND invoice value mismatch"
        );
        let expires = i64::try_from(
            bolt11
                .expires_at()
                .context("BOLT11 expiry overflow")?
                .as_secs(),
        )?;
        ensure!(
            expires <= saved.deadline,
            "BOLT11 outlives merchant invoice"
        );
        let (settled_msat, settled_at) = match self.state.as_str() {
            "SETTLED" => {
                let paid = self
                    .amt_paid_msat
                    .parse::<u64>()
                    .context("LND settlement amount missing")?;
                let at = self
                    .settle_date
                    .parse::<i64>()
                    .context("LND settlement time missing")?;
                ensure!(paid > 0 && at > 0, "invalid LND settlement");
                (Some(paid), Some(at))
            }
            "OPEN" | "ACCEPTED" | "CANCELED" => (None, None),
            _ => bail!("unrecognized LND invoice state"),
        };
        Ok((
            expires,
            Observation {
                settled_msat,
                settled_at,
            },
        ))
    }
}

/// Independently verify a wallet's payment request using a trusted merchant node key.
pub fn verify_payment_request(
    bolt11: &str,
    amount_sats: u64,
    network: Network,
    expected_node_pubkey: &str,
    now: i64,
) -> Result<()> {
    let invoice = signed_terms(bolt11, amount_sats, network, expected_node_pubkey)?;
    let now = u64::try_from(now)?;
    ensure!(
        invoice.duration_since_epoch().as_secs() <= now
            && now
                < invoice
                    .expires_at()
                    .context("BOLT11 expiry overflow")?
                    .as_secs(),
        "BOLT11 is not currently payable"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
    use lightning_invoice::{InvoiceBuilder, PaymentSecret};

    fn sample() -> (RemoteInvoice, Reservation) {
        let secp = Secp256k1::new();
        let signer = SecretKey::from_slice(&[9; 32]).unwrap();
        let hash = sha256::Hash::hash(b"test preimage");
        let bolt11 = InvoiceBuilder::new(Currency::Regtest)
            .description("Test checkout".to_owned())
            .amount_milli_satoshis(501_000)
            .payment_hash(hash)
            .payment_secret(PaymentSecret([42; 32]))
            .duration_since_epoch(Duration::from_secs(1000))
            .expiry_time(Duration::from_secs(60))
            .min_final_cltv_expiry_delta(80)
            .build_signed(|msg| secp.sign_ecdsa_recoverable(msg, &signer))
            .unwrap()
            .to_string();
        (
            RemoteInvoice {
                r_hash: STANDARD.encode(hash.to_byte_array()),
                payment_request: bolt11,
                value_msat: "501000".into(),
                state: "SETTLED".into(),
                amt_paid_msat: "501019".into(),
                settle_date: "1050".into(),
            },
            Reservation {
                hash: hash.to_string(),
                node: PublicKey::from_secret_key(&secp, &signer).to_string(),
                amount: 501,
                network: "regtest".into(),
                deadline: 1060,
                issued: false,
            },
        )
    }

    #[test]
    fn reads_actual_millisatoshis_only_from_settled_invoices() {
        let (mut remote, saved) = sample();
        assert_eq!(
            remote
                .check(&saved, Network::Regtest)
                .unwrap()
                .1
                .settled_msat,
            Some(501019)
        );
        for state in ["OPEN", "ACCEPTED", "CANCELED"] {
            remote.state = state.into();
            assert!(
                remote
                    .check(&saved, Network::Regtest)
                    .unwrap()
                    .1
                    .settled_msat
                    .is_none()
            );
        }
        remote.state = "unknown".into();
        assert!(remote.check(&saved, Network::Regtest).is_err());
    }

    #[test]
    fn cross_checks_signed_payment_terms_and_node_response() {
        let (remote, mut saved) = sample();
        let check = |price, net, node: &str, now| {
            verify_payment_request(&remote.payment_request, price, net, node, now)
        };
        assert!(check(501, Network::Regtest, &saved.node, 1000).is_ok());
        assert!(check(500, Network::Regtest, &saved.node, 1000).is_err());
        assert!(check(501, Network::Bitcoin, &saved.node, 1000).is_err());
        assert!(check(501, Network::Regtest, "different node", 1000).is_err());
        assert!(check(501, Network::Regtest, &saved.node, 1060).is_err());
        assert!(check(501, Network::Regtest, &saved.node, 999).is_err());
        saved.deadline = 1059;
        assert!(remote.check(&saved, Network::Regtest).is_err());
        saved.deadline = 1060;
        saved.hash = sha256::Hash::hash(b"unrelated invoice").to_string();
        assert!(remote.check(&saved, Network::Regtest).is_err());
    }
}
