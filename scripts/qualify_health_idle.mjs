#!/usr/bin/env node
/**
 * Independent GH407 black-box qualification. Linux + Node >=20, no packages.
 * Only launches a supplied binary against a newly created synthetic mailbox.
 * Native build/test qualification is separate. Never removes retained artifacts.
 */
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import http from 'node:http';
import net from 'node:net';
import crypto from 'node:crypto';
import {spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';

export const DEFAULTS = Object.freeze({
  idleSeconds: 8, idleCycles: 3, concurrency: 16, soakSeconds: 30,
  startupTimeout: 90, requestTimeout: 5, commandTimeout: 20,
  shutdownTimeout: 10, resourceInterval: 1, warmupSeconds: 4,
  failureWait: 8, refreshSeconds: 2, maxSampleAge: 6,
  maxFdGrowth: 32, maxThreadGrowth: 16,
  maxRssGrowthBytes: 64 * 1024 * 1024, maxIdleCpuCores: 0.25,
});

export const THREAD_CPU_LIMITS = Object.freeze({
  threads: 256, samples: 1024, bytes: 16 * 1024 * 1024,
  statBytes: 4096, sampleSeconds: 0.025, intervalSeconds: 1,
});

export function qualificationCaseNames(cfg = DEFAULTS) {
  return ['baseline',
    ...Array.from({length: cfg.idleCycles}, (_, i) => ['idle_first_request_' + (i + 1), 'idle_database_' + (i + 1)]).flat(),
    'concurrent_first_requests_after_idle', 'idle_soak', 'idle_soak_cpu',
    'genuine_database_unavailability', 'database_outage_liveness',
    'concurrent_requests_during_database_outage', 'failed_probe_cadence', 'failed_probe_cpu',
    'database_fault_restore', 'recovery_without_readiness_polling', 'recovery_database_operation',
    'bounded_observed_resources', 'healthy_shutdown', 'stale_success_baseline',
    'stale_success_challenge', 'stale_outage_database_operation', 'stale_outage_liveness',
    'stale_fault_restore', 'stale_challenge_recovery', 'stale_recovery_database_operation',
    'stale_shutdown', 'outage_shutdown_baseline', 'outage_shutdown_readiness',
    'outage_shutdown_liveness', 'shutdown_with_database_unavailable',
    'outage_process_cleanup', 'outage_shutdown_schema_restore'];
}

class QualificationError extends Error {
  constructor(message, status = 'FAIL') { super(message); this.status = status; }
}
function requireThat(ok, message) { if (!ok) throw new QualificationError(message); }
function need(ok, message) { if (!ok) throw new QualificationError(message, 'INCOMPLETE'); }
function canonical(value) {
  if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
  if (value && typeof value === 'object') return '{' + Object.keys(value).sort().map(k => JSON.stringify(k) + ':' + canonical(value[k])).join(',') + '}';
  return JSON.stringify(value);
}
function ready(response) {
  return response.status === 200 && response.body?.status === 'ready'
    && response.body?.capabilities?.mail_read === true
    && response.body?.capabilities?.mail_write === true
    && !response.body?.reason;
}
function checkReady(response) {
  requireThat(ready(response), 'First readiness response was not ready: ' + JSON.stringify(response));
}
function checkUnavailable(response, allowStale = false) {
  const reasons = allowStale ? ['database_probe_failed', 'database_probe_stale'] : ['database_probe_failed'];
  requireThat(response.status === 503 && response.body?.status === 'unavailable'
    && reasons.includes(response.body?.reason)
    && response.body?.capabilities?.mail_read === false
    && response.body?.capabilities?.mail_write === false
    && response.body?.capabilities?.lexical_search === false,
  'Database outage was not reported fail-closed: ' + JSON.stringify(response));
}

/** Pure orchestration; all observations come from the supplied IO adapter. */
export async function runQualification(io, options = {}) {
  const cfg = {...DEFAULTS, ...options};
  const report = {
    schema_version: 1, evidence_class: io.kind, identity: io.identity,
    configuration: cfg, cases: [], observations: [], limitations: [
      'Finite resource observations do not prove an absolute lifetime memory or task bound.',
      'Failed-probe log cadence measures completed probes, not exact simultaneous internal jobs.',
      'Schema unavailability complements, and does not substitute for, native pool-exhaustion tests.',
      'A stopped-process stale challenge may observe a fresh failure before the first HTTP response.',
      'Thread CPU diagnostics are bounded, non-atomic samples; residual CPU includes unobserved threads and sampling skew.',
    ],
  };
  const started = io.now();
  report.started_monotonic_seconds = started;
  const observation = (kind, data) => report.observations.push({...data, at_seconds: io.now() - started, kind});
  async function test(name, action) {
    const begin = io.now();
    try {
      const details = await action();
      report.cases.push({name, status: 'PASS', duration_seconds: io.now() - begin, details});
      return true;
    } catch (error) {
      report.cases.push({name, status: error.status || 'FAIL', duration_seconds: io.now() - begin, error: String(error.message || error)});
      return false;
    }
  }
  async function read(s, route) {
    let response;
    try { response = await s.get(route); }
    catch (error) {
      observation('http_error', {session: s.label, route, error: String(error)});
      throw error;
    }
    observation('http', {session: s.label, route, response});
    requireThat(response.duration_ms <= cfg.requestTimeout * 1000,
      'HTTP response exceeded the predeclared request deadline');
    return response;
  }
  async function readAll(s, routes) {
    // A deadline failure cannot release a sibling request into a later idle,
    // restoration or shutdown phase. Every initiated bounded read must settle.
    const outcomes = await Promise.allSettled(routes.map(route => read(s, route)));
    const failed = outcomes.find(value => value.status === 'rejected');
    if (failed) throw failed.reason;
    return outcomes.map(value => value.value);
  }
  async function live(s) {
    const responses = await readAll(s, ['/healthz', '/health/liveness']);
    for (const r of responses) {
      requireThat(r.status === 200 && r.body?.status === 'alive', 'Transport liveness failed: ' + JSON.stringify(r));
    }
    return {responses};
  }
  async function burst(s, unavailable = false, allowStale = false) {
    const results = await readAll(s, Array.from({length: cfg.concurrency}, (_, i) =>
      i % 2 ? '/health/readiness' : '/health'));
    // Inspect every response; a later success never overwrites the first failure.
    for (const result of results) unavailable ? checkUnavailable(result, allowStale) : checkReady(result);
    return results;
  }
  async function start(label) {
    const s = await io.start(label, cfg);
    s.label = label;
    s._resourcePoints = [];
    return s;
  }
  async function baseline(s) {
    const deadline = io.now() + cfg.startupTimeout;
    async function waitReady(phase) {
      observation('startup_phase_start', {session: s.label, phase, deadline_monotonic_seconds: deadline});
      while (io.now() < deadline) {
        const response = await s.get('/health');
        observation('startup_http', {session: s.label, phase, response, deadline_monotonic_seconds: deadline});
        need(!response.startup_fatal, 'Supplied process exited before a healthy baseline: ' + JSON.stringify(response));
        if (ready(response) && io.now() <= deadline) {
          observation('startup_phase_end', {session: s.label, phase});
          return;
        }
        const remaining = deadline - io.now();
        if (remaining > 0) await s.idle(Math.min(0.25, remaining));
      }
      need(false, 'Could not establish ' + phase + ' within the original startup deadline; see startup responses and server log');
    }
    await waitReady('initial_readiness');
    const control = await s.ensureProject('baseline');
    observation('database_control', {session: s.label, phase: 'startup_seed', control});
    need(control.ok, 'Fresh synthetic project could not be created at baseline');
    // Creating the fixture can invalidate lexical readiness. Settle that
    // setup mutation before measuring; both readiness waits share one budget.
    need(deadline - io.now() >= cfg.warmupSeconds, 'Insufficient original startup budget for the full baseline warmup');
    observation('startup_phase_start', {session: s.label, phase: 'post_seed_warmup', seconds: cfg.warmupSeconds,
      deadline_monotonic_seconds: deadline});
    await s.idle(cfg.warmupSeconds);
    observation('startup_phase_end', {session: s.label, phase: 'post_seed_warmup'});
    await waitReady('post_seed_readiness');
    // This final burst is strict and is never retried. Later measured idle
    // first responses and concurrent bursts retain their original semantics.
    observation('startup_phase_start', {session: s.label, phase: 'final_readiness_burst'});
    await burst(s);
    observation('startup_phase_end', {session: s.label, phase: 'final_readiness_burst'});
    await sample(s, 'baseline');
  }
  async function prepareIdle(s, label) {
    // This prepares a NEW trial only. A previous trial's retained first
    // response and verdict are immutable, including when these polls recover.
    const begin = io.now(), deadline = begin + cfg.failureWait;
    observation('precondition_start', {session: s.label, label, timeout_seconds: cfg.failureWait});
    let requests = 0;
    do {
      const response = await s.get('/health');
      requests++;
      observation('precondition_http', {session: s.label, label, response});
      need(!response.startup_fatal, 'Owned process exited before the next idle trial');
      if (ready(response)) {
        const result = {requests, elapsed_seconds: io.now() - begin};
        observation('precondition_end', {session: s.label, label, ...result});
        return result;
      }
      await s.idle(0.25);
    } while (io.now() < deadline);
    need(false, 'Could not establish a healthy precondition; this idle trial was not executed');
  }
  async function sample(s, label) {
    const point = {label, at_seconds: io.now(), ...(await s.resources())};
    s._resourcePoints.push(point);
    observation('resources', {session: s.label, ...point});
    return point;
  }
  async function idle(s, seconds, label) {
    const from = io.now();
    observation('idle_start', {session: s.label, label, seconds});
    await s.idle(seconds); // /proc sampling only; no HTTP, MCP or database work.
    observation('idle_end', {session: s.label, label, elapsed_seconds: io.now() - from});
    requireThat(io.now() - from >= seconds, 'Idle interval was shortened');
  }
  async function newControl(s, tag, expectFailure) {
    const value = await s.ensureProject(tag);
    observation('database_control', {session: s.label, tag, control: value});
    if (expectFailure) need(value.ok === false && value.error_kind === 'database',
      'Fault did not produce an independently attributed MCP database/schema failure');
    else requireThat(value.ok === true, 'Fresh database operation failed: ' + JSON.stringify(value));
    return value;
  }
  async function sql(s, kind, statement) {
    const result = await s.sql(kind, statement);
    observation('sql', {session: s.label, kind, statement, result});
    return result;
  }
  async function prepareFault(s) {
    const names = await sql(s, 'query', "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('projects','gh407_saved_projects') ORDER BY name");
    need(names.ok && canonical(names.rows) === canonical([{name: 'projects'}]), 'Synthetic schema does not have an exclusive unused rename target');
    const before = await sql(s, 'query', 'SELECT * FROM projects ORDER BY id');
    need(before.ok && Array.isArray(before.rows), 'Cannot snapshot synthetic project rows');
    const schema = await sql(s, 'query', 'SELECT type, name, tbl_name FROM sqlite_master ORDER BY type, name');
    need(schema.ok && Array.isArray(schema.rows), 'Cannot snapshot schema object identities');
    return {before: before.rows, schema: schema.rows, attempted: false};
  }
  async function inject(s, fault, mcpControl = true) {
    fault.attempted = true; // Restore even if the command mutates and then fails.
    const renamed = await sql(s, 'exec', 'ALTER TABLE projects RENAME TO gh407_saved_projects');
    need(renamed.ok === true, 'Same-binary schema fault was refused or unsupported');
    const old = await sql(s, 'query', 'SELECT * FROM projects ORDER BY id');
    const saved = await sql(s, 'query', 'SELECT * FROM gh407_saved_projects ORDER BY id');
    need(!old.ok && old.error_kind === 'database' && saved.ok
      && canonical(saved.rows) === canonical(fault.before),
    'Independent same-engine controls did not prove the intended table outage');
    fault.confirmed = true;
    if (mcpControl) await newControl(s, 'outage', true);
  }
  async function restore(s, fault) {
    if (!fault?.attempted) return {needed: false};
    const names = await sql(s, 'query', "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('projects','gh407_saved_projects') ORDER BY name");
    requireThat(names.ok, 'Cannot inspect schema for restoration; retained mailbox needs review');
    const values = names.rows.map(row => row.name);
    if (values.includes('gh407_saved_projects') && !values.includes('projects')) {
      const restored = await sql(s, 'exec', 'ALTER TABLE gh407_saved_projects RENAME TO projects');
      requireThat(restored.ok, 'Failed to restore synthetic projects table; artifacts retained');
    } else requireThat(values.length === 1 && values[0] === 'projects', 'Ambiguous schema during restoration; refusing to overwrite either table');
    const after = await sql(s, 'query', 'SELECT * FROM projects ORDER BY id');
    requireThat(after.ok && canonical(after.rows) === canonical(fault.before), 'Project rows changed or were lost across the reversible fault');
    const schema = await sql(s, 'query', 'SELECT type, name, tbl_name FROM sqlite_master ORDER BY type, name');
    requireThat(schema.ok && canonical(schema.rows) === canonical(fault.schema), 'Schema object identities changed across the reversible fault');
    fault.attempted = false;
    return {project_rows_preserved: true, schema_object_identities_preserved: true, rows: after.rows.length};
  }
  async function restoreCase(s, fault, name) {
    return await test(name, async () => {
      const result = await restore(s, fault); // Cleanup is required even after partial injection.
      need(fault?.confirmed === true, 'No independently confirmed schema outage was installed; restoration was not qualified');
      return result;
    });
  }
  async function recover(s, label, readinessCase, databaseCase) {
    let idleCompleted = false;
    await test(readinessCase, async () => {
      await idle(s, cfg.failureWait, label);
      idleCompleted = true;
      const r = await read(s, '/health/readiness');
      checkReady(r);
      return {first_response: r};
    });
    await test(databaseCase, async () => {
      need(idleCompleted, 'Recovery idle interval did not complete; database recovery was not qualified');
      // Preserve the first health verdict before the fresh database witness.
      // No readiness retry occurs between these independently judged checks.
      return await newControl(s, label, false);
    });
  }
  function cpuBound(before, after, session) {
    // Retain attribution before any aggregate assertion throws. Diagnostic
    // loss cannot rescue, suppress, or replace the existing process-CPU gate.
    let diagnosis;
    try { diagnosis = threadCpuDelta(before?.thread_cpu_sample, after?.thread_cpu_sample); }
    catch (error) { diagnosis = {available: false, reason: 'diagnostic_error', error: String(error).slice(0,512), exhaustive: false}; }
    observation('thread_cpu_window', {session, from_label: before?.label, to_label: after?.label,
      aggregate_before: {at_seconds: before?.at_seconds, at_monotonic_seconds: before?.at_monotonic_seconds,
        cpu_seconds: before?.cpu_seconds, identity: before?.identity, available: before?.available},
      aggregate_after: {at_seconds: after?.at_seconds, at_monotonic_seconds: after?.at_monotonic_seconds,
        cpu_seconds: after?.cpu_seconds, identity: after?.identity, available: after?.available},
      before: before?.thread_cpu || null, after: after?.thread_cpu || null, diagnosis});
    need(before?.available && after?.available, 'CPU telemetry unavailable');
    requireThat(before.identity === after.identity, 'Process identity changed during CPU measurement');
    const seconds = after.at_seconds - before.at_seconds;
    need(seconds > 0, 'No CPU observation interval measured');
    const cores = (after.cpu_seconds - before.cpu_seconds) / seconds;
    requireThat(Number.isFinite(cores) && cores >= 0 && cores <= cfg.maxIdleCpuCores,
      'Idle CPU exceeded bound or counter identity changed');
    return {seconds, cpu_cores: cores};
  }
  async function shutdown(s) {
    const stopped = await s.stop();
    observation('shutdown', {session: s.label, result: stopped});
    requireThat(stopped.already_exited_before_shutdown === false && stopped.sigterm_sent === true
      && stopped.exited && stopped.exit_code === 0 && !stopped.forced
      && stopped.remaining_pids.length === 0 && stopped.port_closed
      && stopped.quiet && stopped.identity_unchanged,
    'Shutdown did not meet clean exit/ownership/quiet criteria: ' + JSON.stringify(stopped));
    return stopped;
  }
  async function resourceBounds(s) {
    const points = s._resourcePoints;
    need(points.length >= 2 && points.every(p => p.available), 'Resource telemetry unavailable; no complete resource qualification');
    const first = points[0];
    for (const p of points) {
      requireThat(p.identity === first.identity && p.children === 0, 'Process identity changed or unexpected child survived a settled sample');
      requireThat(p.fds - first.fds <= cfg.maxFdGrowth, 'Settled file descriptor growth exceeded bound');
      requireThat(p.threads - first.threads <= cfg.maxThreadGrowth, 'Settled thread growth exceeded bound');
      requireThat(p.rss_bytes - first.rss_bytes <= cfg.maxRssGrowthBytes, 'Settled RSS growth exceeded bound');
    }
    return {baseline: first, final: points.at(-1), samples: points.length, scope: 'finite settled growth observations'};
  }

  let s;
  try {
    const startedOk = await test('baseline', async () => { s = await start('healthy'); await baseline(s); });
    if (startedOk) {
      for (let cycle = 0; cycle < cfg.idleCycles; cycle++) {
        let idleCompleted = false;
        await test('idle_first_request_' + (cycle + 1), async () => {
          const preparation = await prepareIdle(s, 'idle_' + cycle);
          await idle(s, cfg.idleSeconds, 'idle_' + cycle);
          idleCompleted = true;
          const r = await read(s, '/health');
          checkReady(r);
          return {first_response: r, preparation};
        });
        await test('idle_database_' + (cycle + 1), async () => {
          need(idleCompleted, 'Idle interval did not complete; the post-idle database witness was not qualified');
          // The first health outcome is already retained, even on timeout.
          // This fresh MCP operation precedes any readiness preparation/retry.
          return await newControl(s, 'idle_' + cycle, false);
        });
        await sample(s, 'idle_' + cycle);
      }
      await test('concurrent_first_requests_after_idle', async () => {
        await prepareIdle(s, 'concurrent_idle');
        await idle(s, cfg.idleSeconds, 'concurrent_idle');
        const results = await burst(s);
        await s.idle(cfg.refreshSeconds);
        await sample(s, 'after_concurrency');
        return {responses: results.length};
      });
      let soak;
      await test('idle_soak', async () => {
        const before = await sample(s, 'soak_start');
        await idle(s, cfg.soakSeconds, 'soak');
        const after = await sample(s, 'soak_end');
        soak = {before, after};
        const first = await read(s, '/health');
        checkReady(first);
        return {first_response: first};
      });
      await test('idle_soak_cpu', async () => {
        need(soak, 'Idle soak did not complete; CPU was not qualified');
        return cpuBound(soak.before, soak.after, s.label);
      });

      let fault;
      let outageWindow;
      let outageConfirmed = false;
      try {
        await test('genuine_database_unavailability', async () => {
          fault = await prepareFault(s);
          await inject(s, fault);
          outageConfirmed = true;
          const logsBefore = await s.logs();
          const resourceBefore = await sample(s, 'outage_start');
          const begin = io.now();
          await idle(s, cfg.failureWait, 'database_outage');
          // Finish the autonomous-probe observation BEFORE any health or
          // liveness request can schedule work and contaminate its evidence.
          const resourceAfter = await sample(s, 'outage');
          const logsAfter = await s.logs();
          const duration = io.now() - begin;
          outageWindow = {logsBefore, logsAfter, resourceBefore, resourceAfter, duration};
          observation('autonomous_probe_window', {session: s.label, ...outageWindow});
          const first = await read(s, '/health');
          checkUnavailable(first);
          return {first_response: first};
        });
        await test('database_outage_liveness', async () => {
          need(outageConfirmed, 'No independently confirmed serving-database outage');
          return await live(s);
        });
        await test('concurrent_requests_during_database_outage', async () => {
          need(outageWindow, 'Database outage interval did not complete');
          const responses = await burst(s, true);
          return {responses: responses.length};
        });
        await test('failed_probe_cadence', async () => {
          need(outageWindow, 'No completed autonomous failed-probe observation window');
          const {logsBefore, logsAfter, duration} = outageWindow;
          need(logsBefore.complete && logsAfter.complete, 'Probe logs unavailable or truncated');
          const failures = logsAfter.probe_failures - logsBefore.probe_failures;
          need(failures >= 2, 'Insufficient failed-probe completions to qualify continued background refresh');
          const maximum = Math.floor(duration / cfg.refreshSeconds) + 2;
          requireThat(failures <= maximum, 'Readiness failure work exceeded the declared refresh cadence');
          return {failure_completions: failures, maximum, observation_seconds: duration, request_free: true};
        });
        await test('failed_probe_cpu', async () => {
          need(outageWindow, 'No completed failed-probe CPU observation window');
          return cpuBound(outageWindow.resourceBefore, outageWindow.resourceAfter, s.label);
        });
      } finally {
        const attempted = fault?.attempted;
        const restored = await restoreCase(s, fault, 'database_fault_restore');
        if (restored && attempted) await recover(s, 'recovery_idle',
          'recovery_without_readiness_polling', 'recovery_database_operation');
      }
      await sample(s, 'final');
      await test('bounded_observed_resources', () => resourceBounds(s));
    }
  } catch (error) {
    report.cases.push({name: 'unexpected_orchestration_error', status: error.status || 'FAIL', error: String(error)});
  } finally {
    if (s) await test('healthy_shutdown', () => shutdown(s));
  }

  // Stale-success safety must not depend on a previous recovery succeeding.
  // This lifetime has its own mailbox, baseline, reversible fault and cleanup.
  let stale;
  let staleFault;
  let suspended = false;
  try {
    const ok = await test('stale_success_baseline', async () => {
      stale = await start('stale_success');
      await baseline(stale);
    });
    if (ok) {
      try {
        await test('stale_success_challenge', async () => {
          checkReady(await read(stale, '/health'));
          staleFault = await prepareFault(stale);
          suspended = true; // Resume even if suspension fails after SIGSTOP.
          try {
            await stale.suspend();
            await inject(stale, staleFault, false);
            await idle(stale, cfg.failureWait, 'suspended_stale_sample');
          } finally { await stale.resume(); suspended = false; }
          const first = await read(stale, '/health/readiness');
          checkUnavailable(first, true);
          return {first_response: first, stale_branch_observed: first.body.reason === 'database_probe_stale'};
        });
        await test('stale_outage_database_operation', async () => {
          need(staleFault?.confirmed && !suspended, 'No confirmed, resumed stale-sample database outage');
          return await newControl(stale, 'stale_outage', true);
        });
        await test('stale_outage_liveness', async () => {
          need(staleFault?.confirmed && !suspended, 'No confirmed, resumed stale-sample database outage');
          return await live(stale);
        });
      } finally {
        if (suspended) { await stale.resume(); suspended = false; }
        const attempted = staleFault?.attempted;
        const restored = await restoreCase(stale, staleFault, 'stale_fault_restore');
        if (restored && attempted) await recover(stale, 'stale_recovery_idle',
          'stale_challenge_recovery', 'stale_recovery_database_operation');
      }
    }
  } catch (error) {
    report.cases.push({name: 'unexpected_stale_challenge_error', status: error.status || 'FAIL', error: String(error)});
  } finally {
    if (stale) await test('stale_shutdown', () => shutdown(stale));
  }

  let outage;
  let finalFault;
  let finalOutageConfirmed = false;
  try {
    const ok = await test('outage_shutdown_baseline', async () => { outage = await start('outage_shutdown'); await baseline(outage); });
    if (ok) {
      await test('outage_shutdown_readiness', async () => {
        finalFault = await prepareFault(outage);
        await inject(outage, finalFault);
        finalOutageConfirmed = true;
        await idle(outage, cfg.failureWait, 'outage_shutdown_idle');
        const first = await read(outage, '/health/readiness');
        checkUnavailable(first);
        return {first_response: first};
      });
      await test('outage_shutdown_liveness', async () => {
        need(finalOutageConfirmed, 'No independently confirmed database outage before shutdown');
        return await live(outage);
      });
      await test('shutdown_with_database_unavailable', async () => {
        need(finalOutageConfirmed, 'No independently confirmed database outage before shutdown');
        return await shutdown(outage);
      });
    }
  } finally {
    if (outage) {
      await test('outage_process_cleanup', () => shutdown(outage));
      await restoreCase(outage, finalFault, 'outage_shutdown_schema_restore');
    }
  }
  const recorded = new Set(report.cases.map(value => value.name));
  for (const name of qualificationCaseNames(cfg)) {
    if (!recorded.has(name)) report.cases.push({name, status: 'INCOMPLETE', executed: false,
      error: 'Required check was not executed because an earlier prerequisite did not complete; see preceding case records'});
  }
  report.duration_seconds = io.now() - started;
  report.status = report.cases.some(c => c.status === 'FAIL') ? 'FAIL'
    : report.cases.some(c => c.status === 'INCOMPLETE') ? 'INCOMPLETE' : 'PASS';
  report.exit_code = {PASS: 0, FAIL: 1, INCOMPLETE: 2}[report.status];
  return report;
}

// Linux/Node adapter. The pure orchestration above is also used by model tests.
const delay = seconds => new Promise(resolve => setTimeout(resolve, seconds * 1000));
function waitWithin(promise, seconds) {
  let timer;
  return Promise.race([promise, new Promise(resolve => { timer = setTimeout(resolve, seconds * 1000); })])
    .finally(() => clearTimeout(timer));
}
const MAX_CAPTURE = 16 * 1024 * 1024;
const nativeNow = () => Number(process.hrtime.bigint()) / 1e9;

/** /proc identity and counters only; never opens a mailbox or requests health. */
export function parseThreadCpuStat(raw, expectedTid) {
  if (typeof raw !== 'string' || Buffer.byteLength(raw) > THREAD_CPU_LIMITS.statBytes) throw new Error('stat size limit');
  const open = raw.indexOf('('), close = raw.lastIndexOf(')');
  const tid = Number(raw.slice(0, open).trim());
  const fields = raw.slice(close + 2).trim().split(/\s+/);
  const user = Number(fields[11]), system = Number(fields[12]), count = Number(fields[17]);
  if (open < 1 || close < open || tid !== expectedTid || !Number.isSafeInteger(tid) || tid <= 0
      || !/^\d+$/.test(fields[19] || '') || ![user, system, count].every(v => Number.isSafeInteger(v) && v >= 0)) {
    throw new Error('invalid stat identity or counters');
  }
  return {tid, identity: fields[19], name: raw.slice(open + 1, close),
    user_ticks: user, system_ticks: system, threads: count};
}

function boundedProcStat(filename) {
  const descriptor = fs.openSync(filename, 'r');
  try {
    const bytes = Buffer.alloc(THREAD_CPU_LIMITS.statBytes + 1);
    const length = fs.readSync(descriptor, bytes, 0, bytes.length, 0);
    if (length > THREAD_CPU_LIMITS.statBytes) throw new Error('stat size limit');
    return bytes.subarray(0, length).toString('utf8');
  } finally { fs.closeSync(descriptor); }
}

/** Reader seams support bounded negative controls; the CLI always uses /proc. */
export function sampleThreadCpu({pid, identity, ticksPerSecond}, source = {}) {
  const now = source.now || nativeNow;
  const readStat = source.readStat || (tid => boundedProcStat('/proc/' + pid + (tid === null ? '/stat' : '/task/' + tid + '/stat')));
  const threadIds = source.threadIds || function* () {
    const directory = fs.opendirSync('/proc/' + pid + '/task');
    try {
      let entry;
      while ((entry = directory.readSync())) if (/^\d+$/.test(entry.name)) yield Number(entry.name);
    } finally { directory.closeSync(); }
  };
  const result = {pid, identity, ticks_per_second: ticksPerSecond, available: false,
    read_started_monotonic_seconds: now(), process_before: null, process_after: null,
    threads: [], enumerated_threads: 0, unreadable_threads: 0, omitted_threads: null,
    omitted_threads_lower_bound: 0, enumeration_complete: false, truncated: false,
    errors: [], error_count: 0, limits: THREAD_CPU_LIMITS};
  const error = (where, value) => {
    result.error_count++;
    if (result.errors.length < 8) result.errors.push({where, error: String(value).slice(0, 512)});
  };
  const read = tid => {
    const start = now();
    return {...parseThreadCpuStat(readStat(tid), tid === null ? pid : tid),
      read_started_monotonic_seconds: start, read_finished_monotonic_seconds: now()};
  };
  try {
    result.process_before = read(null);
    if (result.process_before.identity !== identity) throw new Error('process start identity changed');
    const deadline = result.read_started_monotonic_seconds + THREAD_CPU_LIMITS.sampleSeconds;
    let completed = true;
    for (const tid of threadIds()) {
      if (result.enumerated_threads >= THREAD_CPU_LIMITS.threads || now() >= deadline) {
        result.truncated = true; completed = false; result.omitted_threads_lower_bound = 1;
        result.truncation_reason = result.enumerated_threads >= THREAD_CPU_LIMITS.threads ? 'thread_limit' : 'read_time_budget';
        break;
      }
      result.enumerated_threads++;
      try { result.threads.push(read(tid)); }
      catch (failure) { result.unreadable_threads++; error('thread:' + tid, failure); }
    }
    result.enumeration_complete = completed;
    result.omitted_threads = completed ? 0 : null;
  } catch (failure) { error('census', failure); }
  try { result.process_after = read(null); }
  catch (failure) { error('process_after', failure); }
  result.read_finished_monotonic_seconds = now();
  result.read_budget_overrun_seconds = Math.max(0, result.read_finished_monotonic_seconds
    - result.read_started_monotonic_seconds - THREAD_CPU_LIMITS.sampleSeconds);
  result.available = Number.isFinite(ticksPerSecond) && ticksPerSecond > 0
    && result.process_before?.identity === identity && result.process_after?.identity === identity;
  if (!result.available) result.reason = 'process_identity_or_counter_unavailable';
  // A synchronous /proc syscall can overrun this checked-between-reads budget.
  // Record its actual width; do not describe the collection as atomic/preemptive.
  return result;
}

/** One retained file and one cached sample; no work after a terminal cap/error. */
export function createThreadCpuSampler(options, source = {}) {
  const now = source.now || nativeNow;
  let bytes = 0, sequence = 0, nextAt = -Infinity, last = null, terminal = false;
  const reserve = 4096;
  function retain(sample, reason = null) {
    const record = {sequence: sequence + 1, ...sample};
    const line = JSON.stringify(record) + '\n';
    if (!reason && bytes + Buffer.byteLength(line) > THREAD_CPU_LIMITS.bytes - reserve) {
      return finish('byte_limit');
    }
    const reference = {file: options.filename, sequence: record.sequence,
      offset_bytes: bytes, length_bytes: Buffer.byteLength(line), available: sample.available,
      read_started_monotonic_seconds: sample.read_started_monotonic_seconds,
      read_finished_monotonic_seconds: sample.read_finished_monotonic_seconds,
      persisted: false, cached: false, reason, limits: THREAD_CPU_LIMITS};
    try {
      fs.writeFileSync(options.filename, line, {flag: sequence ? 'a' : 'wx', mode: 0o600});
      reference.persisted = true; bytes += reference.length_bytes; sequence++;
    } catch (error) {
      terminal = true; reference.available = false; reference.reason = 'diagnostic_write_failed';
      reference.error = String(error).slice(0, 512);
      sample = {...sample, available: false, reason: reference.reason};
    }
    last = {reference, sample};
    return last;
  }
  function finish(reason, failure) {
    terminal = true;
    const at = now();
    return retain({pid: options.pid, identity: options.identity, available: false, reason,
      read_started_monotonic_seconds: at, read_finished_monotonic_seconds: at,
      error: failure ? String(failure).slice(0, 512) : undefined,
      retained_samples: sequence, retained_bytes: bytes, limits: THREAD_CPU_LIMITS}, reason);
  }
  return {sample({fresh = false} = {}) {
    if (terminal || (!fresh && now() < nextAt)) return last ? {reference: {...last.reference, cached: true}, sample: last.sample} : null;
    nextAt = now() + THREAD_CPU_LIMITS.intervalSeconds;
    // Reserve the last permitted record and a small byte envelope for refusal.
    if (sequence >= THREAD_CPU_LIMITS.samples - 1) return finish('sample_limit');
    try { return retain(sampleThreadCpu(options, source)); }
    catch (error) { return finish('diagnostic_error', error); }
  }};
}

/** Rank only stable identities; sampled thread totals are never exhaustive. */
export function threadCpuDelta(before, after) {
  const result = {available: false, reason: null, exhaustive: false, threads: [],
    accounting: {matched_threads: 0, new_threads: [], exited_threads: [], reused_tids: [], invalid_threads: [],
      omitted_before: before?.omitted_threads ?? null, omitted_after: after?.omitted_threads ?? null,
      omitted_lower_bound_before: before?.omitted_threads_lower_bound ?? 0,
      omitted_lower_bound_after: after?.omitted_threads_lower_bound ?? 0,
      unreadable_before: before?.unreadable_threads ?? null, unreadable_after: after?.unreadable_threads ?? null,
      enumeration_complete_before: before?.enumeration_complete === true,
      enumeration_complete_after: after?.enumeration_complete === true,
      errors_before: before?.error_count ?? null, errors_after: after?.error_count ?? null,
      population_note: 'New/exited entries are observed appearances/disappearances, not a complete thread-lifecycle census.'},
    aggregate: null, sampling: null};
  const refuse = reason => ({...result, reason});
  const counter = value => Number.isSafeInteger(value) && value >= 0;
  const stamp = value => Number.isFinite(value?.read_started_monotonic_seconds)
    && Number.isFinite(value?.read_finished_monotonic_seconds)
    && value.read_finished_monotonic_seconds >= value.read_started_monotonic_seconds;
  const midpoint = value => (value.read_started_monotonic_seconds + value.read_finished_monotonic_seconds) / 2;
  if (!before?.available || !after?.available) return refuse('sample_unavailable');
  if (!Number.isSafeInteger(before.pid) || before.pid <= 0 || !/^\d+$/.test(before.identity || '')
      || before.pid !== after.pid || before.identity !== after.identity
      || [before.process_before, before.process_after, after.process_before, after.process_after]
        .some(p => p?.tid !== before.pid || p?.identity !== before.identity)) return refuse('process_identity_changed');
  const hz = before.ticks_per_second;
  if (!Number.isFinite(hz) || hz <= 0 || hz !== after.ticks_per_second) return refuse('clock_frequency_changed');
  if (![before, after, before.process_before, before.process_after, after.process_before, after.process_after].every(stamp)
      || after.read_started_monotonic_seconds <= before.read_finished_monotonic_seconds) return refuse('invalid_sample_interval');
  if ([before, after].some(sample => [sample.process_before, sample.process_after].some(p =>
    p.read_started_monotonic_seconds < sample.read_started_monotonic_seconds
      || p.read_finished_monotonic_seconds > sample.read_finished_monotonic_seconds)
    || sample.process_before.read_finished_monotonic_seconds > sample.process_after.read_started_monotonic_seconds)) return refuse('invalid_sample_interval');
  const processPoints = [before.process_before, before.process_after, after.process_before, after.process_after];
  if (!processPoints.every(p => counter(p.user_ticks) && counter(p.system_ticks))
      || processPoints.some((p, i) => i && (p.user_ticks < processPoints[i - 1].user_ticks
        || p.system_ticks < processPoints[i - 1].system_ticks))) return refuse('process_counter_regression');
  const user = (after.process_before.user_ticks - before.process_before.user_ticks) / hz;
  const system = (after.process_before.system_ticks - before.process_before.system_ticks) / hz;
  const interval = midpoint(after.process_before) - midpoint(before.process_before);
  result.aggregate = {user_cpu_seconds: user, system_cpu_seconds: system, total_cpu_seconds: user + system,
    interval_seconds: interval, cpu_cores: (user + system) / interval, matched_thread_cpu_seconds: 0, residual_cpu_seconds: null};
  result.sampling = {before_read_seconds: before.read_finished_monotonic_seconds - before.read_started_monotonic_seconds,
    after_read_seconds: after.read_finished_monotonic_seconds - after.read_started_monotonic_seconds,
    interval_lower_seconds: after.read_started_monotonic_seconds - before.read_finished_monotonic_seconds,
    interval_upper_seconds: after.read_finished_monotonic_seconds - before.read_started_monotonic_seconds,
    interval_midpoint_seconds: midpoint(after) - midpoint(before), maximum_thread_midpoint_skew_seconds: 0};
  function index(sample, side) {
    const rows = new Map();
    const rejected = new Set();
    const extra = Math.max(0, (Array.isArray(sample.threads) ? sample.threads.length : 0) - THREAD_CPU_LIMITS.threads);
    if (extra) {
      result.accounting['omitted_lower_bound_' + side] += extra;
      result.accounting['omitted_' + side] = null;
      result.accounting['enumeration_complete_' + side] = false;
    }
    for (const p of (Array.isArray(sample.threads) ? sample.threads : []).slice(0, THREAD_CPU_LIMITS.threads)) {
      if (!Number.isSafeInteger(p?.tid) || p.tid <= 0 || !/^\d+$/.test(p.identity || '')
          || !counter(p.user_ticks) || !counter(p.system_ticks) || !stamp(p)
          || p.read_started_monotonic_seconds < sample.read_started_monotonic_seconds
          || p.read_finished_monotonic_seconds > sample.read_finished_monotonic_seconds || rows.has(p.tid) || rejected.has(p.tid)) {
        result.accounting.invalid_threads.push({side, tid: p?.tid ?? null, reason: 'invalid_or_duplicate_thread'});
        rows.delete(p?.tid); rejected.add(p?.tid);
      } else rows.set(p.tid, p);
    }
    return rows;
  }
  const first = index(before, 'before'), last = index(after, 'after');
  const description = p => ({tid: p.tid, identity: p.identity, name: p.name});
  for (const [tid, p] of first) {
    const q = last.get(tid);
    if (!q) { result.accounting.exited_threads.push(description(p)); continue; }
    if (p.identity !== q.identity) {
      result.accounting.reused_tids.push({tid, before_identity: p.identity, after_identity: q.identity});
      result.accounting.exited_threads.push(description(p)); result.accounting.new_threads.push(description(q));
      continue;
    }
    const seconds = midpoint(q) - midpoint(p);
    if (q.user_ticks < p.user_ticks || q.system_ticks < p.system_ticks || seconds <= 0) {
      result.accounting.invalid_threads.push({tid, identity: p.identity, reason: 'counter_regression_or_interval'}); continue;
    }
    const u = (q.user_ticks - p.user_ticks) / hz, s = (q.system_ticks - p.system_ticks) / hz;
    result.threads.push({tid, identity: p.identity, name: q.name, previous_name: p.name,
      user_cpu_seconds: u, system_cpu_seconds: s, total_cpu_seconds: u + s,
      interval_seconds: seconds, cpu_cores: (u + s) / seconds});
    result.sampling.maximum_thread_midpoint_skew_seconds = Math.max(result.sampling.maximum_thread_midpoint_skew_seconds,
      Math.abs(midpoint(p) - midpoint(before.process_before)), Math.abs(midpoint(q) - midpoint(after.process_before)));
  }
  for (const [tid, q] of last) if (!first.has(tid)) result.accounting.new_threads.push(description(q));
  result.threads.sort((a, b) => b.total_cpu_seconds - a.total_cpu_seconds || a.tid - b.tid);
  result.accounting.matched_threads = result.threads.length;
  result.aggregate.matched_thread_cpu_seconds = result.threads.reduce((sum, p) => sum + p.total_cpu_seconds, 0);
  result.aggregate.residual_cpu_seconds = result.aggregate.total_cpu_seconds - result.aggregate.matched_thread_cpu_seconds;
  result.available = true;
  return result;
}

function errorKind(text) {
  const value = String(text).toLowerCase();
  if (/unauthorized|forbidden|authentication|bearer|rate.?limit|invalid.?param/.test(value)) return 'protocol';
  if (/timed? ?out|deadline|timeout/.test(value)) return 'timeout';
  if (/no such table[^\n]*projects|table[^\n]*projects[^\n]*(not found|does not exist|missing)|missing[^\n]*projects|schema[_ ]drift/.test(value)) return 'database';
  return 'other';
}
function parseJson(text) { try { return JSON.parse(text); } catch { return null; } }
function procStat(pid) {
  try {
    const raw = fs.readFileSync('/proc/' + pid + '/stat', 'utf8');
    const fields = raw.slice(raw.lastIndexOf(')') + 2).trim().split(/\s+/);
    return {pid, state: fields[0], ppid: Number(fields[1]), group: Number(fields[2]),
      session: Number(fields[3]), ticks: Number(fields[11]) + Number(fields[12]),
      start: fields[19], threads: Number(fields[17])};
  } catch { return null; }
}
function groupMembers(group) {
  return fs.readdirSync('/proc').filter(name => /^\d+$/.test(name)).map(name => procStat(Number(name)))
    .filter(p => p && p.group === group && p.session === group);
}
function signalOwnedGroup(child, signal, requireLiveLeader = false) {
  if (!child.pid) return false;
  const current = procStat(child.pid);
  if (current && child.qualificationStart && current.start !== child.qualificationStart) {
    throw new QualificationError('Refusing to signal a process whose start identity changed');
  }
  if (requireLiveLeader && (!current || ['Z', 'X', 'x'].includes(current.state))) return false;
  try { process.kill(-child.pid, signal); return true; }
  catch (error) { if (error.code !== 'ESRCH') throw error; return false; }
}
async function sha256(filename) {
  const digest = crypto.createHash('sha256');
  for await (const chunk of fs.createReadStream(filename)) digest.update(chunk);
  return digest.digest('hex');
}
async function fingerprintTree(directories) {
  const output = [];
  async function walk(base) {
    let entries;
    try { entries = await fsp.readdir(base, {withFileTypes: true}); } catch (e) { if (e.code === 'ENOENT') return; throw e; }
    for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
      const name = path.join(base, entry.name);
      if (entry.isDirectory()) await walk(name);
      else if (entry.isFile()) {
        const stat = await fsp.stat(name);
        output.push([name, stat.size, stat.mtimeMs, await sha256(name)]);
      } else output.push([name, 'nonregular']);
    }
  }
  for (const directory of directories) await walk(directory);
  return canonical(output);
}
function captureProcess(argv, env, cwd, destination, timeoutSeconds) {
  return new Promise((resolve, reject) => {
    const terminalDeadline = nativeNow() + timeoutSeconds + 4;
    const child = spawn(argv[0], argv.slice(1), {env, cwd, detached: true, stdio: ['ignore', 'pipe', 'pipe']});
    child.qualificationStart = procStat(child.pid)?.start;
    let stdout = '', stderr = '', size = 0, timedOut = false, truncated = false, spawnError = null, finished = false;
    const collect = which => chunk => {
      const bytes = Buffer.from(chunk); const remaining = Math.max(0, MAX_CAPTURE - size); size += bytes.length;
      if (bytes.length > remaining) truncated = true;
      if (which === 'stdout') stdout += bytes.subarray(0, remaining).toString();
      else stderr += bytes.subarray(0, remaining).toString();
    };
    child.stdout?.on('data', collect('stdout')); child.stderr?.on('data', collect('stderr'));
    let killer, terminal;
    const finish = async (code, signal, drainIncomplete = false) => {
      if (finished) return;
      finished = true;
      clearTimeout(timer); clearTimeout(killer); clearTimeout(terminal);
      try {
        const afterExit = groupMembers(child.pid);
        const unexpected = afterExit.filter(p => p.pid !== child.pid).map(p => p.pid);
        const liveGroup = () => groupMembers(child.pid).some(p => !['Z', 'X', 'x'].includes(p.state));
        let forcedCleanup = false, cleanupError = null;
        // Pipe closure can precede the end of a helper's background work.
        // Retain that violation even when bounded cleanup succeeds.
        if (liveGroup()) {
          try {
            signalOwnedGroup(child, 'SIGTERM');
            const grace = Math.min(nativeNow() + 1, terminalDeadline);
            while (liveGroup() && nativeNow() < grace) await delay(0.02);
            if (liveGroup()) {
              forcedCleanup = true;
              signalOwnedGroup(child, 'SIGKILL');
              const drain = Math.min(nativeNow() + 1, terminalDeadline);
              while (liveGroup() && nativeNow() < drain) await delay(0.02);
            }
          } catch (error) { cleanupError = String(error); }
        }
        const remaining = groupMembers(child.pid).map(p => p.pid);
        const result = {argv, exit_code: code, signal, timed_out: timedOut, truncated,
          drain_incomplete: drainIncomplete, unexpected_group_pids: unexpected,
          remaining_group_pids: remaining, forced_cleanup: forcedCleanup, cleanup_error: cleanupError,
          process_group_clean: unexpected.length === 0 && remaining.length === 0 && !cleanupError,
          spawn_error: spawnError, stdout, stderr};
        fs.writeFileSync(destination, JSON.stringify(result, null, 2) + '\n', {flag: 'wx', mode: 0o600});
        resolve(result);
      } catch (error) { reject(new QualificationError('Cannot persist command receipt: ' + error, 'INCOMPLETE')); }
    };
    const timer = setTimeout(() => {
      timedOut = true;
      try { signalOwnedGroup(child, 'SIGTERM'); } catch (error) { spawnError = String(error); }
      killer = setTimeout(() => { try { signalOwnedGroup(child, 'SIGKILL'); } catch (error) { spawnError = String(error); } }, 1000);
    }, timeoutSeconds * 1000);
    // A descendant can retain inherited stdout after its parent exits. Pipe
    // closure is not a reliable process deadline; bound the final drain too.
    terminal = setTimeout(() => {
      timedOut = true;
      try { signalOwnedGroup(child, 'SIGKILL'); } catch (error) { spawnError = String(error); }
      child.stdout?.destroy(); child.stderr?.destroy(); child.unref();
      finish(child.exitCode, child.signalCode, true);
    }, (timeoutSeconds + 4) * 1000);
    child.on('error', error => { spawnError = String(error); });
    child.on('close', (code, signal) => finish(code, signal));
  });
}
async function choosePort() {
  const server = net.createServer();
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}
async function portClosed(port) {
  return await new Promise(resolve => {
    const socket = net.createConnection({host: '127.0.0.1', port});
    const finish = value => { socket.destroy(); resolve(value); };
    socket.once('connect', () => finish(false));
    socket.once('error', () => finish(true));
    socket.setTimeout(500, () => finish(false));
  });
}
function ownsListener(pid, port) {
  try {
    const socketIds = new Set(fs.readdirSync('/proc/' + pid + '/fd').flatMap(fd => {
      try { const value = fs.readlinkSync('/proc/' + pid + '/fd/' + fd).match(/^socket:\[(\d+)\]$/); return value ? [value[1]] : []; } catch { return []; }
    }));
    return ['/proc/net/tcp', '/proc/net/tcp6'].some(filename => fs.readFileSync(filename, 'utf8').trim().split('\n').slice(1).some(line => {
      const parts = line.trim().split(/\s+/);
      return parts[3] === '0A' && parseInt(parts[1].split(':')[1], 16) === port && socketIds.has(parts[9]);
    }));
  } catch { return false; }
}

export async function makeNodeIO(binary, outputDirectory, options = {}) {
  need(process.platform === 'linux', 'This process/descriptor qualification adapter requires Linux /proc');
  const binaryPath = await fsp.realpath(binary);
  const info = await fsp.stat(binaryPath);
  need(info.isFile() && (info.mode & 0o111) !== 0, 'Supplied binary must be an executable regular file');
  const digest = await sha256(binaryPath);
  if (options.expectedSha256) need(digest === options.expectedSha256, 'Supplied binary SHA256 does not match expected SHA256');
  const root = await fsp.mkdtemp(path.join(path.resolve(outputDirectory), 'agent-mail-gh407-'));
  await fsp.chmod(root, 0o700);
  const identity = {binary: binaryPath, binary_sha256: digest, source_sha_asserted: options.sourceSha || null,
    source_binding: options.sourceSha ? 'Caller assertion; verify build receipt independently' : 'No source commit asserted',
    node: process.version, platform: process.platform, run_directory: root,
    harness_sha256: await sha256(fileURLToPath(import.meta.url))};
  const sessions = [];
  const io = {kind: options.evidenceClass || 'supplied_binary', identity, now: nativeNow, root, cancelled: null,
    async start(label, cfg) {
      need(!io.cancelled, 'Qualification interrupted by ' + io.cancelled);
      const directory = path.join(root, label);
      fs.mkdirSync(directory, {mode: 0o700});
      for (const name of ['archive', 'db', 'cwd', 'projects', 'config/mcp-agent-mail', 'data', 'cache', 'state', 'runtime', 'tmp']) fs.mkdirSync(path.join(directory, name), {recursive: true, mode: 0o700});
      fs.writeFileSync(path.join(directory, 'config/mcp-agent-mail/config.env'), '# Private qualification authority\n', {flag: 'wx', mode: 0o600});
      const database = path.join(directory, 'db/mailbox.sqlite3');
      const port = await choosePort();
      const env = {PATH: process.env.PATH || '/usr/bin:/bin', LANG: 'C.UTF-8', TZ: 'UTC',
        XDG_CONFIG_HOME: path.join(directory, 'config'), XDG_DATA_HOME: path.join(directory, 'data'),
        XDG_CACHE_HOME: path.join(directory, 'cache'), XDG_STATE_HOME: path.join(directory, 'state'),
        XDG_RUNTIME_DIR: path.join(directory, 'runtime'), TMPDIR: path.join(directory, 'tmp'),
        STORAGE_ROOT: path.join(directory, 'archive'), DATABASE_URL: 'sqlite://' + database,
        AM_INTERFACE_MODE: 'cli', AM_ALLOW_EPHEMERAL_PROJECT_ROOTS: '1',
        HTTP_HOST: '127.0.0.1', HTTP_PORT: String(port), HTTP_PATH: '/mcp/',
        TUI_ENABLED: 'false', DATABASE_POOL_SIZE: '1', DATABASE_MAX_OVERFLOW: '0',
        LOG_LEVEL: 'warn', LOG_RICH_ENABLED: 'false', LOG_JSON_ENABLED: 'true',
        DB_MAINTENANCE_ENABLED: 'false', INTEGRITY_GUARD_ENABLED: 'false',
      };
      const argv = [binaryPath, 'serve-http', '--host', '127.0.0.1', '--port', String(port), '--path', '/mcp/', '--no-tui', '--no-auth'];
      fs.writeFileSync(path.join(directory, 'launch.json'), JSON.stringify({argv, env, cwd: path.join(directory, 'cwd')}, null, 2) + '\n', {flag: 'wx', mode: 0o600});
      // Resolve measurement prerequisites before starting an owned server.
      const clock = await captureProcess(['/usr/bin/getconf', 'CLK_TCK'], env, directory,
        path.join(directory, 'clock.json'), cfg.commandTimeout);
      const ticksPerSecond = Number(clock.stdout.trim());
      need(clock.exit_code === 0 && !clock.timed_out && !clock.drain_incomplete && !clock.truncated
        && clock.process_group_clean === true && ticksPerSecond > 0,
        'Cannot establish Linux CPU clock frequency from a clean helper');
      const version = await captureProcess([binaryPath, '--version'], env, path.join(directory, 'cwd'),
        path.join(directory, 'version.json'), cfg.commandTimeout);
      identity.version_output = version.stdout.trim();
      need(!io.cancelled, 'Qualification interrupted by ' + io.cancelled);
      need(version.exit_code === 0 && !version.timed_out && !version.drain_incomplete && !version.truncated
        && version.process_group_clean === true,
        'Supplied binary version command did not complete cleanly');
      const logFd = fs.openSync(path.join(directory, 'server.log'), 'wx', 0o600);
      const child = spawn(binaryPath, argv.slice(1), {env, cwd: path.join(directory, 'cwd'), detached: true, stdio: ['ignore', 'pipe', 'pipe']});
      child.qualificationStart = procStat(child.pid)?.start;
      let log = '', logBytes = 0, logComplete = true, exitCode = null, exitSignal = null, exited = false, spawnError = null;
      let evidenceError = null;
      const onData = data => {
        const bytes = Buffer.from(data), remaining = Math.max(0, MAX_CAPTURE - logBytes);
        logBytes += bytes.length;
        if (bytes.length > remaining) logComplete = false;
        const retained = bytes.subarray(0, remaining);
        log += retained.toString();
        try { fs.writeSync(logFd, retained); }
        catch (error) { logComplete = false; evidenceError = String(error); }
      };
      child.stdout.on('data', onData); child.stderr.on('data', onData);
      child.once('error', error => { spawnError = String(error); });
      const exitPromise = new Promise(resolve => child.once('close', (code, signal) => {
        exited = true; exitCode = code; exitSignal = signal;
        try { fs.closeSync(logFd); } catch (error) { evidenceError = String(error); }
        resolve();
      }));
      const birth = child.qualificationStart;
      let sequence = 0, stopped = null, suspended = false;
      const resourceFile = path.join(directory, 'resources.jsonl');
      const threadCpu = createThreadCpuSampler({pid: child.pid, identity: birth, ticksPerSecond,
        filename: path.join(directory, 'thread-cpu.jsonl')});
      function snapshot(includeThreadSample = false) {
        try {
          const current = procStat(child.pid);
          if (!current) return {available: false, identity: 'missing'};
          const status = fs.readFileSync('/proc/' + child.pid + '/status', 'utf8');
          const rss = Number(status.match(/^VmRSS:\s+(\d+)/m)?.[1]);
          const result = {available: Number.isFinite(rss), threads: current.threads,
            fds: fs.readdirSync('/proc/' + child.pid + '/fd').length, rss_bytes: rss * 1024,
            cpu_seconds: current.ticks / ticksPerSecond, identity: current.start === birth ? birth : 'changed',
            children: groupMembers(child.pid).filter(p => p.pid !== child.pid && p.state !== 'Z').length,
            at_monotonic_seconds: nativeNow()};
          let diagnostic;
          // Fresh explicit boundaries cannot borrow a cached sample taken
          // before fault setup or before the request-free observation began.
          try { diagnostic = threadCpu.sample({fresh: includeThreadSample}); }
          catch (error) { diagnostic = {reference: {available: false, reason: 'diagnostic_error', error: String(error).slice(0, 512)}}; }
          result.thread_cpu = diagnostic?.reference || {available: false, reason: 'diagnostic_unavailable'};
          fs.appendFileSync(resourceFile, JSON.stringify(result) + '\n', {mode: 0o600});
          if (includeThreadSample) result.thread_cpu_sample = diagnostic?.sample;
          return result;
        } catch (error) { return {available: false, error: String(error)}; }
      }
      const monitor = setInterval(snapshot, cfg.resourceInterval * 1000);
      function transport(route, payload) {
        const begin = nativeNow();
        if (io.cancelled) return Promise.reject(new QualificationError('Qualification interrupted by ' + io.cancelled, 'INCOMPLETE'));
        if (exited || spawnError) return Promise.resolve({status: 0, body: null, duration_ms: 0, startup_fatal: true, error: spawnError || 'Owned process exited', exit_code: exitCode});
        if (!ownsListener(child.pid, port) || procStat(child.pid)?.start !== birth) return Promise.resolve({status: 0, body: null, duration_ms: 0, error: 'Owned listener not available; no request sent'});
        return new Promise(resolve => {
          const content = payload === undefined ? undefined : Buffer.from(JSON.stringify(payload));
          let completed = false, body = '', total = 0;
          const finish = result => {
            if (completed) return; completed = true; clearTimeout(timer);
            resolve({...result, duration_ms: (nativeNow() - begin) * 1000});
          };
          const request = http.request({host: '127.0.0.1', port, path: route, method: content ? 'POST' : 'GET', agent: false,
            headers: {'User-Agent': 'mcp_agent_mail_rust-gh407-qualification', Accept: 'application/json', Connection: 'close',
              ...(content ? {'Content-Type': 'application/json', 'Content-Length': content.length} : {})}}, response => {
            response.on('data', data => {
              total += data.length;
              if (total > 2 * 1024 * 1024) { request.destroy(); finish({status: response.statusCode, body: null, error: 'Response size limit'}); }
              else body += data.toString();
            });
            response.on('end', () => finish({status: response.statusCode, body: parseJson(body), raw_body: parseJson(body) ? undefined : body}));
            response.on('error', error => finish({status: response.statusCode, body: null, error: String(error)}));
          });
          const timer = setTimeout(() => { request.destroy(); finish({status: 0, body: null, error: 'request timeout'}); }, cfg.requestTimeout * 1000);
          request.on('error', error => finish({status: 0, body: null, error: String(error)}));
          request.end(content);
        });
      }
      const session = {label, directory,
        get: route => transport(route),
        async idle(seconds) {
          const until = nativeNow() + seconds;
          while (nativeNow() < until) {
            need(!io.cancelled, 'Qualification interrupted by ' + io.cancelled);
            await delay(Math.min(0.25, until - nativeNow()));
          }
        },
        resources: async () => snapshot(true),
        logs: async () => ({complete: logComplete && !evidenceError, probe_failures: log.split('serving-pool readiness probe failed').length - 1, evidence_error: evidenceError}),
        async ensureProject(tag) {
          const project = path.join(directory, 'projects', tag + '-' + crypto.randomUUID());
          fs.mkdirSync(project, {mode: 0o700});
          const requestId = crypto.randomUUID();
          const result = await transport('/mcp/', {jsonrpc: '2.0', id: requestId, method: 'tools/call',
            params: {name: 'ensure_project', arguments: {human_key: project}}});
          const rpc = result.body;
          const texts = rpc?.result?.content?.filter(c => c.type === 'text').map(c => c.text).join('\n') || '';
          const value = rpc?.result?.structuredContent || parseJson(texts);
          const correlated = rpc?.jsonrpc === '2.0' && rpc?.id === requestId;
          const ok = correlated && result.status === 200 && !rpc?.error && rpc?.result?.isError !== true
            && value && typeof value === 'object' && value.human_key === project;
          const detail = {response: result, project};
          return {ok: Boolean(ok), error_kind: ok ? null : correlated ? errorKind(JSON.stringify(rpc) + ' ' + (result.error || '')) : 'protocol', detail};
        },
        async sql(kind, statement) {
          const filename = path.join(directory, 'sql-' + String(++sequence).padStart(4, '0') + '.json');
          const result = await captureProcess([binaryPath, 'tooling', kind === 'query' ? 'db-query' : 'db-exec', '--db', database, '--sql', statement, '--json'], env,
            path.join(directory, 'cwd'), filename, cfg.commandTimeout);
          const parsed = parseJson(result.stdout);
          const ok = result.exit_code === 0 && !result.timed_out && !result.truncated
            && !result.drain_incomplete && result.process_group_clean === true
            && (kind === 'query' ? Array.isArray(parsed) : parsed?.ok === true);
          return {ok, rows: kind === 'query' && ok ? parsed : undefined,
            error_kind: ok ? null : result.process_group_clean !== true ? 'process' : errorKind(result.stdout + '\n' + result.stderr), receipt: filename,
            detail: ok ? undefined : result};
        },
        async suspend() {
          need(!exited && procStat(child.pid)?.start === birth, 'Owned process unavailable for stale challenge');
          signalOwnedGroup(child, 'SIGSTOP'); suspended = true;
          const deadline = nativeNow() + 2;
          while (!['T', 't'].includes(procStat(child.pid)?.state) && nativeNow() < deadline) await delay(0.01);
          need(['T', 't'].includes(procStat(child.pid)?.state), 'Could not confirm the owned process stopped');
        },
        async resume() { if (suspended) { signalOwnedGroup(child, 'SIGCONT'); suspended = false; } },
        interrupt() {
          if (suspended) { signalOwnedGroup(child, 'SIGCONT'); suspended = false; }
          if (!exited) signalOwnedGroup(child, 'SIGTERM');
        },
        async stop() {
          if (stopped) return stopped;
          clearInterval(monitor);
          await session.resume();
          const beforeSignal = procStat(child.pid);
          const identityUnchanged = Boolean(beforeSignal && beforeSignal.start === birth);
          requireThat(!beforeSignal || identityUnchanged, 'Refusing shutdown signals after a process start-identity change');
          const alreadyExited = exited || !beforeSignal || ['Z', 'X', 'x'].includes(beforeSignal.state);
          let forced = false;
          // A prior exit, even exit 0, does not exercise the shutdown handler.
          // Still clean up descendants and collect closure/quiet evidence.
          const sigtermSent = !alreadyExited && signalOwnedGroup(child, 'SIGTERM', true);
          await waitWithin(exitPromise, cfg.shutdownTimeout);
          if (!exited || groupMembers(child.pid).some(p => p.state !== 'Z')) {
            forced = true; signalOwnedGroup(child, 'SIGKILL');
            await waitWithin(exitPromise, 3);
          }
          const remaining = groupMembers(child.pid).map(p => p.pid);
          const before = await fingerprintTree([path.join(directory, 'db'), path.join(directory, 'archive')]);
          await delay(1);
          const after = await fingerprintTree([path.join(directory, 'db'), path.join(directory, 'archive')]);
          stopped = {exited, exit_code: exitCode, signal: exitSignal, spawn_error: spawnError, forced,
            remaining_pids: remaining, port_closed: await portClosed(port), quiet: before === after,
            quiet_seconds: 1, identity_unchanged: identityUnchanged,
            already_exited_before_shutdown: alreadyExited, sigterm_sent: sigtermSent,
            process_state_before_shutdown: beforeSignal?.state || null};
          return stopped;
        },
      };
      sessions.push(session);
      return session;
    },
    abort(signal) {
      io.cancelled = signal;
      for (const session of sessions) { try { session.interrupt(); } catch {} }
    },
    async cleanup() { return await Promise.allSettled(sessions.map(session => session.stop())); },
    async verifyBinary() { return (await sha256(binaryPath)) === digest; },
  };
  return io;
}

function usage() {
  return 'Usage: node scripts/qualify_health_idle.mjs --binary /absolute/am [--output-parent DIR]\n'
    + '  [--source-sha SHA] [--expected-sha256 SHA256] [--idle-seconds 8]\n'
    + '  [--idle-cycles 3] [--concurrency 16] [--soak-seconds 30]\n'
    + '  [--evidence-class supplied_binary|harness_surrogate]\n'
    + 'Linux only. Exit 0=PASS, 1=FAIL, 2=INCOMPLETE/usage. No build or remote actions.\n';
}
export async function main(argv = process.argv.slice(2)) {
  const args = {}, cfg = {};
  const numeric = {'idle-seconds': 'idleSeconds', 'idle-cycles': 'idleCycles', concurrency: 'concurrency', 'soak-seconds': 'soakSeconds'};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--help') { process.stdout.write(usage()); return 0; }
    need(argv[i].startsWith('--') && argv[i + 1] && !argv[i + 1].startsWith('--'), usage());
    const key = argv[i].slice(2), value = argv[++i];
    if (Object.hasOwn(numeric, key)) { cfg[numeric[key]] = Number(value); continue; }
    need(['binary', 'output-parent', 'source-sha', 'expected-sha256', 'evidence-class'].includes(key), 'Unknown option --' + key);
    args[key] = value;
  }
  need(args.binary, usage());
  need(!args['evidence-class'] || ['supplied_binary', 'harness_surrogate'].includes(args['evidence-class']), 'Invalid evidence class');
  const effective = {...DEFAULTS, ...cfg};
  need(Number.isFinite(effective.idleSeconds) && effective.idleSeconds > DEFAULTS.maxSampleAge,
    '--idle-seconds must exceed the six-second sample lifetime');
  need(Number.isInteger(effective.idleCycles) && effective.idleCycles >= 2 && effective.idleCycles <= 100, '--idle-cycles must be 2..100');
  need(Number.isInteger(effective.concurrency) && effective.concurrency >= 2 && effective.concurrency <= 128, '--concurrency must be 2..128');
  need(Number.isFinite(effective.soakSeconds) && effective.soakSeconds >= 8, '--soak-seconds must be at least 8');
  const output = path.resolve(args['output-parent'] || os.tmpdir());
  await fsp.mkdir(output, {recursive: true, mode: 0o700});
  const io = await makeNodeIO(args.binary, output, {sourceSha: args['source-sha'], expectedSha256: args['expected-sha256'], evidenceClass: args['evidence-class']});
  process.stderr.write('Qualification artifacts: ' + io.root + '\n');
  let report;
  const onInterrupt = () => io.abort('SIGINT');
  const onTerminate = () => io.abort('SIGTERM');
  process.once('SIGINT', onInterrupt); process.once('SIGTERM', onTerminate);
  try { report = await runQualification(io, cfg); }
  catch (error) { report = {schema_version: 1, identity: io.identity, evidence_class: io.kind, cases: [{name: 'fatal', status: error.status || 'FAIL', error: String(error)}]}; }
  finally {
    const cleanup = await io.cleanup();
    process.removeListener('SIGINT', onInterrupt); process.removeListener('SIGTERM', onTerminate);
    for (const result of cleanup) if (result.status === 'rejected') report.cases.push({name: 'cleanup_error', status: 'FAIL', error: String(result.reason)});
  }
  if (io.cancelled) report.cases.push({name: 'interrupted', status: 'INCOMPLETE', signal: io.cancelled});
  try {
    report.cases.push({name: 'binary_identity_stable', status: await io.verifyBinary() ? 'PASS' : 'FAIL'});
  } catch (error) { report.cases.push({name: 'binary_identity_stable', status: 'INCOMPLETE', error: String(error)}); }
  report.status = report.cases.some(c => c.status === 'FAIL') ? 'FAIL' : report.cases.some(c => c.status === 'INCOMPLETE') ? 'INCOMPLETE' : 'PASS';
  report.exit_code = {PASS: 0, FAIL: 1, INCOMPLETE: 2}[report.status];
  const filename = path.join(io.root, 'report.json');
  await fsp.writeFile(filename, JSON.stringify(report, null, 2) + '\n', {flag: 'wx', mode: 0o600});
  process.stdout.write(JSON.stringify({status: report.status, evidence_class: report.evidence_class, report: filename,
    cases: report.cases.map(({name, status}) => ({name, status}))}, null, 2) + '\n');
  return report.exit_code;
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then(code => { process.exitCode = code; }).catch(error => {
    process.stderr.write(String(error.message || error) + '\n'); process.exitCode = error.status === 'FAIL' ? 1 : 2;
  });
}
