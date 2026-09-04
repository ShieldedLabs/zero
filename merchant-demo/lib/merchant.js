/**
 * [zero] @claude The merchant client: mint invoices, scan for payments, report status.
 *
 * The viewing key held here cannot spend. That is the property the whole design rests on —
 * a merchant service that is compromised leaks payment history, which is bad, but cannot
 * move funds.
 *
 * **The key must not reach a browser in production.** Anyone holding it can see every
 * payment the merchant has ever received, so shipping it to customers would hand each of
 * them the merchant's books. The demo runs it in a page because it has no server and no
 * real money; a real deployment keeps it behind the company's own API.
 */

import { DEFAULT_POLICY, settle, paymentUri } from './invoices.js';
import { MemoryStore } from './store.js';

/** Blocks requested per scan call. */
const BATCH = 500;

export class Merchant {
  /**
   * @param {object} opts
   * @param {object} opts.wasm         The initialised `zcash_merchant_core` module.
   * @param {string} opts.endpoint     lightwalletd URL. In a browser this must speak gRPC-Web.
   * @param {string} opts.viewingKey   Encoded Unified Incoming Viewing Key.
   * @param {string} [opts.network]    'main' or 'test'.
   * @param {object} [opts.store]      Storage; defaults to in-memory.
   * @param {import('./invoices.js').Policy} [opts.policy]
   * @param {number} [opts.lookback]   Blocks of history to scan on first run.
   */
  constructor({ wasm, endpoint, viewingKey, network = 'main', store, policy, lookback = 0 }) {
    this.wasm = wasm;
    this.endpoint = endpoint;
    this.viewingKey = viewingKey;
    this.network = network;
    this.store = store ?? new MemoryStore();
    this.policy = { ...DEFAULT_POLICY, ...(policy ?? {}) };
    // How far back the first scan reaches. Zero is right for a real merchant: there is
    // nothing to find before the first invoice existed. A demo wants a non-zero value so the
    // first scan does visible work instead of correctly finding nothing.
    this.lookback = lookback;
    /** Payments seen, keyed by invoice index. Rebuilt by rescanning; not authoritative. */
    this.seen = new Map();
    /** @type {((event: {type: string, invoice?: any, payment?: any}) => void)[]} */
    this.listeners = [];
    /** Next diversifier index to search from when minting. */
    this.nextIndex = 0;
  }

  /** @param {(event: {type: string, invoice?: any, payment?: any}) => void} fn */
  on(fn) {
    this.listeners.push(fn);
    return () => {
      this.listeners = this.listeners.filter((l) => l !== fn);
    };
  }

  #emit(event) {
    for (const l of this.listeners) l(event);
  }

  /** Current chain tip. */
  async tipHeight() {
    return await this.wasm.tipHeight(this.endpoint);
  }

  /**
   * Creates an invoice with its own address.
   *
   * @param {string} id            Merchant's order identifier.
   * @param {number} expectedZats  Amount expected, in zatoshis.
   */
  async createInvoice(id, expectedZats) {
    const tip = await this.tipHeight();
    // One address per invoice, never reused: reuse is what links a merchant's customers to
    // each other on a public chain, and it also makes two invoices indistinguishable.
    const [minted] = this.wasm.mintAddresses(
      this.network,
      this.viewingKey,
      this.nextIndex,
      1,
    );
    this.nextIndex = minted.index + 1;

    const invoice = {
      id,
      index: minted.index,
      address: minted.address,
      expectedZats,
      createdHeight: tip,
      expiresHeight: tip + this.policy.expiryBlocks,
      status: 'pending',
      paidZats: 0,
      txids: [],
      paidHeight: null,
    };
    await this.store.putInvoice(invoice);

    // A merchant that has never scanned starts from the first invoice rather than the
    // genesis block; there is nothing to find before it.
    if ((await this.store.getCursor()) === null) {
      await this.store.setCursor(Math.max(0, tip - this.lookback));
    }
    this.#emit({ type: 'invoice-created', invoice });
    return { ...invoice, uri: paymentUri(minted.address, expectedZats) };
  }

  /**
   * Scans everything since the last cursor and updates invoices.
   *
   * Safe to call repeatedly and safe to interrupt: the cursor only advances once a range has
   * been scanned, so a crash mid-scan re-reads that range rather than skipping it, and
   * re-reading cannot double-count.
   */
  async poll() {
    const tip = await this.tipHeight();
    let cursor = await this.store.getCursor();
    if (cursor === null) {
      await this.store.setCursor(Math.max(0, tip - this.lookback));
      return { scanned: 0, tip, cursor: await this.store.getCursor() };
    }

    const invoices = await this.store.allInvoices();
    /** @type {Record<string, number>} */
    const addresses = {};
    for (const inv of invoices) addresses[inv.address] = inv.index;

    let scanned = 0;
    while (cursor < tip) {
      const count = Math.min(BATCH, tip - cursor);
      const payments = await this.wasm.scanRange(
        this.endpoint,
        this.network,
        this.viewingKey,
        addresses,
        cursor + 1,
        count,
      );
      for (const p of payments) {
        if (p.index === undefined || p.index === null) {
          // Money for this key that did not land on an invoice address. Surfaced rather
          // than dropped, because silently ignoring received funds is worse than noise.
          this.#emit({ type: 'unattributed-payment', payment: p });
          continue;
        }
        const list = this.seen.get(p.index) ?? [];
        list.push(p);
        this.seen.set(p.index, list);
        this.#emit({ type: 'payment', payment: p });
      }
      cursor += count;
      scanned += count;
      await this.store.setCursor(cursor);
    }

    await this.#settleAll(tip);
    // `cursor` is returned so a caller can distinguish "caught up" from "not running" —
    // both report zero blocks scanned, and without the cursor they look identical.
    return { scanned, tip, cursor };
  }

  async #settleAll(tip) {
    for (const invoice of await this.store.allInvoices()) {
      const before = invoice.status;
      const updated = settle(invoice, this.seen.get(invoice.index) ?? [], tip, this.policy);
      if (
        updated.status !== before ||
        updated.paidZats !== invoice.paidZats ||
        updated.txids.length !== invoice.txids.length
      ) {
        await this.store.putInvoice(updated);
        this.#emit({ type: 'invoice-updated', invoice: updated });
      }
    }
  }

  /**
   * Injects a payment as though the chain had carried one. Demo only.
   *
   * The payment is pushed into the same structure the scanner fills, so everything after
   * that point is the real code: confirmation counting, underpayment tolerance, the invoice
   * state machine, and whatever the customer's page does with the result. What it skips is
   * the chain read and the trial decryption — those are covered by tests, and waiting for
   * someone to send real ZEC is not a way to look at a checkout flow.
   *
   * @param {string} orderId
   * @param {number} [zatoshis]  Defaults to the full amount owed.
   * @param {object} [opts]
   * @param {boolean} [opts.confirmed]  Land it far enough back to be final. Default true.
   */
  async simulatePayment(orderId, zatoshis, { confirmed = true } = {}) {
    const invoices = await this.store.allInvoices();
    const invoice = invoices.find((i) => i.id === orderId);
    if (!invoice) throw new Error(`no such invoice: ${orderId}`);

    const tip = await this.tipHeight();
    // Backdated by the confirmation depth when it should count immediately; otherwise put it
    // at the tip, where it is visible but not yet final — the state a customer sees for the
    // first few minutes after paying, and the one most worth being able to look at.
    const height = confirmed ? tip - this.policy.confirmations + 1 : tip;
    const payment = {
      txid: `simulated-${orderId}-${(this.seen.get(invoice.index) ?? []).length}`,
      zatoshis: zatoshis ?? invoice.expectedZats,
      height,
      simulated: true,
    };

    const list = this.seen.get(invoice.index) ?? [];
    list.push(payment);
    this.seen.set(invoice.index, list);
    this.#emit({ type: 'payment', payment });
    await this.#settleAll(tip);
    return payment;
  }

  /** Polls every `ms`; returns a function that stops it. */
  watch(ms = 15000) {
    let stopped = false;
    const tick = async () => {
      if (stopped) return;
      try {
        await this.poll();
      } catch (e) {
        this.#emit({ type: 'error', payment: String(e?.message ?? e) });
      }
      if (!stopped) setTimeout(tick, ms);
    };
    tick();
    return () => {
      stopped = true;
    };
  }
}

export { paymentUri, COIN } from './invoices.js';
export { MemoryStore } from './store.js';
