// [zero] @claude Policy tests. No chain, no network: `settle` is pure, which is the point.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { DEFAULT_POLICY, settle, paymentUri, COIN } from '../invoices.js';

const base = {
  id: 'order-1', index: 4, address: 'zs1demo', expectedZats: 100_000,
  createdHeight: 1000, expiresHeight: 1040,
  status: 'pending', paidZats: 0, txids: [], paidHeight: null,
};
const P = DEFAULT_POLICY;

test('unpaid and unexpired stays pending', () => {
  assert.equal(settle(base, [], 1010, P).status, 'pending');
});

test('unpaid past expiry becomes expired', () => {
  assert.equal(settle(base, [], 1041, P).status, 'expired');
});

test('a payment with too few confirmations does not count yet', () => {
  const pay = [{ txid: 'a', zatoshis: 100_000, height: 1005 }];
  // 1005..1009 is five blocks; the policy wants ten.
  const out = settle(base, pay, 1009, P);
  assert.equal(out.status, 'pending');
  assert.equal(out.paidZats, 0);
});

test('a payment with enough confirmations settles', () => {
  const pay = [{ txid: 'a', zatoshis: 100_000, height: 1005 }];
  const out = settle(base, pay, 1014, P);
  assert.equal(out.status, 'paid');
  assert.equal(out.paidZats, 100_000);
  assert.equal(out.paidHeight, 1005);
});

test('a shortfall inside tolerance still counts as paid', () => {
  const pay = [{ txid: 'a', zatoshis: 99_500, height: 1005 }];
  assert.equal(settle(base, pay, 1014, P).status, 'paid');
});

test('a real shortfall is underpaid, not paid', () => {
  const pay = [{ txid: 'a', zatoshis: 60_000, height: 1005 }];
  const out = settle(base, pay, 1014, P);
  assert.equal(out.status, 'underpaid');
  assert.equal(out.paidZats, 60_000);
});

test('two partial payments add up', () => {
  const pay = [
    { txid: 'a', zatoshis: 60_000, height: 1005 },
    { txid: 'b', zatoshis: 40_000, height: 1006 },
  ];
  assert.equal(settle(base, pay, 1016, P).status, 'paid');
});

test('rescanning the same payment does not double-count', () => {
  // The failure this guards against: a crash mid-scan means the range is scanned again, and
  // an implementation that accumulated a running total would mark an invoice overpaid.
  const once = [{ txid: 'a', zatoshis: 100_000, height: 1005 }];
  const twice = [...once, { txid: 'a', zatoshis: 100_000, height: 1005 }];
  assert.equal(settle(base, twice, 1014, P).paidZats, settle(base, once, 1014, P).paidZats);
  assert.equal(settle(base, twice, 1014, P).txids.length, 1);
});

test('expiry does not override a payment that already landed', () => {
  const pay = [{ txid: 'a', zatoshis: 100_000, height: 1005 }];
  assert.equal(settle(base, pay, 2000, P).status, 'paid');
});

test('payment uri carries address and amount', () => {
  const uri = paymentUri('zs1demo', COIN / 2);
  assert.equal(uri, 'zcash:zs1demo?amount=0.5');
});
