<?php
use Automattic\WooCommerce\Blocks\Payments\Integrations\AbstractPaymentMethodType;
defined( 'ABSPATH' ) || exit;

final class Paykit_WC_Blocks extends AbstractPaymentMethodType {
    protected $name = 'paykit_checkout';
    public function initialize() {
        $this->settings = get_option( 'woocommerce_paykit_checkout_settings', array() );
    }
    public function is_active() {
        $gateways = WC()->payment_gateways()->payment_gateways();
        return isset( $gateways[ $this->name ] ) && $gateways[ $this->name ]->is_available();
    }
    public function get_payment_method_script_handles() {
        wp_register_script( 'paykit-checkout-blocks', plugins_url( 'blocks.js', __FILE__ ),
            array( 'wc-blocks-registry', 'wp-element' ), '0.1.0', true );
        return array( 'paykit-checkout-blocks' );
    }
}
