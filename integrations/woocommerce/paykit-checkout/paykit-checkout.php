<?php
/**
 * Plugin Name: Paykit Checkout for WooCommerce
 * Description: Accept BTC orders through your merchant-operated Paykit Checkout instance.
 * Version: 0.1.0
 * Requires PHP: 8.1
 * Requires Plugins: woocommerce
 * License: MIT
 */
defined( 'ABSPATH' ) || exit;

add_action( 'before_woocommerce_init', static function () {
    if ( class_exists( '\Automattic\WooCommerce\Utilities\FeaturesUtil' ) ) {
        \Automattic\WooCommerce\Utilities\FeaturesUtil::declare_compatibility( 'custom_order_tables', __FILE__, true );
    }
} );

add_filter( 'woocommerce_currencies', static function ( $currencies ) {
    $currencies['BTC'] = 'Bitcoin';
    return $currencies;
} );
add_filter( 'woocommerce_currency_symbol', static function ( $symbol, $code ) {
    return 'BTC' === $code ? '₿' : $symbol;
}, 10, 2 );

function paykit_wc_option( $name ) {
    return defined( $name ) ? (string) constant( $name ) : '';
}

function paykit_wc_origin( $name ) {
    $origin = rtrim( paykit_wc_option( $name ), '/' );
    $parts = wp_parse_url( $origin );
    $local_http = 'local' === wp_get_environment_type() && 'http' === ( $parts['scheme'] ?? '' );
    if ( empty( $parts['host'] ) || ( ! $local_http && 'https' !== ( $parts['scheme'] ?? '' ) )
        || array_intersect( array( 'path', 'user', 'pass', 'query', 'fragment' ), array_keys( $parts ) ) ) {
        throw new RuntimeException( 'Invalid Paykit origin configuration.' );
    }
    return $origin;
}

function paykit_wc_store_id() {
    $id = get_option( 'paykit_checkout_store_id' );
    if ( ! $id ) {
        add_option( 'paykit_checkout_store_id', wp_generate_uuid4(), '', false );
        $id = get_option( 'paykit_checkout_store_id' );
    }
    if ( ! is_string( $id ) || ! wp_is_uuid( $id, 4 ) ) {
        throw new RuntimeException( 'Invalid Paykit store ID.' );
    }
    return 'wc-' . $id . '-';
}

function paykit_wc_sats( $order ) {
    $total = (string) $order->get_total( 'edit' );
    if ( PHP_INT_SIZE < 8 || 'BTC' !== $order->get_currency() ||
        ! preg_match( '/^([0-9]{1,8})(?:\.([0-9]{1,8})0*)?$/D', $total, $m ) ) {
        throw new RuntimeException( 'Expected a BTC total with whole satoshis.' );
    }
    $sats = (int) $m[1] * 100000000 + (int) str_pad( $m[2] ?? '', 8, '0' );
    if ( $sats < 1 || $sats > 2100000000000000 ) {
        throw new RuntimeException( 'Price outside Bitcoin amount range.' );
    }
    return $sats;
}

function paykit_wc_api( $method, $path, $payload = null ) {
    $token = paykit_wc_option( 'PAYKIT_CHECKOUT_API_TOKEN' );
    if ( '' === $token ) {
        throw new RuntimeException( 'Missing invoice API token.' );
    }
    $args = array(
        'method' => $method, 'timeout' => 10, 'redirection' => 0,
        'headers' => array( 'Authorization' => 'Bearer ' . $token, 'Content-Type' => 'application/json' ),
    );
    if ( null !== $payload ) {
        $args['body'] = wp_json_encode( $payload );
    }
    $response = wp_remote_request( paykit_wc_origin( 'PAYKIT_CHECKOUT_API_ORIGIN' ) . $path, $args );
    if ( is_wp_error( $response ) || 200 !== wp_remote_retrieve_response_code( $response ) ) {
        throw new RuntimeException( 'Invoice service unavailable or request rejected.' );
    }
    $body = json_decode( wp_remote_retrieve_body( $response ), true, 16, JSON_THROW_ON_ERROR );
    if ( ! is_array( $body ) ) {
        throw new RuntimeException( 'Invalid invoice response.' );
    }
    return $body;
}

function paykit_wc_check_terms( $invoice, $order, $id ) {
    if ( ! is_string( $id ) || ! preg_match( '/^[a-f0-9]{32}$/D', $id )
        || ( $invoice['invoice_id'] ?? null ) !== $id
        || ( $invoice['order_id'] ?? null ) !== paykit_wc_store_id() . $order->get_id()
        || ( $invoice['amount_sats'] ?? null ) !== paykit_wc_sats( $order )
        || ! in_array( paykit_wc_option( 'PAYKIT_CHECKOUT_NETWORK' ), array( 'bitcoin', 'regtest' ), true )
        || ( $invoice['network'] ?? null ) !== paykit_wc_option( 'PAYKIT_CHECKOUT_NETWORK' ) ) {
        throw new RuntimeException( 'Invoice and WooCommerce order differ.' );
    }
}

add_action( 'plugins_loaded', static function () {
    if ( ! class_exists( 'WC_Payment_Gateway' ) ) {
        return;
    }
    require_once __DIR__ . '/gateway.php';
    add_filter( 'woocommerce_payment_gateways', static function ( $gateways ) {
        $gateways[] = 'Paykit_WC_Gateway';
        return $gateways;
    } );
} );

add_action( 'woocommerce_blocks_loaded', static function () {
    if ( class_exists( '\Automattic\WooCommerce\Blocks\Payments\Integrations\AbstractPaymentMethodType' ) ) {
        require_once __DIR__ . '/blocks.php';
        add_action( 'woocommerce_blocks_payment_method_type_registration', static function ( $registry ) {
            $registry->register( new Paykit_WC_Blocks() );
        } );
    }
} );

require_once __DIR__ . '/webhook.php';
