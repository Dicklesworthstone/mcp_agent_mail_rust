#!/usr/bin/env node
/**
 * Real-process/HTTP/SQLite surrogate used ONLY to validate the GH407 harness.
 * This is not Agent Mail and uses Node's system SQLite, not FrankenSQLite.
 * It provides positive and deliberately broken controls for harness behavior.
 */
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';

const MODES = new Set(['healthy', 'idle_refresh_missing', 'false_ready',
  'false_capabilities', 'polling_only_recovery', 'ignore_term', 'blocked_fault',
  'ineffective_fault', 'auth_only_fault', 'malformed_health', 'restore_failed']);

function argument(argv, flag, fallback) {
  const index = argv.indexOf(flag);
  return index >= 0 ? argv[index + 1] : fallback;
}

function query(database, sql) {
  const connection = new DatabaseSync(database);
  try {
    return connection.prepare(sql).all();
  } finally {
    connection.close();
  }
}

function execute(database, sql) {
  const connection = new DatabaseSync(database);
  try {
    connection.exec(sql);
  } finally {
    connection.close();
  }
}

export async function fixtureMain(mode, argv) {
  if (!MODES.has(mode)) throw new Error(`Unknown fixture mode: ${mode}`);
  if (argv.length === 1 && ['--version', '-V'].includes(argv[0])) {
    console.log(`GH407_HARNESS_SURROGATE 0 (${mode}); NOT AN AGENT MAIL BUILD`);
    return;
  }
  if (argv[0] === 'tooling') {
    const database = argument(argv, '--db');
    const sql = argument(argv, '--sql');
    try {
      if (!database || !sql) throw new Error('fixture requires --db and --sql');
      if (argv[1] === 'db-query') {
        console.log(JSON.stringify(query(database, sql)));
      } else if (argv[1] === 'db-exec') {
        if (/ALTER\s+TABLE\s+projects\s+RENAME\s+TO\s+gh407_saved_projects/i.test(sql)) {
          if (mode === 'blocked_fault') throw new Error('fixture fault setup explicitly refused');
          if (mode === 'ineffective_fault') {
            console.log(JSON.stringify({ ok: true, db: database }));
            return;
          }
        }
        if (mode === 'restore_failed' && /ALTER\s+TABLE\s+gh407_saved_projects/i.test(sql)) {
          throw new Error('fixture database restore failed');
        }
        execute(database, sql);
        console.log(JSON.stringify({ ok: true, db: database }));
      } else {
        throw new Error(`unknown fixture tooling command: ${argv[1]}`);
      }
    } catch (error) {
      console.error(`fixture database operation failed: ${error.message}`);
      process.exitCode = 1;
    }
    return;
  }
  if (argv[0] !== 'serve-http') throw new Error('fixture supports serve-http and tooling only');
  const root = process.env.STORAGE_ROOT;
  const rawUrl = process.env.DATABASE_URL;
  if (!root || !rawUrl?.startsWith('sqlite://')) throw new Error('explicit isolated mailbox required');
  const database = rawUrl.slice('sqlite://'.length);
  const host = argument(argv, '--host', process.env.HTTP_HOST || '127.0.0.1');
  const port = Number(argument(argv, '--port', process.env.HTTP_PORT));
  if (!Number.isInteger(port) || port <= 0) throw new Error('explicit loopback port required');
  fs.mkdirSync(root, { recursive: true });
  fs.mkdirSync(path.dirname(database), { recursive: true });
  execute(database, `
    CREATE TABLE IF NOT EXISTS projects (
      id INTEGER PRIMARY KEY, slug TEXT NOT NULL, human_key TEXT UNIQUE NOT NULL,
      created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
    );
    CREATE TABLE IF NOT EXISTS agents (id INTEGER PRIMARY KEY);
    CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, project_id INTEGER, sender_id INTEGER);
    CREATE TABLE IF NOT EXISTS message_recipients (id INTEGER PRIMARY KEY);
  `);
  let sample = { ok: true, observed: performance.now() };
  let everFailed = false;
  let acquires = 0;
  const events = path.join(root, 'harness_surrogate_requests.jsonl');

  function probe(fromRequest = false) {
    const observed = performance.now();
    let okay = true;
    try {
      query(database, 'SELECT id FROM projects LIMIT 1');
    } catch (error) {
      okay = false;
      everFailed = true;
      console.log(new Date().toISOString(), 'serving-pool readiness probe failed:', error.message);
    }
    acquires += 1;
    if (!(mode === 'polling_only_recovery' && everFailed && okay && !fromRequest)) {
      sample = { ok: okay, observed };
    }
  }

  const ticker = setInterval(() => {
    if (mode !== 'idle_refresh_missing') probe();
  }, 2000);

  function reply(response, status, object) {
    const data = Buffer.from(JSON.stringify(object));
    response.writeHead(status, { 'Content-Type': 'application/json',
      'Content-Length': data.length, Connection: 'close' });
    response.end(data);
  }

  const server = http.createServer(async (request, response) => {
    fs.appendFileSync(events, JSON.stringify({ method: request.method, path: request.url,
      monotonic_ms: performance.now(), monotonic_seconds: Number(process.hrtime.bigint()) / 1e9,
      wall_time: new Date().toISOString() }) + '\n');
    if (request.method === 'GET') {
      if (['/healthz', '/health/liveness'].includes(request.url)) {
        reply(response, 200, { status: 'alive' });
        return;
      }
      if (!['/health', '/health/readiness'].includes(request.url)) {
        reply(response, 404, { error: 'fixture route absent' });
        return;
      }
      if (mode === 'malformed_health') {
        response.writeHead(200, { 'Content-Type': 'application/json', Connection: 'close' });
        response.end('{broken');
        return;
      }
      const stale = performance.now() - sample.observed > 6000;
      const ready = mode === 'false_ready' || (sample.ok && !stale);
      const body = {
        status: ready ? 'ready' : 'unavailable',
        capabilities: { mail_read: ready || mode === 'false_capabilities',
          mail_write: ready, lexical_search: false },
        lexical_state: 'disabled', lexical_fresh: false, durability_state: 'not_probed',
      };
      if (!ready) body.reason = stale ? 'database_probe_stale' : 'database_probe_failed';
      reply(response, ready ? 200 : 503, body);
      if (['idle_refresh_missing', 'polling_only_recovery'].includes(mode)) {
        setTimeout(() => probe(true), 10);
      }
      return;
    }
    if (request.method !== 'POST' || request.url !== '/mcp/') {
      reply(response, 404, { error: 'fixture route absent' });
      return;
    }
    const expected = process.env.HTTP_BEARER_TOKEN;
    if (expected && request.headers.authorization !== `Bearer ${expected}`) {
      reply(response, 401, { error: 'fixture authentication failed' });
      return;
    }
    let chunks = [];
    let length = 0;
    try {
      for await (const chunk of request) {
        length += chunk.length;
        if (length > 1024 * 1024) throw new Error('oversized fixture request');
        chunks.push(chunk);
      }
      const rpc = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      chunks = [];
      if (rpc.id === undefined) {
        response.writeHead(202, { 'Content-Length': 0, Connection: 'close' });
        response.end();
        return;
      }
      let result;
      if (rpc.method === 'initialize') {
        result = { protocolVersion: rpc.params?.protocolVersion || '2024-11-05',
          capabilities: { tools: {}, resources: {} },
          serverInfo: { name: 'GH407_HARNESS_SURROGATE', version: '0' } };
      } else if (rpc.method === 'tools/call' && rpc.params?.name === 'ensure_project') {
        const key = rpc.params.arguments?.human_key;
        if (typeof key !== 'string' || !path.isAbsolute(key)) {
          reply(response, 200, { jsonrpc: '2.0', id: rpc.id,
            error: { code: -32602, message: 'human_key must be absolute' } });
          return;
        }
        let connection;
        try {
          connection = new DatabaseSync(database);
          connection.prepare('INSERT OR IGNORE INTO projects(slug,human_key) VALUES (?,?)')
            .run(path.basename(key), key);
          const value = connection.prepare('SELECT * FROM projects WHERE human_key=?').get(key);
          result = { content: [{ type: 'text', text: JSON.stringify(value) }],
            structuredContent: value, isError: false };
        } catch (error) {
          if (mode === 'auth_only_fault') {
            reply(response, 401, { error: 'fixture auth failure, not database evidence' });
            return;
          }
          result = { content: [{ type: 'text', text: `database query failed: ${error.message}` }],
            isError: true };
        } finally {
          connection?.close();
        }
      } else if (rpc.method === 'resources/read' && rpc.params?.uri === 'resource://tooling/metrics_core') {
        result = { contents: [{ uri: rpc.params.uri, mimeType: 'application/json',
          text: JSON.stringify({ metrics: { db: { pool_acquires_total: acquires,
            pool_pending_requests: 0, pool_connections_active: 0,
            pool_connections_idle: 1, pool_connections_total: 1 } } }) }] };
      } else {
        reply(response, 200, { jsonrpc: '2.0', id: rpc.id,
          error: { code: -32601, message: 'fixture method absent' } });
        return;
      }
      reply(response, 200, { jsonrpc: '2.0', id: rpc.id, result });
    } catch (error) {
      if (!response.headersSent) reply(response, 400, { error: error.message });
      else response.destroy();
    }
  });
  server.keepAliveTimeout = 1000;
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, host, resolve);
  });
  console.log(`GH407 HARNESS SURROGATE listening on ${host}:${port}; not Agent Mail`);
  await new Promise((resolve) => {
    process.on('SIGTERM', () => {
      if (mode === 'ignore_term') return;
      clearInterval(ticker);
      server.close(() => resolve());
      server.closeAllConnections();
    });
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const mode = argument(process.argv, '--fixture-mode', 'healthy');
  const marker = process.argv.indexOf('--fixture-mode');
  const args = process.argv.slice(2);
  if (marker >= 2) args.splice(marker - 2, 2);
  await fixtureMain(mode, args);
}
