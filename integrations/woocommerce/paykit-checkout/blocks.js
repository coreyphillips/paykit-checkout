/* global wp, wc */
(() => {
    const { createElement: el, useEffect, useState } = wp.element;
    function PaymentFields({ eventRegistration, emitResponse }) {
        const [reader, setReader] = useState('');
        useEffect(() => {
            if (!eventRegistration) return;
            return eventRegistration.onPaymentSetup(() => ({
                type: emitResponse.responseTypes.SUCCESS,
                meta: { paymentMethodData: { paykit_reader: reader } },
            }));
        }, [reader, eventRegistration, emitResponse]);
        return el('div', null,
            el('p', null, 'Pay with a Bitcoin or Lightning wallet.'),
            el('label', null, 'Pubky ID for a private payment request (optional)',
                el('input', { type: 'text', value: reader, maxLength: 57, autoComplete: 'off',
                    onChange: (event) => setReader(event.target.value) })));
    }
    wc.wcBlocksRegistry.registerPaymentMethod({
        name: 'paykit_checkout', label: 'Bitcoin', ariaLabel: 'Pay with Bitcoin',
        content: el(PaymentFields), edit: el('p', null, 'Pay with a Bitcoin or Lightning wallet.'),
        canMakePayment: () => true, supports: { features: ['products'] },
    });
})();
