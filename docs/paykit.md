# Receive a private Paykit payment

A merchant can give a buyer a guest checkout link and also send a private Bitcoin payment request to the buyer's Pubky identity for the same order. The merchant's worker calls Paykit Server. The buyer's wallet resolves the request and pays the Bitcoin address in its terms. This integration uses the receiver's BTC-only Marketplace API at [Paykit Server revision 4b0f971](https://github.com/pubky/paykit-server/tree/4b0f971f7c24999e78b0bf39fe13424d14b387d5).

## Configure the receiver

1. Run Paykit Server with PostgreSQL, a compatible Pubky homeserver and an Electrum endpoint for your chosen Bitcoin network. Follow its [configuration and setup instructions](https://github.com/pubky/paykit-server/blob/4b0f971f7c24999e78b0bf39fe13424d14b387d5/README.md).
2. Complete Paykit Server's creator setup with the merchant's wallet. Approve a dedicated receiving account. Use a different dedicated BIP84 account for guest checkout. Both services allocate external address indices independently.
3. Provision a dedicated Ed25519 signing key for the checkout worker. Place its public key, encoded as a `pubky`-prefixed z-base-32 identity, in Paykit Server's `signed_services.trusted_public_keys`. Save the 32-byte private seed as unpadded base64url in a file readable only by the worker. Every key on that upstream list can call its signed service routes. Keep the file separate from the merchant's Pubky identity key and the wallet.
4. Set the following environment variables. The invoice issuer and web service use the first three. Only the worker needs the signing-key file.

| Variable | Value |
| --- | --- |
| `CHECKOUT_PAYKIT_URL` | Bare HTTPS origin of Paykit Server, or loopback HTTP for a local test |
| `CHECKOUT_PAYKIT_CREATOR` | The merchant's canonical `pubky...` identity approved in step 2 |
| `CHECKOUT_PAYKIT_NETWORK` | `regtest` or `mainnet`, matching the provider's Bitcoin network and the local invoice |
| `CHECKOUT_PAYKIT_SIGNING_KEY_FILE` | Path to the private service signing seed |

For example, the following Python script creates a service key once with a restrictive file mode and prints its public identifier. It requires the Python `cryptography` package. Run it inside the worker's private configuration directory.

```sh
python3 - <<'PY'
import base64
import os
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

key = Ed25519PrivateKey.generate()
raw = serialization.Encoding.Raw
seed = key.private_bytes(raw, serialization.PrivateFormat.Raw, serialization.NoEncryption())
fd = os.open("paykit-service.seed", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "wb") as output:
    output.write(base64.urlsafe_b64encode(seed).rstrip(b"=") + b"\n")
public = key.public_key().public_bytes(raw, serialization.PublicFormat.Raw)
alphabet = str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567", "ybndrfg8ejkmcpqxot1uwisza345h769")
print("pubky" + base64.b32encode(public).decode().rstrip("=").translate(alphabet))
PY
```

## Deliver an invoice

Use an existing payable Reader identity with an authorized Noise key and a wallet registered to receive payment requests. Supply that identity using the invoice command's `--reader` option. For example, set `BUYER_PUBKY` and `GUEST_ACCOUNT_TPUB` to your regtest values, then run these commands in separate terminals.

```sh
paykit-checkout --network regtest invoice --order-id coffee-124 --amount-sats 50000 \
  --account-xpub "$GUEST_ACCOUNT_TPUB" --reader "$BUYER_PUBKY"
paykit-checkout --network regtest worker --min-confirmations 2
```

The invoice command prints its guest checkout path. The worker prepares and activates the private request and polls its status. Use one to six confirmations for this rail. Run `paykit-checkout status <invoice-id>` to inspect delivery and settlement separately. A delivered request tells you the proposal reached the upstream transport. Fulfillment uses the shared ledger's settlement event.

## Retries, expiry and recovery

SQLite binds the invoice to its Reader, merchant identity, provider origin and a random request reference. Creation retries preserve the reference and terms. An exact replay recovers the original upstream invoice when a response was lost. A second Reader on the same order conflicts. If the provider explicitly reports that an unactivated preparation expired, the worker can prepare a new reference within the original local window.

The worker signs the RFC 8785 canonical JSON body with the upstream domain, HTTP method and exact path. Requests send the random reference, amount, creator and Reader to the provider. The store's order ID and guest address stay local. Paykit Server's own Bitcoin observer verifies the invoice amount and confirmation depth.

The original checkout deadline governs automatic fulfillment. The upstream payment window starts on activation, which can occur later. After downtime, the worker can prove timely payment when the upstream deadline was within the store's deadline, or when it previously recorded full, timely evidence for the same transaction. A later upstream deadline and a first observation after the local deadline require merchant review. Underpayments, overpayments, payments on multiple rails and reorgs remain visible through the shared ledger.

Back up the checkout database with the order and binding state. Restore it alongside the right network, creator and provider origin; the worker selects jobs for that exact configuration. Follow Paykit Server's separate recovery procedure for its PostgreSQL data and hosted SDK state. Its maintainers designate that server as pre-production.

## Reproduce the SDK payment

Install Rust with rustup, Python 3, curl, tar, patch, and Docker with Compose v2. Start Docker. From the checkout repository, run the [isolated regtest driver](../tests/paykit-regtest/run.py).

```sh
python3 tests/paykit-regtest/run.py
```

The driver verifies the digest of the pinned upstream source and substitutes this checkout client for the example's invoice issuer. It uses disposable PostgreSQL, Bitcoin Core, Electrum and Pubky identities. The Paykit SDK wallet receives, resolves and accepts the private request. Bitcoin Core pays 50,000 regtest sats. The checkout worker reopens its database and checks for six confirmations and exactly one settlement event. The run ends with `PASS: checkout recorded six confirmations and one fulfillment event.` The four small status fixtures in its directory come from the [upstream HTTP fixture suite](https://github.com/pubky/paykit-server/tree/4b0f971f7c24999e78b0bf39fe13424d14b387d5/docs/fixtures/marketplace-payment-request-status).

The SDK authenticates the counterparty and resolves the address bound to those request terms. A wallet that also needs an independently authorized merchant destination must verify an additional attestation against a separately pinned merchant key. The guest checkout's offline verification workflow is documented in the [project README](../README.md).
