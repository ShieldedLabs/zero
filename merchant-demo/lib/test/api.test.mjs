// [zero] @claude The customer/merchant boundary. Node has BroadcastChannel, so the same
// request/response protocol the two pages use can be exercised without a browser.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { MerchantApiClient, MerchantApiServer } from '../api.js';

test('the checkout can create and read an invoice through the API', async () => {
  const invoices = new Map();
  const server = new MerchantApiServer({
    createInvoice: async (orderId, zatoshis) => {
      const inv = { id: orderId, address: 'zs1demo', expectedZats: zatoshis, status: 'pending', paidZats: 0 };
      invoices.set(orderId, inv);
      return { ...inv, uri: `zcash:zs1demo?amount=0.01`, qr: '<svg/>' };
    },
    getInvoice: async (orderId) => invoices.get(orderId) ?? null,
  });
  const client = new MerchantApiClient(3000);

  const created = await client.createInvoice('order-9', 1_000_000);
  assert.equal(created.address, 'zs1demo');
  assert.equal(created.expectedZats, 1_000_000);
  assert.ok(created.qr.includes('svg'));

  const fetched = await client.getInvoice('order-9');
  assert.equal(fetched.status, 'pending');
  assert.equal(await client.getInvoice('nope'), null);

  client.close();
  server.close();
});

test('an error on the merchant side reaches the checkout as an error', async () => {
  const server = new MerchantApiServer({
    createInvoice: async () => { throw new Error('out of stock'); },
  });
  const client = new MerchantApiClient(3000);
  await assert.rejects(() => client.createInvoice('x', 1), /out of stock/);
  client.close();
  server.close();
});

test('an unreachable merchant times out rather than hanging', async () => {
  // The failure a customer actually hits: the shop is down. It must surface, not hang.
  const client = new MerchantApiClient(300);
  await assert.rejects(() => client.getInvoice('x'), /did not respond/);
  client.close();
});
