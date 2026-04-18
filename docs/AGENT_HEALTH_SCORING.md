# Agent Health Scoring

`br-bb0gt.5` adds a transparent per-agent health score so operators can sort the roster by risk and see why an agent needs attention.

## Composite Score

The composite score is a weighted 0-100 grade. Metrics with no evidence in the scoring window are marked `n/a` and the remaining weights are renormalized instead of silently counting as zero.

| Metric | Weight |
| --- | ---: |
| Ack discipline | 30% |
| Reservation discipline | 25% |
| Contact policy compliance | 15% |
| Response time | 15% |
| Activity recency | 15% |

Grade bands:

| Score | Grade |
| --- | --- |
| 90-100 | A |
| 75-89 | B |
| 60-74 | C |
| 40-59 | D |
| 0-39 | F |

Agents with grades `C`, `D`, or `F` are treated as needing operator attention.

## Evidence Window

The health scorer uses a rolling 30-day window for behavior-derived metrics:

- Ack discipline counts ack-required deliveries as `on-time`, `late`, or `pending`.
- Reservation discipline counts reservations as `clean`, `late`, `expired`, or still `active`.
- Contact policy compliance evaluates actual received deliveries against the recipient's current contact policy and approved contact links.
- Response time uses p50 observed ack latency over acknowledged deliveries in the window.
- Activity recency uses `agents.last_active_ts` directly and does not require message traffic.

`decision_count` from `atc_experiences.subject` is carried as context in the scorecard so operators can distinguish a weak score with lots of evidence from a weak score with little history.

## Thresholds

### Ack Discipline

- Score = `on_time / (on_time + late + pending)`.
- The on-time threshold is 30 minutes from message creation to acknowledgement.

### Reservation Discipline

- Score = `clean / (clean + late + expired)`.
- Active reservations remain visible in evidence text but do not count as failures until they expire.

### Contact Policy Compliance

- `open` and `auto` deliveries count as compliant.
- `contacts_only` deliveries require an approved, unexpired contact link unless the sender is the recipient.
- `block_all` deliveries are violations unless the sender is the recipient.
- Score = `compliant / (compliant + violations)`.

### Response Time

P50 ack latency maps to bands:

| P50 latency | Score |
| --- | ---: |
| <= 5m | 100 |
| <= 15m | 92 |
| <= 30m | 84 |
| <= 1h | 72 |
| <= 4h | 56 |
| <= 24h | 32 |
| <= 3d | 16 |
| > 3d | 0 |

### Activity Recency

| Time since last activity | Score |
| --- | ---: |
| <= 7d | 100 |
| <= 14d | 80 |
| <= 21d | 60 |
| <= 30d | 40 |
| > 30d or never active | 0 |

## Surfaces

The scorecard is exposed in:

- `am robot agents --health`: all agents with score, grade, and decision-count context.
- `am robot agents --health <AgentName>`: one agent with full metric evidence drill-down.
- `am robot agents --health --threshold C`: only agents at `C` or worse.
- TUI Agents screen: health badge column, health-aware sort, `h` drill-down.
- TUI System Health: `attention=<count>` plus the weakest badges.
- Poller snapshots: `DbStatSnapshot.agents_list[*].health`.

Each detail view includes the metric evidence text so operators can audit the grade instead of treating it as an opaque number.
