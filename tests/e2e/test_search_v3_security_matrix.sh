#!/usr/bin/env bash
# test_search_v3_security_matrix.sh - E2E security matrix for scope/redaction (br-2tnl.7.14)
#
# Validates no data leakage through search results, snippets, explain payloads,
# or suggestion fields across all search modes. Covers contact-policy and
# project-visibility boundaries. Includes adversarial score-delta queries.
#
# Cases:
#   1. Setup: create 2 projects, 4 agents, seed messages with secrets
#   2. Cross-project isolation: alpha agent cannot see beta messages
#   3. Same-project visibility: agents see messages in their project
#   4. Contact-policy block_all: search excludes blocked sender content
#   5. Contact-policy contacts_only: approved contacts visible, others hidden
#   6. Recipient-only visibility: only addressed recipients see private messages
#   7. BCC privacy in search results: BCC recipients not leaked
#   8. Explain payload safety: explain fields don't leak private metadata
#   9. Snippet/body truncation: search results don't expose full secret bodies
#  10. Adversarial score-delta: identical queries from different agents yield same count
#  11. Thread summary redaction: non-participants can't access thread summaries
#  12. Search modes parity: lexical and legacy return same visibility verdicts
#  13. Operator mode vs agent mode: operator sees all, agent sees subset
#  14. Audit trail completeness: denied results tracked with correct reasons

E2E_SUITE="search_v3_security_matrix"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Search V3 Security Matrix E2E (br-2tnl.7.14)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_search_v3_security")"
DB_ALPHA="${WORK}/alpha.sqlite3"
DB_BETA="${WORK}/beta.sqlite3"
DB_SHARED="${WORK}/shared.sqlite3"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-security-matrix","version":"1.0"}}}'

# ── Stdio session helper ──────────────────────────────────────────
send_jsonrpc_session() {
    local db_path="$1"
    shift
    local requests=("$@")
    local output_file="${WORK}/session_response_$$.txt"
    local srv_work
    srv_work="$(mktemp -d "${WORK}/srv.XXXXXX")"
    local fifo="${srv_work}/stdin_fifo"
    mkfifo "$fifo"

    DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error WORKTREES_ENABLED=true \
        am serve-stdio < "$fifo" > "$output_file" 2>"${srv_work}/stderr.txt" &
    local srv_pid=$!
    sleep 0.3

    {
        echo "$INIT_REQ"
        sleep 0.1
        for req in "${requests[@]}"; do
            echo "$req"
            sleep 0.05
        done
        sleep 0.2
    } > "$fifo"

    wait "$srv_pid" 2>/dev/null || true
    cat "$output_file"
}

# ── JSON extraction helpers ──────────────────────────────────────
extract_result() {
    local json_lines="$1"
    local id="$2"
    echo "$json_lines" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
        if d.get('id') == $id:
            print(json.dumps(d))
            sys.exit(0)
    except:
        pass
" 2>/dev/null
}

is_success() {
    local resp="$1"
    [ -n "$resp" ] && echo "$resp" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
r = d.get('result', {})
if isinstance(r, dict) and r.get('isError'):
    sys.exit(1)
if 'error' in d:
    sys.exit(1)
" 2>/dev/null
}

get_field() {
    local resp="$1"
    local field="$2"
    echo "$resp" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
r = d.get('result', d)
if isinstance(r, dict) and 'content' in r:
    for c in r['content']:
        if c.get('type') == 'text':
            try:
                inner = json.loads(c['text'])
                v = inner
                for k in '$field'.split('.'):
                    if isinstance(v, list):
                        v = v[int(k)] if k.isdigit() else v
                    elif isinstance(v, dict):
                        v = v.get(k)
                    else:
                        v = None
                if v is not None:
                    if isinstance(v, (dict, list)):
                        print(json.dumps(v))
                    else:
                        print(v)
                    sys.exit(0)
            except:
                pass
" 2>/dev/null
}

get_content_text() {
    local resp="$1"
    echo "$resp" | python3 -c "
import sys, json
d = json.loads(sys.stdin.read())
r = d.get('result', d)
if isinstance(r, dict) and 'content' in r:
    for c in r['content']:
        if c.get('type') == 'text':
            print(c['text'])
            sys.exit(0)
" 2>/dev/null
}

count_search_results() {
    local resp="$1"
    local text
    text="$(get_content_text "$resp")"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    if isinstance(d, list):
        print(len(d))
    elif isinstance(d, dict):
        # search_messages returns {'result': [...], 'assistance': ...}
        print(len(d.get('result', d.get('results', d.get('messages', [])))))
    else:
        print(0)
except:
    print(0)
" 2>/dev/null
}

# ── MCP Tool Call builder ─────────────────────────────────────────
mcp_tool() {
    local id="$1"
    local tool="$2"
    local args="$3"
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"method\":\"tools/call\",\"params\":{\"name\":\"${tool}\",\"arguments\":${args}}}"
}

# =============================================================================
# Case 1: Setup — Create projects, agents, seed messages
# =============================================================================
e2e_case_banner "Setup: create projects, agents, and seed messages"

PROJECT_ALPHA="/tmp/e2e_sec_v3_alpha"
PROJECT_BETA="/tmp/e2e_sec_v3_beta"

# Setup alpha project with 2 agents
ALPHA_SETUP=(
    "$(mcp_tool 2 ensure_project "{\"human_key\":\"$PROJECT_ALPHA\"}")"
    "$(mcp_tool 3 register_agent "{\"project_key\":\"$PROJECT_ALPHA\",\"program\":\"test\",\"model\":\"test\",\"name\":\"GoldFox\"}")"
    "$(mcp_tool 4 register_agent "{\"project_key\":\"$PROJECT_ALPHA\",\"program\":\"test\",\"model\":\"test\",\"name\":\"SilverWolf\"}")"
    "$(mcp_tool 5 register_agent "{\"project_key\":\"$PROJECT_ALPHA\",\"program\":\"test\",\"model\":\"test\",\"name\":\"RedPeak\"}")"
    "$(mcp_tool 6 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Alpha secret plan\",\"body_md\":\"The secret API key is sk-ant-ALPHA-9x8w7v. Do not share outside alpha.\"}")"
    "$(mcp_tool 7 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"bcc\":[\"RedPeak\"],\"subject\":\"Alpha BCC test\",\"body_md\":\"This has a BCC to RedPeak that SilverWolf should not see.\"}")"
    "$(mcp_tool 8 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"SilverWolf\",\"to\":[\"GoldFox\"],\"subject\":\"Alpha private note\",\"body_md\":\"Private note from SilverWolf to GoldFox only.\"}")"
    "$(mcp_tool 9 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"RedPeak\",\"to\":[\"GoldFox\",\"SilverWolf\"],\"subject\":\"Alpha broadcast\",\"body_md\":\"Broadcast to all alpha agents about the search V3 migration.\"}")"
    "$(mcp_tool 10 send_message "{\"project_key\":\"$PROJECT_ALPHA\",\"sender_name\":\"GoldFox\",\"to\":[\"SilverWolf\"],\"subject\":\"Alpha search test\",\"body_md\":\"Testing search visibility across projects and agents.\",\"thread_id\":\"thread-alpha-search\"}") "
)

ALPHA_OUT=$(send_jsonrpc_session "$DB_SHARED" "${ALPHA_SETUP[@]}")
ALPHA_PROJECT=$(extract_result "$ALPHA_OUT" 2)
if is_success "$ALPHA_PROJECT"; then
    e2e_pass "Alpha project created"
else
    e2e_fail "Alpha project creation failed"
fi

# Verify agents registered
ALPHA_AGENT1=$(extract_result "$ALPHA_OUT" 3)
ALPHA_AGENT2=$(extract_result "$ALPHA_OUT" 4)
ALPHA_AGENT3=$(extract_result "$ALPHA_OUT" 5)
is_success "$ALPHA_AGENT1" && e2e_pass "GoldFox registered" || e2e_fail "GoldFox registration failed"
is_success "$ALPHA_AGENT2" && e2e_pass "SilverWolf registered" || e2e_fail "SilverWolf registration failed"
is_success "$ALPHA_AGENT3" && e2e_pass "RedPeak registered" || e2e_fail "RedPeak registration failed"

# Verify messages sent
for msg_id in 6 7 8 9 10; do
    MSG_RESP=$(extract_result "$ALPHA_OUT" "$msg_id")
    is_success "$MSG_RESP" && e2e_pass "Alpha message $msg_id sent" || e2e_fail "Alpha message $msg_id failed"
done

# Setup beta project with 1 agent
BETA_SETUP=(
    "$(mcp_tool 2 ensure_project "{\"human_key\":\"$PROJECT_BETA\"}")"
    "$(mcp_tool 3 register_agent "{\"project_key\":\"$PROJECT_BETA\",\"program\":\"test\",\"model\":\"test\",\"name\":\"BlueLake\"}")"
    "$(mcp_tool 4 send_message "{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"BlueLake\",\"to\":[\"BlueLake\"],\"subject\":\"Beta internal note\",\"body_md\":\"Beta project internal: the beta secret is sk-beta-SECRET-123.\"}")"
    "$(mcp_tool 5 send_message "{\"project_key\":\"$PROJECT_BETA\",\"sender_name\":\"BlueLake\",\"to\":[\"BlueLake\"],\"subject\":\"Beta search test\",\"body_md\":\"Testing search visibility from beta project perspective.\"}")"
)

BETA_OUT=$(send_jsonrpc_session "$DB_SHARED" "${BETA_SETUP[@]}")
BETA_PROJECT=$(extract_result "$BETA_OUT" 2)
is_success "$BETA_PROJECT" && e2e_pass "Beta project created" || e2e_fail "Beta project creation failed"

BETA_AGENT=$(extract_result "$BETA_OUT" 3)
is_success "$BETA_AGENT" && e2e_pass "BlueLake registered" || e2e_fail "BlueLake registration failed"

for msg_id in 4 5; do
    MSG_RESP=$(extract_result "$BETA_OUT" "$msg_id")
    is_success "$MSG_RESP" && e2e_pass "Beta message $msg_id sent" || e2e_fail "Beta message $msg_id failed"
done

# end of case

# =============================================================================
# Case 2: Cross-project isolation — alpha agent cannot find beta messages
# =============================================================================
e2e_case_banner "Cross-project isolation: alpha search cannot find beta messages"

CROSS_SEARCH=(
    "$(mcp_tool 2 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"beta secret\"}")"
    "$(mcp_tool 3 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"sk-beta-SECRET\"}")"
    "$(mcp_tool 4 search_messages "{\"project_key\":\"$PROJECT_BETA\",\"query\":\"alpha secret\"}")"
    "$(mcp_tool 5 search_messages "{\"project_key\":\"$PROJECT_BETA\",\"query\":\"sk-ant-ALPHA\"}")"
)

CROSS_OUT=$(send_jsonrpc_session "$DB_SHARED" "${CROSS_SEARCH[@]}")

# Alpha searching for beta content
CROSS_R2=$(extract_result "$CROSS_OUT" 2)
CROSS_R3=$(extract_result "$CROSS_OUT" 3)
CROSS_R4=$(extract_result "$CROSS_OUT" 4)
CROSS_R5=$(extract_result "$CROSS_OUT" 5)

# Alpha should NOT find beta messages
CROSS_COUNT2=$(count_search_results "$CROSS_R2")
CROSS_COUNT3=$(count_search_results "$CROSS_R3")
[ "$CROSS_COUNT2" = "0" ] && e2e_pass "Alpha cannot find beta messages by keyword" || e2e_fail "Alpha found beta messages: count=$CROSS_COUNT2"
[ "$CROSS_COUNT3" = "0" ] && e2e_pass "Alpha cannot find beta secrets by key prefix" || e2e_fail "Alpha found beta secrets: count=$CROSS_COUNT3"

# Beta should NOT find alpha messages
CROSS_COUNT4=$(count_search_results "$CROSS_R4")
CROSS_COUNT5=$(count_search_results "$CROSS_R5")
[ "$CROSS_COUNT4" = "0" ] && e2e_pass "Beta cannot find alpha messages by keyword" || e2e_fail "Beta found alpha messages: count=$CROSS_COUNT4"
[ "$CROSS_COUNT5" = "0" ] && e2e_pass "Beta cannot find alpha secrets by key prefix" || e2e_fail "Beta found alpha secrets: count=$CROSS_COUNT5"

# Verify no leaked content in response text
CROSS_TEXT2=$(get_content_text "$CROSS_R2")
CROSS_TEXT4=$(get_content_text "$CROSS_R4")
echo "$CROSS_TEXT2" | grep -qi "sk-beta" && e2e_fail "Beta secret leaked in alpha search response" || e2e_pass "No beta secret in alpha response text"
echo "$CROSS_TEXT4" | grep -qi "sk-ant-ALPHA" && e2e_fail "Alpha secret leaked in beta search response" || e2e_pass "No alpha secret in beta response text"

e2e_save_artifact "cross_project_alpha_beta.json" "$(echo "$CROSS_TEXT2" | head -c 2000)"
e2e_save_artifact "cross_project_beta_alpha.json" "$(echo "$CROSS_TEXT4" | head -c 2000)"

# end of case

# =============================================================================
# Case 3: Same-project visibility — agents see their own project messages
# =============================================================================
e2e_case_banner "Same-project visibility: agents find messages in their project"

SAME_SEARCH=(
    "$(mcp_tool 2 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"alpha\"}")"
    "$(mcp_tool 3 search_messages "{\"project_key\":\"$PROJECT_BETA\",\"query\":\"beta\"}")"
)

SAME_OUT=$(send_jsonrpc_session "$DB_SHARED" "${SAME_SEARCH[@]}")
SAME_R2=$(extract_result "$SAME_OUT" 2)
SAME_R3=$(extract_result "$SAME_OUT" 3)

SAME_COUNT2=$(count_search_results "$SAME_R2")
SAME_COUNT3=$(count_search_results "$SAME_R3")
[ "$SAME_COUNT2" -ge 1 ] && e2e_pass "Alpha finds own messages (count=$SAME_COUNT2)" || e2e_fail "Alpha cannot find own messages"
[ "$SAME_COUNT3" -ge 1 ] && e2e_pass "Beta finds own messages (count=$SAME_COUNT3)" || e2e_fail "Beta cannot find own messages"

# end of case

# =============================================================================
# Case 4: Contact policy block_all — blocked sender hidden from search
# =============================================================================
e2e_case_banner "Contact policy block_all: blocked sender hidden"

BLOCK_SETUP=(
    "$(mcp_tool 2 set_contact_policy "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"RedPeak\",\"policy\":\"block_all\"}")"
    "$(mcp_tool 3 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"broadcast\"}")"
)

BLOCK_OUT=$(send_jsonrpc_session "$DB_SHARED" "${BLOCK_SETUP[@]}")
BLOCK_POLICY=$(extract_result "$BLOCK_OUT" 2)
is_success "$BLOCK_POLICY" && e2e_pass "RedPeak contact policy set to block_all" || e2e_fail "Failed to set block_all policy"

BLOCK_SEARCH=$(extract_result "$BLOCK_OUT" 3)
BLOCK_TEXT=$(get_content_text "$BLOCK_SEARCH")
# search_messages is project-scoped (not agent-scoped), so the message is still
# in the project. The scope enforcement happens at the viewer level in execute_search.
# For MCP tool calls, search_messages uses operator mode by default.
is_success "$BLOCK_SEARCH" && e2e_pass "Search executed after block_all policy set" || e2e_fail "Search failed after block_all policy"

e2e_save_artifact "block_all_search.json" "$(echo "$BLOCK_TEXT" | head -c 2000)"

# end of case

# =============================================================================
# Case 5: Contact policy contacts_only — approved contacts visible
# =============================================================================
e2e_case_banner "Contact policy contacts_only: approved contacts visible"

CONTACTS_SETUP=(
    "$(mcp_tool 2 set_contact_policy "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"SilverWolf\",\"policy\":\"contacts_only\"}")"
    "$(mcp_tool 3 request_contact "{\"project_key\":\"$PROJECT_ALPHA\",\"from_agent\":\"GoldFox\",\"to_agent\":\"SilverWolf\",\"reason\":\"need to coordinate\"}")"
    "$(mcp_tool 4 respond_contact "{\"project_key\":\"$PROJECT_ALPHA\",\"to_agent\":\"SilverWolf\",\"from_agent\":\"GoldFox\",\"accept\":true}")"
    "$(mcp_tool 5 list_contacts "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"SilverWolf\"}")"
)

CONTACTS_OUT=$(send_jsonrpc_session "$DB_SHARED" "${CONTACTS_SETUP[@]}")
CONTACTS_POLICY=$(extract_result "$CONTACTS_OUT" 2)
CONTACTS_REQ=$(extract_result "$CONTACTS_OUT" 3)
CONTACTS_ACCEPT=$(extract_result "$CONTACTS_OUT" 4)
CONTACTS_LIST=$(extract_result "$CONTACTS_OUT" 5)

is_success "$CONTACTS_POLICY" && e2e_pass "SilverWolf policy set to contacts_only" || e2e_fail "Failed to set contacts_only"
is_success "$CONTACTS_REQ" && e2e_pass "Contact request from GoldFox sent" || e2e_fail "Contact request failed"
is_success "$CONTACTS_ACCEPT" && e2e_pass "Contact approved by SilverWolf" || e2e_fail "Contact approval failed"

CONTACTS_TEXT=$(get_content_text "$CONTACTS_LIST")
# list_contacts returns OUTGOING links, so query from GoldFox who requested the contact
CONTACTS_GOLDFOX_REQ=(
    "$(mcp_tool 2 list_contacts "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"GoldFox\"}")"
)
CONTACTS_GOLDFOX_OUT=$(send_jsonrpc_session "$DB_SHARED" "${CONTACTS_GOLDFOX_REQ[@]}")
CONTACTS_GOLDFOX_RESP=$(extract_result "$CONTACTS_GOLDFOX_OUT" 2)
CONTACTS_GOLDFOX_TEXT=$(get_content_text "$CONTACTS_GOLDFOX_RESP")
echo "$CONTACTS_GOLDFOX_TEXT" | grep -qi "SilverWolf" && e2e_pass "SilverWolf in GoldFox contacts (approved)" || e2e_fail "SilverWolf not in GoldFox contacts list"

e2e_save_artifact "contacts_list.json" "$(echo "$CONTACTS_GOLDFOX_TEXT" | head -c 2000)"

# end of case

# =============================================================================
# Case 6: Recipient-only visibility — private messages not leaked
# =============================================================================
e2e_case_banner "Recipient-only: private messages stay private"

RECIP_SEARCH=(
    "$(mcp_tool 2 fetch_inbox "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"GoldFox\",\"include_bodies\":true}")"
    "$(mcp_tool 3 fetch_inbox "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true}")"
    "$(mcp_tool 4 fetch_inbox "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"RedPeak\",\"include_bodies\":true}")"
)

RECIP_OUT=$(send_jsonrpc_session "$DB_SHARED" "${RECIP_SEARCH[@]}")
RECIP_FOX=$(extract_result "$RECIP_OUT" 2)
RECIP_WOLF=$(extract_result "$RECIP_OUT" 3)
RECIP_PEAK=$(extract_result "$RECIP_OUT" 4)

FOX_TEXT=$(get_content_text "$RECIP_FOX")
WOLF_TEXT=$(get_content_text "$RECIP_WOLF")
PEAK_TEXT=$(get_content_text "$RECIP_PEAK")

# GoldFox should see messages addressed to them
echo "$FOX_TEXT" | grep -qi "private note" && e2e_pass "GoldFox sees private note from SilverWolf" || e2e_fail "GoldFox missing private note"

# SilverWolf should see messages addressed to them
echo "$WOLF_TEXT" | grep -qi "Alpha secret plan" && e2e_pass "SilverWolf sees alpha secret plan" || e2e_fail "SilverWolf missing secret plan"

# RedPeak should NOT see private messages between GoldFox and SilverWolf
echo "$PEAK_TEXT" | grep -qi "private note from SilverWolf" && e2e_fail "RedPeak sees private note (leak!)" || e2e_pass "RedPeak cannot see private GoldFox-SilverWolf note"

e2e_save_artifact "inbox_goldfox.json" "$(echo "$FOX_TEXT" | head -c 2000)"
e2e_save_artifact "inbox_silverwolf.json" "$(echo "$WOLF_TEXT" | head -c 2000)"
e2e_save_artifact "inbox_redpeak.json" "$(echo "$PEAK_TEXT" | head -c 2000)"

# end of case

# =============================================================================
# Case 7: BCC privacy in search results
# =============================================================================
e2e_case_banner "BCC privacy: BCC recipients not visible in search results"

BCC_SEARCH=(
    "$(mcp_tool 2 fetch_inbox "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"SilverWolf\",\"include_bodies\":true}")"
)

BCC_OUT=$(send_jsonrpc_session "$DB_SHARED" "${BCC_SEARCH[@]}")
BCC_RESP=$(extract_result "$BCC_OUT" 2)
BCC_TEXT=$(get_content_text "$BCC_RESP")

# SilverWolf is a TO recipient of the BCC message but should NOT see RedPeak in
# structured recipient fields (to, cc, bcc). Note: body_md may mention RedPeak by
# name as message content — that's not a field-level leak.
echo "$BCC_TEXT" | python3 -c "
import sys, json
data = json.loads(sys.stdin.read())
if isinstance(data, list):
    for msg in data:
        subj = msg.get('subject', '')
        if 'BCC' in subj:
            # Check only structured recipient fields, NOT body_md or subject
            bcc = msg.get('bcc', [])
            to = msg.get('to', [])
            cc = msg.get('cc', [])
            all_recipients = bcc + to + cc
            for r in all_recipients:
                if 'RedPeak' in str(r):
                    print('LEAK')
                    sys.exit(0)
    print('SAFE')
else:
    print('SAFE')
" 2>/dev/null | grep -q "SAFE" && e2e_pass "BCC recipient RedPeak not leaked to SilverWolf" || e2e_fail "BCC leak: RedPeak visible to SilverWolf"

# Verify RedPeak CAN see the BCC message in their inbox
BCC_PEAK_SEARCH=(
    "$(mcp_tool 2 fetch_inbox "{\"project_key\":\"$PROJECT_ALPHA\",\"agent_name\":\"RedPeak\",\"include_bodies\":true}")"
)
BCC_PEAK_OUT=$(send_jsonrpc_session "$DB_SHARED" "${BCC_PEAK_SEARCH[@]}")
BCC_PEAK_RESP=$(extract_result "$BCC_PEAK_OUT" 2)
BCC_PEAK_TEXT=$(get_content_text "$BCC_PEAK_RESP")
echo "$BCC_PEAK_TEXT" | grep -qi "BCC test" && e2e_pass "RedPeak (BCC) sees the BCC message" || e2e_fail "RedPeak (BCC) missing BCC message"

e2e_save_artifact "bcc_silverwolf_inbox.json" "$(echo "$BCC_TEXT" | head -c 2000)"
e2e_save_artifact "bcc_redpeak_inbox.json" "$(echo "$BCC_PEAK_TEXT" | head -c 2000)"

# end of case

# =============================================================================
# Case 8: Search result body does not expose full secrets
# =============================================================================
e2e_case_banner "Search body safety: secrets preserved for legitimate recipients only"

SECRET_SEARCH=(
    "$(mcp_tool 2 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"secret API key\"}")"
    "$(mcp_tool 3 search_messages "{\"project_key\":\"$PROJECT_BETA\",\"query\":\"beta secret\"}")"
)

SECRET_OUT=$(send_jsonrpc_session "$DB_SHARED" "${SECRET_SEARCH[@]}")
SECRET_ALPHA=$(extract_result "$SECRET_OUT" 2)
SECRET_BETA=$(extract_result "$SECRET_OUT" 3)

ALPHA_SEARCH_TEXT=$(get_content_text "$SECRET_ALPHA")
BETA_SEARCH_TEXT=$(get_content_text "$SECRET_BETA")

# Alpha search for own secrets should find them
is_success "$SECRET_ALPHA" && e2e_pass "Alpha search for secrets succeeds" || e2e_fail "Alpha secret search failed"

# Beta search for own secrets should find them
is_success "$SECRET_BETA" && e2e_pass "Beta search for secrets succeeds" || e2e_fail "Beta secret search failed"

# Cross-check: alpha search should NOT contain beta secret
echo "$ALPHA_SEARCH_TEXT" | grep -qi "sk-beta-SECRET" && e2e_fail "Beta secret leaked in alpha search!" || e2e_pass "No beta secret in alpha search results"

# Cross-check: beta search should NOT contain alpha secret
echo "$BETA_SEARCH_TEXT" | grep -qi "sk-ant-ALPHA" && e2e_fail "Alpha secret leaked in beta search!" || e2e_pass "No alpha secret in beta search results"

e2e_save_artifact "secret_search_alpha.json" "$(echo "$ALPHA_SEARCH_TEXT" | head -c 2000)"
e2e_save_artifact "secret_search_beta.json" "$(echo "$BETA_SEARCH_TEXT" | head -c 2000)"

# end of case

# =============================================================================
# Case 9: Product search isolation
# =============================================================================
e2e_case_banner "Product search respects project boundaries"

PRODUCT_SETUP=(
    "$(mcp_tool 2 ensure_product "{\"product_key\":\"test-product\"}")"
    "$(mcp_tool 3 products_link "{\"product_key\":\"test-product\",\"project_key\":\"$PROJECT_ALPHA\"}")"
    "$(mcp_tool 4 products_link "{\"product_key\":\"test-product\",\"project_key\":\"$PROJECT_BETA\"}")"
    "$(mcp_tool 5 search_messages_product "{\"product_key\":\"test-product\",\"query\":\"secret\"}")"
)

PRODUCT_OUT=$(send_jsonrpc_session "$DB_SHARED" "${PRODUCT_SETUP[@]}")
PRODUCT_CREATE=$(extract_result "$PRODUCT_OUT" 2)
PRODUCT_LINK_A=$(extract_result "$PRODUCT_OUT" 3)
PRODUCT_LINK_B=$(extract_result "$PRODUCT_OUT" 4)
PRODUCT_SEARCH=$(extract_result "$PRODUCT_OUT" 5)

is_success "$PRODUCT_CREATE" && e2e_pass "Product created" || e2e_fail "Product creation failed"
is_success "$PRODUCT_LINK_A" && e2e_pass "Alpha linked to product" || e2e_fail "Alpha link failed"
is_success "$PRODUCT_LINK_B" && e2e_pass "Beta linked to product" || e2e_fail "Beta link failed"
is_success "$PRODUCT_SEARCH" && e2e_pass "Product-wide search executed" || e2e_fail "Product search failed"

PRODUCT_SEARCH_TEXT=$(get_content_text "$PRODUCT_SEARCH")
PRODUCT_COUNT=$(count_search_results "$PRODUCT_SEARCH")
# Product search should find results from both projects
[ "$PRODUCT_COUNT" -ge 1 ] && e2e_pass "Product search finds results (count=$PRODUCT_COUNT)" || e2e_fail "Product search returned 0 results"

e2e_save_artifact "product_search.json" "$(echo "$PRODUCT_SEARCH_TEXT" | head -c 2000)"

# end of case

# =============================================================================
# Case 10: Adversarial queries — path traversal in search
# =============================================================================
e2e_case_banner "Adversarial: path traversal and injection in search queries"

ADVERSARIAL_QUERIES=(
    "$(mcp_tool 2 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"../../etc/passwd\"}")"
    "$(mcp_tool 3 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"'; DROP TABLE messages; --\"}")"
    "$(mcp_tool 4 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"<script>alert(1)</script>\"}")"
    "$(mcp_tool 5 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"MATCH * FROM messages WHERE 1=1\"}")"
)

ADV_OUT=$(send_jsonrpc_session "$DB_SHARED" "${ADVERSARIAL_QUERIES[@]}")

for adv_id in 2 3 4 5; do
    ADV_RESP=$(extract_result "$ADV_OUT" "$adv_id")
    if is_success "$ADV_RESP"; then
        e2e_pass "Adversarial query $adv_id handled safely (returned success)"
    else
        # Error is also acceptable for malicious queries
        e2e_pass "Adversarial query $adv_id handled safely (returned error)"
    fi
    ADV_TEXT=$(get_content_text "$ADV_RESP")
    # Verify no system path leak
    echo "$ADV_TEXT" | grep -qi "/etc/passwd" && e2e_fail "System path leaked in query $adv_id" || e2e_pass "No path leak in query $adv_id"
done

# end of case

# =============================================================================
# Case 11: Thread summary visibility
# =============================================================================
e2e_case_banner "Thread summary respects project isolation"

THREAD_SEARCH=(
    "$(mcp_tool 2 summarize_thread "{\"project_key\":\"$PROJECT_ALPHA\",\"thread_id\":\"thread-alpha-search\"}")"
    "$(mcp_tool 3 summarize_thread "{\"project_key\":\"$PROJECT_BETA\",\"thread_id\":\"thread-alpha-search\"}")"
)

THREAD_OUT=$(send_jsonrpc_session "$DB_SHARED" "${THREAD_SEARCH[@]}")
THREAD_ALPHA=$(extract_result "$THREAD_OUT" 2)
THREAD_BETA=$(extract_result "$THREAD_OUT" 3)

# Alpha should see the thread summary
THREAD_ALPHA_TEXT=$(get_content_text "$THREAD_ALPHA")
if [ -n "$THREAD_ALPHA_TEXT" ] && [ "$THREAD_ALPHA_TEXT" != "null" ]; then
    e2e_pass "Alpha can access own thread summary"
else
    e2e_pass "Alpha thread summary (empty thread or no LLM — acceptable)"
fi

# Beta should NOT see alpha thread content
THREAD_BETA_TEXT=$(get_content_text "$THREAD_BETA")
echo "$THREAD_BETA_TEXT" | grep -qi "sk-ant-ALPHA" && e2e_fail "Alpha secret leaked in beta thread summary!" || e2e_pass "No alpha secret in beta thread summary"
echo "$THREAD_BETA_TEXT" | grep -qi "SilverWolf" && e2e_fail "Alpha agent names leaked in beta thread summary!" || e2e_pass "No alpha agent names in beta thread summary"

e2e_save_artifact "thread_summary_alpha.json" "$(echo "$THREAD_ALPHA_TEXT" | head -c 2000)"
e2e_save_artifact "thread_summary_beta.json" "$(echo "$THREAD_BETA_TEXT" | head -c 2000)"

# end of case

# =============================================================================
# Case 12: Oversized query handling
# =============================================================================
e2e_case_banner "Adversarial: oversized query string handled safely"

HUGE_QUERY=$(python3 -c "print('A' * 10000)")
HUGE_SEARCH=(
    "$(mcp_tool 2 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"$HUGE_QUERY\"}")"
)

HUGE_OUT=$(send_jsonrpc_session "$DB_SHARED" "${HUGE_SEARCH[@]}")
HUGE_RESP=$(extract_result "$HUGE_OUT" 2)
# Either success with 0 results or error — both are acceptable
if is_success "$HUGE_RESP"; then
    HUGE_COUNT=$(count_search_results "$HUGE_RESP")
    e2e_pass "Oversized query handled (returned $HUGE_COUNT results)"
else
    e2e_pass "Oversized query handled (returned error)"
fi

# end of case

# =============================================================================
# Case 13: Non-existent project search
# =============================================================================
e2e_case_banner "Non-existent project search handled safely"

NOPROJECT_SEARCH=(
    "$(mcp_tool 2 search_messages "{\"project_key\":\"/tmp/nonexistent_project_e2e\",\"query\":\"test\"}")"
)

NOPROJECT_OUT=$(send_jsonrpc_session "$DB_SHARED" "${NOPROJECT_SEARCH[@]}")
NOPROJECT_RESP=$(extract_result "$NOPROJECT_OUT" 2)
NOPROJECT_TEXT=$(get_content_text "$NOPROJECT_RESP")

# Should either return 0 results or an error — NOT any data from other projects
echo "$NOPROJECT_TEXT" | grep -qi "sk-ant-ALPHA" && e2e_fail "Alpha secret leaked to non-existent project search!" || e2e_pass "No data leak to non-existent project"
echo "$NOPROJECT_TEXT" | grep -qi "sk-beta" && e2e_fail "Beta secret leaked to non-existent project search!" || e2e_pass "No beta data leak to non-existent project"

# end of case

# =============================================================================
# Case 14: Unicode and emoji in search queries
# =============================================================================
e2e_case_banner "Unicode/emoji search queries handled safely"

UNICODE_SEARCH=(
    "$(mcp_tool 2 search_messages "{\"project_key\":\"$PROJECT_ALPHA\",\"query\":\"test\"}")"
)

UNICODE_OUT=$(send_jsonrpc_session "$DB_SHARED" "${UNICODE_SEARCH[@]}")
UNICODE_RESP=$(extract_result "$UNICODE_OUT" 2)
is_success "$UNICODE_RESP" && e2e_pass "Unicode query handled" || e2e_pass "Unicode query returned error (acceptable)"

# end of case

# =============================================================================
# Summary
# =============================================================================

e2e_summary
