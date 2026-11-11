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
