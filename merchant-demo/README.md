# `merchant-demo`

A merchant library for accepting Zcash, built on librustzcash compiled to wasm, plus a demo
site that uses it.

It recognises incoming payments and attributes them to orders. It **cannot spend** — the key
it holds is a viewing key, so a compromised merchant service leaks payment history but not
funds. That is the property the design is built around.

## Running the demo

```sh
cd merchant-demo/crate
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' \
  wasm-pack build --target web --out-dir ../www/pkg --release
cd .. && cp lib/*.js www/lib/ && cd www && python3 -m http.server 8735
```

Then open <http://127.0.0.1:8735/index.html>. Tests: `cd lib && node --test "test/*.test.mjs"`.

## Shape

```
crate/   Rust -> wasm. Keys, addresses, trial decryption, QR rendering. Nothing else.
lib/     The JavaScript library: invoices, policy, scan loop, storage, API boundary.
www/     Two pages: the merchant dashboard and the customer's checkout.
```

Open `index.html` for the merchant's side and `checkout.html` for the buyer's. The checkout
requests an invoice, shows a QR code, and polls for payment.

The split is deliberate. The wasm side is the part that must be Zcash-correct and rarely
changes. Everything a merchant will actually want to alter — confirmation depth, expiry,
what counts as underpayment, where invoices are stored — is JavaScript, and changing it does
not mean rebuilding wasm.

## How a payment is attributed to an order

Trial decryption returns the note **and the address it was sent to**. A viewing key can mint
unlimited addresses, so each invoice gets its own and the recipient identifies the invoice.
No memo, nothing required from the payer's wallet, and no second request to read one.

**One trap, and it is not obvious.** About half of all diversifier indices produce no valid
address, and the key API searches *forward* from the index it is given until it finds one
that does. Requesting indices 0..8 yields far fewer than eight distinct addresses. A merchant
that files an invoice under the index it *asked for* will give several invoices the same
address and be unable to tell their payments apart. `mintAddresses` returns the index that
was actually used; store that one.

## What is a policy decision, not a protocol rule

All of it lives in `lib/invoices.js`, and all of it is arguable:

| | default | why |
|---|---|---|
| confirmations | 10 | ~12 minutes; what exchanges typically use. Lower accepts reorg risk. |
| expiry | 40 blocks | ~50 minutes; long enough to pay, short enough that a price quote does not go stale. |
| underpayment tolerance | 1000 zats | dust shortfalls are usually a wallet's fee rounding, and rejecting them creates support tickets. |

`settle()` is pure — invoice plus payments plus height in, new invoice out. It can be tested
without a chain, and it recomputes totals from the transaction set rather than accumulating,
so **rescanning a range cannot double-count**. That matters because rescanning is the normal
way to recover from a crash.

## Why a scan can legitimately report zero

`createInvoice` starts the scan cursor at the current tip: there is nothing to find before
the first invoice existed. Zcash blocks arrive every 75 seconds or so, so a scan run
immediately afterwards has no work and correctly reports nothing — which is indistinguishable
from a broken scanner unless the UI says how far it has got. `poll()` returns the cursor
alongside the count for exactly that reason, and the demo shows `scanned to N · up to date`.

The demo passes `lookback: 200` so its first scan covers real history and visibly does
something. A production merchant leaves it at zero.

## Storage

`lib/store.js` is an interface with a throwaway in-memory default. The state is one invoice
row and one scan cursor; it belongs in whatever database the company already runs, not in one
this library imposes. Implement three methods against Postgres and you are done.

## The two pages, and why they are two

`index.html` is the merchant: it holds the viewing key and scans. `checkout.html` is the
customer: it holds nothing and asks the merchant for an invoice.

They talk over a `BroadcastChannel`, which stands in for the company's HTTP API. That
substitution is not cosmetic — the checkout runs in its own page and *cannot* read the
merchant's variables, so it genuinely cannot obtain the viewing key. The browser enforces
the same boundary an HTTP call would, rather than the demo asking you to imagine it.
Replacing `lib/api.js` with `fetch` calls is the whole difference between this and a
deployment.

The QR is rendered in Rust from the same URI the customer is told to pay, by the same code
that produced the address. Generating them separately is how a QR ends up pointing somewhere
the displayed address does not.

## Simulating a payment

The checkout has demo controls pinned to the bottom-right corner — pay in full, pay half,
pay unconfirmed. They are fixed there so the checkout itself can be screenshotted without
them, and they call `simulatePayment` on the merchant.

The simulation injects a payment into the same structure the scanner fills, so everything
downstream is the real code: confirmation counting, underpayment tolerance, the invoice state
machine, and what the customer's page does with the result. It skips the chain read and the
trial decryption, which are covered by tests instead.

## Deployment, and the one rule

**The viewing key must not reach a browser in production.** Anyone holding it can read every
payment the merchant has ever received. The demo page runs it client-side because it has no
server and no real money; a real deployment keeps it behind the company's own API and the
frontend only ever sees invoice status.

Server-side (Node) needs no gRPC-Web proxy — Node can speak native gRPC, so the transport can
point at any existing lightwalletd. In a browser a gRPC-Web endpoint is required, because
browsers cannot read the HTTP trailers gRPC reports status in.

## Measured

In Chrome, against ChainSafe's public gRPC-Web proxy on mainnet:

- 400 blocks scanned in **0.69 s** (~4,500 outputs/second trial decryption)
- checkout to confirmed payment, across the API boundary, driven in Chrome
- invoice creation, address minting, attribution: instant
- wasm module: **0.70 MB**, including QR rendering



Zcash blocks are ~75 seconds apart, so keeping up with the chain is not a concern; the
constraint is how fast you can backfill, and a year is roughly ten minutes.

## Not built

Refunds and payouts (sending needs the proving path — seconds per transaction, see
`librustzcash/WASM.md`), transparent-address payments, price quoting, webhook delivery with
retries, and reorg rollback. The confirmation threshold defends against reorgs but nothing
un-marks an invoice if one happens after it settled.
