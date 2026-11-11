# Paykit Checkout

Paykit Checkout is a self-hosted Bitcoin checkout for merchants. The first working feature is guest checkout on Bitcoin regtest. A merchant creates an invoice from the command line and the buyer opens a payment page with the amount, a QR code, and a wallet link. The buyer can use a regtest Bitcoin wallet without a Pubky account.

The project aims to give a merchant one order record whether the buyer pays from a guest wallet or receives a private request through Paykit. Today the application issues payment instructions only. Payment detection, fulfillment notifications, and the Paykit connection are still in development.

## Try a guest checkout

Install [stable Rust](https://rustup.rs/), clone this repository, and run the commands below from its root. The example uses a public test account. Use it only with regtest coins.

```sh
cargo build --locked

account_tpub=tpubDC8msFGeGuwnKG9Upg7DM2b4DaRqg3CUZa5g8v2SRQ6K4NSkxUgd7HsL2XVWbVm39yBA4LAxysQAm397zwQSQoQgewGiYZqrA9DsP4zbQ1M
checkout_path=$(./target/debug/paykit-checkout invoice \
  --order-id coffee-1001 \
  --amount-sats 50000 \
  --account-xpub "$account_tpub")

echo "http://127.0.0.1:8080$checkout_path"
./target/debug/paykit-checkout serve
```

Open the printed URL in a browser on the same computer. With a fresh database, the first invoice requests 50,000 sats at `bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk`. You can inspect the page without running a Bitcoin node. Sending a payment requires a wallet funded on the same regtest chain as the recipient.

## Invoice and wallet behavior

- The merchant command stores invoices in `checkout.sqlite3` in the current directory. Use `--database PATH` on both commands to choose a different file. The guest server opens the file read-only and listens on `127.0.0.1:8080` by default.
- Repeating an order ID with the same amount and account returns its original checkout path, including after a restart. Reusing the order ID with a different amount fails.
- For your own tests, export a dedicated BIP84 regtest account public key (`tpub`) from `m/84'/1'/account'`. Each new order reserves the next external address at `/0/index`. Keep the wallet's spending keys in the wallet.
- Keep one invoice database per receiving account. A new database starts at index zero. Reusing the account with a second database, or restoring an old snapshot, can reissue addresses. Back up the database along with the wallet's usual recovery information.
- The checkout URL contains a random invoice ID. Anyone with that link can view its amount and receiving address. The page keeps the merchant's order reference private.

## First integration milestone

The next milestone is to fulfill a regtest order exactly once, through either guest checkout or a private Paykit request.

1. Observe guest payments through the merchant's Bitcoin backend. Match the invoice's destination and amount, apply a confirmation policy, and recover after restarts or reorganizations.
2. Connect the same order flow to [Paykit Server](https://github.com/pubky/paykit-server) for buyers with a Paykit identity. Its [SDK regtest example](https://github.com/pubky/paykit-server/tree/master/sdk-example) demonstrates private request delivery and independent on-chain verification.
3. Add a merchant API and signed, retryable settlement webhooks. Exercise duplicate events and payments arriving through both checkout paths.

Lightning support through a merchant-operated node is planned after this on-chain milestone.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

## License

Paykit Checkout is available under the [MIT License](LICENSE).
