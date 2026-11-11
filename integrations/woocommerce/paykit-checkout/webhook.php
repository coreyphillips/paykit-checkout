<?php
defined( 'ABSPATH' ) || exit;

add_action( 'rest_api_init', static function () {
    register_rest_route( 'paykit-checkout/v1', '/events', array(
        'methods' => 'POST', 'permission_callback' => 'paykit_wc_verify_event', 'callback' => 'paykit_wc_handle_event',
    ) );
} );

function paykit_wc_verify_event( WP_REST_Request $request ) {
    $secret = paykit_wc_option( 'PAYKIT_CHECKOUT_WEBHOOK_SECRET' );
    $timestamp = $request->get_header( 'x-checkout-timestamp' );
    $body = $request->get_body();
    if ( '' === $secret || strlen( $body ) > 65536 || ! is_string( $timestamp )
        || ! preg_match( '/^[0-9]{1,12}$/D', $timestamp ) || abs( time() - (int) $timestamp ) > 300 ) {
        return new WP_Error( 'paykit_auth', 'Invalid webhook.', array( 'status' => 401 ) );
    }
    $expected = 'v1=' . hash_hmac( 'sha256', $timestamp . '.' . $body, $secret );
    return hash_equals( $expected, (string) $request->get_header( 'x-checkout-signature' ) )
        ? true : new WP_Error( 'paykit_auth', 'Invalid webhook.', array( 'status' => 401 ) );
}

function paykit_wc_handle_event( WP_REST_Request $request ) {
    global $wpdb;
    $event = $request->get_json_params();
    if ( ! is_array( $event ) || ! wp_is_uuid( $event['event_id'] ?? '', 4 )
        || $request->get_header( 'x-checkout-event' ) !== $event['event_id']
        || ! is_string( $event['order_id'] ?? null ) ) {
        return new WP_Error( 'paykit_event', 'Invalid event.', array( 'status' => 400 ) );
    }
    $prefix = paykit_wc_store_id();
    if ( ! str_starts_with( $event['order_id'], $prefix ) ) {
        return rest_ensure_response( array( 'result' => 'unrelated_order' ) );
    }
    $order_id = substr( $event['order_id'], strlen( $prefix ) );
    if ( ! preg_match( '/^[1-9][0-9]{0,17}$/D', $order_id ) ) {
        return new WP_Error( 'paykit_order', 'Invalid order reference.', array( 'status' => 400 ) );
    }
    // A database connection lock is released automatically when PHP disconnects.
    $lock = 'paykit:' . substr( hash( 'sha256', $event['order_id'] ), 0, 48 );
    if ( '1' !== (string) $wpdb->get_var( $wpdb->prepare( 'SELECT GET_LOCK(%s, 3)', $lock ) ) ) {
        return new WP_Error( 'paykit_busy', 'Retry this event.', array( 'status' => 503 ) );
    }
    try {
        $order = wc_get_order( (int) $order_id );
        if ( ! $order || 'paykit_checkout' !== $order->get_payment_method() ) {
            throw new RuntimeException( 'Order is unavailable.' );
        }
        $invoice_id = $order->get_meta( '_paykit_checkout_invoice' );
        paykit_wc_check_terms( $event, $order, $invoice_id );
        $invoice = paykit_wc_api( 'GET', '/v1/invoices/' . $invoice_id );
        paykit_wc_check_terms( $invoice, $order, $invoice_id );
        $state = $invoice['state'] ?? '';
        $fulfilled = $order->get_meta( '_paykit_checkout_completed' ) || $order->get_date_paid();
        if ( 'settled' === $state && ! $fulfilled && $order->has_status( array( 'pending', 'on-hold', 'failed' ) ) ) {
            // payment_complete saves this marker alongside the paid status and transaction ID.
            $order->update_meta_data( '_paykit_checkout_completed', $invoice_id );
            if ( ! $order->payment_complete( $invoice_id ) ) {
                throw new RuntimeException( 'WooCommerce could not complete this order.' );
            }
        }
        $previous = $order->get_meta( '_paykit_checkout_state' );
        if ( $previous !== $state ) {
            if ( in_array( $state, array( 'partial', 'overpaid', 'late', 'multiple_payments' ), true )
                || ( $fulfilled && 'settled' !== $state )
                || ( 'settled' === $state && ! $order->get_date_paid() ) ) {
                $order->add_order_note( 'Paykit payment needs review. Current invoice state: ' . sanitize_text_field( $state ) );
            }
            $order->update_meta_data( '_paykit_checkout_state', $state );
            $order->save();
        }
        return rest_ensure_response( array( 'result' => 'processed' ) );
    } catch ( Throwable $e ) {
        return new WP_Error( 'paykit_sync', 'Payment could not be reconciled. Retry this event.', array( 'status' => 503 ) );
    } finally {
        $wpdb->get_var( $wpdb->prepare( 'SELECT RELEASE_LOCK(%s)', $lock ) );
    }
}
