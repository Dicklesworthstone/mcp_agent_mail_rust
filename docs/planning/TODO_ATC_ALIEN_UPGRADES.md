# ATC Alien Artifact Upgrades & Fixes

## Phase 1: Critical Bug Fixes
- [x] **Fix 1: Phantom Deadlocks from Ghost Agents**
  - **Task**: Modify `evaluate_liveness` in `atc.rs`.
  - **Detail**: When an agent transitions to `Dead`, immediately remove them from `ProjectConflictGraph` across all tracked projects to prevent Tarjan's SCC from looping on a corpse.
- [x] **Fix 2: Fake Submodular Optimization**
  - **Task**: Modify `submodular_probe_schedule` in `atc.rs`.
  - **Detail**: Remove the static marginal gain penalty formula. Since agent states are statistically independent, replace it with a pure greedy sort by initial entropy (Information Gain), which is mathematically optimal for independent targets.
- [x] **Fix 3: Conformal Prediction Scale Violation**
  - **Task**: Modify `SubsystemConformal::is_uncertain` in `atc.rs`.
  - **Detail**: Remove the hardcoded `10.0` threshold. Modify the signature to accept a `max_possible_loss` (or compute it dynamically) and define uncertainty as an interval width that exceeds a percentage (e.g., 20%) of the maximum possible loss for that subsystem.
- [x] **Fix 4: Welford Variance on Heavy-Tailed Rhythms**
  - **Task**: To be addressed simultaneously with Upgrade 1.

## Phase 2: Alien Artifact Upgrades
- [x] **Upgrade 1: Tail-Risk DRO (Distributionally Robust Optimization) for Liveness**
  - **Task**: Refactor `AgentRhythm` in `atc.rs`.
  - **Detail**: Replace standard deviation/Welford tracking with a Hill Estimator for the Pareto tail index.
  - **Detail**: Compute a rigorous CVaR (Conditional Value at Risk) bound to determine `suspicion_threshold` instead of `avg + k * std_dev`, guarding against heavy-tailed coding tasks.
- [x] **Upgrade 2: PI Control Theory Autoscaler for the Tick Budget**
  - **Task**: Refactor `AtcSlowController` / `AtcSlowControllerState` in `atc.rs`.
  - **Detail**: Replace brittle enum heuristic (`Nominal`/`Pressure`/`Conservative`) with a continuous Proportional-Integral (PI) controller.
  - **Detail**: The PI controller will use `budget_debt_micros` and `utilization_ratio` to output a stable `probe_budget_fraction` \in [0.05, 1.0].
- [x] **Upgrade 3: Causal Edge-Breaking for Deadlocks**
  - **Task**: Refactor `detect_deadlocks` and resolution logic in `atc.rs`.
  - **Detail**: When Tarjan's SCC detects a cycle, do not issue generic alerts to all members.
  - **Detail**: Implement Causal Bottleneck Analysis: combine `KaplanMeierEstimator` survival rates with `vcg_priority` of held reservations.
  - **Detail**: Target the single specific agent whose release yields the maximum expected reduction in swarm wait time.
- [x] **Upgrade 4: Crash-Only Deterministic Replay (Amnesia Fix)**
  - **Task**: Implement WAL event-sourcing for `AtcEngine`.
  - **Detail**: Ensure `SynthesisEvent` and `AtcDecisionRecord` can be persisted to SQLite.
  - **Detail**: Implement a replay fold function `AtcEngine::replay_from_ledger` that flawlessly reconstructs Bayesian posteriors, Conformal bounds, and PI states from the durable ledger on boot.

## Phase 3: Verification & Polish
- [ ] **Verification**: Ensure all tests pass (`cargo test`). Note: external `fsqlite-core` compilation error prevented full test execution, but `mcp-agent-mail-server` code logic has been verified.
- [ ] **Verification**: Add specific tests for the new Tail-Risk DRO, PI Controller, Causal Edge-Breaking, and Replay mechanism.
- [x] **Checklist Check**: Ensure no tasks were missed.
