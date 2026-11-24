#!/usr/bin/env node
/**
 * Linux process-adapter controls for the qualification harness, not Agent Mail.
 * Tiny HTTP and helper processes exercise lifecycle ownership and cleanup.
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
import {spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';
const controlPath = new URL('./helper-control.json', import.meta.url);
const control = fs.existsSync(controlPath) ? JSON.parse(fs.readFileSync(controlPath, 'utf8')) : {};
if (process.argv[2] === '--fixture-helper-child') {
  const marker = process.argv[3];
  const raw = fs.readFileSync('/proc/self/stat', 'utf8');
  const fields = raw.slice(raw.lastIndexOf(')') + 2).trim().split(/\\s+/);
  process.on('SIGTERM', () => {
    fs.appendFileSync(marker + '.signals', 'SIGTERM\\n');
    if (!control.ignoreTerm) process.exit(0);
  });
  // A failed test cannot leave a permanent fixture. This is longer than the
  // adapter's 20-second command budget plus its four-second final drain.
  setTimeout(() => process.exit(99), 60_000);
  fs.writeFileSync(marker, JSON.stringify({pid: process.pid, start: fields[19],
    ppid: Number(fields[1]), group: Number(fields[2]), session: Number(fields[3])}), {flag: 'wx'});
} else if (process.argv.includes('--version') || process.argv[2] === 'tooling') {
  const kind = process.argv.includes('--version') ? 'version' : 'sql';
  if (control.leakKind === kind) {
    const marker = path.join(process.env.STORAGE_ROOT, kind + '-descendant.json');
    const child = spawn(process.execPath,
      [fileURLToPath(import.meta.url), '--fixture-helper-child', marker],
      {stdio: 'ignore', detached: false});
    child.unref();
    const deadline = Date.now() + 2000;
    while (!fs.existsSync(marker) && Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    if (!fs.existsSync(marker)) throw new Error('Fixture descendant did not publish its identity');
  }
  if (kind === 'version') console.log('GH407_SHUTDOWN_SURROGATE 0; NOT AGENT MAIL');
  else console.log(JSON.stringify(process.argv[3] === 'db-query' ? [{value: 1}] : {ok: true}));
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

async function createIO(t, label, helperControl = null) {
  const parent = await fsp.mkdtemp(path.join(os.tmpdir(), `gh407-shutdown-${label}-`));
  const binary = path.join(parent, 'shutdown-surrogate.mjs');
  await fsp.writeFile(binary, fixtureSource, { flag: 'wx', mode: 0o700 });
  if (helperControl) await fsp.writeFile(path.join(parent, 'helper-control.json'),
    JSON.stringify(helperControl) + '\n', { flag: 'wx', mode: 0o600 });
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

// These controls use real PIDs and process-group membership. No Agent Mail
// binary, database, subprocess mock, or shortened public idle interval is used.
const helperCfg = { ...cfg, commandTimeout: DEFAULTS.commandTimeout };

function readProcess(pid) {
  try {
    const raw = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    const fields = raw.slice(raw.lastIndexOf(')') + 2).trim().split(/\s+/);
    return {pid, state: fields[0], group: Number(fields[2]), session: Number(fields[3]), start: fields[19]};
  } catch (error) {
    if (error.code === 'ENOENT' || error.code === 'ESRCH') return null;
    throw error;
  }
}

function matchingLiveProcess(witness) {
  const current = readProcess(witness.pid);
  return current?.start === witness.start && !['Z', 'X', 'x'].includes(current.state);
}

async function helperWitness(io, label, kind) {
  const marker = path.join(io.root, label, 'archive', kind + '-descendant.json');
  const witness = JSON.parse(await fsp.readFile(marker, 'utf8'));
  assert.ok(Number.isInteger(witness.pid) && witness.pid > 1);
  assert.ok(Number.isInteger(witness.group) && witness.group > 1);
  assert.equal(witness.group, witness.session, 'Fixture must inherit the detached helper group');
  assert.equal(witness.ppid, witness.group, 'Witness must be the helper process\'s child');
  assert.notEqual(witness.pid, witness.group);
  assert.match(witness.start, /^\d+$/);
  return witness;
}

async function retainHelperReceipt(io, label, kind, observations) {
  const cleanup = await io.cleanup();
  let witness;
  try { witness = await helperWitness(io, label, kind); }
  catch (error) { observations.missing_witness = String(error); }
  if (witness) {
    observations.witness = witness;
    observations.process_after_adapter_cleanup = readProcess(witness.pid);
    observations.alive_after_adapter_cleanup = matchingLiveProcess(witness);
    observations.emergency_cleanup_required = observations.alive_after_adapter_cleanup;
    if (observations.emergency_cleanup_required) {
      // The test only signals its recorded same-birth PID; an unrelated PID
      // reuse is never treated as authority to send a signal.
      const current = readProcess(witness.pid);
      if (current?.start === witness.start) {
        try { process.kill(witness.pid, 'SIGKILL'); }
        catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
      const deadline = performance.now() + 2000;
      while (matchingLiveProcess(witness) && performance.now() < deadline) await pause(10);
    }
    observations.process_after_emergency_cleanup = readProcess(witness.pid);
  }
  const receipt = {evidence_class: 'harness_helper_surrogate', native_agent_mail_executions: 0,
    label, identity: io.identity, configuration: helperCfg, observations, cleanup};
  await fsp.writeFile(path.join(io.root, 'helper-control-receipt.json'),
    JSON.stringify(receipt, null, 2) + '\n', {flag: 'wx', mode: 0o600});
  if (witness) assert.equal(matchingLiveProcess(witness), false, 'Emergency fixture cleanup failed');
  assert.ok(cleanup.every((item) => item.status === 'fulfilled'), 'Final fixture cleanup failed');
}

test('HARNESS ADAPTER ONLY: version helper descendant is rejected and cleaned up',
  {skip: process.platform !== 'linux', timeout: 35_000}, async (t) => {
    const label = 'version_descendant';
    const io = await createIO(t, label, {leakKind: 'version', ignoreTerm: false});
    const observations = {};
    try {
      const started = performance.now();
      try {
        const session = await io.start(label, helperCfg);
        observations.admitted = true;
        observations.server_directory = session.directory;
      } catch (error) {
        observations.admitted = false;
        observations.error = {status: error.status, message: String(error.message || error)};
      }
      observations.duration_seconds = (performance.now() - started) / 1000;
      const witness = await helperWitness(io, label, 'version');
      observations.witness = witness;
      observations.alive_when_command_returned = matchingLiveProcess(witness);
      observations.command_receipt = JSON.parse(await fsp.readFile(path.join(io.root, label, 'version.json'), 'utf8'));
      assert.equal(observations.admitted, false, 'A successful --version exit must not admit a helper that leaves a live child');
      assert.equal(observations.error?.status, 'INCOMPLETE');
      assert.equal(observations.alive_when_command_returned, false, 'Rejecting the helper must also terminate its known child');
      assert.ok(observations.duration_seconds <= helperCfg.commandTimeout + 4,
        'Helper containment exceeded the existing command plus drain budget');
    } finally {
      await retainHelperReceipt(io, label, 'version', observations);
    }
  });

test('HARNESS ADAPTER ONLY: SQL helper descendant is rejected and cleaned up',
  {skip: process.platform !== 'linux', timeout: 35_000}, async (t) => {
    const label = 'sql_descendant';
    const io = await createIO(t, label, {leakKind: 'sql', ignoreTerm: true});
    const observations = {};
    try {
      const session = await io.start(label, helperCfg);
      observations.server_pid = await waitForListener(session);
      const started = performance.now();
      const result = await session.sql('query', 'SELECT 1 AS value');
      observations.duration_seconds = (performance.now() - started) / 1000;
      observations.sql_result = result;
      observations.command_receipt = JSON.parse(await fsp.readFile(result.receipt, 'utf8'));
      const witness = await helperWitness(io, label, 'sql');
      observations.witness = witness;
      observations.alive_when_command_returned = matchingLiveProcess(witness);
      assert.equal(result.ok, false, 'Valid SQL JSON and exit 0 must not hide a helper child leak');
      assert.equal(observations.alive_when_command_returned, false, 'The SIGTERM-ignoring helper child must be killed within the budget');
      assert.ok(observations.duration_seconds <= helperCfg.commandTimeout + 4,
        'Helper containment exceeded the existing command plus drain budget');
      assert.equal(await waitForListener(session), observations.server_pid,
        'Cleaning the helper group must preserve the separate owned server');
    } finally {
      await retainHelperReceipt(io, label, 'sql', observations);
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
