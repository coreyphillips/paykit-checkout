//! Local, read-only checkout for wallets on Bitcoin regtest.

use std::{net::Ipv4Addr, path::PathBuf};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use bitcoin::{Address, Network, address::NetworkUnchecked};
use qrcode::{QrCode, render::svg};
use tokio::{net::TcpListener, task};

use crate::store::Store;

/// Serve invoices from an existing database on the loopback interface.
pub async fn serve(path: PathBuf, port: u16) -> Result<()> {
    let database = path.clone();
    task::spawn_blocking(move || Store::open_read_only(&database).map(drop))
        .await?
        .context("could not open the invoice database for checkout")?;

    let app = Router::new()
        .route("/checkout/{id}", get(checkout))
        .layer(middleware::map_response(checkout_headers))
        .with_state(path);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    eprintln!("Checkout listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn checkout(State(path): State<PathBuf>, Path(id): Path<String>) -> Response {
    let result = task::spawn_blocking(move || -> Result<Option<String>> {
        let store = Store::open_read_only(&path)?;
        let Some(invoice) = store.get(&id)? else {
            return Ok(None);
        };
        render_payment(invoice.amount_sats, &invoice.address).map(Some)
    })
    .await;

    match result {
        Ok(Ok(Some(page))) => Html(page).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "Invoice not found.").into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "Checkout unavailable.").into_response(),
    }
}

async fn checkout_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
    );
    response
}

fn render_payment(amount_sats: u64, stored_address: &str) -> Result<String> {
    let address = stored_address
        .parse::<Address<NetworkUnchecked>>()?
        .require_network(Network::Regtest)?;
    let btc = format!(
        "{}.{:08}",
        amount_sats / 100_000_000,
        amount_sats % 100_000_000
    );
    let uri = format!("bitcoin:{address}?amount={btc}");
    let qr = QrCode::new(uri.as_bytes())?
        .render::<svg::Color>()
        .min_dimensions(208, 208)
        .build();
    // A standalone SVG document can include an XML declaration; inline HTML only needs the SVG.
    let qr = qr.split_once("?>").map_or(qr.as_str(), |(_, svg)| svg);

    Ok(format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Bitcoin checkout</title>
<style>
:root {{ font: 100%/1.5 system-ui,sans-serif; color: #1c2825; background: #eff3f0 }}
body {{ margin: 0; padding: 2rem 1rem }}
main {{ max-width: 24rem; margin: 0 auto; padding: 1.75rem; border: 1px solid #c6d4cb;
        border-radius: 1rem; background: white }}
h1 {{ font-size: 1.3rem; margin: 0 }} p {{ margin: .2rem 0 }}
.amount {{ font-size: 2rem; font-weight: 700 }}
figure {{ margin: 1rem 0; text-align: center }} svg {{ width: 13rem; max-width: 100%; height: auto }}
code {{ display: block; overflow-wrap: anywhere; padding: .55rem 0; user-select: all }}
a {{ display: block; text-align: center; padding: .65rem 1rem; border-radius: .45rem;
     color: white; background: #155443; font-weight: 650; text-decoration: none }}
</style></head><body><main>
<h1>Bitcoin checkout</h1><p>Regtest</p>
<p class="amount">{amount_sats} sats</p><p>{btc} BTC</p>
<figure aria-label="Regtest Bitcoin payment QR code">{qr}</figure>
<a href="{uri}">Open in wallet</a>
<p>Payment address</p><code>{address}</code>
</main></body></html>"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::ScriptBuf;

    #[tokio::test]
    async fn unknown_invoice_returns_not_found() {
        let database = tempfile::NamedTempFile::new().unwrap();
        Store::open(database.path()).unwrap();

        let response = checkout(
            State(database.path().to_owned()),
            Path("missing".to_owned()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn wallet_link_preserves_single_satoshi_precision() {
        let address = Address::p2wsh(&ScriptBuf::new(), Network::Regtest);
        let page = render_payment(100_000_001, &address.to_string()).unwrap();
        assert!(page.contains(&format!("href=\"bitcoin:{address}?amount=1.00000001\"")));
        assert!(page.contains(&format!("<code>{address}</code>")));
        assert!(page.contains("<svg"));
        assert!(page.contains("Regtest"));
    }

    #[test]
    fn rejects_wrong_network_and_untrusted_address_text() {
        let mainnet_address = Address::p2wsh(&ScriptBuf::new(), Network::Bitcoin);
        assert!(render_payment(1, &mainnet_address.to_string()).is_err());
        assert!(render_payment(1, "\"><script>alert(1)</script>").is_err());
    }
}
