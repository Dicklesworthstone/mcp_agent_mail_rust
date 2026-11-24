#!/usr/bin/env node
/**
 * Linux process-adapter controls for the qualification harness, not Agent Mail.
 * A tiny HTTP process either handles SIGTERM or exits 0 voluntarily first.
 * All fixture files and receipts are retained under /tmp for inspection.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { makeNodeIO, DEFAULTS } from '../scripts/qualify_health_idle.mjs';

const cfg = { ...DEFAULTS, requestTimeout: 2, commandTimeout: 5,
  startupTimeout: 5, resourceInterval: 0.1 };
const pause = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

const fixtureSource = `#!/usr/bin/env node
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
if (process.argv.includes('--version')) {
  console.log('GH407_SHUTDOWN_SURROGATE 0; NOT AGENT MAIL');
} else {
  const port = Number(process.argv[process.argv.indexOf('--port') + 1]);
  const root = process.env.STORAGE_ROOT;
  const server = http.createServer((request, response) => {
    const closing = request.url === '/exit-cleanly';
    response.writeHead(200, {'Content-Type': 'application/json', Connection: 'close'});
    response.end(JSON.stringify({
      status: closing ? 'closing' : 'ready', pid: process.pid,
      capabilities: {mail_read: true, mail_write: true, lexical_search: false}
    }), () => {
      if (closing) {
        fs.writeFileSync(path.join(root, 'voluntary-exit.txt'), 'voluntary exit 0\\n', {flag: 'wx'});
        server.close();
        server.closeAllConnections();
      }
    });
  });
  process.on('SIGTERM', () => {
    fs.appendFileSync(path.join(root, 'sigterm-observed.txt'), 'SIGTERM\\n');
    server.close();
    server.closeAllConnections();
  });
  server.listen(port, '127.0.0.1');
}
`;

async function createIO(t, label) {
  const parent = await fsp.mkdtemp(path.join(os.tmpdir(), `gh407-shutdown-${label}-`));
  const binary = path.join(parent, 'shutdown-surrogate.mjs');
  await fsp.writeFile(binary, fixtureSource, { flag: 'wx', mode: 0o700 });
  const io = await makeNodeIO(binary, parent);
  io.kind = 'harness_shutdown_surrogate';
  t.diagnostic(`Retained HARNESS-ONLY artifacts: ${io.root}`);
  return io;
}

async function waitForListener(session) {
  const deadline = performance.now() + cfg.startupTimeout * 1000;
  let response;
  do {
    response = await session.get('/health');
    if (response.status === 200 && response.body?.status === 'ready') {
      assert.ok(Number.isInteger(response.body.pid) && response.body.pid > 1);
      return response.body.pid;
    }
    await pause(20);
  } while (performance.now() < deadline);
  assert.fail(`Surrogate did not become ready: ${JSON.stringify(response)}`);
}

async function waitForProcessGone(pid) {
  const deadline = performance.now() + 5000;
  while (fs.existsSync(`/proc/${pid}`) && performance.now() < deadline) await pause(20);
  assert.equal(fs.existsSync(`/proc/${pid}`), false,
    'Voluntary exit must be observed before invoking stop; an arbitrary sleep is insufficient');
}

function assertCleanup(receipt) {
  assert.equal(receipt.exited, true);
  assert.equal(receipt.exit_code, 0);
  assert.equal(receipt.forced, false);
  assert.deepEqual(receipt.remaining_pids, []);
  assert.equal(receipt.port_closed, true);
  assert.equal(receipt.quiet, true);
}

async function retainReceipt(io, label, observations) {
  const cleanup = await io.cleanup();
  const receipt = { evidence_class: 'harness_shutdown_surrogate', native_agent_mail_executions: 0,
    label, identity: io.identity, configuration: cfg, observations, cleanup };
  await fsp.writeFile(path.join(io.root, 'shutdown-control-receipt.json'),
    JSON.stringify(receipt, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
  assert.ok(cleanup.every((item) => item.status === 'fulfilled'), 'Final fixture cleanup failed');
}

test('HARNESS ADAPTER ONLY: live process handles SIGTERM and repeated stop retains receipt',
  { skip: process.platform !== 'linux' }, async (t) => {
    const io = await createIO(t, 'live');
    const observations = {};
    try {
      const session = await io.start('live', cfg);
      observations.pid = await waitForListener(session);
      const first = await session.stop();
      observations.first_stop = first;
      assertCleanup(first);
      assert.equal(first.already_exited_before_shutdown, false);
      assert.equal(first.sigterm_sent, true);
      assert.equal(first.identity_unchanged, true);
      assert.ok(!['Z', 'X', 'x', null].includes(first.process_state_before_shutdown));
      assert.equal(await fsp.readFile(path.join(session.directory, 'archive/sigterm-observed.txt'), 'utf8'),
        'SIGTERM\n', 'The fixture itself must witness the signal, beyond the adapter syscall receipt');
      const second = await session.stop();
      observations.second_stop = second;
      assert.strictEqual(second, first, 'Idempotent stop must retain the original receipt object');
      assert.deepEqual(second, first);
    } finally {
      await retainReceipt(io, 'live_sigterm', observations);
    }
  });

test('HARNESS ADAPTER ONLY: prior exit 0 is not counted as a tested SIGTERM shutdown',
  { skip: process.platform !== 'linux' }, async (t) => {
    const io = await createIO(t, 'premature');
    const observations = {};
    try {
      const session = await io.start('premature', cfg);
      const pid = await waitForListener(session);
      observations.pid = pid;
      const request = await session.get('/exit-cleanly');
      observations.voluntary_exit_request = request;
      assert.equal(request.status, 200);
      assert.equal(request.body?.status, 'closing');
      await waitForProcessGone(pid);
      observations.process_absent_before_stop = true;
      const first = await session.stop();
      observations.first_stop = first;
      assertCleanup(first);
      assert.equal(first.already_exited_before_shutdown, true,
        'A known prior clean exit cannot qualify the signal-handling path');
      assert.equal(first.sigterm_sent, false);
      assert.equal(first.identity_unchanged, false);
      assert.equal(first.process_state_before_shutdown, null);
      assert.equal(fs.existsSync(path.join(session.directory, 'archive/sigterm-observed.txt')), false);
      assert.equal(await fsp.readFile(path.join(session.directory, 'archive/voluntary-exit.txt'), 'utf8'),
        'voluntary exit 0\n');
      const second = await session.stop();
      observations.second_stop = second;
      assert.strictEqual(second, first);
      assert.deepEqual(second, first);
    } finally {
      await retainReceipt(io, 'premature_clean_exit', observations);
    }
  });
