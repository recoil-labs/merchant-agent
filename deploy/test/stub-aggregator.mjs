#!/usr/bin/env node
// A stand-in for the RecoilPay aggregator, for the image smoke tests only.
// It answers the routes `doctor` and a quiet `run` call (health, chains,
// the operator's quotes, payee key, trades and heartbeat) with the shapes
// pinned in vectors/merchant-agent-api.json, for one operator, and has no
// trades. Never ship it, and never point a production agent at it.
//
//   STUB_PORT=3999 STUB_SOLVER_ID=solver-… STUB_API_KEY=… STUB_PAYEE_PUB=0x04… \
//     node deploy/test/stub-aggregator.mjs
//
// The end-to-end tests against the real aggregator live in the RecoilPay
// repo (recoilpay-intent-kit/e2e/run-merchant-agent.sh).
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';

const fixture = JSON.parse(
  readFileSync(new URL('../../vectors/merchant-agent-api.json', import.meta.url), 'utf8'),
);
const port = Number(process.env.STUB_PORT ?? 3999);
const host = process.env.STUB_HOST ?? '0.0.0.0';
const solverId = must('STUB_SOLVER_ID');
const apiKey = must('STUB_API_KEY');
const payeePub = must('STUB_PAYEE_PUB');
let quotes = [];

function must(name) {
  const v = process.env[name];
  if (!v) throw new Error(`set ${name}`);
  return v;
}

function send(res, status, body) {
  if (body === undefined) {
    res.writeHead(status).end();
    return;
  }
  res.writeHead(status, { 'content-type': 'application/json' }).end(JSON.stringify(body));
}

function error(res, status, code, message) {
  send(res, status, { ...fixture.error, error: code, message, timestamp: Math.floor(Date.now() / 1000) });
}

createServer((req, res) => {
  const url = new URL(req.url, 'http://stub');
  const path = url.pathname;
  let body = '';
  req.on('data', (c) => (body += c));
  req.on('end', () => {
    if (req.method === 'GET' && path === '/health') return send(res, 200, { status: 'healthy' });
    if (req.method === 'GET' && path === '/api/v1/chains') return send(res, 200, fixture.chains);
    if (req.method === 'GET' && path.startsWith('/api/v1/fiat/trades/')) {
      return error(res, 404, 'NOT_FOUND', 'no such trade');
    }

    const m = path.match(/^\/solver-api\/operators\/([^/]+)\/(.+)$/);
    if (!m) return error(res, 404, 'NOT_FOUND', 'unknown route');
    if (req.headers['x-api-key'] !== apiKey || m[1] !== solverId) {
      return error(res, 401, 'UNAUTHORIZED', 'invalid API key');
    }
    const route = `${req.method} ${m[2]}`;
    if (route === 'GET fiat-quotes') return send(res, 200, { ...fixture.fiatQuotes, quotes });
    if (route === 'PUT fiat-quotes') {
      // The real aggregator assigns new ids; the stub only ever sees the
      // empty book a pause writes, so it keeps what it is given.
      quotes = (JSON.parse(body || '{}').quotes ?? []);
      return send(res, 200, { ...fixture.fiatQuotes, quotes });
    }
    if (route === 'GET fiat-payee-key') return send(res, 200, { publicKey: payeePub });
    if (route === 'GET fiat-trades') return send(res, 200, []);
    if (route === 'POST heartbeat') return send(res, 204);
    return error(res, 404, 'NOT_FOUND', 'unknown route');
  });
}).listen(port, host, () => console.log(`stub aggregator on ${host}:${port} for ${solverId}`));
