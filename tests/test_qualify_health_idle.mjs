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

export function modelIO(mode = 'healthy') {
  let clock = 0;
  const events = [];
  const sessions = [];
  const io = {
    kind: 'harness_model',
    identity: { label: `GH407 HARNESS MODEL: ${mode}`, native: false },
    now: () => clock,
    events,
    sessions,
    async start(label, cfg) {
      const startTime = clock;
      let fault = false;
      let suspended = false;
      let stopped = false;
      let nextProbe = clock + 2;
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
        if (!(mode === 'polling_only_recovery' && everFailed && !fault && !fromRequest)) {
          sample = { ok: !fault, observed: at };
        }
      }

      function pump(until) {
        if (suspended || stopped) return;
        while (nextProbe <= until) {
          if (mode !== 'idle_refresh_missing') probe(nextProbe);
          nextProbe += mode === 'unbounded_probe_rate' ? 0.05 : 2;
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
            return { status: fault && mode === 'liveness_failed' ? 503 : 200,
              body: { status: 'alive' }, duration_ms: 1 };
          }
          if (mode === 'malformed_health') {
            return { status: 200, body: 'not a readiness object', duration_ms: 1 };
          }
          const n = (requestsAtTime.get(clock) || 0) + 1;
          requestsAtTime.set(clock, n);
          const stale = clock - sample.observed > 6;
          let ready = sample.ok && !stale;
          if (fault && mode === 'false_ready') ready = true;
          if (fault && resumedWithFault && mode === 'stale_false_ready') ready = true;
          if (!fault && mode === 'concurrent_one_failure' && n === cfg.concurrency) ready = false;
          const body = {
            status: ready ? 'ready' : 'unavailable',
            capabilities: { mail_read: ready, mail_write: ready, lexical_search: false },
            lexical_state: 'disabled', lexical_fresh: false, durability_state: 'not_probed',
          };
          if (!ready) body.reason = stale ? 'database_probe_stale' : 'database_probe_failed';
          if (fault && mode === 'false_capabilities') body.capabilities.mail_read = true;
          if (fault && mode === 'missing_capabilities') delete body.capabilities;
          const response = { status: ready ? 200 : 503, body, duration_ms: 1 };
          // Preserve the first response. A later refresh must never launder it.
          if (mode === 'idle_refresh_missing' || mode === 'polling_only_recovery') {
            probe(clock, true);
          }
          return response;
        },
        async ensureProject(tag) {
          pump(clock);
          event('mcp', { tag });
          if (fault) {
            if (mode === 'mcp_fault_ineffective') return { ok: true, detail: 'cached response' };
            if (mode === 'mcp_auth_error') return { ok: false, error_kind: 'auth', detail: '401' };
            return { ok: false, error_kind: 'database', detail: 'no such table: projects' };
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
              if (mode === 'fault_refused') return { ok: false, error_kind: 'other', detail: 'refused' };
              if (mode !== 'ineffective_fault') fault = true;
              return { ok: true };
            }
            if (normalized.includes('alter table gh407_saved_projects rename to projects')) {
              if (mode === 'restore_failed') return { ok: false, error_kind: 'database', detail: 'restore failed' };
              fault = false;
              if (mode === 'seed_lost_on_restore') rows.splice(0, rows.length);
              return { ok: true };
            }
            throw new Error(`Model does not understand mutation: ${sql}`);
          }
          if (normalized.includes('sqlite_master')) {
            return { ok: true, rows: [{ name: fault ? 'gh407_saved_projects' : 'projects' }] };
          }
          const namedSaved = /from gh407_saved_projects\b/.test(normalized);
          if ((namedSaved && !fault) || (!namedSaved && fault)) {
            return { ok: false, error_kind: mode === 'query_wrong_error' ? 'auth' : 'database',
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
          if (mode === 'resources_unavailable') return { available: false, detail: 'unsupported' };
          const elapsed = clock - startTime;
          const growth = mode === 'resource_growth' ? Math.floor(elapsed / 2) * 8 : 0;
          return { available: true, threads: 5 + growth, fds: 12 + growth,
            rss_bytes: 16 * 1024 * 1024 + growth * 1024 * 1024,
            cpu_seconds: elapsed * (mode === 'idle_cpu_runaway' ? 2 : 0.001),
            identity: mode === 'identity_changed' && elapsed >= 8 ? 'changed' : 'same',
            children: mode === 'child_leaked' ? 1 : 0 };
        },
        async logs() {
          pump(clock);
          event('logs');
          return { complete: mode !== 'logs_unavailable', probe_failures: failures };
        },
        async suspend() {
          pump(clock);
          event('suspend');
          suspended = true;
          if (mode === 'partial_suspend_error') {
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
          const forced = mode === 'forced_shutdown';
          return { exited: true, exit_code: forced ? 137 : 0, forced,
            remaining_pids: mode === 'child_leaked' ? [99999] : [],
            port_closed: mode !== 'port_left_open', quiet: mode !== 'post_shutdown_mutation',
            identity_unchanged: mode !== 'identity_changed' };
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
  ['false_ready', 'FAIL'],
  ['false_capabilities', 'FAIL'],
  ['missing_capabilities', 'FAIL'],
  ['polling_only_recovery', 'FAIL'],
  ['stale_false_ready', 'FAIL'],
  ['concurrent_one_failure', 'FAIL'],
  ['liveness_failed', 'FAIL'],
  ['resource_growth', 'FAIL'],
  ['idle_cpu_runaway', 'FAIL'],
  ['unbounded_probe_rate', 'FAIL'],
  ['forced_shutdown', 'FAIL'],
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
    if (mode === 'healthy') {
      assert.equal(io.sessions.length, 2, 'healthy and active-fault shutdown need separate lifetimes');
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
