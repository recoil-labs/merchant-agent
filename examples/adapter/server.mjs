// A payout adapter for the RecoilPay Merchant Agent: the template to copy.
//
// The agent calls this service to look accounts up, send payouts, ask what
// happened to a payout, and read the balance (the adapter protocol,
// docs/adapters.md). This file speaks the protocol and keeps the one rule
// that makes paying safe: a payout key is paid at most once, ever.
//
// It runs in TEST MODE as written: an in-memory bank with test accounts,
// kept in a JSON file. To connect your real bank or payout API, replace the
// four functions in "YOUR BANK" below, and keep everything else.
//
//   ADAPTER_TOKEN=$(openssl rand -hex 32) node server.mjs
//
// Settings (environment variables):
//   ADAPTER_TOKEN    required; the agent sends it as a bearer token
//   PORT, HOST       default 8750, 127.0.0.1
//   ADAPTER_LEDGER   where payouts are kept; default ./adapter-ledger.json
//   TLS_CERT, TLS_KEY  PEM files: serve HTTPS (production requires it,
//                    or put this behind your TLS proxy)
//
// Plain Node 20+, no dependencies.

import { createHash, timingSafeEqual } from 'node:crypto';
import { existsSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import https from 'node:https';

const TOKEN = process.env.ADAPTER_TOKEN ?? '';
if (TOKEN.length < 16) throw new Error('set ADAPTER_TOKEN to a long random value (openssl rand -hex 32)');
const PORT = Number(process.env.PORT ?? 8750);
const HOST = process.env.HOST ?? '127.0.0.1';
const LEDGER = process.env.ADAPTER_LEDGER ?? './adapter-ledger.json';

// ── YOUR BANK ─────────────────────────────────────────────────────────────
// Replace these four with calls to your bank or payout API. Each may throw
// when the bank can't be reached: the adapter then answers 503 and the agent
// asks again later. Never throw after the bank may have moved money without
// recording it first (see sendPayout).

const TEST_ACCOUNTS = new Map([
  ['058/0000000001', 'Test Payee One'],
  ['058/0000000002', 'Test Payee Two'],
]);
const testBank = { balance: 1_000_000_00n }; // ₦1,000,000, in kobo

/** The account holder's name, or null if the account doesn't exist. */
async function bankLookup(bankCode, accountNumber) {
  return TEST_ACCOUNTS.get(`${bankCode}/${accountNumber}`) ?? null;
}

/**
 * Pay. Return { ok: true, reference } once the bank has the transfer, or
 * { ok: false, reason } if it refused and no money moved. Pass `key` to your
 * bank as its idempotency key or reference where it takes one.
 */
async function bankTransfer({ key, amountMinor, bankCode, accountNumber, accountName, narration }) {
  if (!TEST_ACCOUNTS.has(`${bankCode}/${accountNumber}`)) return { ok: false, reason: 'the account does not exist' };
  if (amountMinor > testBank.balance) return { ok: false, reason: 'insufficient funds' };
  testBank.balance -= amountMinor;
  return { ok: true, reference: `TEST-${createHash('sha256').update(key).digest('hex').slice(0, 12).toUpperCase()}` };
}

/** What the bank says now about a transfer it accepted: 'succeeded' | 'pending' | 'failed'. */
async function bankTransferStatus(reference) {
  return { state: 'succeeded' };
}

/** Available balance in kobo (a BigInt), or null if the bank can't say. */
async function bankBalance() {
  return testBank.balance;
}

// ── THE PROTOCOL (keep as it is) ─────────────────────────────────────────

/** key → { reference, amountMinor, state, reason } */
const payouts = new Map();
if (existsSync(LEDGER)) {
  const saved = JSON.parse(readFileSync(LEDGER, 'utf8'));
  for (const [k, v] of Object.entries(saved.payouts)) payouts.set(k, v);
  if (saved.testBalance) testBank.balance = BigInt(saved.testBalance);
}
function save() {
  const tmp = `${LEDGER}.tmp`;
  writeFileSync(tmp, JSON.stringify({ payouts: Object.fromEntries(payouts), testBalance: testBank.balance.toString() }), { mode: 0o600 });
  renameSync(tmp, LEDGER);
}

const KEY = /^[a-z0-9_-]{8,100}$/;
/** Keys being sent right now: a second request for one waits for the first. */
const inFlight = new Map();

async function sendPayout(body) {
  const { key, amount_minor, currency, bank_code, account_number, account_name, narration } = body;
  if (!KEY.test(key ?? '') || !/^\d+$/.test(amount_minor ?? '')) return [400, { error: 'bad request' }];
  if (currency !== 'NGN') return [200, { result: 'rejected', reason: 'only NGN' }];

  // The rule: one key, one payout. A key seen before gets its first answer.
  if (inFlight.has(key)) await inFlight.get(key).catch(() => {});
  const known = payouts.get(key);
  if (known) {
    if (known.state === 'rejected') return [200, { result: 'rejected', reason: known.reason }];
    // Recorded, but the bank's answer was lost (a crash mid-call): only a
    // person, checking the bank, can say. Never send it again.
    if (known.state === 'unknown') return [200, { result: 'unknown' }];
    return [200, { result: 'accepted', reference: known.reference }];
  }

  const work = (async () => {
    // Recorded BEFORE the bank is asked: if this process dies mid-call, the
    // key is known as "unknown", never retried blindly.
    payouts.set(key, { state: 'unknown', amountMinor: amount_minor });
    save();
    const out = await bankTransfer({ key, amountMinor: BigInt(amount_minor), bankCode: bank_code, accountNumber: account_number, accountName: account_name, narration });
    payouts.set(key, out.ok
      ? { state: 'pending', reference: out.reference, amountMinor: amount_minor }
      : { state: 'rejected', reason: out.reason, amountMinor: amount_minor });
    save();
    return out;
  })();
  inFlight.set(key, work);
  try {
    const out = await work;
    return [200, out.ok ? { result: 'accepted', reference: out.reference } : { result: 'rejected', reason: out.reason }];
  } catch (e) {
    // The bank call failed in a way that may have moved money: say so.
    return [200, { result: 'unknown' }];
  } finally {
    inFlight.delete(key);
  }
}

async function payoutStatus(key) {
  if (!KEY.test(key)) return [400, { error: 'bad key' }];
  if (inFlight.has(key)) await inFlight.get(key).catch(() => {});
  const p = payouts.get(key);
  if (!p) return [200, { result: 'not_found' }];
  if (p.state === 'rejected') return [200, { result: 'failed', reason: p.reason }];
  if (p.state === 'unknown') return [200, { result: 'unknown' }];
  const now = await bankTransferStatus(p.reference);
  if (now.state !== p.state) {
    payouts.set(key, { ...p, state: now.state, reason: now.reason });
    save();
  }
  if (now.state === 'succeeded') return [200, { result: 'succeeded', reference: p.reference }];
  if (now.state === 'failed') return [200, { result: 'failed', reason: now.reason ?? 'the bank says it failed' }];
  return [200, { result: 'pending' }];
}

function authorised(req) {
  const got = Buffer.from(req.headers.authorization ?? '');
  const want = Buffer.from(`Bearer ${TOKEN}`);
  return got.length === want.length && timingSafeEqual(got, want);
}

async function readJson(req) {
  let size = 0;
  const chunks = [];
  for await (const c of req) {
    size += c.length;
    if (size > 16_384) throw new Error('too large');
    chunks.push(c);
  }
  return JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}');
}

async function handle(req, res) {
  const reply = (code, body) => {
    res.writeHead(code, { 'content-type': 'application/json' });
    res.end(JSON.stringify(body));
  };
  if (!authorised(req)) return reply(401, { error: 'unauthorised' });
  const url = new URL(req.url, 'http://adapter');
  try {
    if (req.method === 'GET' && url.pathname === '/v1/info') {
      return reply(200, { protocol: 1, name: 'node example adapter (test mode)', idempotent_send: true });
    }
    if (req.method === 'POST' && url.pathname === '/v1/lookup') {
      const { bank_code, account_number } = await readJson(req);
      const name = await bankLookup(bank_code, account_number);
      return reply(200, name ? { result: 'found', account_name: name } : { result: 'not_found' });
    }
    if (req.method === 'POST' && url.pathname === '/v1/payouts') {
      const payout = await readJson(req);
      const [code, body] = await sendPayout(payout);
      // Never log account numbers or names: the key, amount and result only.
      console.log(JSON.stringify({ at: new Date().toISOString(), key: payout.key, amount_minor: payout.amount_minor, result: body.result ?? code }));
      return reply(code, body);
    }
    const m = /^\/v1\/payouts\/([^/]+)$/.exec(url.pathname);
    if (req.method === 'GET' && m) return reply(...(await payoutStatus(decodeURIComponent(m[1]))));
    if (req.method === 'GET' && url.pathname === '/v1/balance') {
      const b = await bankBalance();
      return reply(200, { currency: 'NGN', available_minor: b === null ? null : b.toString() });
    }
    return reply(404, { error: 'not found' });
  } catch (e) {
    // The bank couldn't be asked: the agent will ask again.
    return reply(503, { error: 'unavailable' });
  }
}

const tls = process.env.TLS_CERT && process.env.TLS_KEY
  ? { cert: readFileSync(process.env.TLS_CERT), key: readFileSync(process.env.TLS_KEY) }
  : null;
const server = tls ? https.createServer(tls, handle) : http.createServer(handle);
server.listen(PORT, HOST, () => {
  console.log(`payout adapter on ${tls ? 'https' : 'http'}://${HOST}:${PORT} (test mode; ledger ${LEDGER})`);
});
