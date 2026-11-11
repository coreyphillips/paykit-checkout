# Accept Bitcoin in WooCommerce

Install the gateway in a WooCommerce store that prices orders in bitcoin. A guest can use a Bitcoin or Lightning wallet. A buyer who supplies their Pubky ID can also receive a private payment request when the merchant has configured Paykit delivery.

## Install the plugin

Copy `integrations/woocommerce/paykit-checkout` to `wp-content/plugins/paykit-checkout` and activate **Paykit Checkout for WooCommerce**. Use PHP 8.1 or later on a 64-bit system. WordPress must be able to reach the merchant API, the buyer must be able to reach the public checkout, and the worker must be able to POST to the store.

1. In **WooCommerce > Settings > General**, select **Bitcoin (BTC)** and set the number of decimals to **8**. Set product, shipping and tax prices in BTC. For example, `0.00000501` BTC costs 501 sats.
2. In **WooCommerce > Settings > Payments**, enable **Paykit Checkout**. Both the checkout block and the classic checkout shortcode support it.
3. Set up the invoice token and webhook secret below. Your WordPress site needs the dedicated invoice API token, which can create invoices and read their status. Keep the administrator token on the merchant host.

## Connect the store

Configure `CHECKOUT_INVOICE_API_TOKEN_FILE` on the merchant server and `CHECKOUT_WEBHOOK_SECRET_FILE` on the worker. Each should point to a different private file containing a strong, printable random secret. Set `CHECKOUT_WEBHOOK_URL` to `https://shop.example/wp-json/paykit-checkout/v1/events`. For plain permalinks use `https://shop.example/?rest_route=/paykit-checkout/v1/events`. Keep the shop and worker clocks synchronized.

WooCommerce can round block checkout orders internally to six decimal places even when the cart displays eight. Set `WC_ROUNDING_PRECISION` before WooCommerce loads so a 501-sat product remains 501 sats in the order. The gateway requires at least eight digits of internal precision.

Make those two secrets available to WordPress in private files outside the document root. Add the following definitions to `wp-config.php` before WordPress loads. Match the network to your server, using `bitcoin` for mainnet or `regtest` for isolated testing.

```php
define( 'WC_ROUNDING_PRECISION', 10 );
define( 'PAYKIT_CHECKOUT_API_ORIGIN', 'https://merchant.example' );
define( 'PAYKIT_CHECKOUT_PUBLIC_ORIGIN', 'https://pay.example' );
define( 'PAYKIT_CHECKOUT_NETWORK', 'bitcoin' );
define( 'PAYKIT_CHECKOUT_API_TOKEN', trim( file_get_contents( '/run/secrets/paykit-invoice-token' ) ) );
define( 'PAYKIT_CHECKOUT_WEBHOOK_SECRET', trim( file_get_contents( '/run/secrets/paykit-webhook' ) ) );
```

Use origins without paths. You may serve both from the same HTTPS origin. In a local WordPress development environment (`WP_ENVIRONMENT_TYPE=local`), you can use HTTP origins for an isolated regtest deployment.

## Process an order

WooCommerce sends the exact BTC order total in satoshis. Each shop has a persistent ID in its WordPress database, so retrying the same order recovers the original invoice. Back up the `paykit_checkout_store_id` option with the store. Each independent store installation should have its own ID.

A settled webhook triggers an authenticated status lookup. The gateway checks the invoice reference, shop order, amount and network, then calls WooCommerce's payment completion routine for an eligible unpaid order. It serializes simultaneous deliveries for the same order. Recovery after a chain reorganization or delivery retry checks current settlement and the existing paid marker before completing an order.

Partial, late, excess and multiple payments need merchant review. A change in settlement after payment adds an order note. Review fulfilment and any refund in WooCommerce before taking further action. The gateway does not send payments or issue automatic refunds.

## Repeat the regtest checkout checks

Use a disposable WordPress shop, its own merchant database, and a funded Bitcoin Core regtest wallet. Point the worker at an Electrs instance indexing that same node. These steps change the test chain. Run the worker with `--once` to control when it observes payments and delivers events.

1. In WooCommerce create a virtual product priced at `0.00000501` BTC. Place one order through the checkout block and another through a page containing `[woocommerce_checkout]`. Both public checkout pages must show **501 sats**. Check that both WooCommerce orders show a total of `0.00000501` BTC. Each order must have its own receiving address.
2. Send `0.00000501` BTC to each invoice address. Mine two blocks, wait for Electrs to index them, then run the worker with webhook delivery enabled. Verify that WooCommerce assigns the checkout invoice ID as its transaction ID and marks both orders paid.
3. For a third order, pay its invoice and mine two blocks on an otherwise idle chain. Record the hash of the first block containing the payment. Once Electrs has indexed both blocks, run the worker once with both webhook environment variables unset. Confirm the SQLite `payment_events` table contains an undelivered `invoice.settled` for this invoice.
4. On the disposable node, call `invalidateblock` with the first block's hash. Generate three empty blocks by calling `generateblock MINER_ADDRESS '[]'` three times. Wait for Electrs to index the longer branch, then run the worker with webhook delivery enabled. The queued settlement event and subsequent settlement change should both be delivered. The WooCommerce order should still be pending.
5. Mine two ordinary blocks with `generatetoaddress` so the payment returns to the chain. After Electrs catches up, run the worker with webhooks enabled. The resulting `invoice.restored` should complete the order. To test delivery retries, reset `delivered` and `next_attempt` to zero on the restored event in the disposable merchant database and run the worker again. The WooCommerce transaction ID and date paid should remain unchanged.

For a direct callback count, a test-only WordPress plugin can register the following observer before placing the orders. Read the option using `wp option get paykit_test_completions_ORDERID`. The third order should have no counter after step 4 and a value of one after step 5 and each subsequent retry.

```php
add_action( 'woocommerce_payment_complete', static function ( $order_id ) {
    $key = 'paykit_test_completions_' . $order_id;
    update_option( $key, 1 + (int) get_option( $key, 0 ) );
} );
```
