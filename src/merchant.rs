//! A single merchant's API and authenticated operations dashboard.

use crate::{
    config, identity, ledger, paykit,
    store::{Invoice, Store},
};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bitcoin::Network;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use subtle::ConstantTimeEq;

#[derive(Clone)]
struct Merchant {
    path: PathBuf,
    network: Network,
    account: String,
    token_hash: [u8; 32],
    store_token_hash: Option<[u8; 32]>,
    identity_pin: Option<String>,
}

pub fn routes(path: PathBuf, network: Network) -> Result<Router> {
    let file = config::optional("CHECKOUT_API_TOKEN_FILE")?;
    let account = config::optional("CHECKOUT_ACCOUNT_XPUB")?;
    if file.is_none() && account.is_none() {
        return Ok(Router::new());
    }
    let secret = config::secret(FsPath::new(
        &file.context("missing CHECKOUT_API_TOKEN_FILE")?,
    ))?;
    let account = account.context("missing CHECKOUT_ACCOUNT_XPUB")?;
    crate::store::derive_address(&account, network, 0)?;
    let store_token = config::optional("CHECKOUT_INVOICE_API_TOKEN_FILE")?
        .map(|f| config::secret(FsPath::new(&f)))
        .transpose()?;
    anyhow::ensure!(
        store_token.as_ref() != Some(&secret),
        "store and administrator credentials must differ"
    );
    let store_token_hash = store_token.map(|t| Sha256::digest(t).into());
    let state = Arc::new(Merchant {
        path,
        network,
        account,
        store_token_hash,
        token_hash: Sha256::digest(secret).into(),
        identity_pin: config::optional("CHECKOUT_MERCHANT_PUBKY")?,
    });
    Ok(Router::new()
        .route("/v1/invoices", post(create))
        .route("/v1/invoices/{id}", get(lookup))
        .route("/v1/invoices/{id}/paykit", post(request_paykit))
        .route("/v1/invoices/{id}/attestation", post(attach))
        .route("/merchant", get(dashboard))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(state))
}

async fn authorize(State(s): State<Arc<Merchant>>, req: Request, next: Next) -> Response {
    let raw = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let basic = raw
        .strip_prefix("Basic ")
        .and_then(|s| STANDARD.decode(s).ok())
        .and_then(|v| String::from_utf8(v).ok());
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| basic.as_deref().and_then(|b| b.strip_prefix("merchant:")));
    let path = req.uri().path();
    let invoice_route = (req.method() == axum::http::Method::POST && path == "/v1/invoices")
        || (req.method() == axum::http::Method::GET
            && path
                .strip_prefix("/v1/invoices/")
                .is_some_and(|id| id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())));
    let authorized = token.is_some_and(|t| {
        let digest: [u8; 32] = Sha256::digest(t).into();
        bool::from(digest.ct_eq(&s.token_hash))
            || (invoice_route
                && raw.starts_with("Bearer ")
                && s.store_token_hash
                    .is_some_and(|key| bool::from(digest.ct_eq(&key))))
    });
    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"Merchant checkout\"",
            )],
            "Merchant credentials required.",
        )
            .into_response()
    }
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Order {
    order_id: String,
    amount_sats: u64,
    reader: Option<String>,
}

type ApiResult = Result<Value, (StatusCode, &'static str)>;
fn unavailable(_: anyhow::Error) -> (StatusCode, &'static str) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Invoice storage or service unavailable.",
    )
}
fn summary(db: &Store, invoice: Invoice) -> ApiResult {
    let status = ledger::status(db.connection(), &invoice.id).map_err(unavailable)?;
    let index: u32 = db
        .connection()
        .query_row(
            "SELECT address_index FROM invoices WHERE id=?",
            [&invoice.id],
            |r| r.get(0),
        )
        .map_err(|e| unavailable(e.into()))?;
    Ok(
        json!({"invoice_id":invoice.id,"order_id":invoice.order_id,"amount_sats":invoice.amount_sats,
        "address":invoice.address,"network":invoice.network.to_string(),"address_index":index,
        "expires_at":invoice.expires_at,"state":status.state,"rails":status.rails,"checkout_path":format!("/checkout/{}",invoice.id)}),
    )
}
fn invoice(store: &Store, id: &str) -> Result<Invoice, (StatusCode, &'static str)> {
    store
        .get(id)
        .map_err(unavailable)?
        .ok_or((StatusCode::NOT_FOUND, "Invoice not found."))
}
async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> ApiResult + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Request failed.").into_response(),
    }
}

async fn create(State(s): State<Arc<Merchant>>, Json(order): Json<Order>) -> Response {
    blocking(move || {
        let reader = order
            .reader
            .as_deref()
            .map(paykit::Reader::parse)
            .transpose()
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid Paykit reader."))?;
        let mut store = Store::open(&s.path).map_err(unavailable)?;
        let invoice = store
            .create_with_reader(
                &order.order_id,
                order.amount_sats,
                &s.account,
                s.network,
                reader.as_ref(),
            )
            .map_err(|_| {
                (
                    StatusCode::CONFLICT,
                    "Order, amount, or merchant account conflicts with this database.",
                )
            })?;
        summary(&store, invoice)
    })
    .await
}

async fn lookup(State(s): State<Arc<Merchant>>, Path(id): Path<String>) -> Response {
    blocking(move || {
        let db = Store::open_read_only(&s.path).map_err(unavailable)?;
        summary(&db, invoice(&db, &id)?)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipient {
    reader: String,
}

async fn request_paykit(
    State(s): State<Arc<Merchant>>,
    Path(id): Path<String>,
    Json(input): Json<Recipient>,
) -> Response {
    blocking(move || {
        let db = Store::open_read_only(&s.path).map_err(unavailable)?;
        let inv = invoice(&db, &id)?;
        let reader = paykit::Reader::parse(&input.reader)
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid reader."))?;
        paykit::bind(&s.path, &inv, &reader)
            .map_err(|_| (StatusCode::CONFLICT, "Paykit request could not be bound."))?;
        Ok(json!({"invoice_id":id,"delivery":paykit::delivery(&s.path,&id).map_err(unavailable)?}))
    })
    .await
}
async fn attach(
    State(s): State<Arc<Merchant>>,
    Path(id): Path<String>,
    Json(proof): Json<identity::Attestation>,
) -> Response {
    blocking(move || {
        let db = Store::open(&s.path).map_err(unavailable)?;
        let inv = invoice(&db, &id)?;
        let pin = s
            .identity_pin
            .as_ref()
            .ok_or((StatusCode::CONFLICT, "Merchant identity is not configured."))?;
        identity::attach(db.connection(), &inv, pin, &proof)
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid merchant attestation."))?;
        Ok(json!({"invoice_id":id}))
    })
    .await
}

async fn dashboard(State(s): State<Arc<Merchant>>) -> Response {
    let result=tokio::task::spawn_blocking(move||->Result<String>{
        let db=Store::open_read_only(&s.path)?;
        let mut rows=String::new();
        for invoice in db.all_invoices()?.into_iter().rev() {
            let status=ledger::status(db.connection(),&invoice.id)?;
            let observed=status.rails.iter().map(|r|format!("{}: {} sats",escape(&r.rail),r.observed_sats)).collect::<Vec<_>>().join(", ");
            rows.push_str(&format!("<tr><td><a href='/checkout/{}'>{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",escape(&invoice.id),escape(&invoice.order_id),invoice.amount_sats,status.state,observed));
        }
        Ok(format!("<!doctype html><html lang='en'><meta charset='utf-8'><meta name='viewport' content='width=device-width,initial-scale=1'><title>Merchant orders</title><style>body{{font:1rem/1.5 system-ui;margin:2rem}}td,th{{padding:.5rem;text-align:left;border-bottom:1px solid #ddd}}table{{border-collapse:collapse}}</style><main><h1>Merchant orders</h1><table><tr><th>Order</th><th>Price in sats</th><th>Status</th><th>Received</th></tr>{rows}</table></main></html>"))
    }).await;
    match result {
        Ok(Ok(page)) => Html(page).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "Dashboard unavailable.").into_response(),
    }
}
