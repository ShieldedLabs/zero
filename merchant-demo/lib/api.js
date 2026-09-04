/**
 * [zero] @claude The boundary between the customer and the merchant.
 *
 * In a real deployment the customer's browser calls the company's HTTP API, and the viewing
 * key lives on the far side of that call. The demo has no server, so the boundary is a
 * `BroadcastChannel` between two pages instead.
 *
 * That substitution is not cosmetic. The checkout page cannot reach the merchant page's
 * variables, so it genuinely cannot obtain the viewing key — the same property the HTTP
 * boundary gives you, enforced by the browser rather than by convention. Swapping this file
 * for `fetch` calls is the entire difference between the demo and a deployment.
 *
 * Messages are the API surface a real one would expose:
 *   createInvoice { orderId, zatoshis } -> { id, address, uri, qr, expectedZats, expiresHeight }
 *   getInvoice    { orderId }           -> invoice or null
 */

const CHANNEL = 'zcash-merchant-demo';

/** Serves API requests. Runs in the merchant page, which holds the key. */
export class MerchantApiServer {
  /**
   * @param {object} handlers
   * @param {(orderId: string, zatoshis: number) => Promise<any>} handlers.createInvoice
   * @param {(orderId: string) => Promise<any>} handlers.getInvoice
   */
  constructor(handlers) {
    this.handlers = handlers;
    this.channel = new BroadcastChannel(CHANNEL);
    this.channel.onmessage = (ev) => this.#serve(ev.data);
  }

  async #serve(msg) {
    if (!msg || msg.kind !== 'request') return;
    let reply;
    try {
      const handler = this.handlers[msg.method];
      if (!handler) throw new Error(`no such method: ${msg.method}`);
      reply = { kind: 'response', id: msg.id, ok: true, body: await handler(...msg.args) };
    } catch (e) {
      reply = { kind: 'response', id: msg.id, ok: false, error: String(e?.message ?? e) };
    }
    this.channel.postMessage(reply);
  }

  close() {
    this.channel.close();
  }
}

/** Calls the API. Runs in the checkout page, which holds no key. */
export class MerchantApiClient {
  constructor(timeoutMs = 8000) {
    this.channel = new BroadcastChannel(CHANNEL);
    this.timeoutMs = timeoutMs;
    this.pending = new Map();
    this.channel.onmessage = (ev) => {
      const msg = ev.data;
      if (!msg || msg.kind !== 'response') return;
      const entry = this.pending.get(msg.id);
      if (!entry) return;
      this.pending.delete(msg.id);
      clearTimeout(entry.timer);
      msg.ok ? entry.resolve(msg.body) : entry.reject(new Error(msg.error));
    };
  }

  #call(method, ...args) {
    const id = crypto.randomUUID();
    return new Promise((resolve, reject) => {
      // A merchant page that is closed looks exactly like one that is slow. Time out so the
      // checkout can say "the shop is not reachable" rather than hanging.
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error('the merchant service did not respond'));
      }, this.timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      this.channel.postMessage({ kind: 'request', id, method, args });
    });
  }

  /** @param {string} orderId @param {number} zatoshis */
  createInvoice(orderId, zatoshis) {
    return this.#call('createInvoice', orderId, zatoshis);
  }

  /** @param {string} orderId */
  getInvoice(orderId) {
    return this.#call('getInvoice', orderId);
  }

  close() {
    this.channel.close();
  }
}
