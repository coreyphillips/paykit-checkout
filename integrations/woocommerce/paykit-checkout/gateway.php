<?php
defined( 'ABSPATH' ) || exit;

final class Paykit_WC_Gateway extends WC_Payment_Gateway {
    public function __construct() {
        $this->id = 'paykit_checkout';
        $this->method_title = 'Paykit Checkout';
        $this->method_description = 'Accept Bitcoin and Lightning through your Paykit Checkout server.';
        $this->has_fields = true;
        $this->supports = array( 'products' );
        $this->form_fields = array(
            'enabled' => array( 'title' => 'Enable Paykit Checkout', 'type' => 'checkbox', 'default' => 'no' ),
        );
        $this->init_settings();
        $this->enabled = $this->get_option( 'enabled', 'no' );
        $this->title = 'Bitcoin';
        $this->description = 'Pay with a Bitcoin or Lightning wallet.';
        add_action( 'woocommerce_update_options_payment_gateways_' . $this->id, array( $this, 'process_admin_options' ) );
    }

    public function is_available() {
        return parent::is_available() && 'BTC' === get_woocommerce_currency()
            && 8 === wc_get_price_decimals() && defined( 'WC_ROUNDING_PRECISION' ) && WC_ROUNDING_PRECISION >= 8
            && '' !== paykit_wc_option( 'PAYKIT_CHECKOUT_API_TOKEN' )
            && '' !== paykit_wc_option( 'PAYKIT_CHECKOUT_WEBHOOK_SECRET' );
    }

    public function payment_fields() {
        echo '<p>' . esc_html( $this->description ) . '</p>';
        echo '<label>Pubky ID for a private payment request (optional) <input type="text" name="paykit_reader" maxlength="57" autocomplete="off"></label>';
    }

    public function process_payment( $order_id ) {
        $order = wc_get_order( $order_id );
        try {
            if ( ! $this->is_available() ) {
                throw new RuntimeException( 'Gateway configuration unavailable.' );
            }
            $sats = paykit_wc_sats( $order );
            $payload = array( 'order_id' => paykit_wc_store_id() . $order->get_id(), 'amount_sats' => $sats );
            $reader = isset( $_POST['paykit_reader'] ) ? wp_unslash( $_POST['paykit_reader'] ) : '';
            if ( ! is_string( $reader ) ) {
                throw new InvalidArgumentException();
            }
            $reader = trim( $reader );
            if ( '' !== $reader ) {
                if ( ! preg_match( '/^pubky[ybndrfg8ejkmcpqxot1uwisza345h769]{52}$/D', $reader ) ) {
                    throw new InvalidArgumentException();
                }
                $payload['reader'] = $reader;
            }
            $invoice = paykit_wc_api( 'POST', '/v1/invoices', $payload );
            $id = $invoice['invoice_id'] ?? null;
            paykit_wc_check_terms( $invoice, $order, $id );
            $old_id = $order->get_meta( '_paykit_checkout_invoice' );
            if ( ( $old_id && $id !== $old_id ) || ( $invoice['checkout_path'] ?? '' ) !== '/checkout/' . $id ) {
                throw new RuntimeException( 'Unexpected invoice identity or checkout path.' );
            }
            $link = paykit_wc_origin( 'PAYKIT_CHECKOUT_PUBLIC_ORIGIN' ) . '/checkout/' . $id;
            $order->update_meta_data( '_paykit_checkout_invoice', $id );
            $order->save();
            return array( 'result' => 'success', 'redirect' => $link );
        } catch ( InvalidArgumentException $e ) {
            wc_add_notice( 'Enter your complete Pubky ID or leave the field blank.', 'error' );
        } catch ( Throwable $e ) {
            wc_add_notice( 'Bitcoin checkout is unavailable for this order. Please try again.', 'error' );
        }
        return array( 'result' => 'failure' );
    }
}
