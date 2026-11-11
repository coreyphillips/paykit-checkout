//! Guest checkout pages and independently verifiable merchant instructions.

use std::{net::Ipv4Addr, path::PathBuf};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Form, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use bitcoin::{Address, Network, address::NetworkUnchecked};
use qrcode::{QrCode, render::svg};
use tokio::{net::TcpListener, task};

use crate::{identity, ledger, lightning, merchant, paykit, store::Store};

/// Serve invoices from an existing database on the loopback interface.
pub async fn serve(path: PathBuf, port: u16, network: Network) -> Result<()> {
    let database = path.clone();
    task::spawn_blocking(move || Store::open(&database)?.pin_network(network))
        .await?
        .context("could not open the invoice database for checkout")?;

    let app = Router::new()
        .route("/checkout/{id}", get(checkout).post(select_reader))
        .route("/checkout/{id}/attestation", get(attestation))
        .layer(DefaultBodyLimit::max(2048))
        .with_state(path.clone())
        .merge(merchant::routes(path, network)?)
        .layer(middleware::map_response(checkout_headers));
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
        let status = ledger::status(store.connection(), &id)?;
        let lightning = lightning::current_bolt11(store.connection(), &id, ledger::now())?;
        let proof = identity::get(store.connection(), &id)?.is_some();
        let delivery = paykit::delivery(&path, &id)?;
        render_payment(
            invoice.amount_sats,
            &invoice.address,
            invoice.network,
            status.state,
            lightning.as_deref(),
            proof.then_some(invoice.id.as_str()),
            delivery.as_deref(),
        )
        .map(Some)
    })
    .await;

    match result {
        Ok(Ok(Some(page))) => Html(page).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "Invoice not found.").into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "Checkout unavailable.").into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Buyer {
    reader: String,
}

/// The unguessable order link lets its holder choose one private delivery identity.
async fn select_reader(
    State(path): State<PathBuf>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(buyer): Form<Buyer>,
) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<axum::http::Uri>().ok());
    let same_origin = origin
        .as_ref()
        .and_then(|o| o.authority())
        .zip(headers.get(header::HOST).and_then(|h| h.to_str().ok()))
        .is_some_and(|(origin, host)| origin.as_str().eq_ignore_ascii_case(host));
    if !same_origin {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(reader) = paykit::Reader::parse(buyer.reader.trim()) else {
        return (StatusCode::BAD_REQUEST, "Enter a valid Pubky identity.").into_response();
    };
    let redirect = format!("/checkout/{}", id);
    let result = task::spawn_blocking(move || -> Result<bool> {
        let store = Store::open_read_only(&path)?;
        let Some(invoice) = store.get(&id)? else {
            return Ok(false);
        };
        anyhow::ensure!(
            ledger::status(store.connection(), &id)?.state == "unpaid",
            "invoice already received a payment"
        );
        paykit::bind(&path, &invoice, &reader)?;
        Ok(true)
    })
    .await;
    match result {
        Ok(Ok(true)) => Redirect::to(&redirect).into_response(),
        Ok(Ok(false)) => StatusCode::NOT_FOUND.into_response(),
        _ => (
            StatusCode::CONFLICT,
            "Private request unavailable or already assigned.",
        )
            .into_response(),
    }
}

async fn attestation(State(path): State<PathBuf>, Path(id): Path<String>) -> Response {
    let result = task::spawn_blocking(move || {
        identity::get(Store::open_read_only(&path)?.connection(), &id)
    })
    .await;
    match result {
        Ok(Ok(Some(proof))) => {
            ([(header::CONTENT_TYPE, "application/json")], proof).into_response()
        }
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
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
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    );
    response
}

fn render_payment(
    amount_sats: u64,
    stored_address: &str,
    network: Network,
    status: &str,
    lightning: Option<&str>,
    proof_id: Option<&str>,
    delivery: Option<&str>,
) -> Result<String> {
    let address = stored_address
        .parse::<Address<NetworkUnchecked>>()?
        .require_network(network)?;
    let btc = format!(
        "{}.{:08}",
        amount_sats / 100_000_000,
        amount_sats % 100_000_000
    );
    let mut uri = format!("bitcoin:{address}?amount={btc}");
    let mut lightning_link = String::new();
    if let Some(bolt11) = lightning {
        let invoice = bolt11.parse::<lightning_invoice::Bolt11Invoice>()?;
        anyhow::ensure!(
            invoice.amount_milli_satoshis() == amount_sats.checked_mul(1000)
                && invoice.network() == network,
            "Lightning invoice terms disagree"
        );
        let invoice = invoice.to_string();
        uri.push_str("&lightning=");
        uri.push_str(&invoice);
        lightning_link = format!("<a href='lightning:{invoice}'>Pay with Lightning</a>");
    }
    let qr = QrCode::new(uri.as_bytes())?
        .render::<svg::Color>()
        .min_dimensions(208, 208)
        .build();
    // A standalone SVG document can include an XML declaration; inline HTML only needs the SVG.
    let qr = qr.split_once("?>").map_or(qr.as_str(), |(_, svg)| svg);

    let network_label = match network {
        Network::Bitcoin => "Bitcoin mainnet",
        Network::Regtest => "Regtest",
        _ => "Bitcoin test network",
    };
    let proof = proof_id.map(|id|format!("<p><a href='/checkout/{}/attestation'>Signed merchant payment instructions</a></p>", merchant::escape(id))).unwrap_or_default();
    let buyer_form = if status == "unpaid"
        && delivery.is_none()
        && std::env::var_os("CHECKOUT_PAYKIT_URL").is_some()
    {
        "<form method='post'><label>Receive this request in your Paykit wallet<input name='reader' required minlength='57' maxlength='57' placeholder='pubky...' autocomplete='off'></label><button type='submit'>Request in wallet</button></form>"
    } else {
        ""
    };
    let delivery = delivery
        .map(|d| format!("<p>Private Paykit request: {}</p>", merchant::escape(d)))
        .unwrap_or_default();
    let controls = if status == "unpaid" {
        format!(
            "<figure aria-label='Bitcoin payment QR code'>{qr}</figure><a href=\"{}\">Open in wallet</a>{lightning_link}<p>Payment address</p><code>{address}</code>",
            merchant::escape(&uri)
        )
    } else {
        String::new()
    };
    let status_label = match status {
        "unpaid" => "Awaiting payment",
        "expired" => "Invoice expired",
        "partial" => "Partial payment received. Contact the merchant before sending more.",
        "confirming" => "Payment detected. Waiting for confirmations.",
        "settled" => "Payment confirmed",
        "late" => "Late payment. Merchant review required.",
        "overpaid" => "Overpayment. Merchant review required.",
        "multiple_payments" => "Multiple payments. Merchant review required.",
        _ => "Checking payment",
    };
    Ok(format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Bitcoin checkout</title>
<style>
:root {{ font: 100%/1.5 "Inter",system-ui,sans-serif; color: #1c2825; background: #eff3f0 }}
body {{ margin: 0; padding: 2rem 1rem }}
main {{ max-width: 24rem; margin: 0 auto; padding: 1.75rem; border: 1px solid #c6d4cb;
        border-radius: 1rem; background: white }}
h1 {{ font-size: 1.3rem; margin: 0 }} p {{ margin: .2rem 0 }}
.amount {{ font-size: 2rem; font-weight: 700 }}
figure {{ margin: 1rem 0; text-align: center }} svg {{ width: 13rem; max-width: 100%; height: auto }}
code {{ display: block; overflow-wrap: anywhere; padding: .55rem 0; user-select: all }}
form {{ margin:1.25rem 0 }} input,button {{ box-sizing:border-box; width:100%; padding:.55rem; margin:.25rem 0 }}
a {{ display: block; text-align: center; padding: .65rem 1rem; border-radius: .45rem;
     color: white; background: #155443; font-weight: 650; text-decoration: none }}
</style></head><body><main>
<h1>Bitcoin checkout</h1><p>{network_label}</p>
<p class="amount">{amount_sats} sats</p><p>{btc} BTC</p>
<p>{status_label}</p>{controls}{buyer_form}{delivery}{proof}<p><a href="">Refresh payment status</a></p>
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
        let page = render_payment(
            100_000_001,
            &address.to_string(),
            Network::Regtest,
            "unpaid",
            None,
            None,
            None,
        )
        .unwrap();
        assert!(page.contains(&format!("href=\"bitcoin:{address}?amount=1.00000001\"")));
        assert!(page.contains(&format!("<code>{address}</code>")));
        assert!(page.contains("<svg"));
        assert!(page.contains("Regtest"));
    }

    #[test]
    fn rejects_wrong_network_and_untrusted_address_text() {
        let mainnet_address = Address::p2wsh(&ScriptBuf::new(), Network::Bitcoin);
        assert!(
            render_payment(
                1,
                &mainnet_address.to_string(),
                Network::Regtest,
                "unpaid",
                None,
                None,
                None
            )
            .is_err()
        );
        assert!(
            render_payment(
                1,
                "\"><script>alert(1)</script>",
                Network::Regtest,
                "unpaid",
                None,
                None,
                None
            )
            .is_err()
        );
    }
}
