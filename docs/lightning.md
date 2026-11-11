# Receive Lightning payments with LND

Run LND on a machine controlled by the merchant. Connect it to the same Bitcoin network as the store and arrange inbound channel liquidity. The receiver supports Bitcoin mainnet and regtest. It creates fixed-amount BOLT11 invoices and queries LND for the settled amount and time.

## Configure the receiver

On the LND machine, bake a macaroon restricted to the three RPC methods the receiver calls. Select the network that matches your node.

```sh
lncli --network=regtest bakemacaroon --save_to checkout-receive.macaroon \
  uri:/lnrpc.Lightning/GetInfo \
  uri:/lnrpc.Lightning/AddInvoice \
  uri:/lnrpc.Lightning/LookupInvoice
```

Copy that macaroon and the node's public TLS certificate to the merchant backend. Generate the invoice secret **once** inside a new, private merchant configuration directory.

```sh
umask 077
openssl rand -out checkout-invoice-secret 32
```

Set the following variables on the merchant processes that issue and reconcile invoices. Leave them unset for a store that accepts only on-chain payments.

| Variable | Value |
| --- | --- |
| `PAYKIT_LND_URL` | HTTPS origin of the LND REST interface, such as `https://127.0.0.1:8080`. Use a hostname or IP in the node's certificate. |
| `PAYKIT_LND_TLS_CERT` | Path to the PEM certificate for the merchant's LND instance. |
| `PAYKIT_LND_MACAROON` | Path to the binary macaroon created above. |
| `PAYKIT_LND_INVOICE_SECRET` | Path to the file containing the 32 random bytes. |

Keep the macaroon and invoice secret away from the public checkout process. Back up the secret with the merchant's private configuration and back up the invoice database. The secret derives a distinct payment preimage from the local invoice ID and network. Reusing the same secret on retries lets the receiver recover an invoice after a lost response. Possession of the secret and an invoice ID reveals its preimage.

## Verify a payment

The merchant backend matches LND's network, node identity, payment hash and invoice amount against the stored reservation and signed BOLT11. The checkout shows an open invoice while both the BOLT11 and merchant invoice are valid. The receiver asks LND for the final state and records the actual millisatoshis and settlement time returned for a settled payment. It can reconcile a payment after the invoice expires.

A buyer who independently knows the merchant's LND public key can check the signed destination, amount, network and expiry with `paykit-checkout --network mainnet verify-lightning --invoice "$BOLT11" --amount-sats 50000 --node-pubkey "$TRUSTED_NODE_PUBKEY"`. The wallet also needs an authenticated link between the merchant, the order and that particular invoice to verify the purchase.
