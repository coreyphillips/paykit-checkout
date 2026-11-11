# Connect a store

Configure the web process and worker using the [mainnet setup](../README.md#run-a-merchant-on-mainnet). Give the store the contents of the store token file. Send it over HTTPS in the `Authorization: Bearer` header. The store token grants invoice creation and lookup. The merchant token also grants access to the dashboard, private request binding, and attestation upload.

## Create an invoice

`POST /v1/invoices` accepts an order reference and integer price in satoshis. An optional `reader` identifies the buyer who should receive a private Paykit request. The authenticated merchant supplies the request terms. The checkout URL itself contains only a random invoice ID.

```json
{"order_id":"coffee-1001","amount_sats":50000}
```

The response contains `invoice_id`, `checkout_path`, `order_id`, `amount_sats`, `network`, `address`, `address_index`, `expires_at`, `state`, and `rails`. Build the buyer's URL from the configured checkout origin and returned relative path. A retry with the same order and terms recovers the same invoice. A changed price or conflicting recipient is rejected. Ensure references are unique for the lifetime of the merchant database.

`GET /v1/invoices/{id}` returns the current invoice. On mainnet its network field is `bitcoin`. Expiry is Unix time in seconds. A payment observed at the deadline qualifies as on time. New payment instructions close when the clock reaches the deadline. The Bitcoin worker uses first observation of the actual outputs. A worker offline across the deadline may surface a payment for late review. Rail observations show amounts received and confirmed. Aggregate amounts only within one rail. A store should fulfill a settled order, and route other terminal or ambiguous states to an operator.

The merchant can add a Paykit reader to an existing invoice by posting `{"reader":"pubky..."}` to `/v1/invoices/{id}/paykit`. The worker delivers the request asynchronously. When Paykit is configured, a buyer holding a guest link can also enter their own Pubky identity on that checkout page. The first valid selection binds the recipient for the order. Treat the link as private. The page displays delivery state and keeps the reader identity private.

## Receive webhooks

The worker POSTs a JSON document to the operator-configured HTTPS endpoint. A body contains the event ID, type, invoice ID, order ID, original amount in sats, network, state, and rail observations. The worker signs the exact request body with the contents of the webhook secret file, excluding its trailing newline.

1. Read the raw body and the `X-Checkout-Timestamp` and `X-Checkout-Signature` headers. Check the Unix timestamp against a short tolerance such as five minutes. Each retry receives a new timestamp and signature.
2. Compute HMAC-SHA256 over the ASCII timestamp, a period, and the unmodified request body. Prefix the lowercase hexadecimal digest with `v1=` and compare in constant time.
3. Check `X-Checkout-Event` against the signed body's `event_id`. Verify that the invoice, order ID, amount, and network match the order recorded by your store. Read the current API state before fulfillment.
4. On `invoice.settled`, record the event ID and complete the matching order idempotently. `invoice.updated` reports other state transitions. A reorg or second full payment can produce `invoice.settlement_changed`. `invoice.restored` means eligibility returned for an order that previously qualified. Reconcile the store's actual order state to avoid a second fulfillment.
5. Return a 2xx response once processing is durable. The outbox retries transport failures and other responses with increasing delays up to one hour. A failed event blocks later deliveries for the same invoice. A restored order can need its first fulfillment if the original settlement webhook arrived during a reorg.

Expiry by clock alone appears in invoice status and the dashboard. Payment observations drive webhook events. The dashboard shows the latest local observations. After an indexer outage, wait for a successful worker cycle before making a manual settlement decision.
