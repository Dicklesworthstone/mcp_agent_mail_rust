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
import { runQualification, DEFAULTS } from '../scripts/qualify_health_idle.mjs';

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
