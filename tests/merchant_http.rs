//! Black-box mainnet checkout using a synthetic public account and no live funds.

use anyhow::{Context, Result};
use bitcoin::{NetworkKind, bip32::Xpub};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn mainnet_store_scopes_guest_and_invoice_replay() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("mainnet.sqlite3");
    let admin = "example-merchant-admin-credential-00001";
    let token = "example-store-scoped-credential-0000002";
    fs::write(dir.path().join("admin"), admin)?;
    fs::write(dir.path().join("store"), token)?;
    let mut account:Xpub="tpubDC8msFGeGuwnKG9Upg7DM2b4DaRqg3CUZa5g8v2SRQ6K4NSkxUgd7HsL2XVWbVm39yBA4LAxysQAm397zwQSQoQgewGiYZqrA9DsP4zbQ1M".parse()?;
    account.network = NetworkKind::Main;
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_paykit-checkout"))
            .env_clear()
            .env("CHECKOUT_API_TOKEN_FILE", dir.path().join("admin"))
            .env("CHECKOUT_INVOICE_API_TOKEN_FILE", dir.path().join("store"))
            .env("CHECKOUT_ACCOUNT_XPUB", account.to_string())
            .env(
                "CHECKOUT_PAYKIT_CREATOR",
                "pubkytkrq8zmwb8a3m9k15csu3q17qmfgqnp9dskbrg9uq1rydpyxp7qy",
            )
            .env("CHECKOUT_PAYKIT_NETWORK", "bitcoin")
            .env("CHECKOUT_PAYKIT_URL", "http://127.0.0.1:1")
            .args(["--network", "mainnet", "--database"])
            .arg(&db)
            .args(["serve", "--port", "0"])
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut startup = String::new();
    BufReader::new(server.0.stderr.take().context("web process stderr")?)
        .read_line(&mut startup)?;
    let origin = startup
        .trim()
        .strip_prefix("Checkout listening on ")
        .context("server startup failed")?;
    let api = format!("{origin}/v1/invoices");
    let http = Client::builder().no_proxy().build()?;
    let order = json!({"order_id":"private-reference","amount_sats":50000});
    assert_eq!(
        http.post(&api).json(&order).send().await?.status(),
        StatusCode::UNAUTHORIZED
    );
    let create = || http.post(&api).bearer_auth(token).json(&order);
    let issued: Value = create().send().await?.error_for_status()?.json().await?;
    let duplicate: Value = create().send().await?.error_for_status()?.json().await?;
    assert_eq!(duplicate, issued);
    assert_eq!(issued["network"], "bitcoin");
    let address = issued["address"].as_str().unwrap();
    assert!(address.starts_with("bc1q"));
    let page = http
        .get(format!(
            "{origin}{}",
            issued["checkout_path"].as_str().unwrap()
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(page.contains("Bitcoin mainnet") && page.contains(address));
    assert!(!page.contains("private-reference"));
    let buyer_url = format!("{origin}{}", issued["checkout_path"].as_str().unwrap());
    let reader = "pubky3f4khszd4gzg38bsazw88dmcfws36js9ct19s9dikb3a9ht3r84y";
    let submit = |from: &str, key: &str| {
        http.post(&buyer_url)
            .header("origin", from)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("reader={key}"))
    };
    assert!(page.contains("Request in wallet"));
    assert_eq!(
        submit("https://another.example", reader)
            .send()
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    let selected = submit(origin, reader)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(selected.contains("queued") && !selected.contains(reader));
    let other = "pubky7ir1ttte48bcp4zjychjyscicrwi1j34mtt91ptsafdbjmr8g9eo";
    assert_eq!(
        submit(origin, other).send().await?.status(),
        StatusCode::CONFLICT
    );
    let resource = format!("{api}/{}", issued["invoice_id"].as_str().unwrap());
    assert_eq!(
        http.get(&resource)
            .bearer_auth(token)
            .send()
            .await?
            .json::<Value>()
            .await?,
        issued
    );
    assert_eq!(
        http.post(format!("{resource}/paykit"))
            .bearer_auth(token)
            .json(&json!({"reader":"invalid"}))
            .send()
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(format!("{origin}/merchant"))
            .bearer_auth(token)
            .send()
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let dashboard = http
        .get(format!("{origin}/merchant"))
        .basic_auth("merchant", Some(admin))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(dashboard.contains("private-reference"));
    assert_eq!(
        http.post(&api)
            .bearer_auth(token)
            .json(&json!({"order_id":"private-reference","amount_sats":7}))
            .send()
            .await?
            .status(),
        StatusCode::CONFLICT
    );
    Ok(())
}
