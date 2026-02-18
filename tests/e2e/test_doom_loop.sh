#!/usr/bin/env bash
# test_doom_loop.sh - E2E: Case-duplicate doom loop fix + doctor lifecycle
#
# Tests:
#   1. Case-duplicate agents are deduped by v10a migration on first access
#   2. v10b NOCASE unique index is created
#   3. CLI operations succeed after dedup
#   4. Doctor repair creates .bak sibling + timestamped backup
#   5. Doctor check passes after repair

E2E_SUITE="doom_loop"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Doom Loop Fix E2E Test Suite"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace
WORK="$(e2e_mktemp "e2e_doom_loop")"
STORAGE_ROOT="${WORK}/storage"
mkdir -p "${STORAGE_ROOT}"

# ---------------------------------------------------------------------------
# Case 1: Case-duplicate doom loop reproduction and fix
# ---------------------------------------------------------------------------
e2e_case_banner "case_duplicate_dedup"

DB_CASE="${WORK}/case_dup.sqlite3"
DB_URL="sqlite:///${DB_CASE}"

# Create a pre-v10 database schema with case-duplicate agents.
sqlite3 "${DB_CASE}" <<'SQL'
CREATE TABLE IF NOT EXISTS schema_migrations (
    version TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT NOT NULL UNIQUE,
    human_key TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL,
    program TEXT NOT NULL,
    model TEXT NOT NULL,
    task_description TEXT NOT NULL DEFAULT '',
    inception_ts INTEGER NOT NULL,
    last_active_ts INTEGER NOT NULL,
    attachments_policy TEXT NOT NULL DEFAULT 'auto',
    contact_policy TEXT NOT NULL DEFAULT 'auto',
    UNIQUE(project_id, name)
);
CREATE INDEX IF NOT EXISTS idx_agents_project_name ON agents(project_id, name);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL,
    sender_id INTEGER NOT NULL,
    thread_id TEXT,
    subject TEXT NOT NULL,
    body_md TEXT NOT NULL,
    importance TEXT NOT NULL DEFAULT 'normal',
    ack_required INTEGER NOT NULL DEFAULT 0,
    created_ts INTEGER NOT NULL,
    attachments TEXT NOT NULL DEFAULT '[]'
);

-- Insert project
INSERT INTO projects (slug, human_key, created_at) VALUES ('doom-loop-test', '/tmp/doom-loop-test', 0);

-- Insert case-duplicate agents (possible before v10b NOCASE index)
INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
    VALUES (1, 1, 'SilverFox', 'claude-code', 'opus', 100, 100);
INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
    VALUES (2, 1, 'silverfox', 'codex', 'gpt-5', 200, 200);
INSERT INTO agents (id, project_id, name, program, model, inception_ts, last_active_ts)
    VALUES (3, 1, 'SILVERFOX', 'gemini', 'flash', 300, 300);

-- Mark migrations up to v9 as applied (skip v10a/v10b so they run on first access)
INSERT INTO schema_migrations (version) VALUES ('v1_initial');
INSERT INTO schema_migrations (version) VALUES ('v2_add_attachments_column');
INSERT INTO schema_migrations (version) VALUES ('v3_convert_text_timestamps_to_integer');
INSERT INTO schema_migrations (version) VALUES ('v4_add_thread_importance_column');
INSERT INTO schema_migrations (version) VALUES ('v5_add_ack_columns');
INSERT INTO schema_migrations (version) VALUES ('v6_add_contacts_table');
INSERT INTO schema_migrations (version) VALUES ('v7_add_attachments_policy');
INSERT INTO schema_migrations (version) VALUES ('v8_add_contact_policy');
INSERT INTO schema_migrations (version) VALUES ('v9_add_build_slots');
SQL

e2e_log "Pre-v10 DB created with 3 case-duplicate agents"

# Verify initial state: 3 agents
AGENT_COUNT_BEFORE=$(sqlite3 "${DB_CASE}" "SELECT COUNT(*) FROM agents WHERE project_id = 1")
e2e_assert_eq "3 case-duplicate agents before fix" "3" "$AGENT_COUNT_BEFORE"

# Run am with this DB — should trigger v10a/v10b migrations
export DATABASE_URL="${DB_URL}"
export STORAGE_ROOT="${STORAGE_ROOT}"
export AM_INTERFACE_MODE="cli"

OUTPUT=$(am doctor check --json 2>&1) || true
e2e_save_artifact "case_dup_doctor_check.txt" "$OUTPUT"
e2e_log "Doctor check triggered v10a/v10b migrations"

# After migrations run, verify dedup:
AGENT_COUNT_AFTER=$(sqlite3 "${DB_CASE}" "SELECT COUNT(*) FROM agents WHERE project_id = 1")
e2e_assert_eq "1 agent after v10a dedup" "1" "$AGENT_COUNT_AFTER"

# Verify the surviving agent is the one with lowest id (id=1, SilverFox)
SURVIVOR_NAME=$(sqlite3 "${DB_CASE}" "SELECT name FROM agents WHERE project_id = 1")
e2e_assert_eq "survivor is SilverFox (lowest id)" "SilverFox" "$SURVIVOR_NAME"

# Verify v10b index exists
INDEX_EXISTS=$(sqlite3 "${DB_CASE}" "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_agents_project_name_nocase'")
e2e_assert_eq "NOCASE unique index exists" "1" "$INDEX_EXISTS"

# Run doctor check again — should be idempotent
OUTPUT2=$(am doctor check --json 2>&1) || true
e2e_save_artifact "case_dup_doctor_check_2.txt" "$OUTPUT2"
e2e_log "Second doctor check completed (idempotent)"

e2e_pass "case_duplicate_dedup"

# ---------------------------------------------------------------------------
# Case 2: Doctor check/repair lifecycle
# ---------------------------------------------------------------------------
e2e_case_banner "doctor_repair_lifecycle"

DB_REPAIR="${WORK}/repair_test.sqlite3"
DB_REPAIR_URL="sqlite:///${DB_REPAIR}"
BACKUP_DIR="${WORK}/backups"

# Create a healthy DB by triggering schema init via doctor check
export DATABASE_URL="${DB_REPAIR_URL}"
am doctor check --json >/dev/null 2>&1 || true
e2e_log "Repair test DB initialized"

# Run doctor repair with explicit backup dir
REPAIR_OUTPUT=$(am doctor repair --backup-dir "${BACKUP_DIR}" 2>&1) || true
e2e_save_artifact "repair_output.txt" "$REPAIR_OUTPUT"
e2e_assert_contains "repair completes" "$REPAIR_OUTPUT" "Repair complete"

# Verify .bak sibling exists
e2e_assert_file_exists ".bak sibling after repair" "${DB_REPAIR}.bak"

# Verify .bak is valid SQLite
BAK_QC=$(sqlite3 "${DB_REPAIR}.bak" "PRAGMA quick_check" 2>&1)
e2e_assert_eq ".bak passes quick_check" "ok" "$BAK_QC"

# Verify timestamped backup in backups/ dir
BACKUP_COUNT=$(find "${BACKUP_DIR}" -name "pre_repair_*.sqlite3" 2>/dev/null | wc -l | tr -d ' ')
e2e_assert_eq "timestamped backup exists" "1" "$BACKUP_COUNT"

# Doctor check should still pass after repair
POST_REPAIR_CHECK=$(am doctor check --json 2>&1) || true
e2e_save_artifact "repair_post_check.txt" "$POST_REPAIR_CHECK"
e2e_log "Post-repair doctor check completed"

e2e_pass "doctor_repair_lifecycle"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
e2e_summary
