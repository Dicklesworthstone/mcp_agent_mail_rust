#!/usr/bin/env node
/**
 * Deterministic, adversarial tests of the qualification harness itself.
 *
 * These models are NOT Agent Mail, FrankenSQLite, or Asupersync evidence. They
 * execute the exact orchestration/decision code using virtual time and injected
 * I/O, preserving the real 2 s refresh / 6 s sample age / 8 s idle contract.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { runQualification, DEFAULTS, THREAD_CPU_LIMITS, parseThreadCpuStat,
  sampleThreadCpu, createThreadCpuSampler, threadCpuDelta } from '../scripts/qualify_health_idle.mjs';

export function modelIO(mode = 'healthy', additionalFaults = {}) {
  const has = (name) => mode === name || additionalFaults[name] === true;
  let clock = 0;
  const events = [];
  const sessions = [];
  const io = {
    kind: 'harness_model',
    identity: { label: `GH407 HARNESS MODEL: ${mode}`, additionalFaults, native: false },
    now: () => clock,
    events,
    sessions,
    async start(label, cfg) {
      const startTime = clock;
      let fault = false;
      let suspended = false;
      let stopped = false;
      let nextProbe = clock + 2;
      let requestedProbe = null;
      let sample = { ok: true, observed: clock };
      let failures = 0;
      let everFailed = false;
      let resumedWithFault = false;
      const rows = [];
      const requestsAtTime = new Map();
      const event = (kind, data = {}) => events.push({ t: clock, label, kind, ...data });
      const copyRows = () => rows.map((r) => ({ ...r }));

      function probe(at, fromRequest = false) {
        if (stopped || suspended) return;
        if (fault) {
          failures += 1;
          everFailed = true;
        }
        if (!(has('polling_only_recovery') && everFailed && !fault && !fromRequest)
            && !(has('outage_sample_stale') && fault)) {
          sample = { ok: !fault, observed: at };
        }
      }

      function pump(until) {
        if (suspended || stopped) return;
        while (Math.min(nextProbe, requestedProbe ?? Infinity) <= until) {
          if (requestedProbe !== null && requestedProbe <= nextProbe) {
            const at = requestedProbe;
            requestedProbe = null;
            probe(at, true);
          } else {
            if (!has('idle_refresh_missing') && !has('delayed_idle_refresh')) probe(nextProbe);
            nextProbe += has('unbounded_probe_rate') ? 0.05 : 2;
          }
        }
      }

      const session = {
        label,
        async get(path) {
          pump(clock);
          event('http', { path });
          if (stopped || suspended) {
            return { status: 0, body: null, duration_ms: 5000, error: 'unavailable' };
          }
          if (path === '/healthz' || path === '/health/liveness') {
            return { status: fault && has('liveness_failed') ? 503 : 200,
              body: { status: 'alive' }, duration_ms: 1 };
          }
          if (has('malformed_health')) {
            return { status: 200, body: 'not a readiness object', duration_ms: 1 };
          }
          const n = (requestsAtTime.get(clock) || 0) + 1;
          requestsAtTime.set(clock, n);
          const stale = clock - sample.observed > 6;
          let ready = sample.ok && !stale;
          if (fault && has('false_ready')) ready = true;
          if (fault && resumedWithFault && has('stale_false_ready')) ready = true;
          if (!fault && has('concurrent_one_failure') && n === cfg.concurrency) ready = false;
          const body = {
            status: ready ? 'ready' : 'unavailable',
            capabilities: { mail_read: ready, mail_write: ready, lexical_search: false },
            lexical_state: 'disabled', lexical_fresh: false, durability_state: 'not_probed',
          };
          if (!ready) body.reason = stale ? 'database_probe_stale' : 'database_probe_failed';
          if (fault && has('false_capabilities')) body.capabilities.mail_read = true;
          if (fault && has('missing_capabilities')) delete body.capabilities;
          const response = { status: ready ? 200 : 503, body, duration_ms: 1 };
          // Preserve the first response. A later refresh must never launder it.
          if (has('delayed_idle_refresh')) {
            // A returned response precedes the request-triggered worker. This
            // reproduces the native prerequisite cascade without sleeping or
            // shortening any public readiness interval.
            requestedProbe ??= clock + 0.01;
          } else if (has('idle_refresh_missing') || has('polling_only_recovery')) {
            probe(clock, true);
          }
          return response;
        },
        async ensureProject(tag) {
          pump(clock);
          event('mcp', { tag });
          if (fault) {
            if (has('mcp_fault_ineffective')) return { ok: true, detail: 'cached response' };
            if (has('mcp_auth_error')) return { ok: false, error_kind: 'auth', detail: '401' };
            return { ok: false, error_kind: 'database', detail: 'no such table: projects' };
          }
          if (has('recovery_database_failed') && everFailed) {
            return { ok: false, error_kind: 'database', detail: 'independent recovery operation failed' };
          }
          if (!rows.some((r) => r.human_key === tag)) {
            rows.push({ id: rows.length + 1, slug: String(tag), human_key: String(tag), created_at: 'fixture' });
          }
          return { ok: true, value: rows.find((r) => r.human_key === tag) };
        },
        async sql(kind, sql) {
          pump(clock);
          event('sql', { operation: kind, sql });
          const normalized = sql.replace(/["`]/g, '').replace(/\s+/g, ' ').trim().toLowerCase();
          if (kind === 'exec') {
            if (normalized.includes('alter table projects rename to gh407_saved_projects')) {
              if (has('fault_refused')) return { ok: false, error_kind: 'other', detail: 'refused' };
              if (!has('ineffective_fault')) fault = true;
              return { ok: true };
            }
            if (normalized.includes('alter table gh407_saved_projects rename to projects')) {
              if (has('restore_failed')) return { ok: false, error_kind: 'database', detail: 'restore failed' };
              fault = false;
              if (has('seed_lost_on_restore')) rows.splice(0, rows.length);
              return { ok: true };
            }
            throw new Error(`Model does not understand mutation: ${sql}`);
          }
          if (normalized.includes('sqlite_master')) {
            return { ok: true, rows: [{ name: fault ? 'gh407_saved_projects' : 'projects' }] };
          }
          const namedSaved = /from gh407_saved_projects\b/.test(normalized);
          if ((namedSaved && !fault) || (!namedSaved && fault)) {
            return { ok: false, error_kind: has('query_wrong_error') ? 'auth' : 'database',
              detail: 'no such table: projects' };
          }
          return { ok: true, rows: copyRows() };
        },
        async idle(seconds) {
          event('idle_begin', { seconds });
          const until = clock + seconds;
          pump(until);
          clock = until;
          event('idle_end', { seconds });
        },
        async resources() {
          pump(clock);
          event('resources');
          if (has('resources_unavailable')) return { available: false, detail: 'unsupported' };
          const elapsed = clock - startTime;
          const growth = has('resource_growth') ? Math.floor(elapsed / 2) * 8 : 0;
          return { available: true, threads: 5 + growth, fds: 12 + growth,
            rss_bytes: 16 * 1024 * 1024 + growth * 1024 * 1024,
            cpu_seconds: elapsed * (has('idle_cpu_runaway') ? 2 : 0.001),
            identity: has('identity_changed') && elapsed >= 8 ? 'changed' : 'same',
            children: has('child_leaked') ? 1 : 0 };
        },
        async logs() {
          pump(clock);
          event('logs');
          return { complete: !has('logs_unavailable'), probe_failures: failures };
        },
        async suspend() {
          pump(clock);
          event('suspend');
          suspended = true;
          if (has('partial_suspend_error')) {
            throw new Error('The process stopped, but confirmation of the stop failed');
          }
        },
        async resume() {
          event('resume');
          resumedWithFault = fault;
          suspended = false;
          // First external request races an owned worker, as in a real process.
          nextProbe = clock + 0.2;
        },
        async stop() {
          event('stop', { fault });
          stopped = true;
          const forced = has('forced_shutdown');
          return { exited: true, exit_code: forced ? 137 : 0, forced,
            remaining_pids: has('child_leaked') ? [99999] : [],
            port_closed: !has('port_left_open'), quiet: !has('post_shutdown_mutation'),
            already_exited_before_shutdown: has('exited_before_shutdown'),
            sigterm_sent: !has('exited_before_shutdown') && !has('sigterm_not_sent'),
            identity_unchanged: !has('identity_changed') };
        },
      };
      sessions.push(session);
      event('start');
      return session;
    },
  };
  return io;
}

const expectations = [
  ['healthy', 'PASS'],
  ['idle_refresh_missing', 'FAIL'],
  ['delayed_idle_refresh', 'FAIL'],
  ['false_ready', 'FAIL'],
  ['false_capabilities', 'FAIL'],
  ['missing_capabilities', 'FAIL'],
  ['polling_only_recovery', 'FAIL'],
  ['recovery_database_failed', 'FAIL'],
  ['stale_false_ready', 'FAIL'],
  ['concurrent_one_failure', 'FAIL'],
  ['liveness_failed', 'FAIL'],
  ['resource_growth', 'FAIL'],
  ['idle_cpu_runaway', 'FAIL'],
  ['unbounded_probe_rate', 'FAIL'],
  ['forced_shutdown', 'FAIL'],
  ['exited_before_shutdown', 'FAIL'],
  ['sigterm_not_sent', 'FAIL'],
  ['port_left_open', 'FAIL'],
  ['post_shutdown_mutation', 'FAIL'],
  ['identity_changed', 'FAIL'],
  ['child_leaked', 'FAIL'],
  ['ineffective_fault', 'INCOMPLETE'],
  ['fault_refused', 'INCOMPLETE'],
  ['mcp_fault_ineffective', 'INCOMPLETE'],
  ['mcp_auth_error', 'INCOMPLETE'],
  ['query_wrong_error', 'INCOMPLETE'],
  ['resources_unavailable', 'INCOMPLETE'],
  ['logs_unavailable', 'INCOMPLETE'],
];

function failureSummary(report) {
  return JSON.stringify({ status: report.status, cases: report.cases
    .filter((c) => c.status !== 'PASS')
    .map((c) => ({ name: c.name, status: c.status, error: c.error?.slice(0, 240) })) });
}

for (const [mode, expected] of expectations) {
  test(`HARNESS MODEL ONLY: ${mode} yields ${expected}`, async () => {
    const io = modelIO(mode);
    const report = await runQualification(io);
    assert.equal(report.status, expected, failureSummary(report));
    assert.equal(report.exit_code, { PASS: 0, FAIL: 1, INCOMPLETE: 2 }[expected]);
    assert.ok(report.cases.length > 0, 'zero-case result cannot qualify anything');
    if (['exited_before_shutdown', 'sigterm_not_sent'].includes(mode)) {
      assert.deepEqual(report.cases.filter((c) => c.status !== 'PASS').map((c) => c.name),
        ['healthy_shutdown', 'stale_shutdown', 'shutdown_with_database_unavailable', 'outage_process_cleanup'],
        'the shutdown oracle must reject the receipt in all three server lifetimes');
    }
    if (mode === 'healthy') {
      assert.deepEqual(io.sessions.map((session) => session.label),
        ['healthy', 'stale_success', 'outage_shutdown'],
        'stale-sample safety must have its own healthy fixture after earlier failures');
      const idleWindows = io.events.filter((e) => e.kind === 'idle_begin' && e.seconds >= 8);
      assert.ok(idleWindows.length >= DEFAULTS.idleCycles, 'exact idle windows were lost');
      assert.ok(io.events.some((e) => e.kind === 'stop' && e.fault), 'no shutdown while fault active');
      assert.ok(io.events.some((e) => e.kind === 'suspend'), 'stale sample arm absent');
      assert.ok(io.events.some((e) => e.kind === 'resume'), 'suspended process was not resumed');
      const groups = new Map();
      for (const e of io.events.filter((e) => e.kind === 'http' && ['/health', '/health/readiness'].includes(e.path))) {
        const key = `${e.label}:${e.t}`;
        groups.set(key, (groups.get(key) || 0) + 1);
      }
      assert.ok(Math.max(...groups.values()) >= DEFAULTS.concurrency, 'concurrent burst was lost');
    }
  });
}

test('HARNESS MODEL ONLY: failed restore cannot be called PASS', async () => {
  const report = await runQualification(modelIO('restore_failed'));
  assert.notEqual(report.status, 'PASS', failureSummary(report));
});

test('HARNESS MODEL ONLY: preserved project data is part of recovery', async () => {
  const report = await runQualification(modelIO('seed_lost_on_restore'));
  assert.notEqual(report.status, 'PASS', failureSummary(report));
});

test('HARNESS MODEL ONLY: malformed 200 does not count as readiness', async () => {
  const report = await runQualification(modelIO('malformed_health'));
  assert.notEqual(report.status, 'PASS', failureSummary(report));
});

test('HARNESS MODEL ONLY: partially failed suspension is resumed before later work', async () => {
  const io = modelIO('partial_suspend_error');
  const report = await runQualification(io);
  assert.equal(report.status, 'FAIL', failureSummary(report));
  const stopped = io.events.findIndex((e) => e.kind === 'suspend');
  assert.ok(stopped >= 0, 'suspension control was not reached');
  assert.equal(io.events[stopped + 1]?.kind, 'resume',
    'a partial stop failure must run the resume finally path immediately');
  const clean = report.cases.find((c) => c.name === 'healthy_shutdown');
  assert.equal(clean?.status, 'PASS', failureSummary(report));
});

function caseStatus(report, name) {
  const matches = report.cases.filter((entry) => entry.name === name);
  assert.equal(matches.length, 1, `Expected one ${name} case: ${failureSummary(report)}`);
  return matches[0].status;
}

function assertSilentIdleWindows(io) {
  for (let index = 0; index < io.events.length; index += 1) {
    const begin = io.events[index];
    if (begin.kind !== 'idle_begin' || begin.seconds < DEFAULTS.idleSeconds) continue;
    const end = io.events.findIndex((entry, position) => position > index
      && entry.label === begin.label && entry.kind === 'idle_end');
    assert.ok(end > index, 'Every measured idle window must have a retained end');
    assert.ok(io.events[end].t - begin.t >= begin.seconds, 'Measured idle was shortened');
    assert.deepEqual(io.events.slice(index + 1, end)
      .filter((entry) => ['http', 'mcp', 'sql'].includes(entry.kind)), [],
    'Preparation and database witnesses must stay outside measured idle windows');
  }
}

function assertMcpAfterStaleFirstResponse(report, session, idleLabel) {
  const observations = report.observations;
  const end = observations.findIndex((entry) => entry.kind === 'idle_end'
    && entry.session === session && entry.label === idleLabel);
  assert.ok(end >= 0, `The ${session}/${idleLabel} idle interval did not execute`);
  const first = observations.findIndex((entry, index) => index > end
    && entry.session === session && entry.kind === 'http');
  assert.ok(first > end, 'The first response after idle must be retained');
  assert.equal(observations[first].response.status, 503);
  assert.equal(observations[first].response.body.reason, 'database_probe_stale');
  const control = observations.findIndex((entry, index) => index > first
    && entry.session === session && entry.kind === 'database_control');
  assert.ok(control > first, 'A stale health response must not suppress its fresh database witness');
  assert.equal(observations[control].control.ok, true,
    'The independent operation must establish that the synthetic database is usable');
  assert.deepEqual(observations.slice(first + 1, control)
    .filter((entry) => entry.session === session
      && ['http', 'database_control', 'sql'].includes(entry.kind)), [],
  'The fresh MCP operation must precede every readiness retry or other client operation');
}

test('HARNESS MODEL ONLY: delayed request refresh preserves every repeated idle failure and database witness', async () => {
  const io = modelIO('delayed_idle_refresh');
  const report = await runQualification(io);
  assert.equal(report.status, 'FAIL', failureSummary(report));
  for (let cycle = 1; cycle <= DEFAULTS.idleCycles; cycle += 1) {
    assert.equal(caseStatus(report, `idle_first_request_${cycle}`), 'FAIL');
    assert.equal(caseStatus(report, `idle_database_${cycle}`), 'PASS');
    assertMcpAfterStaleFirstResponse(report, 'healthy', `idle_${cycle - 1}`);
  }
  const completed = report.observations.filter((entry) => entry.kind === 'idle_end'
    && entry.session === 'healthy' && /^idle_\d+$/.test(entry.label));
  assert.equal(completed.length, DEFAULTS.idleCycles,
    'A delayed refresh after one failed trial must not prevent later complete idle trials');
  assertSilentIdleWindows(io);
});

test('HARNESS MODEL ONLY: stale readiness cannot hide bad outage liveness, cadence, or CPU', async () => {
  const io = modelIO('outage_sample_stale', {
    liveness_failed: true, unbounded_probe_rate: true, idle_cpu_runaway: true,
  });
  const report = await runQualification(io);
  assert.equal(report.status, 'FAIL', failureSummary(report));
  for (const name of ['genuine_database_unavailability', 'database_outage_liveness',
    'concurrent_requests_during_database_outage', 'failed_probe_cadence', 'failed_probe_cpu']) {
    assert.equal(caseStatus(report, name), 'FAIL', `${name} was hidden by an earlier failure`);
  }
  assertSilentIdleWindows(io);
});

test('HARNESS MODEL ONLY: recovery records a fresh database success after a retained stale first response', async () => {
  const io = modelIO('delayed_idle_refresh');
  const report = await runQualification(io);
  assert.equal(caseStatus(report, 'recovery_without_readiness_polling'), 'FAIL');
  assert.equal(caseStatus(report, 'recovery_database_operation'), 'PASS');
  assertMcpAfterStaleFirstResponse(report, 'healthy', 'recovery_idle');
  assertSilentIdleWindows(io);
});

test('HARNESS MODEL ONLY: readiness success cannot hide an independently failed recovery operation', async () => {
  const report = await runQualification(modelIO('recovery_database_failed'));
  assert.equal(caseStatus(report, 'recovery_without_readiness_polling'), 'PASS');
  assert.equal(caseStatus(report, 'recovery_database_operation'), 'FAIL');
  assert.equal(report.status, 'FAIL', failureSummary(report));
});

test('HARNESS MODEL ONLY: stale-sample challenge uses a fresh fixture despite earlier idle and recovery failures', async () => {
  const io = modelIO('delayed_idle_refresh');
  const report = await runQualification(io);
  assert.equal(caseStatus(report, 'idle_first_request_1'), 'FAIL');
  assert.equal(caseStatus(report, 'recovery_without_readiness_polling'), 'FAIL');
  for (const name of ['stale_success_baseline', 'stale_success_challenge',
    'stale_outage_database_operation', 'stale_outage_liveness', 'stale_fault_restore', 'stale_shutdown']) {
    assert.equal(caseStatus(report, name), 'PASS', `${name} did not execute independently`);
  }
  assert.equal(caseStatus(report, 'stale_challenge_recovery'), 'FAIL');
  assert.equal(caseStatus(report, 'stale_recovery_database_operation'), 'PASS');
  assert.deepEqual(io.sessions.map((session) => session.label),
    ['healthy', 'stale_success', 'outage_shutdown']);
  const suspended = io.events.findIndex((entry) => entry.kind === 'suspend');
  assert.ok(suspended >= 0, 'The fresh stale challenge never suspended its process');
  assert.equal(io.events[suspended].label, 'stale_success');
  assert.ok(io.events.slice(suspended + 1).some((entry) => entry.kind === 'resume'
    && entry.label === 'stale_success'), 'The separate fixture must always be resumed');
  const restoration = report.cases.find((entry) => entry.name === 'stale_fault_restore');
  assert.equal(restoration.details?.project_rows_preserved, true, 'A no-op is not actual restoration');
  assertMcpAfterStaleFirstResponse(report, 'stale_success', 'stale_recovery_idle');
  assertSilentIdleWindows(io);
});

test('HARNESS MODEL ONLY: outage shutdown executes despite failed readiness and liveness', async () => {
  const io = modelIO('outage_sample_stale', {liveness_failed: true});
  const report = await runQualification(io);
  assert.equal(caseStatus(report, 'outage_shutdown_readiness'), 'FAIL');
  assert.equal(caseStatus(report, 'outage_shutdown_liveness'), 'FAIL');
  assert.equal(caseStatus(report, 'shutdown_with_database_unavailable'), 'PASS');
  const stopped = io.events.findIndex((entry) => entry.label === 'outage_shutdown'
    && entry.kind === 'stop' && entry.fault === true);
  assert.ok(stopped >= 0, 'The database fault must still be installed at shutdown');
  assert.ok(io.events.slice(stopped + 1).some((entry) => entry.label === 'outage_shutdown'
    && entry.kind === 'sql' && /ALTER TABLE gh407_saved_projects RENAME TO projects/.test(entry.sql)),
  'Shutdown must occur before the actual inverse rename');
});

test('HARNESS MODEL ONLY: stale soak readiness cannot suppress the independent CPU failure', async () => {
  const report = await runQualification(modelIO('delayed_idle_refresh', {idle_cpu_runaway: true}));
  assert.equal(caseStatus(report, 'idle_soak'), 'FAIL');
  assert.equal(caseStatus(report, 'idle_soak_cpu'), 'FAIL');
});

test('HARNESS MODEL ONLY: an unestablished baseline records blocked checks and no-op restoration as incomplete', async () => {
  const io = modelIO('malformed_health');
  const report = await runQualification(io);
  assert.equal(report.status, 'INCOMPLETE', failureSummary(report));
  for (const name of ['idle_first_request_1', 'idle_database_1', 'genuine_database_unavailability',
    'database_fault_restore', 'recovery_without_readiness_polling', 'recovery_database_operation',
    'stale_success_challenge', 'stale_fault_restore', 'stale_challenge_recovery',
    'stale_recovery_database_operation', 'shutdown_with_database_unavailable',
    'outage_shutdown_schema_restore']) {
    assert.equal(caseStatus(report, name), 'INCOMPLETE', `${name} cannot disappear or pass without its prerequisite`);
  }
  assert.equal(io.events.filter((entry) => entry.kind === 'sql').length, 0,
    'No database fault may be installed without an initially healthy fixture');
  assert.equal(new Set(report.cases.map((entry) => entry.name)).size, report.cases.length,
    'Blocked cases must not duplicate recorded outcomes');
});

test('HARNESS MODEL ONLY: an unattributed MCP error blocks outage qualification without hiding restoration', async () => {
  const report = await runQualification(modelIO('mcp_auth_error'));
  assert.equal(caseStatus(report, 'genuine_database_unavailability'), 'INCOMPLETE');
  for (const name of ['database_outage_liveness', 'concurrent_requests_during_database_outage',
    'failed_probe_cadence', 'failed_probe_cpu']) {
    assert.equal(caseStatus(report, name), 'INCOMPLETE', `${name} needs the attributed database fault`);
  }
  assert.equal(caseStatus(report, 'database_fault_restore'), 'PASS',
    'The attempted real mutation still requires verified restoration');
  assert.equal(report.status, 'INCOMPLETE', failureSummary(report));
});

test('HARNESS MODEL ONLY: a failed concurrent read cannot advance while its launched sibling is pending',
  {timeout: 1000}, async () => {
    const io = modelIO();
    const originalStart = io.start.bind(io);
    const prematureActions = [];
    let pending = false;
    let deferredCount = 0;
    let siblingSettled = Promise.resolve();
    io.start = async (...arguments_) => {
      if (pending) prematureActions.push('start next fixture');
      const session = await originalStart(...arguments_);
      if (session.label !== 'healthy') return session;
      let faultInstalled = false;
      let outageReadinessRequests = 0;
      for (const method of ['sql', 'ensureProject', 'idle', 'resources', 'logs', 'suspend', 'resume', 'stop']) {
        const original = session[method].bind(session);
        session[method] = async (...parameters) => {
          if (pending) prematureActions.push(method);
          const result = await original(...parameters);
          if (method === 'sql' && parameters[0] === 'exec' && result.ok) {
            if (parameters[1] === 'ALTER TABLE projects RENAME TO gh407_saved_projects') faultInstalled = true;
            if (parameters[1] === 'ALTER TABLE gh407_saved_projects RENAME TO projects') faultInstalled = false;
          }
          return result;
        };
      }
      const originalGet = session.get.bind(session);
      session.get = async (route) => {
        const readiness = ['/health', '/health/readiness'].includes(route);
        const ordinal = faultInstalled && readiness ? ++outageReadinessRequests : null;
        if (pending && (ordinal === null || ordinal > DEFAULTS.concurrency + 1)) {
          prematureActions.push('HTTP outside the launched burst');
        }
        const response = await originalGet(route);
        // Ordinal 1 is the independently retained first outage response.
        // The next two requests belong to the same concurrent burst.
        if (ordinal === 2) return {...response, duration_ms: DEFAULTS.requestTimeout * 1000 + 1};
        if (ordinal === 3) {
          deferredCount += 1;
          pending = true;
          siblingSettled = new Promise((resolve) => setTimeout(() => {
            pending = false;
            resolve(response);
          }, 20));
          return await siblingSettled;
        }
        return response;
      };
      return session;
    };
    const report = await runQualification(io);
    await siblingSettled;
    assert.equal(deferredCount, 1, 'The adversary must launch its deferred sibling');
    assert.deepEqual(prematureActions, [],
      'Failure must be retained only after every already launched bounded request has settled');
    assert.equal(caseStatus(report, 'concurrent_requests_during_database_outage'), 'FAIL');
    assert.equal(report.status, 'FAIL', failureSummary(report));
    assert.equal(caseStatus(report, 'database_fault_restore'), 'PASS');
    assert.equal(caseStatus(report, 'healthy_shutdown'), 'PASS');
  });

function statText(tid, {identity = String(tid * 10), name = 'same worker',
  user = 100, system = 20, threads = 1} = {}) {
  const fields = Array(50).fill('0');
  fields[0] = 'S'; fields[1] = '1'; fields[2] = '1000'; fields[3] = '1000';
  fields[11] = String(user); fields[12] = String(system);
  fields[17] = String(threads); fields[19] = identity;
  return `${tid} (${name}) ${fields.join(' ')}\n`;
}

function threadPoint(tid, user, system = 0, identity = String(tid * 10)) {
  return {tid, identity, name: 'same worker', user_ticks: user, system_ticks: system};
}

function threadSample(at, user, system, threads, overrides = {}) {
  const stamp = {read_started_monotonic_seconds: at, read_finished_monotonic_seconds: at + 0.01};
  const process = {tid: 1000, identity: '9000', user_ticks: user, system_ticks: system,
    read_started_monotonic_seconds: at, read_finished_monotonic_seconds: at + 0.001};
  return {pid: 1000, identity: '9000', available: true, ticks_per_second: 100, ...stamp,
    process_before: {...process}, process_after: {...process,
      read_started_monotonic_seconds: at + 0.009, read_finished_monotonic_seconds: at + 0.01},
    threads: threads.map(row => ({...row, read_started_monotonic_seconds: at + 0.002,
      read_finished_monotonic_seconds: at + 0.004})),
    enumerated_threads: threads.length, unreadable_threads: 0, omitted_threads: 0,
    omitted_threads_lower_bound: 0, enumeration_complete: true, errors: [], truncated: false,
    ...overrides};
}

const closeTo = (actual, expected) => assert.ok(Math.abs(actual - expected) < 1e-9,
  `Expected ${actual} to be approximately ${expected}`);

test('HARNESS DIAGNOSTIC ONLY: stat parsing preserves identities and rejects unsafe counters', () => {
  const parsed = parseThreadCpuStat(statText(41, {name: 'worker ) (busy', identity: '987654321',
    user: 1234, system: 567, threads: 8}), 41);
  assert.deepEqual(parsed, {tid: 41, identity: '987654321', name: 'worker ) (busy',
    user_ticks: 1234, system_ticks: 567, threads: 8});
  for (const raw of [statText(42), statText(41, {identity: 'not-a-start-time'}),
    statText(41, {user: -1}), statText(41, {system: Number.MAX_SAFE_INTEGER + 1}),
    statText(41, {name: 'x'.repeat(THREAD_CPU_LIMITS.statBytes)}), '41 (truncated) S']) {
    assert.throws(() => parseThreadCpuStat(raw, 41), undefined,
      'Malformed or mismatched stat data must not produce attributable CPU counters');
  }
});

test('HARNESS DIAGNOSTIC ONLY: stable thread identities rank independently of names and retain residual/skew', () => {
  const before = threadSample(10, 100, 20, [threadPoint(11, 10, 5), threadPoint(12, 20, 10)]);
  const after = threadSample(12, 160, 40, [threadPoint(12, 30, 20), threadPoint(11, 50, 15)]);
  const delta = threadCpuDelta(before, after);
  assert.equal(delta.available, true);
  assert.equal(delta.exhaustive, false, 'A finite census cannot prove complete lifetime attribution');
  assert.deepEqual(delta.threads.map(row => [row.tid, row.identity]), [[11, '110'], [12, '120']]);
  assert.equal(delta.threads[0].name, delta.threads[1].name,
    'Identical or truncated OS names cannot merge independent thread counters');
  closeTo(delta.threads[0].user_cpu_seconds, 0.4);
  closeTo(delta.threads[0].system_cpu_seconds, 0.1);
  closeTo(delta.threads[0].cpu_cores, 0.25);
  closeTo(delta.aggregate.total_cpu_seconds, 0.8);
  closeTo(delta.aggregate.matched_thread_cpu_seconds, 0.7);
  closeTo(delta.aggregate.residual_cpu_seconds, 0.1);
  closeTo(delta.sampling.interval_lower_seconds, 1.99);
  closeTo(delta.sampling.interval_upper_seconds, 2.01);
  assert.ok(delta.sampling.maximum_thread_midpoint_skew_seconds > 0);

  const skewed = threadSample(12, 100, 20, after.threads);
  const negative = threadCpuDelta(before, skewed);
  assert.equal(negative.available, true);
  closeTo(negative.aggregate.residual_cpu_seconds, -0.7);
  assert.equal(negative.exhaustive, false,
    'Sampling skew must remain visible, not be clamped into a complete accounting claim');
});

test('HARNESS DIAGNOSTIC ONLY: reuse, disappearing threads and regressing counters never acquire false CPU deltas', () => {
  const before = threadSample(10, 100, 20,
    [threadPoint(11, 10), threadPoint(12, 10), threadPoint(13, 10), threadPoint(14, 10)]);
  const after = threadSample(12, 150, 20,
    [threadPoint(11, 30), threadPoint(12, 1000, 0, '121'), threadPoint(13, 5), threadPoint(15, 1000)],
    {truncated: true, enumeration_complete: false, omitted_threads: null,
      omitted_threads_lower_bound: 1, unreadable_threads: 2});
  const delta = threadCpuDelta(before, after);
  assert.equal(delta.available, true);
  assert.deepEqual(delta.threads.map(row => row.tid), [11]);
  closeTo(delta.aggregate.matched_thread_cpu_seconds, 0.2);
  assert.ok(delta.accounting.reused_tids.some(row => row.tid === 12));
  assert.ok(delta.accounting.invalid_threads.some(row => row.tid === 13));
  assert.ok(delta.accounting.new_threads.some(row => row.tid === 15));
  assert.ok(delta.accounting.exited_threads.some(row => row.tid === 14));
  assert.equal(delta.accounting.omitted_after, null, 'An incomplete census has no exact omitted count');
  assert.equal(delta.accounting.omitted_lower_bound_after, 1);
  assert.equal(delta.accounting.unreadable_after, 2);
  const ambiguous = threadCpuDelta(before, threadSample(12, 150, 20,
    [threadPoint(11, 30), threadPoint(11, 40), threadPoint(11, 50)]));
  assert.equal(ambiguous.threads.length, 0, 'A third duplicate cannot resurrect a rejected TID');
  assert.ok(ambiguous.accounting.invalid_threads.some(row => row.tid === 11));
  for (const broken of [undefined, {...after, available: false}, {...after, pid: 1001},
    {...after, identity: '9001'}, {...after, ticks_per_second: 200},
    threadSample(10, 150, 20, []), threadSample(12, 99, 20, [])]) {
    const result = threadCpuDelta(before, broken);
    assert.equal(result.available, false, 'Process reuse, invalid intervals and counters must refuse attribution');
    assert.deepEqual(result.threads, []);
    assert.equal(result.aggregate, null);
  }
});

test('HARNESS DIAGNOSTIC ONLY: census has fixed read budgets and retains unreadable/missing identity evidence', () => {
  assert.deepEqual(THREAD_CPU_LIMITS, {threads: 256, samples: 1024, bytes: 16 * 1024 * 1024,
    statBytes: 4096, sampleSeconds: 0.025, intervalSeconds: 1});
  assert.equal(Object.isFrozen(THREAD_CPU_LIMITS), true);
  const options = {pid: 1000, identity: '9000', ticksPerSecond: 100};
  let reads = 0, yielded = 0;
  const bounded = sampleThreadCpu(options, {now: () => 10,
    readStat(tid) { reads++; return statText(tid ?? 1000, {identity: tid === null ? '9000' : String(tid)}); },
    threadIds: function* () { for (let tid = 1; tid < 1_000_000; tid++) { yielded++; yield tid; } }});
  assert.equal(bounded.threads.length, THREAD_CPU_LIMITS.threads);
  assert.ok(reads <= THREAD_CPU_LIMITS.threads + 2, 'The reader cannot chase an unbounded task directory');
  assert.ok(yielded <= THREAD_CPU_LIMITS.threads + 1);
  assert.equal(bounded.truncated, true);
  assert.equal(bounded.truncation_reason, 'thread_limit');
  assert.equal(bounded.omitted_threads, null);
  assert.equal(bounded.omitted_threads_lower_bound, 1);

  let clock = 20, timedReads = 0;
  const timed = sampleThreadCpu(options, {now: () => clock,
    readStat(tid) { timedReads++; clock += 0.01; return statText(tid ?? 1000, {identity: tid === null ? '9000' : String(tid)}); },
    threadIds: function* () { for (let tid = 1; tid < 1000; tid++) yield tid; }});
  assert.equal(timed.truncation_reason, 'read_time_budget');
  assert.ok(timedReads <= 4, 'Check the elapsed budget between reads, including the initial process read');
  assert.ok(timed.read_budget_overrun_seconds > 0,
    'A synchronous read crossing the budget must expose its actual overrun');

  const missing = sampleThreadCpu(options, {now: () => 30, threadIds: () => [1, 2],
    readStat(tid) { if (tid === 1) throw new Error('thread exited during sample');
      return statText(tid ?? 1000, {identity: tid === null ? '9000' : String(tid)}); }});
  assert.equal(missing.available, true);
  assert.equal(missing.unreadable_threads, 1);
  assert.deepEqual(missing.threads.map(row => row.tid), [2]);
  assert.equal(missing.enumeration_complete, true);
  assert.ok(missing.errors.some(error => error.where === 'thread:1'));
  const vanished = sampleThreadCpu(options, {now: () => 40, threadIds: () => [],
    readStat() { throw new Error('owned process absent'); }});
  assert.equal(vanished.available, false);
  assert.equal(vanished.threads.length, 0);
  assert.ok(vanished.errors.length > 0);
  const failures = sampleThreadCpu(options, {now: () => 50,
    threadIds: () => Array.from({length: 20}, (_, index) => index + 1),
    readStat(tid) { if (tid !== null) throw new Error('unreadable '.repeat(1000));
      return statText(1000, {identity: '9000'}); }});
  assert.equal(failures.error_count, 20, 'All read failures must remain counted');
  assert.equal(failures.errors.length, 8, 'Repeated diagnostics cannot create unbounded error detail');
  assert.ok(failures.errors.every(error => error.error.length <= 512));
});

test('HARNESS DIAGNOSTIC ONLY: retained samples stop at fixed rate/count/byte/error limits', (t) => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'gh407-thread-caps-'));
  t.diagnostic(`Retained HARNESS-ONLY cap controls: ${directory}`);
  const options = {pid: 1000, identity: '9000', ticksPerSecond: 100};
  let now = 10, reads = 0;
  const source = {now: () => now, threadIds: () => [11],
    readStat(tid) { reads++; return statText(tid ?? 1000, {identity: tid === null ? '9000' : String(tid)}); }};
  const filename = path.join(directory, 'count.jsonl');
  const sampler = createThreadCpuSampler({...options, filename}, source);
  const first = sampler.sample();
  assert.equal(first.reference.persisted, true);
  const initialReads = reads;
  for (let i = 0; i < 100; i++) assert.equal(sampler.sample().reference.cached, true);
  assert.equal(reads, initialReads, 'Repeated resource calls cannot bypass the sampling interval');
  const fresh = sampler.sample({fresh: true});
  assert.equal(fresh.reference.cached, false);
  assert.equal(fresh.reference.sequence, 2, 'An explicit CPU boundary must retain a new sample');
  assert.ok(reads > initialReads);
  let last;
  for (let i = 1; i < THREAD_CPU_LIMITS.samples + 4; i++) { now += 2; last = sampler.sample(); }
  assert.equal(last.reference.reason, 'sample_limit');
  assert.equal(last.sample.available, false);
  const lines = fs.readFileSync(filename, 'utf8').trimEnd().split('\n').map(JSON.parse);
  assert.equal(lines.length, THREAD_CPU_LIMITS.samples, 'The terminal receipt consumes a reserved sample slot');
  assert.equal(lines.at(-1).reason, 'sample_limit');
  const finalReads = reads;
  now += 100;
  assert.equal(sampler.sample({fresh: true}).reference.cached, true);
  assert.equal(reads, finalReads, 'A terminal cap must stop all further /proc reads');

  const bytesFile = path.join(directory, 'bytes.jsonl');
  const bytesSampler = createThreadCpuSampler({...options, filename: bytesFile}, {...source,
    threadIds: () => Array.from({length: THREAD_CPU_LIMITS.threads}, (_, i) => i + 1)});
  for (let i = 0; i < THREAD_CPU_LIMITS.samples + 2; i++) {
    now += 2; last = bytesSampler.sample();
    if (last.reference.reason) break;
  }
  assert.equal(last.reference.reason, 'byte_limit');
  assert.ok(fs.statSync(bytesFile).size <= THREAD_CPU_LIMITS.bytes,
    'The refusal receipt itself must fit within the fixed file budget');
  const bytesReads = reads;
  now += 100; bytesSampler.sample();
  assert.equal(reads, bytesReads);

  const failed = createThreadCpuSampler({...options, filename: path.join(directory, 'absent-parent', 'samples.jsonl')}, source);
  const refusal = failed.sample();
  assert.equal(refusal.reference.persisted, false);
  assert.equal(refusal.reference.reason, 'diagnostic_write_failed');
  assert.equal(refusal.sample.available, false);
  const failedReads = reads;
  now += 100; failed.sample();
  assert.equal(reads, failedReads, 'Write failure cannot start a recurring diagnostic retry loop');
});

test('HARNESS MODEL ONLY: absent, invalid or low thread diagnostics cannot rescue aggregate CPU failures', async () => {
  assert.equal(DEFAULTS.maxIdleCpuCores, 0.25, 'Diagnostic work cannot raise the existing aggregate CPU threshold');
  for (const mode of ['absent', 'process_reused', 'zero_thread_cpu']) {
    const io = modelIO('idle_cpu_runaway');
    const originalStart = io.start.bind(io);
    io.start = async (...arguments_) => {
      const session = await originalStart(...arguments_);
      const originalResources = session.resources.bind(session);
      session.resources = async () => {
        const point = await originalResources();
        if (mode !== 'absent') {
          point.thread_cpu_sample = threadSample(io.now(), 0, 0, [threadPoint(11, 0)]);
          if (mode === 'process_reused') point.thread_cpu_sample.identity = String(io.now());
        }
        return point;
      };
      return session;
    };
    const report = await runQualification(io);
    assert.equal(report.status, 'FAIL', `${mode}: ${failureSummary(report)}`);
    assert.equal(caseStatus(report, 'idle_soak_cpu'), 'FAIL');
    assert.equal(caseStatus(report, 'failed_probe_cpu'), 'FAIL');
    const windows = report.observations.filter(row => row.kind === 'thread_cpu_window');
    assert.equal(windows.length, 2, 'CPU evidence must be recorded before the aggregate threshold assertion');
    if (mode === 'zero_thread_cpu') {
      assert.ok(windows.every(row => row.diagnosis.available && row.diagnosis.aggregate.total_cpu_seconds === 0));
    } else assert.ok(windows.every(row => row.diagnosis.available === false));
  }
});

function postSeedModel({mode = 'healthy', initialDelay = 0, settleSeconds = 6,
  failFinalBurst = false} = {}) {
  const io = modelIO(mode);
  io.seedTransitions = [];
  const originalStart = io.start.bind(io);
  io.start = async (...arguments_) => {
    const session = await originalStart(...arguments_);
    const trace = {session: session.label, started: io.now(), seeded: null,
      final_burst_failure: null, requests: []};
    io.seedTransitions.push(trace);
    const originalControl = session.ensureProject.bind(session);
    session.ensureProject = async (tag) => {
      const result = await originalControl(tag);
      if (tag === 'baseline' && result.ok) trace.seeded = io.now();
      return result;
    };
    const originalGet = session.get.bind(session);
    session.get = async (route) => {
      const result = await originalGet(route);
      if (!['/health', '/health/readiness'].includes(route)) return result;
      let reason = null;
      if (io.now() - trace.started < initialDelay) reason = 'database_initializing';
      else if (trace.seeded !== null && io.now() - trace.seeded < settleSeconds) reason = 'lexical_initializing';
      else if (failFinalBurst && trace.seeded !== null && route === '/health/readiness'
          && trace.final_burst_failure === null) {
        reason = 'test_final_burst_failure';
        trace.final_burst_failure = io.now();
      }
      const response = reason ? {...result, status: 503, body: {...result.body,
        status: 'unavailable', reason, lexical_state: 'initializing',
        capabilities: {mail_read: reason === 'lexical_initializing',
          mail_write: reason === 'lexical_initializing', lexical_search: false}}} : result;
      trace.requests.push({at: io.now(), route, status: response.status,
        reason: response.body?.reason ?? null, after_seed: trace.seeded !== null});
      return response;
    };
    return session;
  };
  return io;
}

function retainSeedControl(t, label, report, io) {
  if (!process.env.GH407_TEST_EVIDENCE_ROOT) return;
  const directory = fs.mkdtempSync(path.join(path.resolve(process.env.GH407_TEST_EVIDENCE_ROOT), 'baseline-'));
  const filename = path.join(directory, label + '.json');
  fs.writeFileSync(filename, JSON.stringify({evidence_class: 'harness_model',
    native_agent_mail_executions: 0, label, report, seed_transitions: io.seedTransitions,
    events: io.events}, null, 2) + '\n', {flag: 'wx', mode: 0o600});
  t.diagnostic(`Retained HARNESS-ONLY baseline control: ${filename}`);
}

test('HARNESS MODEL ONLY: post-seed transient readiness settles before measuring and cannot retry an idle failure', async (t) => {
  for (const mode of ['healthy', 'delayed_idle_refresh']) {
    const io = postSeedModel({mode});
    const report = await runQualification(io);
    retainSeedControl(t, 'transient_' + mode, report, io);
    for (const name of ['baseline', 'stale_success_baseline', 'outage_shutdown_baseline']) {
      assert.equal(caseStatus(report, name), 'PASS', `${mode}: ${failureSummary(report)}`);
    }
    assert.equal(report.status, mode === 'healthy' ? 'PASS' : 'FAIL', failureSummary(report));
    for (const trace of io.seedTransitions) {
      const pending = trace.requests.filter(row => row.after_seed && row.reason === 'lexical_initializing');
      assert.ok(pending.length > 0, 'The control must actually observe the post-seed transient');
      assert.ok(pending.every(row => row.at >= trace.seeded + DEFAULTS.warmupSeconds),
        'The complete predeclared warmup precedes readiness preparation');
      const readyAfterSeed = trace.requests.find(row => row.after_seed && row.status === 200);
      assert.ok(readyAfterSeed.at >= trace.seeded + 6, 'An initializing response cannot establish readiness');
    }
    for (let cycle = 1; cycle <= DEFAULTS.idleCycles; cycle++) {
      assert.equal(caseStatus(report, `idle_first_request_${cycle}`), mode === 'healthy' ? 'PASS' : 'FAIL');
      assert.equal(caseStatus(report, `idle_database_${cycle}`), 'PASS');
      if (mode === 'delayed_idle_refresh') assertMcpAfterStaleFirstResponse(report, 'healthy', `idle_${cycle - 1}`);
    }
    assertSilentIdleWindows(io);
  }
});

test('HARNESS MODEL ONLY: persistent post-seed unreadiness cannot reset the original startup deadline', async (t) => {
  assert.equal(DEFAULTS.startupTimeout, 90);
  const io = postSeedModel({initialDelay: 80, settleSeconds: Infinity});
  const report = await runQualification(io);
  retainSeedControl(t, 'persistent_post_seed', report, io);
  assert.equal(report.status, 'INCOMPLETE', failureSummary(report));
  for (const name of ['baseline', 'stale_success_baseline', 'outage_shutdown_baseline']) {
    assert.equal(caseStatus(report, name), 'INCOMPLETE', failureSummary(report));
    assert.equal(report.cases.find(row => row.name === name).duration_seconds, 90,
      'Initial startup, fixture creation, warmup and post-seed readiness share one 90-second budget');
  }
  for (const trace of io.seedTransitions) {
    assert.equal(trace.seeded - trace.started, 80);
    const pending = trace.requests.filter(row => row.after_seed);
    assert.ok(pending.length > 0);
    assert.ok(pending.every(row => row.status === 503 && row.reason === 'lexical_initializing'));
    assert.ok(pending.every(row => row.at < trace.started + 90), 'No new request starts after the original deadline');
  }
  assert.equal(caseStatus(report, 'idle_first_request_1'), 'INCOMPLETE');
  assert.ok(!report.observations.some(row => row.kind === 'idle_start'),
    'A missing post-seed baseline cannot be mislabeled as a measured idle trial');
});

test('HARNESS MODEL ONLY: a genuine final baseline burst failure remains strict and is never retried', async (t) => {
  const io = postSeedModel({settleSeconds: 0, failFinalBurst: true});
  const report = await runQualification(io);
  retainSeedControl(t, 'final_baseline_burst_failure', report, io);
  assert.equal(report.status, 'FAIL', failureSummary(report));
  for (const name of ['baseline', 'stale_success_baseline', 'outage_shutdown_baseline']) {
    assert.equal(caseStatus(report, name), 'FAIL', failureSummary(report));
  }
  for (const trace of io.seedTransitions) {
    const reads = report.observations.filter(row => row.kind === 'http' && row.session === trace.session);
    assert.equal(reads.length, DEFAULTS.concurrency, 'One complete final burst is retained, without a retry burst');
    assert.equal(reads.filter(row => row.response.status === 503).length, 1);
    assert.equal(reads.filter(row => row.response.status === 200).length, DEFAULTS.concurrency - 1);
    assert.equal(trace.requests.filter(row => row.reason === 'test_final_burst_failure').length, 1);
  }
  assert.equal(caseStatus(report, 'idle_first_request_1'), 'INCOMPLETE');
});
