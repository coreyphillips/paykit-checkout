# Paykit Checkout

Paykit Checkout is a self-hosted Bitcoin payment server for a single merchant. A store creates an order in satoshis and gets a checkout link. Guest wallets can pay a fresh Bitcoin address or an LND invoice. A buyer with a Paykit identity can also enter it on the payment page and receive a private request through the merchant's Paykit Server. All three paths update the same order.

Bitcoin mainnet and regtest are supported. The merchant keeps the Bitcoin spending keys in their wallet. The worker uses a watch-only public account, a merchant-selected Electrum server, and optional credentials for Lightning and Paykit.

## Try it on regtest

Install [stable Rust](https://rustup.rs/) and run these commands from the repository root. This public example account is for disposable regtest coins.

```sh
cargo build --locked

account_tpub=tpubDC8msFGeGuwnKG9Upg7DM2b4DaRqg3CUZa5g8v2SRQ6K4NSkxUgd7HsL2XVWbVm39yBA4LAxysQAm397zwQSQoQgewGiYZqrA9DsP4zbQ1M
checkout_path=$(./target/debug/paykit-checkout invoice \
  --order-id coffee-1001 --amount-sats 50000 --account-xpub "$account_tpub")
echo "http://127.0.0.1:8080$checkout_path"
./target/debug/paykit-checkout serve
```

Open the printed URL. The first invoice in a fresh database requests 50,000 sats at `bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk`. Repeating the command with the same order and amount returns the same URL. A wallet link and QR code work before a node is connected. To detect payments, run the worker against an Electrum server indexing the same regtest chain.

## Run a merchant on mainnet

Build the release binary with `cargo build --release --locked`. Use two processes sharing one local SQLite file. The web process serves the checkout, store API, and merchant dashboard. The worker observes payments and delivers notifications. Run the processes under a supervisor that restarts them after failure.

1. From your Bitcoin wallet, export a dedicated native SegWit account public key at `m/84'/0'/account'` in `xpub` format. Reserve the account's external branch for this checkout. Use a separate account for the upstream Paykit Server and each independent database.
2. Create a private merchant configuration directory outside the repository. In that directory, create independent credentials for the dashboard, the store API, and the webhook consumer.

   ```sh
   umask 077
   openssl rand -hex 32 > merchant-token
   openssl rand -hex 32 > store-token
   openssl rand -hex 32 > webhook-secret
   ```

3. Set the following variables on the web process. Substitute the absolute paths for your deployment and the public account key you exported. The last command assumes the release binary is on your PATH.

   ```sh
   export CHECKOUT_ACCOUNT_XPUB='YOUR_DEDICATED_MAINNET_ACCOUNT_XPUB'
   export CHECKOUT_API_TOKEN_FILE=/etc/paykit-checkout/merchant-token
   export CHECKOUT_INVOICE_API_TOKEN_FILE=/etc/paykit-checkout/store-token
   paykit-checkout --database /var/lib/paykit-checkout/mainnet.sqlite3 --network mainnet serve --port 8080
   ```

4. Forward HTTPS from your shop's checkout hostname to the loopback listener. Preserve the original HTTP Host header so browser submissions can verify their origin (for nginx, `proxy_set_header Host $http_host;`). Permit guest access to `/checkout/`. The merchant dashboard at `/merchant` uses HTTP Basic with username `merchant` and the merchant token as its password. The store integration uses the scoped store token.
5. On the worker, point `CHECKOUT_ELECTRUM_URL` at your Bitcoin mainnet indexer. Set the webhook URL and secret file for your store. The worker polls continuously. `--once` runs one cycle and exits with an error if a backend failed.

   ```sh
   export CHECKOUT_ELECTRUM_URL='ssl://electrum.example.com:50002'
   export CHECKOUT_WEBHOOK_URL='https://shop.example.com/payment-events'
   export CHECKOUT_WEBHOOK_SECRET_FILE=/etc/paykit-checkout/webhook-secret
   paykit-checkout --database /var/lib/paykit-checkout/mainnet.sqlite3 --network mainnet worker --min-confirmations 2
   ```

6. Configure the [WooCommerce gateway](docs/woocommerce.md) or connect a store using the [merchant API and webhook contract](docs/merchant-api.md). To enable additional rails, follow the [LND setup](docs/lightning.md) and [private Paykit setup](docs/paykit.md). The buyer's Paykit wallet must support the receiver's Payment Request flow. Upstream [Paykit Server](https://github.com/pubky/paykit-server) describes its receiver implementation as pre-production. Pin the tested server version in the private Paykit setup guide.

## Settlement and recovery

Invoices expire after one hour. The worker keeps checking expired invoices for late payments. The guest page and merchant dashboard distinguish detection from confirmation. Late, excess, or conflicting full payments appear for merchant review. Partial payments on a guest Bitcoin address can add up across transactions. The policy counts confirmed outputs at the configured depth and reevaluates them after a reorg.

A settlement transition and its webhook are committed together. Delivery can repeat after a timeout or restart. The store must deduplicate the event and apply its own order fulfillment once. A later reorg or a second full payment emits a separate status change. See [receiving webhooks](docs/merchant-api.md#receive-webhooks).

Back up the SQLite database consistently using SQLite's backup facility. Preserve the wallet recovery information, the LND invoice secret, and the merchant configuration separately. A wallet seed restores control of funds. The database restores order mappings, address allocation, Lightning reservations, Paykit requests, and pending webhook deliveries. Starting over from an old address counter can allocate a previously issued address.

A compromised checkout host can redirect future buyers. For a buyer whose wallet already knows the merchant's Pubky identity, the project includes an independent on-chain signing and verification flow. See [verify a merchant destination](docs/merchant-identity.md).

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

For the disposable Bitcoin Core and Electrs integration run, use `python3 tests/regtest-chain/run.py`. To pay through a real Pubky testnet, Paykit Server, and SDK reader, use `python3 tests/paykit-regtest/run.py`. Both require Docker and use regtest coins.

Paykit Checkout is available under the [MIT License](LICENSE).
