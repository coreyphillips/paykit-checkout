# Verify a merchant destination

A watch-only checkout keeps Bitcoin spending keys off the web host. If someone controls that host, they can still replace the Bitcoin address on a page. An independent merchant signer and a buyer who already trusts the merchant's Pubky public key can verify the destination before paying.

This flow covers the guest on-chain address. It requires an explicit verifier. Scanning the ordinary checkout QR in a wallet performs the wallet's usual Bitcoin URI checks. Private Paykit requests and Lightning use their own protocol authentication.

## Merchant signing

1. Keep the merchant's Ed25519 identity signing seed on an independent machine. The signer accepts a file containing the 32-byte seed encoded as unpadded base64url. Set `CHECKOUT_MERCHANT_PUBKY` on the web process to the corresponding public identity in `pubky...` form. Distribute this identity to buyers through an independent channel.
2. Read the invoice's network, ID, amount, address, expiry and address index using the merchant API. On the signing machine, check the amount and order against the merchant's own order system. Supply JSON with the following fields in a file named `terms.json`. Use the actual unexpired terms from the invoice.

   ```json
   {"invoice_id":"0123456789abcdef0123456789abcdef","network":"bitcoin","amount_sats":50000,"address":"REPLACE_WITH_INVOICE_ADDRESS","expires_at":1800000000}
   ```

3. Set `MERCHANT_ACCOUNT_XPUB` to the public account key from the merchant's own wallet and `ADDRESS_INDEX` to the index from the API. Sign the terms. The signer rederives the address before signing.

   ```sh
   paykit-checkout --network mainnet sign --terms terms.json \
     --account-xpub "$MERCHANT_ACCOUNT_XPUB" --index "$ADDRESS_INDEX" \
     --identity-key-file /private/merchant-identity > proof.json
   ```

4. With the merchant API token, POST the contents of `proof.json` as JSON to `/v1/invoices/{id}/attestation`. The server checks the configured identity and exact stored terms. The checkout then links to the downloadable proof at `/checkout/{id}/attestation`.

## Buyer verification

Download the proof. On a trusted device, supply the merchant's previously known public key, expected invoice ID, network and amount. The verifier checks the signature, expiry, network, identity and purchase terms, then prints the receiving address.

```sh
paykit-checkout --network mainnet verify --proof proof.json \
  --merchant "$TRUSTED_MERCHANT_PUBKY" --invoice-id "$EXPECTED_INVOICE_ID" \
  --amount-sats 50000
```

Pay the verified address from your wallet. Treat a missing proof, verification error, or a different destination as an unresolved payment instruction.
