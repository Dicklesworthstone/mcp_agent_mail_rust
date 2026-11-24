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
import { makeNodeIO, DEFAULTS, THREAD_CPU_LIMITS, threadCpuDelta } from '../scripts/qualify_health_idle.mjs';

const cfg = { ...DEFAULTS, requestTimeout: 2, commandTimeout: 5,
  startupTimeout: 5, resourceInterval: 0.1 };
const pause = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

const fixtureSource = `#!/usr/bin/env node
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import {spawn} from 'node:child_process';
import {Worker} from 'node:worker_threads';
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
  let worker = null, workerIdentity = null, workerFinished = false;
  if (control.cpuWorker) {
    worker = new Worker(\`
      const {parentPort} = require('node:worker_threads');
      const fs = require('node:fs');
      const fields = fs.readFileSync('/proc/thread-self/stat', 'utf8');
      const values = fields.slice(fields.lastIndexOf(')') + 2).trim().split(/\\\\s+/);
      parentPort.postMessage({kind: 'ready', tid: Number(fields.slice(0, fields.indexOf(' '))), start: values[19], stat: fields});
      parentPort.on('message', message => {
        if (message !== 'busy') return;
        // A finite CPU contrast; a failed test cannot leave a spinning worker.
        const until = performance.now() + 2000;
        let value = 1;
        while (performance.now() < until) value = Math.imul(value ^ 12345, 1664525);
        parentPort.postMessage({kind: 'finished', value});
      });
    \`, {eval: true});
    worker.on('message', message => {
      if (message.kind === 'ready') workerIdentity = {tid: message.tid, start: message.start, stat: message.stat};
      if (message.kind === 'finished') workerFinished = true;
    });
  }
  const server = http.createServer((request, response) => {
    const closing = request.url === '/exit-cleanly';
    if (request.url === '/cpu/start' && worker) worker.postMessage('busy');
    response.writeHead(200, {'Content-Type': 'application/json', Connection: 'close'});
    response.end(JSON.stringify({
      status: closing ? 'closing' : 'ready', pid: process.pid,
      ...(worker ? {worker: workerIdentity, worker_finished: workerFinished} : {}),
      capabilities: {mail_read: true, mail_write: true, lexical_search: false}
    }), () => {
      if (closing) {
        fs.writeFileSync(path.join(root, 'voluntary-exit.txt'), 'voluntary exit 0\\n', {flag: 'wx'});
        server.close();
        server.closeAllConnections();
      }
    });
  });
  process.on('SIGTERM', async () => {
    fs.appendFileSync(path.join(root, 'sigterm-observed.txt'), 'SIGTERM\\n');
    server.close();
    server.closeAllConnections();
    if (worker) await worker.terminate();
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

test('HARNESS ADAPTER ONLY: bounded real worker CPU is attributed to its OS identity with raw evidence',
  {skip: process.platform !== 'linux', timeout: 15_000}, async (t) => {
    const io = await createIO(t, 'thread_cpu', {cpuWorker: true});
    const observations = {};
    try {
      const session = await io.start('thread_cpu', cfg);
      observations.server_pid = await waitForListener(session);
      const deadline = performance.now() + 3000;
      let response;
      do {
        response = await session.get('/health');
        if (response.body?.worker) break;
        await pause(20);
      } while (performance.now() < deadline);
      observations.worker_witness = response.body?.worker;
      const witness = observations.worker_witness;
      assert.ok(Number.isInteger(witness?.tid) && witness.tid > 0, 'Worker must publish its actual /proc TID');
      assert.match(witness.start, /^\d+$/);
      assert.ok(witness.stat.startsWith(`${witness.tid} (`));
      assert.notEqual(witness.tid, observations.server_pid, 'The busy control must be a separate OS thread');

      observations.idle_before = await session.resources();
      await pause(1100);
      observations.idle_after = await session.resources();
      observations.idle_diagnosis = threadCpuDelta(observations.idle_before.thread_cpu_sample,
        observations.idle_after.thread_cpu_sample);
      observations.busy_before = await session.resources();
      observations.busy_command = await session.get('/cpu/start');
      await pause(1250);
      observations.busy_after = await session.resources();
      observations.busy_diagnosis = threadCpuDelta(observations.busy_before.thread_cpu_sample,
        observations.busy_after.thread_cpu_sample);
      // Persist the window in finally even when a threshold below fails.
      const delta = observations.busy_diagnosis;
      const busy = delta.threads.find(row => row.tid === witness.tid && row.identity === witness.start);
      const idle = observations.idle_diagnosis.threads.find(row => row.tid === witness.tid && row.identity === witness.start);
      assert.equal(delta.available, true, JSON.stringify(delta));
      assert.ok(busy && idle, 'Both windows must match the independently witnessed same-start thread');
      assert.ok(busy.total_cpu_seconds >= 0.1,
        `Known busy worker gained too little measured CPU: ${JSON.stringify(busy)}`);
      assert.ok(busy.total_cpu_seconds > idle.total_cpu_seconds + 0.08,
        'The sampler must distinguish actual CPU work from a live idle thread');
      assert.equal(delta.threads[0].tid, witness.tid, 'The known busy worker must rank above idle companions');
      assert.ok(delta.aggregate.total_cpu_seconds >= 0.1);
      assert.equal(delta.exhaustive, false);
      for (const point of [observations.busy_before, observations.busy_after]) {
        assert.equal(point.thread_cpu.persisted, true);
        assert.equal(point.thread_cpu.cached, false, 'Explicit CPU windows require fresh sample boundaries');
        assert.ok(point.thread_cpu_sample.threads.length <= THREAD_CPU_LIMITS.threads);
        const retained = fs.readFileSync(point.thread_cpu.file).subarray(point.thread_cpu.offset_bytes,
          point.thread_cpu.offset_bytes + point.thread_cpu.length_bytes).toString();
        const raw = JSON.parse(retained);
        assert.equal(raw.sequence, point.thread_cpu.sequence);
        assert.equal(raw.pid, observations.server_pid);
        assert.deepEqual(raw.threads, point.thread_cpu_sample.threads,
          'The reported interval must be reproducible from its retained exact-byte raw record');
      }
      observations.shutdown = await session.stop();
      assertCleanup(observations.shutdown);
      assert.equal(observations.shutdown.sigterm_sent, true);
      t.diagnostic(`Measured worker ${witness.tid}/${witness.start}: idle ${idle.total_cpu_seconds.toFixed(3)} CPU s; busy ${busy.total_cpu_seconds.toFixed(3)} CPU s; process ${delta.aggregate.total_cpu_seconds.toFixed(3)} CPU s; residual ${delta.aggregate.residual_cpu_seconds.toFixed(3)} CPU s`);
    } finally {
      await retainReceipt(io, 'actual_thread_cpu_contrast', observations);
    }
  });
