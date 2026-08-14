//! Task scheduler — generates and dispatches tasks to System Workers.
//!
//! When System A receives a request to start or stop a unit, the scheduler:
//! 1. Determines which units must be started/stopped (dependency expansion).
//! 2. Creates Job records for each operation.
//! 3. Dispatches WorkerTask messages to the appropriate workers.

pub mod job_type;
pub mod transaction;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Result};
use prost::Message;
use sysa::l10n;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::state::{
    generate_invocation_id, next_job_id, next_task_id, AllocatorHandle, AllocatorState,
    DesiredState, Job, JobCompletion, JobKind, JobMode, JobNewInfo, JobResultKind, JobStatus,
    StartLimitState,
};
use crate::unit::types::{
    ExitKind, MountSection, RestartPolicy, StartLimitAction, UnitFile, UnitKind, UnitSection,
};
use sysa::proto::{
    AutomountConfig, MountConfig, PathConfig, ServiceConfig, SocketAddress, SocketConfig, TimerConfig,
    DeviceConfig, UnitConfig, ScopeConfig,
};

use crate::scheduler::job_type::{job_type_collapse, JobType, UnitActiveState};
use crate::scheduler::transaction::{build_plan, PlannerMode};

/// Map a transaction job type onto the public job kind used for worker
/// dispatch. `Nop` and `VerifyActive` steps need no worker interaction:
/// they are resolved by the planner against the cached runtime state.
fn job_kind_from_type(t: JobType) -> Option<JobKind> {
    match t {
        JobType::Start => Some(JobKind::Start),
        JobType::Stop => Some(JobKind::Stop),
        JobType::Restart => Some(JobKind::Restart),
        JobType::Reload => Some(JobKind::Reload),
        JobType::VerifyActive | JobType::Nop => None,
        JobType::TryRestart | JobType::TryReload | JobType::ReloadOrStart => None,
    }
}

/// Mode/type validation from systemd's `manager_add_job_full()`
/// (`manager.c:2321-2347`):
///
/// - `triggering` is only valid for stop jobs;
/// - `restart-dependencies` is only valid for start jobs;
/// - `isolate` requires `AllowIsolate=yes` on the unit.
fn check_mode_constraints(mode: JobMode, kind: JobKind, unit_name: &str, allow_isolate: bool) -> Result<()> {
    if mode == JobMode::Triggering && kind != JobKind::Stop {
        bail!("{}", l10n::fmt(l10n::t_("--job-mode=triggering is only valid for stop."), &[]));
    }
    if mode == JobMode::RestartDependencies && kind != JobKind::Start {
        bail!("{}", l10n::fmt(
            l10n::t_("--job-mode=restart-dependencies is only valid for start."),
            &[],
        ));
    }
    if mode == JobMode::Isolate && !allow_isolate {
        bail!("{}", l10n::fmt(
            l10n::t_("Operation refused, unit {unit_name} may not be isolated."),
            &[("unit_name", unit_name)],
        ));
    }
    Ok(())
}

/// Enqueue a start job with explicit mode.
pub async fn enqueue_start_with_mode(
    allocator: AllocatorHandle,
    unit_name: &str,
    mode: JobMode,
) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Start, mode).await
}

/// Activate a transient unit (created via `StartTransientUnit`).
///
/// Scopes are dispatched through the normal job machinery: the System E
/// worker attaches the transient `PIDs=` to the scope's cgroup and reports
/// back, so the job completes when the worker's `task.result` arrives.
/// Other transient units (slices, auxiliaries) wrap already-existing
/// processes and need no worker: the unit is marked active and the job
/// completes immediately as "done".  This honours the systemd1
/// `StartTransientUnit` contract for callers such as logind.
pub async fn activate_transient_unit(
    allocator: AllocatorHandle,
    unit_name: &str,
    mode: JobMode,
) -> Result<u64> {
    info!("Activating transient unit {} (mode={:?})", unit_name, mode);

    let is_scope = {
        let state = allocator.read();
        state
            .units
            .get(unit_name)
            .map(|u| u.kind == UnitKind::Scope)
            .unwrap_or(false)
    };
    if is_scope {
        return enqueue_start_with_mode(allocator, unit_name, mode).await;
    }

    // Validate mode/type constraints like enqueue_job does.
    {
        let state = allocator.read();
        let allow_isolate = state
            .units
            .get(unit_name)
            .map(|u| u.unit.allow_isolate)
            .unwrap_or(false);
        check_mode_constraints(mode, JobKind::Start, unit_name, allow_isolate)?;
    }

    let job_id = next_job_id();
    let invocation_id = generate_invocation_id();
    {
        let mut state = allocator.write();
        if !state.units.contains_key(unit_name) {
            bail!(
                "{}",
                l10n::fmt(
                    l10n::t_("Transient unit {unit_name} is not loaded."),
                    &[("unit_name", unit_name)],
                )
            );
        }

        // Mark the unit active immediately.  A transient unit wraps existing
        // processes, so there is nothing to wait for.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        {
            let entry = state.unit_states.entry(unit_name.to_string()).or_default();
            entry.active_state = "active".to_string();
            entry.sub_state = "running".to_string();
            entry.invocation_id = invocation_id.clone();
            entry.active_enter_timestamp = now;
            entry.inactive_enter_timestamp = 0;
        }
        state
            .invocation_ids
            .insert(unit_name.to_string(), invocation_id);
        state
            .desired
            .insert(unit_name.to_string(), DesiredState::Active);

        // Create the job and complete it immediately as "done".
        state.jobs.insert(
            job_id,
            Job {
                id: job_id,
                unit_name: unit_name.to_string(),
                kind: JobKind::Start,
                status: JobStatus::Running,
                timeout_abort: None,
            },
        );
        emit_job_new(&mut state, job_id, unit_name, JobKind::Start);
        if let Some(job) = state.jobs.get_mut(&job_id) {
            job.status = JobStatus::Done;
        }
        if let Some(ref tx) = state.job_completion_tx {
            let _ = tx.send(JobCompletion {
                job_id,
                unit_name: unit_name.to_string(),
                result: JobResultKind::Done,
            });
        }
    }
    Ok(job_id)
}

/// Enqueue a job by full systemd job type, collapsing state-dependent
/// types (`try-restart`, `try-reload`, `reload-or-start`) against the
/// cached runtime state before planning — mirroring `manager_add_job_full()`
/// + `job_type_collapse()` (`job.c:482`).
///
/// `reload_if_possible` mirrors systemd's `ReloadOrRestartUnit` /
/// `ReloadOrTryRestartUnit` semantics (`unit_queue_job_check_and_mangle_type`,
/// `unit.c:7160`): when the unit can reload, `restart` is mangled into
/// `reload-or-start` and `try-restart` into `try-reload` before collapsing.
///
/// When the root collapses to `Nop` (e.g. try-restart of an inactive unit)
/// systemd creates a nop job that completes immediately with `JOB_DONE`
/// (`job.c:959-963`); a directly requested `verify-active` root completes
/// `done` when the unit is active-like and `skipped` otherwise (systemd
/// waits for activating units; System A completes immediately). In both
/// cases the job record is created, `JobNew`/`JobRemoved` are emitted and
/// no worker is touched.
///
/// Returns the job id and the collapsed kind so callers can track desired
/// state exactly.
pub async fn enqueue_job_type(
    allocator: AllocatorHandle,
    unit_name: &str,
    job_type: JobType,
    reload_if_possible: bool,
    mode: JobMode,
) -> Result<(u64, JobKind)> {
    // Resolve alias names to their canonical unit before anything else.
    let resolved = allocator.read().resolve_unit_name(unit_name);
    let unit_name: &str = &resolved;
    info!(
        "Scheduling {} for {} (mode={:?})",
        job_type.as_str(),
        unit_name,
        mode
    );

    // Mangle reload-if-possible, then collapse against the cached state.
    let (mangled, collapsed, state_now, allow_isolate) = {
        let state = allocator.read();
        let unit = state.units.get(unit_name);
        let can_reload = unit
            .and_then(|u| u.service.as_ref())
            .map(|s| !s.exec_reload.is_empty())
            .unwrap_or(false);
        let mangled = if reload_if_possible && can_reload {
            match job_type {
                JobType::Restart => JobType::ReloadOrStart,
                JobType::TryRestart => JobType::TryReload,
                other => other,
            }
        } else {
            job_type
        };
        let state_now = state
            .unit_states
            .get(unit_name)
            .map(|c| UnitActiveState::from_active_state_str(&c.active_state))
            .unwrap_or(UnitActiveState::Unknown);
        let collapsed = job_type_collapse(mangled, state_now);
        (
            mangled,
            collapsed,
            state_now,
            unit.map(|u| u.unit.allow_isolate).unwrap_or(false),
        )
    };

    // Validate mode/type combinations on the *requested* (mangled) type,
    // like systemd's manager_add_job_full() does before collapsing.
    let requested_kind = job_kind_from_type(mangled).unwrap_or(JobKind::Nop);
    check_mode_constraints(mode, requested_kind, unit_name, allow_isolate)?;

    match collapsed {
        JobType::Nop => {
            let job_id = next_job_id();
            let mut st = allocator.write();
            st.jobs.insert(
                job_id,
                Job {
                    id: job_id,
                    unit_name: unit_name.to_string(),
                    kind: JobKind::Nop,
                    status: JobStatus::Done,
                    timeout_abort: None,
                },
            );
            emit_job_new(&mut st, job_id, unit_name, JobKind::Nop);
            if let Some(ref tx) = st.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Done,
                });
            }
            Ok((job_id, JobKind::Nop))
        }
        JobType::VerifyActive => {
            // Directly requested verify-active root (`EnqueueUnitJob`).
            // systemd: active-like → done, activating → wait, else → skipped.
            let result = if state_now.is_active_or_reloading() {
                JobResultKind::Done
            } else {
                JobResultKind::Skipped
            };
            let job_id = next_job_id();
            let mut st = allocator.write();
            st.jobs.insert(
                job_id,
                Job {
                    id: job_id,
                    unit_name: unit_name.to_string(),
                    kind: JobKind::Nop,
                    status: JobStatus::Done,
                    timeout_abort: None,
                },
            );
            emit_job_new(&mut st, job_id, unit_name, JobKind::Nop);
            if let Some(ref tx) = st.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result,
                });
            }
            Ok((job_id, JobKind::Nop))
        }
        other => {
            let kind = job_kind_from_type(other).expect("collapsed job type dispatches to a worker");
            let job_id = enqueue_job(allocator, unit_name, kind, mode).await?;
            Ok((job_id, kind))
        }
    }
}

/// Core job enqueueing logic.
pub async fn enqueue_job(
    allocator: AllocatorHandle,
    unit_name: &str,
    kind: JobKind,
    mode: JobMode,
) -> Result<u64> {
    // Resolve alias names to their canonical unit before anything else, so an
    // alias (e.g. `display-manager.service`) never schedules a separate job.
    let resolved = allocator.read().resolve_unit_name(unit_name);
    let unit_name: &str = &resolved;
    info!("Scheduling {:?} for {} (mode={:?})", kind, unit_name, mode);

    // --- Mode/type validation (manager_add_job_full) ---
    {
        let state = allocator.read();
        let allow_isolate = state
            .units
            .get(unit_name)
            .map(|u| u.unit.allow_isolate)
            .unwrap_or(false);
        check_mode_constraints(mode, kind, unit_name, allow_isolate)?;
    }

    // --- Non-transient scopes are refused (systemd scope_start) ---
    //
    // Scopes wrap externally-created processes and exist only as transient
    // units: a `.scope` on disk cannot be started by us.  Mirrors
    // systemd's `scope_start()` returning -ENOENT for non-transient scopes.
    if kind == JobKind::Start {
        let state = allocator.read();
        if let Some(unit) = state.units.get(unit_name) {
            if unit.kind == UnitKind::Scope && !unit.transient {
                bail!(
                    "{}",
                    l10n::fmt(
                        l10n::t_("Scope {unit_name} is not transient and cannot be started."),
                        &[("unit_name", unit_name)],
                    )
                );
            }
        }
    }

    // --- Early check: ensure at least one worker exists for the root unit ---
    {
        let state = allocator.read();
        let unit = state.units.get(unit_name);
        let unit_type = unit
            .map(|u| u.kind.worker_type().to_string())
            .unwrap_or_else(|| "service".to_string());
        let has_worker = state
            .workers
            .values()
            .any(|w| w.unit_types.contains(&unit_type));
        if !has_worker {
            bail!("{}", l10n::fmt(l10n::t_("No worker available for unit type '{unit_type}' (unit: {unit_name}). Cannot execute {kind:?} operation. Is the corresponding System Worker running?"), &[
                ("unit_type", &unit_type),
                ("unit_name", unit_name),
                ("kind", &format!("{:?}", kind)),
            ]));
        }
    }

    // --- Flush mode: cancel all pending jobs first ---
    if mode == JobMode::Flush {
        let to_cancel: Vec<u64> = {
            let state = allocator.read();
            state
                .jobs
                .values()
                .filter(|j| matches!(j.status, JobStatus::Running))
                .map(|j| j.id)
                .collect()
        };
        for jid in to_cancel {
            let mut state = allocator.write();
            let name = state
                .jobs
                .get(&jid)
                .map(|j| j.unit_name.clone())
                .unwrap_or_default();
            if let Some(job) = state.jobs.get_mut(&jid) {
                job.status = JobStatus::Cancelled;
            }
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id: jid,
                    unit_name: name,
                    result: JobResultKind::Cancelled,
                });
            }
        }
    }

    // --- Condition checks: skip start (not failure) if conditions not met ---
    let ignore_deps = mode == JobMode::IgnoreDependencies || mode == JobMode::IgnoreRequirements;
    if matches!(kind, JobKind::Start | JobKind::Restart) && !ignore_deps {
        let conditions_met = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .map(|u| check_conditions(&u.unit))
                .unwrap_or(true)
        };
        if !conditions_met {
            info!(
                "Conditions not met for {}; skipping start (unit stays inactive)",
                unit_name
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Skipped,
                });
            }
            emit_job_new(&mut state, job_id, unit_name, kind);
            return Ok(job_id);
        }
    }

    // --- Assert checks: fail start if asserts not met ---
    if matches!(kind, JobKind::Start | JobKind::Restart) && !ignore_deps {
        let asserts_met = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .map(|u| check_asserts(&u.unit))
                .unwrap_or(true)
        };
        if !asserts_met {
            warn!(
                "Assert check failed for {}; unit start prevented",
                unit_name
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Dependency,
                });
            }
            emit_job_new(&mut state, job_id, unit_name, kind);
            return Ok(job_id);
        }
    }

    // --- Start rate limiting (systemd unit_start → unit_test_start_limit) ---
    // Every start attempt counts: manual starts, auto-restarts and
    // dependency-triggered starts all funnel through enqueue_job(Start).
    if matches!(kind, JobKind::Start | JobKind::Restart) {
        let (interval_sec, burst, action) = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .map(|u| {
                    (
                        u.unit.start_limit_interval_sec,
                        u.unit.start_limit_burst,
                        u.unit.start_limit_action.clone(),
                    )
                })
                .unwrap_or((10, 5, StartLimitAction::None))
        };
        let rate_ok = {
            let mut state = allocator.write();
            let limit_state = state
                .start_limit_state
                .entry(unit_name.to_string())
                .or_insert_with(StartLimitState::new);
            limit_state.check_rate_limit(Duration::from_secs(interval_sec as u64), burst)
        };
        if !rate_ok {
            warn!(
                "Start rate limit exceeded for {} (interval={}s burst={}), refusing to start",
                unit_name, interval_sec, burst
            );
            execute_start_limit_action(&action, unit_name);
            bail!("{}", l10n::fmt(l10n::t_("Start rate limit exceeded for {unit_name} (interval={interval_sec}s burst={burst})."), &[
                ("unit_name", unit_name),
                ("interval_sec", &interval_sec.to_string()),
                ("burst", &burst.to_string()),
            ]));
        }
    }

    // --- Job conflict detection ---
    {
        let read_state = allocator.read();
        let existing: Option<(u64, String)> = read_state
            .jobs
            .values()
            .find(|j| {
                j.unit_name == unit_name && j.kind == kind && matches!(j.status, JobStatus::Running)
            })
            .map(|j| (j.id, j.unit_name.clone()));
        drop(read_state);

        if let Some((existing_id, _)) = existing {
            match mode {
                JobMode::Fail => {
                    bail!("{}", l10n::fmt(l10n::t_("Job already exists for unit {unit_name} (kind={kind:?}, id={existing_id})."), &[
                        ("unit_name", unit_name),
                        ("kind", &format!("{:?}", kind)),
                        ("existing_id", &existing_id.to_string()),
                    ]));
                }
                // systemd merges a job that is already running for the same
                // unit *and* same type into the existing one (job.c
                // `job_merge()`: `unit_get_job()` with a matching type),
                // regardless of job mode — it never re-dispatches it.
                //
                // Cancelling and re-spawning here is what turns a
                // self-recursive `systemctl start` inside a unit's ExecStart
                // into an infinite spawn loop: SysV init scripts (e.g.
                // `/etc/init.d/virtualbox-guest-utils`) detect systemd and
                // delegate to `systemctl start $unit`, which must resolve to
                // the already-running job instead of starting another copy.
                _ => {
                    return Ok(existing_id);
                }
            }
        }
    }

    // --- Build the transaction plan (systemd transaction_activate()) ---
    // The plan is the closure of every unit pulled in through the
    // dependency atoms (Requires/Wants/Requisite/BindsTo/Upholds/Conflicts/
    // PartOf/PropagatesReloadTo), merged to one job per unit, ordered by the
    // After=/Before= ordering graph, with ordering cycles broken.
    let plan = {
        let state = allocator.read();
        let units: HashMap<String, UnitFile> = state.units.clone();
        let states: HashMap<String, UnitActiveState> = state
            .unit_states
            .iter()
            .map(|(n, c)| {
                (
                    n.clone(),
                    UnitActiveState::from_active_state_str(&c.active_state),
                )
            })
            .collect();
        let installed: HashMap<String, JobType> = state
            .jobs
            .values()
            .filter(|j| matches!(j.status, JobStatus::Running))
            .map(|j| (j.unit_name.clone(), JobType::from_job_kind(j.kind)))
            .collect();
        build_plan(
            &units,
            &states,
            &installed,
            unit_name,
            JobType::from_job_kind(kind),
            PlannerMode::from_job_mode(mode),
        )
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(e) => {
            warn!("Transaction for {} ({kind:?}, mode={mode:?}) failed: {e}", unit_name);
            bail!("{}", e);
        }
    };

    debug!(
        "Plan for {unit_name}: {:?}",
        plan.steps
            .iter()
            .map(|s| (s.unit.as_str(), s.job_type.as_str()))
            .collect::<Vec<_>>()
    );

    // --- Requisite verification ---
    // Surviving VerifyActive= steps check the cached runtime state; an
    // unknown state counts as not active (the plan kept the step because
    // the unit is not known to be active), matching systemd.
    if let Some(v) = plan
        .steps
        .iter()
        .find(|s| s.job_type == JobType::VerifyActive)
    {
        let active = {
            let state = allocator.read();
            state
                .unit_states
                .get(&v.unit)
                .map(|c| UnitActiveState::from_active_state_str(&c.active_state))
                .unwrap_or(UnitActiveState::Unknown)
                .is_active_or_reloading()
        };
        if !active {
            warn!(
                "Requisite check failed: {} depends on {} which is not active",
                unit_name, v.unit
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id,
                    unit_name: unit_name.to_string(),
                    result: JobResultKind::Dependency,
                });
            }
            emit_job_new(&mut state, job_id, unit_name, kind);
            return Ok(job_id);
        }
    }

    // Before dispatching any dependencies, re-check that the root doesn't
    // already have a running job (handles races after the conflict detection above).
    {
        let state = allocator.read();
        if let Some(existing) = state.jobs.values().find(|j| {
            j.unit_name == unit_name && j.kind == kind && matches!(j.status, JobStatus::Running)
        }) {
            debug!(
                "Root unit {} already has a {:?} job (id={}) — returning existing ID",
                unit_name, kind, existing.id
            );
            return Ok(existing.id);
        }
    }

    let primary_job_id = next_job_id();
    let serial_mode = mode == JobMode::Replace;

    // For serial execution, chain tasks so each waits for the previous.
    let mut serial_chain_rx: Option<tokio::sync::oneshot::Receiver<()>> = None;

    // Create job records and dispatch tasks.
    let mut created_job_ids: Vec<(u64, String)> = Vec::new();
    for step in &plan.steps {
        let name = &step.unit;
        // Steps that need no worker interaction (Nop/VerifyActive) are
        // resolved by the planner; nothing is dispatched for them.
        let Some(step_kind) = job_kind_from_type(step.job_type) else {
            debug!(
                "Skipping worker dispatch for {} ({})",
                name,
                step.job_type.as_str()
            );
            continue;
        };
        // The primary job ID belongs to the unit that was directly requested.
        let is_root = name.as_str() == unit_name;
        let job_id = if is_root {
            primary_job_id
        } else {
            next_job_id()
        };
        created_job_ids.push((job_id, name.clone()));

        // Idempotency: skip if there is already an in-flight job of the same kind.
        // Note: "already in desired state" check is removed — runtime state is not
        // cached.  Workers handle no-ops on their side.
        {
            let state = allocator.read();
            let existing_running_job_id: Option<u64> = state
                .jobs
                .values()
                .find(|j| {
                    j.unit_name == *name
                        && j.kind == step_kind
                        && matches!(j.status, JobStatus::Running)
                })
                .map(|j| j.id);

            if let Some(existing_jid) = existing_running_job_id {
                debug!("Skipping {:?} for {} (existing job running)", step_kind, name);
                if is_root {
                    return Ok(existing_jid);
                }
                continue;
            }
        }

        // Find the appropriate worker.
        // NOTE: read lock is dropped before match so the error path can
        // acquire the write lock without deadlocking.
        let (worker_chan, worker_id, task_id, unit_type) = {
            let state = allocator.read();
            let unit = state.units.get(name.as_str());
            let unit_type = unit
                .map(|u| u.kind.worker_type().to_string())
                .unwrap_or_else(|| "service".to_string());

            let worker = state
                .workers
                .values()
                .find(|w| w.unit_types.contains(&unit_type));

            let tid = next_task_id();
            (
                worker.map(|w| w.envelope_tx.clone()),
                worker.map(|w| w.worker_id.clone()),
                tid,
                unit_type,
            )
        };

        let (worker_envelope_tx, task_id) = match (worker_chan, task_id) {
            (Some(tx), tid) => (tx, tid),
            (None, _) => {
                let err = l10n::fmt(l10n::t_("No worker registered for unit type '{unit_type}' (unit: {unit_name}). Cannot process dependency chain for '{name}'."), &[
                    ("unit_type", &unit_type),
                    ("unit_name", unit_name),
                    ("name", name),
                ]);
                warn!("{}", err);
                {
                    let mut state = allocator.write();
                    emit_job_new(&mut state, job_id, name, step_kind);

                    // Cancel all jobs already created for this request.
                    for (jid, _) in &created_job_ids {
                        if let Some(job) = state.jobs.get_mut(jid) {
                            job.status = JobStatus::Cancelled;
                        }
                        state.serial_completion_txs.remove(jid);
                    }

                    // Notify the caller that the root job has failed.
                    if let Some(ref tx) = state.job_completion_tx {
                        let _ = tx.send(JobCompletion {
                            job_id: primary_job_id,
                            unit_name: unit_name.to_string(),
                            result: JobResultKind::Failed,
                        });
                    }
                }
                bail!("{}", err);
            }
        };

        let unit_file = {
            let state = allocator.read();
            state.units.get(name.as_str()).cloned()
        };

        // Generate invocation ID for Start/Restart tasks.
        let invocation_id = match step_kind {
            JobKind::Start | JobKind::Restart => Some(generate_invocation_id()),
            _ => None,
        };

        // Create a serial chain entry for this task.
        let (next_serial_tx, next_serial_rx) = tokio::sync::oneshot::channel::<()>();

        // Record the job and the task_id → job_kind mapping.
        // Track invocation_id for GetUnitByInvocationID lookups.
        {
            let mut state = allocator.write();
            if let Some(ref inv_id) = invocation_id {
                state.invocation_ids.insert(name.clone(), inv_id.clone());
            }
            state.jobs.insert(
                job_id,
                Job {
                    id: job_id,
                    unit_name: name.clone(),
                    kind: step_kind,
                    status: JobStatus::Running,
                    timeout_abort: None,
                },
            );
            state.task_kinds.insert(task_id, step_kind);
            // Assign unit ownership to the dispatching worker.  Starting an
            // automount implicitly assigns ownership of its companion mount
            // unit (same worker handles both).
            if let Some(wid) = &worker_id {
                state.unit_owners.insert(name.clone(), wid.clone());
                if step_kind == JobKind::Start && name.ends_with(".automount") {
                    let mount_name = format!("{}.mount", name.trim_end_matches(".automount"));
                    if state.units.contains_key(&mount_name) {
                        state.unit_owners.insert(mount_name, wid.clone());
                    }
                }
            }
            if serial_mode {
                state.serial_completion_txs.insert(task_id, next_serial_tx);
            }
        }

        // Emit JobNew signal for this job.
        emit_job_new_after_lock(allocator.clone(), job_id, name, step_kind);

        // --- Timeout monitoring ---
        let abort_handle = spawn_job_timeout(allocator.clone(), job_id, name, step_kind, &unit_file);
        if let Some(handle) = abort_handle {
            let mut state = allocator.write();
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.timeout_abort = Some(handle);
            }
        }

        // Build MethodCall envelope for this job.
        let method_name = match step_kind {
            JobKind::Start => "start",
            JobKind::Stop => "stop",
            JobKind::Restart => "restart",
            JobKind::Reload => "reload",
            // Nop steps are never dispatched (job_kind_from_type yields
            // None for them); keep the match exhaustive defensively.
            JobKind::Nop => "nop",
        };
        let unit_config = {
            let state = allocator.read();
            unit_file
                .as_ref()
                .map(|uf| build_unit_config(uf, &state.units))
        };
        let mut args = Vec::new();
        if let Some(ref config) = unit_config {
            config.encode(&mut args).unwrap_or_default();
        }
        let call = sysa::proto::MethodCall {
            method: method_name.to_string(),
            unit_name: name.clone(),
            args,
            invocation_id: invocation_id.clone().unwrap_or_default(),
        };
        let call_env =
            sysa::ipc::make_envelope(task_id, "system-a", &unit_type, "method.call", call)?;
        let mut buf = bytes::BytesMut::new();
        call_env.encode(&mut buf)?;

        // In serial mode, wait for the previous task to complete before
        // sending the next one.  The previous task's handle_task_result
        // will signal through the serial chain channel.
        if serial_mode && is_root && serial_chain_rx.is_some() {
            if let Some(rx) = serial_chain_rx.take() {
                let _ = rx.await;
            }
        }
        if serial_mode && !is_root {
            if let Some(rx) = serial_chain_rx.take() {
                let _ = rx.await;
            }
        }

        if worker_envelope_tx.send(buf.freeze()).await.is_err() {
            warn!("Worker channel closed for unit {}", name);
            let mut state = allocator.write();
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.status = JobStatus::Failed("Worker disconnected".to_string());
            }
            if is_root {
                if let Some(ref tx) = state.job_completion_tx {
                    let _ = tx.send(JobCompletion {
                        job_id,
                        unit_name: name.clone(),
                        result: JobResultKind::Failed,
                    });
                }
            }
        }

        // Set up for the next iteration: the current task's serial_rx
        // will be consumed after the next task completes.
        serial_chain_rx = Some(next_serial_rx);
    }

    Ok(primary_job_id)
}

/// Ask every registered worker for a full state snapshot (`unit.sync_request`).
///
/// Workers reply asynchronously with `unit.sync_report` full-snapshot
/// updates which the IPC server feeds into the `unit_states` cache through
/// the same path as push events.  Used on daemon-reload.
pub async fn request_all_worker_syncs(allocator: AllocatorHandle) {
    let targets: Vec<String> = {
        let state = allocator.read();
        state.workers.keys().cloned().collect()
    };
    for worker_id in targets {
        let req = sysa::proto::UnitSyncRequest {};
        let env = match sysa::ipc::make_envelope(
            next_task_id(),
            "system-a",
            &worker_id,
            "unit.sync_request",
            req,
        ) {
            Ok(env) => env,
            Err(e) => {
                warn!("Failed to build unit.sync_request for '{worker_id}': {e}");
                continue;
            }
        };
        let mut buf = bytes::BytesMut::new();
        if env.encode(&mut buf).is_err() {
            warn!("Failed to encode unit.sync_request for '{worker_id}'");
            continue;
        }
        let worker = {
            let state = allocator.read();
            state.workers.get(&worker_id).map(|w| w.envelope_tx.clone())
        };
        if let Some(tx) = worker {
            if tx.send(buf.freeze()).await.is_err() {
                warn!("Worker channel closed while sending unit.sync_request to '{worker_id}'");
            }
        }
    }
}

/// Emit a JobNew signal for a newly created job (must hold the write lock).
fn emit_job_new(state: &mut AllocatorState, job_id: u64, unit_name: &str, _kind: JobKind) {
    if let Some(ref tx) = state.job_new_tx {
        let _ = tx.send(JobNewInfo {
            job_id,
            unit_name: unit_name.to_string(),
        });
    }
}

/// Emit JobNew without holding the write lock (acquires it briefly).
fn emit_job_new_after_lock(
    allocator: AllocatorHandle,
    job_id: u64,
    unit_name: &str,
    _kind: JobKind,
) {
    let state = allocator.read();
    if let Some(ref tx) = state.job_new_tx {
        let _ = tx.send(JobNewInfo {
            job_id,
            unit_name: unit_name.to_string(),
        });
    }
}

/// Update unit runtime state from a task result received from a worker.
/// Compute which units should receive a propagated `Start` because
/// `unit_name` (a dependency they `BindsTo`) just started successfully.
///
/// Propagation is gated on two conditions:
///   A. `unit_name` must be cached as `active` — a spawn-only success is
///      not enough to trust the binding (systemd only considers the
///      dependency satisfied once the unit is actually active);
///   B. a candidate must not already have an in-flight Start/Restart job,
///      and must not already be cached as `active`/`activating` — this
///      prevents "one more start on top of an in-flight start".
fn binds_to_start_propagation(
    state: &AllocatorState,
    unit_name: &str,
    success: bool,
    kind: JobKind,
) -> Vec<String> {
    if !success || !matches!(kind, JobKind::Start | JobKind::Restart) {
        return Vec::new();
    }

    // Gate A: only propagate when the dependency is genuinely active.
    let dep_active = state
        .unit_states
        .get(unit_name)
        .map(|c| c.active_state == "active")
        .unwrap_or(false);
    if !dep_active {
        debug!(
            "BindsTo start propagation suppressed: dependency {} not cached as active",
            unit_name
        );
        return Vec::new();
    }

    let mut targets = Vec::new();
    for (other_name, other_unit) in &state.units {
        if !other_unit.unit.binds_to.contains(unit_name) {
            continue;
        }

        // Gate B: skip units that already have a running Start/Restart job
        // or are already cached as active/activating.
        let has_running_job = state.jobs.values().any(|j| {
            j.unit_name == *other_name
                && matches!(j.kind, JobKind::Start | JobKind::Restart)
                && matches!(j.status, JobStatus::Running)
        });
        let target_active = state
            .unit_states
            .get(other_name)
            .map(|c| matches!(c.active_state.as_str(), "active" | "activating"))
            .unwrap_or(false);
        if has_running_job || target_active {
            debug!(
                "BindsTo start propagation suppressed: {} already has running job or is active",
                other_name
            );
            continue;
        }

        targets.push(other_name.clone());
    }
    targets
}

pub fn handle_task_result(
    allocator: AllocatorHandle,
    task_id: u64,
    success: bool,
    message: &str,
    unit_name: &str,
    kind: JobKind,
) {
    let mut post_actions: Vec<PostAction> = Vec::new();

    let task_restart_info: Option<(RestartPolicy, ExitKind)> = {
        let mut state = allocator.write();

        // Clean up the task_id → kind mapping.
        state.task_kinds.remove(&task_id);

        // Signal serial chain continuation if this task was part of one.
        if let Some(tx) = state.serial_completion_txs.remove(&task_id) {
            let _ = tx.send(());
        }

        // Clean up invocation_id tracking.
        if !success || kind == JobKind::Stop {
            state.invocation_ids.remove(unit_name);
        }

        // Find and update the associated job.
        let job_id = state
            .jobs
            .values()
            .find(|j| j.unit_name == unit_name && matches!(j.status, JobStatus::Running))
            .map(|j| j.id);

        if let Some(jid) = job_id {
            let result_kind = if success {
                JobResultKind::Done
            } else {
                JobResultKind::Failed
            };
            if let Some(job) = state.jobs.get_mut(&jid) {
                if let Some(abort) = job.timeout_abort.take() {
                    abort.abort();
                }
                job.status = if success {
                    JobStatus::Done
                } else {
                    JobStatus::Failed(message.to_string())
                };
            }
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id: jid,
                    unit_name: unit_name.to_string(),
                    result: result_kind,
                });
            }
        }

        // --- Failure propagation (systemd job_fail_dependencies()) ---
        // On a failed Start job, fail the start jobs of every unit pulling
        // the failed unit in through Requires=/Requisite=/BindsTo=
        // (UNIT_ATOM_PROPAGATE_START_FAILURE).  On a failed Stop job, the
        // same for units listed in the failed unit's Conflicts=
        // (UNIT_ATOM_PROPAGATE_STOP_FAILURE; only the Conflicts reverse
        // edge carries this atom).  Failures cascade recursively; every
        // affected job completes with JobResultKind::Dependency.
        if !success {
            fail_dependents(&mut state, unit_name, kind);
        }

        // --- BindsTo= lifecycle binding (simplified: no runtime check) ---
        if matches!(kind, JobKind::Stop) || !success {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.binds_to.contains(unit_name) {
                    post_actions.push(PostAction::Stop(other_name.clone()));
                }
            }
        }

        // --- PartOf= stop propagation (simplified: no runtime check) ---
        if matches!(kind, JobKind::Stop) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.part_of.contains(unit_name) {
                    post_actions.push(PostAction::Stop(other_name.clone()));
                }
            }
        }

        // --- BindsTo= start propagation (gated: dependency active, target
        // not already in-flight/active) ---
        for name in binds_to_start_propagation(&state, unit_name, success, kind) {
            post_actions.push(PostAction::Start(name));
        }

        // --- PartOf= start propagation (simplified: always propagate) ---
        if success && matches!(kind, JobKind::Start | JobKind::Restart) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.part_of.contains(unit_name) {
                    post_actions.push(PostAction::Start(other_name.clone()));
                }
            }
        }

        // --- OnSuccess= / OnFailure= triggers ---
        if let Some(unit) = state.units.get(unit_name) {
            if success && matches!(kind, JobKind::Start | JobKind::Restart) {
                for target in &unit.unit.on_success {
                    post_actions.push(PostAction::Start(target.clone()));
                }
            }
            if !success {
                for target in &unit.unit.on_failure {
                    post_actions.push(PostAction::Start(target.clone()));
                }
            }
        }

        // --- Upholds= continuous activation (simplified: no runtime check) ---
        if !success || kind == JobKind::Stop {
            for other_unit in state.units.values() {
                if other_unit.unit.upholds.contains(unit_name) {
                    post_actions.push(PostAction::Start(unit_name.to_string()));
                    break;
                }
            }
        }

        // --- Restart policy: determine exit kind and check if restart needed ---
        let restart_info_inner: Option<(RestartPolicy, ExitKind)> =
            if !success && matches!(kind, JobKind::Start | JobKind::Restart) {
                let exit_kind = if message.contains("Timeout") {
                    ExitKind::Timeout
                } else if message.contains("Watchdog") || message.contains("watchdog") {
                    ExitKind::Watchdog
                } else if message.contains("Signal") || message.contains("signal") {
                    ExitKind::Signal(-1)
                } else if let Some(code_str) = message.to_lowercase().split("exit code").nth(1) {
                    let code = code_str
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<i32>().ok())
                        .unwrap_or(1);
                    ExitKind::ExitCode(code)
                } else {
                    ExitKind::ExitCode(1)
                };
                state
                    .units
                    .get(unit_name)
                    .and_then(|u| u.service.as_ref())
                    .map(|svc| (svc.restart.clone(), exit_kind))
            } else {
                None
            };
        restart_info_inner
    }; // drop write lock

    // Execute post-actions asynchronously.
    if !post_actions.is_empty() {
        let alloc = allocator.clone();
        tokio::spawn(async move {
            for action in post_actions {
                match action {
                    PostAction::Stop(name) => {
                        info!("Propagating stop to {}", name);
                        if let Err(e) =
                            enqueue_job(alloc.clone(), &name, JobKind::Stop, JobMode::Replace).await
                        {
                            warn!("Failed to propagate stop to {}: {}", name, e);
                        }
                    }
                    PostAction::Start(name) => {
                        info!("Triggering start for {}", name);
                        if let Err(e) =
                            enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace)
                                .await
                        {
                            warn!("Failed to trigger start for {}: {}", name, e);
                        }
                    }
                }
            }
        });
    }

    // Schedule restart if the restart policy triggered.
    if let Some((ref policy, ref exit_kind)) = task_restart_info {
        if should_restart_service(policy, exit_kind) {
            schedule_automatic_restart(allocator.clone(), unit_name);
        }
    }
}

/// Fail the running start jobs of every unit depending on `failed` through
/// the propagation atom implied by `kind`, mirroring systemd's
/// `job_fail_dependencies()`:
///
/// - `JobKind::Start` → `UNIT_ATOM_PROPAGATE_START_FAILURE`: units that
///   list `failed` in Requires=/Requisite=/BindsTo=;
/// - `JobKind::Stop` → `UNIT_ATOM_PROPAGATE_STOP_FAILURE`: units listed in
///   `failed`'s Conflicts=.
///
/// Only start jobs are affected (systemd restricts to JOB_START /
/// JOB_VERIFY_ACTIVE).  Each affected job is completed with
/// `JobResultKind::Dependency` and the failure cascades recursively.
fn fail_dependents(state: &mut AllocatorState, failed: &str, kind: JobKind) {
    let is_start = matches!(kind, JobKind::Start);
    let is_stop = matches!(kind, JobKind::Stop);
    if !is_start && !is_stop {
        return;
    }
    let mut queue = std::collections::VecDeque::new();
    let mut visited = std::collections::HashSet::new();
    queue.push_back(failed.to_string());
    while let Some(name) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let dependents: Vec<String> = state
            .units
            .iter()
            .filter(|(n, u)| {
                let connected = if is_start {
                    u.unit.requires.contains(&name)
                        || u.unit.requisite.contains(&name)
                        || u.unit.binds_to.contains(&name)
                } else {
                    u.unit.conflicts.contains(&name)
                };
                connected
                    && state.jobs.values().any(|j| {
                        j.unit_name == **n
                            && j.kind == JobKind::Start
                            && matches!(j.status, JobStatus::Running)
                    })
            })
            .map(|(n, _)| n.clone())
            .collect();
        for dep in dependents {
            let jid = state
                .jobs
                .values()
                .find(|j| j.unit_name == dep && matches!(j.status, JobStatus::Running))
                .map(|j| j.id);
            if let Some(jid) = jid {
                if let Some(job) = state.jobs.get_mut(&jid) {
                    if let Some(abort) = job.timeout_abort.take() {
                        abort.abort();
                    }
                    job.status = JobStatus::Failed(format!("dependency failed: {name}"));
                }
                if let Some(ref tx) = state.job_completion_tx {
                    let _ = tx.send(JobCompletion {
                        job_id: jid,
                        unit_name: dep.clone(),
                        result: JobResultKind::Dependency,
                    });
                }
                queue.push_back(dep);
            }
        }
    }
}

/// Post-processing actions to be taken after a task result is handled.
enum PostAction {
    Stop(String),
    Start(String),
}

fn build_unit_config(uf: &UnitFile, all_units: &HashMap<String, UnitFile>) -> UnitConfig {
    let service = uf.service.as_ref().map(|svc| ServiceConfig {
        // Only the first ExecStart command is sent to the worker.
        // Multiple ExecStart directives (Type=oneshot) will be supported in Phase 2.
        exec_start: svc
            .exec_start
            .first()
            .map(|c| c.raw.clone())
            .unwrap_or_default(),
        exec_stop: svc
            .exec_stop
            .first()
            .map(|c| c.raw.clone())
            .unwrap_or_default(),
        exec_reload: svc
            .exec_reload
            .first()
            .map(|c| c.raw.clone())
            .unwrap_or_default(),
        working_directory: svc.working_directory.clone(),
        user: svc.user.clone(),
        group: svc.group.clone(),
        environment: svc.environment.clone(),
        restart_policy: svc.restart.as_str().to_string(),
        restart_delay_secs: svc.restart_sec,
        service_type: svc.service_type.as_str().to_string(),
        pid_file: svc.pid_file.clone(),
        timeout_start_secs: svc.timeout_start_sec,
        timeout_stop_secs: svc.timeout_stop_sec,
        standard_input: svc.standard_input.clone(),
        standard_output: svc.standard_output.clone(),
        standard_error: svc.standard_error.clone(),
        tty_path: svc.tty_path.clone(),
    });

    let socket = uf.socket.as_ref().map(|sk| {
        let mut listen: Vec<SocketAddress> = Vec::new();
        for addr in &sk.listen_stream {
            listen.push(SocketAddress {
                stream: addr.clone(),
                ..Default::default()
            });
        }
        for addr in &sk.listen_datagram {
            listen.push(SocketAddress {
                datagram: addr.clone(),
                ..Default::default()
            });
        }
        for addr in &sk.listen_sequential_packet {
            listen.push(SocketAddress {
                sequential_packet: addr.clone(),
                ..Default::default()
            });
        }
        for addr in &sk.listen_fifo {
            listen.push(SocketAddress {
                fifo: addr.clone(),
                ..Default::default()
            });
        }
        // Derive the associated service name per systemd convention:
        // "foo.socket" -> "foo.service".
        let svc_name = uf.name.replace(".socket", ".service");

        SocketConfig {
            listen,
            accept: sk.accept,
            backlog: sk.backlog,
            socket_mode: sk.socket_mode.clone(),
            socket_user: sk.socket_user.clone(),
            socket_group: sk.socket_group.clone(),
            service: svc_name,
        }
    });

    // For automount units, preload the companion `.mount` unit's config so
    // the worker can satisfy kernel trigger requests locally (scheme A).
    // The companion's [Mount] section wins if the automount unit itself
    // carries one (it normally does not).
    let mount = if uf.automount.is_some() && uf.mount.is_none() {
        let mount_name = format!("{}.mount", uf.name.trim_end_matches(".automount"));
        all_units
            .get(&mount_name)
            .and_then(|muf| muf.mount.as_ref())
            .map(mount_config_from_section)
    } else {
        uf.mount.as_ref().map(mount_config_from_section)
    };

    let automount = uf.automount.as_ref().map(|a| AutomountConfig {
        r#where: a.where_.clone(),
        extra_options: a.extra_options.clone(),
        timeout_idle_sec: a.timeout_idle_sec,
        directory_mode: a.directory_mode.clone(),
    });

    let timer = uf.timer.as_ref().map(|t| TimerConfig {
        on_active_sec: t.on_active_sec,
        on_boot_sec: t.on_boot_sec,
        on_startup_sec: t.on_startup_sec,
        on_unit_active_sec: t.on_unit_active_sec,
        on_unit_inactive_sec: t.on_unit_inactive_sec,
        on_calendar: t.on_calendar.clone(),
        accuracy_sec: t.accuracy_sec,
        randomized_delay_sec: t.randomized_delay_sec,
        unit: t.unit.clone(),
        persistent: t.persistent,
    });

    let device = uf.device.as_ref().map(|d| DeviceConfig {
        device_name: d.device_name.clone(),
        device_path: d.device_path.clone(),
        sysfs_path: d.sysfs_path.clone(),
        property: d.property.clone(),
    });

    let path = uf.path.as_ref().map(|p| PathConfig {
        path_exists: p.path_exists.clone(),
        path_exists_glob: p.path_exists_glob.clone(),
        path_changed: p.path_changed.clone(),
        path_modified: p.path_modified.clone(),
        directory_not_empty: p.directory_not_empty.clone(),
        unit: p.unit.clone(),
        make_directory: p.make_directory,
        directory_mode: p.directory_mode.clone(),
        trigger_limit_interval_sec: p.trigger_limit_interval_sec,
        trigger_limit_burst: p.trigger_limit_burst,
    });

    let scope = uf.scope.as_ref().map(|s| ScopeConfig {
        pids: s
            .pids
            .iter()
            .filter_map(|p| p.trim().parse::<u32>().ok())
            .collect(),
        timeout_stop_secs: s.timeout_stop_sec,
        runtime_max_secs: s.runtime_max_sec,
        kill_signal: s.kill_signal.clone(),
        send_sighup: s.send_sighup,
        controller: String::new(),
        slice: uf.unit.slice.clone(),
    });

    UnitConfig {
        unit_name: uf.name.clone(),
        description: uf.unit.description.clone(),
        service,
        socket,
        mount,
        automount,
        timer,
        device,
        path,
        scope,
    }
}

fn mount_config_from_section(m: &MountSection) -> MountConfig {
    MountConfig {
        what: m.what.clone(),
        r#where: m.where_.clone(),
        r#type: m.type_.clone(),
        options: m.options.clone(),
        timeout_sec: m.timeout_sec,
        lazy_unmount: m.lazy_unmount,
        force_unmount: m.force_unmount,
        directory_mode: m.directory_mode.clone(),
        sloppy_options: m.sloppy_options,
    }
}

// ---------------------------------------------------------------------------
// Job timeout helpers
// ---------------------------------------------------------------------------

/// Spawn a timeout task for a job, returning an `AbortHandle` that can be used
/// to cancel the timeout if the job completes normally.
///
/// Start/Restart jobs use `TimeoutStartSec` (start timeout) plus a longer
/// job-running watchdog.  Stop jobs use `TimeoutStopSec`.  Reload jobs use a
/// default 60 s timeout.
fn spawn_job_timeout(
    allocator: AllocatorHandle,
    job_id: u64,
    name: &str,
    kind: JobKind,
    unit_file: &Option<UnitFile>,
) -> Option<AbortHandle> {
    let svc = unit_file.as_ref().and_then(|u| u.service.as_ref());

    match kind {
        JobKind::Start | JobKind::Restart => {
            let start_timeout = svc.and_then(|s| {
                let t = s.timeout_start_sec;
                if t > 0 {
                    Some(t as u64)
                } else {
                    None
                }
            });
            start_timeout?;
            let start_secs = start_timeout.unwrap();
            let alloc = allocator.clone();
            let name_clone = name.to_string();
            let handle = tokio::spawn(async move {
                // Start timeout
                tokio::time::sleep(Duration::from_secs(start_secs)).await;
                let mut state = alloc.write();
                if let Some(job) = state.jobs.get_mut(&job_id) {
                    if matches!(job.status, JobStatus::Running) {
                        job.status = JobStatus::Failed("TimeoutStartSec exceeded".to_string());
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id,
                                unit_name: name_clone,
                                result: JobResultKind::Timeout,
                            });
                        }
                    }
                }
            })
            .abort_handle();
            Some(handle)
        }
        JobKind::Stop => {
            let stop_secs = svc
                .and_then(|s| {
                    let t = s.timeout_stop_sec;
                    if t > 0 {
                        Some(t as u64)
                    } else {
                        None
                    }
                })
                .unwrap_or(30);
            let alloc = allocator.clone();
            let name_clone = name.to_string();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(stop_secs)).await;
                let mut state = alloc.write();
                if let Some(job) = state.jobs.get_mut(&job_id) {
                    if matches!(job.status, JobStatus::Running) {
                        job.status = JobStatus::Failed("TimeoutStopSec exceeded".to_string());
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id,
                                unit_name: name_clone,
                                result: JobResultKind::Timeout,
                            });
                        }
                    }
                }
            })
            .abort_handle();
            Some(handle)
        }
        JobKind::Reload => {
            // Reload timeout: default 60 seconds.
            let reload_secs: u64 = 60;
            let alloc = allocator.clone();
            let name_clone = name.to_string();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(reload_secs)).await;
                let mut state = alloc.write();
                if let Some(job) = state.jobs.get_mut(&job_id) {
                    if matches!(job.status, JobStatus::Running) {
                        job.status = JobStatus::Failed("Reload timeout exceeded".to_string());
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id,
                                unit_name: name_clone,
                                result: JobResultKind::Timeout,
                            });
                        }
                    }
                }
            })
            .abort_handle();
            Some(handle)
        }
        // Nop jobs complete immediately and are never timed out; keep the
        // match exhaustive defensively.
        JobKind::Nop => None,
    }
}

// ---------------------------------------------------------------------------
// Unified restart-policy helpers
// ---------------------------------------------------------------------------

/// Determine whether a service should be restarted based on its `RestartPolicy`
/// and how it exited.  Mirrors systemd's behaviour table:
///
/// | Policy       | ExitCode 0 | ExitCode != 0 | Signal | Timeout | Watchdog |
/// |--------------|-----------|---------------|--------|---------|----------|
/// | no           |     ✗     |       ✗       |   ✗    |    ✗    |    ✗     |
/// | on-success   |     ✓     |       ✗       |   ✗    |    ✗    |    ✗     |
/// | on-failure   |     ✗     |       ✓       |   ✓    |    ✓    |    ✗     |
/// | on-abnormal  |     ✗     |       ✗       |   ✓    |    ✓    |    ✗     |
/// | on-watchdog  |     ✗     |       ✗       |   ✗    |    ✗    |    ✓     |
/// | on-abort     |     ✗     |       ✗       |   ✓    |    ✗    |    ✗     |
/// | always       |     ✓     |       ✓       |   ✓    |    ✓    |    ✓     |
pub fn should_restart_service(policy: &RestartPolicy, exit_kind: &ExitKind) -> bool {
    use ExitKind::*;
    match policy {
        RestartPolicy::No => false,
        RestartPolicy::Always => true,
        RestartPolicy::OnSuccess => matches!(exit_kind, ExitCode(0)),
        RestartPolicy::OnFailure => {
            matches!(exit_kind, ExitCode(c) if *c != 0) || matches!(exit_kind, Signal(_) | Timeout)
        }
        RestartPolicy::OnAbnormal => matches!(exit_kind, Signal(_) | Timeout),
        RestartPolicy::OnWatchdog => matches!(exit_kind, Watchdog),
        RestartPolicy::OnAbort => matches!(exit_kind, Signal(_)),
    }
}

/// Schedule an automatic restart of `unit_name` after its `RestartSec`.
///
/// The start rate limit (`StartLimitIntervalSec=` / `StartLimitBurst=`) is
/// enforced centrally in [`enqueue_job`], so every start attempt — manual,
/// auto-restart, or dependency-triggered — counts against it.  This function
/// only sleeps the configured `RestartSec` and then enqueues a `Start` job.
pub fn schedule_automatic_restart(allocator: AllocatorHandle, unit_name: &str) {
    let restart_sec = {
        let state = allocator.read();
        state
            .units
            .get(unit_name)
            .and_then(|u| u.service.as_ref())
            .map(|s| s.restart_sec as u64)
            .unwrap_or(0)
    };

    let alloc = allocator.clone();
    let name = unit_name.to_string();
    tokio::spawn(async move {
        if restart_sec > 0 {
            tokio::time::sleep(Duration::from_secs(restart_sec)).await;
        }
        info!("Auto-restarting {} after exit/failure", name);
        if let Err(e) = enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace).await {
            warn!("Failed to auto-restart {}: {}", name, e);
        }
    });
}

/// Execute the `StartLimitAction=` configured for a unit whose start rate
/// limit was exceeded (mirrors systemd's `unit_start_limit_action()`).
fn execute_start_limit_action(action: &StartLimitAction, unit_name: &str) {
    match action {
        StartLimitAction::None => {}
        StartLimitAction::Reboot
        | StartLimitAction::RebootForce
        | StartLimitAction::RebootImmediate => {
            warn!("StartLimitAction={:?} for {}: rebooting system", action, unit_name);
            let _ = std::process::Command::new("shutdown")
                .args(["-r", "now", "StartLimitAction triggered by systema"])
                .spawn();
        }
        StartLimitAction::Poweroff => {
            warn!("StartLimitAction=poweroff for {}: powering off system", unit_name);
            let _ = std::process::Command::new("shutdown")
                .args(["-P", "now", "StartLimitAction triggered by systema"])
                .spawn();
        }
        StartLimitAction::Exit => {
            warn!("StartLimitAction=exit for {} (no-op, logging only)", unit_name);
        }
    }
}

// ---------------------------------------------------------------------------
// Condition and assert evaluation
// ---------------------------------------------------------------------------

/// Evaluate all `Condition*=` directives in `unit`.
///
/// Returns `true` if all conditions pass (unit should start), `false` if any
/// condition fails (unit should be silently skipped, staying inactive).
///
/// A value prefixed with `!` negates the check.
fn check_conditions(unit: &UnitSection) -> bool {
    for path in &unit.condition_path_exists {
        if !eval_condition_bool(path, |p| std::path::Path::new(p).exists()) {
            return false;
        }
    }
    for glob in &unit.condition_path_exists_glob {
        if !eval_condition_bool(glob, path_glob_matches) {
            return false;
        }
    }
    for path in &unit.condition_file_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::metadata(p).map(|m| m.len() != 0).unwrap_or(false)
        }) {
            return false;
        }
    }
    for path in &unit.condition_directory_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::read_dir(p)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        }) {
            return false;
        }
    }
    for spec in &unit.condition_ac_power {
        let (negate, value) = strip_negate(spec);
        let on_ac = is_on_ac_power();
        let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
        if (on_ac != want) != negate {
            return false;
        }
    }
    // ConditionFirstBoot=yes passes only on the first boot.
    for spec in &unit.condition_first_boot {
        let (negate, value) = strip_negate(spec);
        let first = is_first_boot();
        let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
        if (first != want) != negate {
            return false;
        }
    }
    true
}

/// Evaluate all `Assert*=` directives in `unit`.
///
/// Returns `true` if all asserts pass, `false` if any assert fails (unit
/// should be marked failed, not just skipped).
fn check_asserts(unit: &UnitSection) -> bool {
    for path in &unit.assert_path_exists {
        if !eval_condition_bool(path, |p| std::path::Path::new(p).exists()) {
            return false;
        }
    }
    for glob in &unit.assert_path_exists_glob {
        if !eval_condition_bool(glob, path_glob_matches) {
            return false;
        }
    }
    for path in &unit.assert_file_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::metadata(p).map(|m| m.len() != 0).unwrap_or(false)
        }) {
            return false;
        }
    }
    for path in &unit.assert_directory_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::read_dir(p)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        }) {
            return false;
        }
    }
    for spec in &unit.assert_first_boot {
        let (negate, value) = strip_negate(spec);
        let first = is_first_boot();
        let want = matches!(value.to_lowercase().as_str(), "yes" | "true" | "1");
        if (first != want) != negate {
            return false;
        }
    }
    true
}

/// Strip a leading `!` from `spec`, returning `(negated, rest)`.
fn strip_negate(spec: &str) -> (bool, &str) {
    if let Some(rest) = spec.strip_prefix('!') {
        (true, rest)
    } else {
        (false, spec)
    }
}

/// Evaluate a single condition string against a predicate.
///
/// If the spec starts with `!`, the result is negated.
fn eval_condition_bool<F: Fn(&str) -> bool>(spec: &str, pred: F) -> bool {
    let (negate, path) = strip_negate(spec);
    let result = pred(path);
    if negate {
        !result
    } else {
        result
    }
}

/// Check if any filesystem path matches a simple glob pattern.
///
/// Uses the same glob logic as the rest of systema (no external crate).
fn path_glob_matches(pattern: &str) -> bool {
    // Split into directory and file-name glob parts.
    let (dir, file_pattern) = match pattern.rfind('/') {
        Some(pos) => (&pattern[..pos], &pattern[pos + 1..]),
        None => (".", pattern),
    };
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| simple_glob_match(file_pattern, n))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Minimal shell-style glob: `*` matches any sequence, `?` matches any char.
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_impl(&pat, &txt)
}

fn glob_match_impl(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some(&'*'), _) => (0..=txt.len()).any(|i| glob_match_impl(&pat[1..], &txt[i..])),
        (Some(&'?'), Some(_)) => glob_match_impl(&pat[1..], &txt[1..]),
        (Some(p), Some(t)) if p == t => glob_match_impl(&pat[1..], &txt[1..]),
        _ => false,
    }
}

/// Returns `true` if the system appears to be running on AC power.
/// Best-effort: returns `true` (assume AC) if the check cannot be performed.
fn is_on_ac_power() -> bool {
    // Linux: check /sys/class/power_supply/*/online
    let path = std::path::Path::new("/sys/class/power_supply");
    if !path.exists() {
        return true; // assume AC if sysfs is unavailable
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                let online = e.path().join("online");
                std::fs::read_to_string(&online)
                    .map(|s| s.trim() == "1")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(true)
}

/// Returns `true` if this appears to be the first boot of the system.
/// Heuristic: `/run/systemd/first-boot` or `/run/machine-id` does not exist.
fn is_first_boot() -> bool {
    std::path::Path::new(sysa::paths::instance().systemd_first_boot_file).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CachedUnitState, StartLimitState, WorkerEntry};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    // =========================================================================
    // JobMode tests
    // =========================================================================

    #[test]
    fn test_job_mode_from_str_unknown_is_none() {
        assert_eq!(JobMode::from_str("unknown"), None);
        assert_eq!(JobMode::from_str(""), None);
        assert_eq!(JobMode::from_str("REPLACE"), None);
    }

    #[test]
    fn test_job_mode_from_str_all() {
        assert_eq!(JobMode::from_str("replace"), Some(JobMode::Replace));
        assert_eq!(JobMode::from_str("fail"), Some(JobMode::Fail));
        assert_eq!(JobMode::from_str("lenient"), Some(JobMode::Lenient));
        assert_eq!(JobMode::from_str("queue"), Some(JobMode::Queue));
        assert_eq!(JobMode::from_str("isolate"), Some(JobMode::Isolate));
        assert_eq!(JobMode::from_str("flush"), Some(JobMode::Flush));
        assert_eq!(
            JobMode::from_str("replace-irreversibly"),
            Some(JobMode::ReplaceIrreversibly)
        );
        assert_eq!(
            JobMode::from_str("ignore-dependencies"),
            Some(JobMode::IgnoreDependencies)
        );
        assert_eq!(
            JobMode::from_str("ignore-requirements"),
            Some(JobMode::IgnoreRequirements)
        );
        assert_eq!(JobMode::from_str("triggering"), Some(JobMode::Triggering));
        assert_eq!(
            JobMode::from_str("restart-dependencies"),
            Some(JobMode::RestartDependencies)
        );
    }

    #[test]
    fn test_job_mode_as_str_roundtrip() {
        for mode in &[
            JobMode::Replace,
            JobMode::Fail,
            JobMode::Lenient,
            JobMode::Queue,
            JobMode::Isolate,
            JobMode::Flush,
            JobMode::ReplaceIrreversibly,
            JobMode::IgnoreDependencies,
            JobMode::IgnoreRequirements,
            JobMode::Triggering,
            JobMode::RestartDependencies,
        ] {
            assert!(matches!(
                mode,
                JobMode::Replace
                    | JobMode::Fail
                    | JobMode::Lenient
                    | JobMode::Queue
                    | JobMode::Isolate
                    | JobMode::Flush
                    | JobMode::ReplaceIrreversibly
                    | JobMode::IgnoreDependencies
                    | JobMode::IgnoreRequirements
                    | JobMode::Triggering
                    | JobMode::RestartDependencies
            ));
        }
    }

    #[test]
    fn test_job_mode_equality() {
        assert_eq!(JobMode::Replace, JobMode::Replace);
        assert_ne!(JobMode::Replace, JobMode::Fail);
        assert_ne!(JobMode::Isolate, JobMode::Flush);
    }

    // =========================================================================
    // StartLimitState tests
    // =========================================================================

    #[test]
    fn test_start_limit_state_allows_first_attempt() {
        let mut state = StartLimitState::new();
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
    }

    #[test]
    fn test_start_limit_state_within_burst() {
        let mut state = StartLimitState::new();
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
    }

    #[test]
    fn test_start_limit_state_exceeds_burst() {
        let mut state = StartLimitState::new();
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        assert!(state.check_rate_limit(Duration::from_secs(10), 3));
        // The 4th attempt should be rate-limited
        assert!(!state.check_rate_limit(Duration::from_secs(10), 3));
    }

    #[test]
    fn test_start_limit_state_prunes_old_timestamps() {
        let mut state = StartLimitState::new();
        // Add some timestamps far in the past
        state
            .timestamps
            .push(std::time::Instant::now() - Duration::from_secs(100));
        state
            .timestamps
            .push(std::time::Instant::now() - Duration::from_secs(100));
        // With short interval, they should be pruned
        assert!(state.check_rate_limit(Duration::from_secs(1), 5));
        assert_eq!(state.timestamps.len(), 1); // only the new one remains
    }

    #[test]
    fn test_start_limit_zero_interval_disables_rate_limiting() {
        // systemd: StartLimitIntervalSec=0 disables rate limiting.
        let mut state = StartLimitState::new();
        for _ in 0..100 {
            assert!(state.check_rate_limit(Duration::from_secs(0), 5));
        }
        // Disabled limiting must not record timestamps.
        assert!(state.timestamps.is_empty());
    }

    #[test]
    fn test_start_limit_zero_burst_disables_rate_limiting() {
        // systemd: StartLimitBurst=0 disables rate limiting.
        let mut state = StartLimitState::new();
        for _ in 0..100 {
            assert!(state.check_rate_limit(Duration::from_secs(10), 0));
        }
        assert!(state.timestamps.is_empty());
    }

    // =========================================================================
    // Unit helper tests
    // =========================================================================

    fn make_unit(name: &str) -> UnitFile {
        UnitFile::new(name)
    }

    #[test]
    fn test_strip_negate_normal() {
        let (neg, val) = strip_negate("/some/path");
        assert!(!neg);
        assert_eq!(val, "/some/path");
    }

    #[test]
    fn test_strip_negate_negated() {
        let (neg, val) = strip_negate("!/some/path");
        assert!(neg);
        assert_eq!(val, "/some/path");
    }

    #[test]
    fn test_eval_condition_bool_normal() {
        assert!(eval_condition_bool("true", |_| true));
        assert!(!eval_condition_bool("false", |_| false));
    }

    #[test]
    fn test_eval_condition_bool_negated() {
        assert!(eval_condition_bool("!false", |_| false));
        assert!(!eval_condition_bool("!true", |_| true));
    }

    #[test]
    fn test_simple_glob_match_exact() {
        assert!(simple_glob_match("foo.service", "foo.service"));
        assert!(!simple_glob_match("foo.service", "bar.service"));
    }

    #[test]
    fn test_simple_glob_match_wildcard() {
        assert!(simple_glob_match("*.service", "foo.service"));
        assert!(simple_glob_match("foo.*", "foo.service"));
        assert!(!simple_glob_match("*.service", "foo.txt"));
    }

    #[test]
    fn test_simple_glob_match_question_mark() {
        assert!(simple_glob_match("foo.???????", "foo.service"));
        assert!(!simple_glob_match("foo.??????", "foo.service"));
    }

    #[test]
    fn test_simple_glob_match_empty() {
        assert!(simple_glob_match("", ""));
        assert!(!simple_glob_match("", "foo"));
        assert!(!simple_glob_match("foo", ""));
    }

    // =========================================================================
    // check_conditions / check_asserts tests
    // =========================================================================

    #[test]
    fn test_check_conditions_no_conditions() {
        let unit = UnitSection::default();
        assert!(check_conditions(&unit));
    }

    #[test]
    fn test_check_asserts_no_asserts() {
        let unit = UnitSection::default();
        assert!(check_asserts(&unit));
    }

    // =========================================================================
    // build_unit_config tests
    // =========================================================================

    #[test]
    fn test_build_unit_config_service() {
        let uf = make_unit("test.service");
        let mut uf = uf;
        uf.service = Some(crate::unit::types::ServiceSection::default());
        let config = build_unit_config(&uf, &HashMap::new());
        assert!(config.service.is_some());
    }

    #[test]
    fn test_build_unit_config_automount_preloads_companion_mount() {
        let mut auto = make_unit("mnt-data.automount");
        auto.automount = Some(crate::unit::types::AutomountSection {
            where_: "/mnt/data".to_string(),
            timeout_idle_sec: 60,
            ..Default::default()
        });
        let mut mount = make_unit("mnt-data.mount");
        mount.mount = Some(crate::unit::types::MountSection {
            what: "/dev/sdb1".to_string(),
            where_: "/mnt/data".to_string(),
            type_: "ext4".to_string(),
            options: "defaults".to_string(),
            ..Default::default()
        });
        let mut units = HashMap::new();
        units.insert(mount.name.clone(), mount);
        let config = build_unit_config(&auto, &units);
        let mount_cfg = config.mount.expect("companion mount config preloaded");
        assert_eq!(mount_cfg.what, "/dev/sdb1");
        assert_eq!(mount_cfg.r#where, "/mnt/data");
        assert_eq!(mount_cfg.r#type, "ext4");
    }

    // =========================================================================
    // Job conflict detection logic
    // =========================================================================

    #[test]
    fn test_job_mode_from_str_is_idempotent() {
        for s in &[
            "replace",
            "fail",
            "lenient",
            "queue",
            "isolate",
            "flush",
            "replace-irreversibly",
            "ignore-dependencies",
            "ignore-requirements",
            "triggering",
            "restart-dependencies",
        ] {
            let mode = JobMode::from_str(s);
            let mode2 = JobMode::from_str(s);
            assert_eq!(mode, mode2);
        }
    }

    // =========================================================================
    // Mode validation (check_mode_constraints)
    // =========================================================================

    #[test]
    fn test_check_mode_constraints_triggering_is_stop_only() {
        assert!(check_mode_constraints(JobMode::Triggering, JobKind::Stop, "x.service", false).is_ok());
        for kind in [JobKind::Start, JobKind::Restart, JobKind::Reload] {
            assert!(check_mode_constraints(JobMode::Triggering, kind, "x.service", false).is_err());
        }
    }

    #[test]
    fn test_check_mode_constraints_restart_dependencies_is_start_only() {
        assert!(
            check_mode_constraints(JobMode::RestartDependencies, JobKind::Start, "x.service", false)
                .is_ok()
        );
        for kind in [JobKind::Stop, JobKind::Restart, JobKind::Reload] {
            assert!(
                check_mode_constraints(JobMode::RestartDependencies, kind, "x.service", false)
                    .is_err()
            );
        }
    }

    #[test]
    fn test_check_mode_constraints_isolate_requires_allow_isolate() {
        assert!(check_mode_constraints(JobMode::Isolate, JobKind::Start, "x.service", false).is_err());
        assert!(check_mode_constraints(JobMode::Isolate, JobKind::Start, "x.service", true).is_ok());
    }

    #[test]
    fn test_check_mode_constraints_plain_modes_always_pass() {
        for mode in [JobMode::Replace, JobMode::Flush, JobMode::Queue, JobMode::Lenient] {
            for kind in [JobKind::Start, JobKind::Stop, JobKind::Restart, JobKind::Reload] {
                assert!(check_mode_constraints(mode, kind, "x.service", false).is_ok());
            }
        }
    }

    // =========================================================================
    // enqueue_job_type: state-dependent collapse (step E)
    // =========================================================================

    fn alloc_with_state(active_state: &str) -> AllocatorHandle {
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        alloc
            .write()
            .units
            .insert("demo.service".to_string(), make_unit("demo.service"));
        alloc.write().unit_states.insert(
            "demo.service".to_string(),
            CachedUnitState {
                active_state: active_state.to_string(),
                sub_state: String::new(),
                main_pid: 0,
                invocation_id: String::new(),
                active_enter_timestamp: 0,
                inactive_enter_timestamp: 0,
                extensions: HashMap::new(),
                pids: Vec::new(),
                controller: String::new(),
            },
        );
        alloc
    }

    /// Register a fake worker for the "service" unit type.
    fn register_service_worker(state: &mut AllocatorState) {
        let (tx, _rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
        state.workers.insert(
            "test-worker".to_string(),
            WorkerEntry {
                worker_id: "test-worker".to_string(),
                unit_types: vec!["service".to_string()],
                envelope_tx: tx,
            },
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_try_restart_of_inactive_unit_is_nop() {
        // try-restart of an inactive unit collapses to Nop: the job is
        // recorded and completes as done without touching any worker
        // (systemd: JOB_NOP finishes immediately with JOB_DONE).
        let alloc = alloc_with_state("inactive");
        let (job_id, kind) =
            enqueue_job_type(alloc.clone(), "demo.service", JobType::TryRestart, false, JobMode::Replace)
                .await
                .unwrap();
        assert_eq!(kind, JobKind::Nop);
        let state = alloc.read();
        let job = state.jobs.get(&job_id).expect("nop job recorded");
        assert_eq!(job.kind, JobKind::Nop);
        assert_eq!(job.status, JobStatus::Done);
        // No worker interaction: nothing was dispatched.
        assert!(state.task_kinds.is_empty());
    }

    // =========================================================================
    // activate_transient_unit (StartTransientUnit support)
    // =========================================================================

    #[tokio::test]
    async fn test_activate_transient_scope_requires_scope_worker() {
        // A transient scope is dispatched through the normal job machinery
        // to the System E worker.  Without a registered scope worker the
        // activation must fail ("No worker available"), unlike transient
        // slices which still take the instantaneous fast path.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut uf = UnitFile::new("session-1.scope");
            uf.transient = true;
            alloc.write().units.insert("session-1.scope".to_string(), uf);
        }

        let err = activate_transient_unit(alloc.clone(), "session-1.scope", JobMode::Replace)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No worker available"), "{}", err);

        let state = alloc.read();
        assert!(state.jobs.is_empty());
        assert!(!state.unit_states.contains_key("session-1.scope"));
    }

    #[tokio::test]
    async fn test_activate_transient_scope_dispatches_to_worker() {
        // With a registered scope worker, activating a transient scope
        // enqueues a real Start job and dispatches a task to the worker;
        // the job stays Running until the worker reports back.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut uf = UnitFile::new("session-1.scope");
            uf.transient = true;
            alloc.write().units.insert("session-1.scope".to_string(), uf);
        }
        {
            let mut state = alloc.write();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            // Keep the worker side alive (drain incoming envelopes) so the
            // dispatch never sees a "Worker disconnected" failure.
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            state.workers.insert(
                "system-e-1".to_string(),
                WorkerEntry {
                    worker_id: "system-e-1".to_string(),
                    unit_types: vec!["scope".to_string()],
                    envelope_tx: tx,
                },
            );
        }

        let job_id = activate_transient_unit(alloc.clone(), "session-1.scope", JobMode::Replace)
            .await
            .expect("scope activation dispatches to worker");

        let state = alloc.read();
        let job = state.jobs.get(&job_id).expect("job recorded");
        assert_eq!(job.kind, JobKind::Start);
        assert_eq!(job.status, JobStatus::Running);
        // A task was dispatched to the worker (task_kinds holds the mapping).
        assert!(!state.task_kinds.is_empty());
        // Desired state is committed only when the worker reports back.
        assert_eq!(state.desired.get("session-1.scope"), None);
    }

    #[tokio::test]
    async fn test_activate_transient_slice_still_fast_path() {
        // Transient slices (and other non-scope transient units) keep the
        // instantaneous fast path: marked active, job done, no worker.
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        {
            let mut uf = UnitFile::new("test.slice");
            uf.transient = true;
            alloc.write().units.insert("test.slice".to_string(), uf);
        }

        let job_id = activate_transient_unit(alloc.clone(), "test.slice", JobMode::Replace)
            .await
            .expect("transient slice activation succeeds");

        let state = alloc.read();
        let job = state.jobs.get(&job_id).expect("job recorded");
        assert_eq!(job.kind, JobKind::Start);
        assert_eq!(job.status, JobStatus::Done);
        let cached = state
            .unit_states
            .get("test.slice")
            .expect("unit marked active");
        assert_eq!(cached.active_state, "active");
        assert_eq!(cached.sub_state, "running");
        // No worker interaction: nothing was dispatched.
        assert!(state.task_kinds.is_empty());
    }

    #[tokio::test]
    async fn test_activate_transient_unit_rejects_unknown_unit() {
        let alloc = Arc::new(parking_lot::RwLock::new(AllocatorState::new()));
        let err = activate_transient_unit(alloc.clone(), "missing.scope", JobMode::Replace)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not loaded"));
    }

    #[tokio::test]
    async fn test_enqueue_job_type_try_reload_of_failed_unit_is_nop() {
        let alloc = alloc_with_state("failed");
        let (job_id, kind) =
            enqueue_job_type(alloc.clone(), "demo.service", JobType::TryReload, false, JobMode::Replace)
                .await
                .unwrap();
        assert_eq!(kind, JobKind::Nop);
        assert!(matches!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        ));
    }

    #[tokio::test]
    async fn test_enqueue_job_type_try_restart_works_without_worker() {
        // The collapsed-nop path must succeed even when no worker is
        // registered (it never dispatches).
        let alloc = alloc_with_state("inactive");
        let (job_id, _) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::TryRestart,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_unknown_state_keeps_try_restart() {
        // Unknown state is conservative: try-restart collapses to restart,
        // which needs a worker — and fails without one.
        let alloc = alloc_with_state("unmapped-state");
        let res = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::TryRestart,
            false,
            JobMode::Replace,
        )
        .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_enqueue_job_type_active_try_restart_dispatches_restart() {
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());
        let (job_id, kind) =
            enqueue_job_type(alloc.clone(), "demo.service", JobType::TryRestart, false, JobMode::Replace)
                .await
                .unwrap();
        assert_eq!(kind, JobKind::Restart);
        assert_eq!(alloc.read().jobs.get(&job_id).unwrap().kind, JobKind::Restart);
    }

    #[tokio::test]
    async fn test_enqueue_job_merges_identical_running_job() {
        // Regression: a second Start for a unit that already has a Running
        // Start job must merge into it (systemd job_merge), never cancel and
        // re-dispatch. Otherwise a SysV init script that calls
        // `systemctl start $unit` from inside its own ExecStart (e.g.
        // /etc/init.d/virtualbox-guest-utils) spawns an infinite loop of
        // processes.
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());

        // Prime a Running Start job.
        let first_id = {
            let mut state = alloc.write();
            let jid = next_job_id();
            state.jobs.insert(
                jid,
                Job {
                    id: jid,
                    unit_name: "demo.service".to_string(),
                    kind: JobKind::Start,
                    status: JobStatus::Running,
                    timeout_abort: None,
                },
            );
            jid
        };

        // A second StartUnit-style request with Replace must resolve to the
        // existing job instead of creating another.
        let (second_id, kind) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::Start,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(kind, JobKind::Start);
        assert_eq!(second_id, first_id);

        let state = alloc.read();
        let running: Vec<_> = state
            .jobs
            .values()
            .filter(|j| {
                j.unit_name == "demo.service" && matches!(j.status, JobStatus::Running)
            })
            .collect();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, first_id);
    }

    #[tokio::test]
    async fn test_start_rate_limit_accumulates_across_successful_starts() {
        // Regression: systemd does NOT reset the start rate limiter on a
        // successful start — StartLimitIntervalSec/Burst is a sliding window
        // over start *attempts* (unit_test_start_limit / ratelimit). A SysV
        // init script that calls `systemctl start $unit` from inside its own
        // ExecStart (e.g. /etc/init.d/virtualbox-guest-utils) recurses; every
        // spawn succeeds, so without this accumulation the loop would run
        // unboundedly. The default 10s/5 limit must trip on the 6th attempt
        // even though every prior start succeeded.
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());

        for attempt in 1..=5 {
            let (job_id, kind) = enqueue_job_type(
                alloc.clone(),
                "demo.service",
                JobType::Start,
                false,
                JobMode::Replace,
            )
            .await
            .unwrap_or_else(|e| panic!("start #{attempt} should be allowed: {e}"));
            assert_eq!(kind, JobKind::Start);
            // The start completes successfully — this must NOT clear the
            // accumulated rate-limit state.
            handle_task_result(alloc.clone(), job_id, true, "ok", "demo.service", JobKind::Start);
        }

        let err = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::Start,
            false,
            JobMode::Replace,
        )
        .await
        .expect_err("6th start within the interval must be rate-limited");
        let msg = err.to_string();
        assert!(
            msg.contains("rate limit exceeded"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_reload_if_possible_mangles_to_reload() {
        // ReloadOrRestartUnit on an active, reloadable unit → reload.
        let alloc = alloc_with_state("active");
        {
            let mut state = alloc.write();
            state.units.get_mut("demo.service").unwrap().service = Some(
                crate::unit::types::ServiceSection {
                    exec_reload: vec![crate::unit::types::ExecCommand::parse(
                        "/usr/bin/kill -HUP $MAINPID",
                    )],
                    ..Default::default()
                },
            );
            register_service_worker(&mut state);
        }
        let (job_id, kind) =
            enqueue_job_type(alloc.clone(), "demo.service", JobType::Restart, true, JobMode::Replace)
                .await
                .unwrap();
        assert_eq!(kind, JobKind::Reload);
        assert_eq!(alloc.read().jobs.get(&job_id).unwrap().kind, JobKind::Reload);
    }

    #[tokio::test]
    async fn test_enqueue_job_type_reload_or_restart_without_reload_stays_restart() {
        // ReloadOrRestartUnit on an active unit without ExecReload → restart.
        let alloc = alloc_with_state("active");
        register_service_worker(&mut alloc.write());
        let (job_id, kind) =
            enqueue_job_type(alloc.clone(), "demo.service", JobType::Restart, true, JobMode::Replace)
                .await
                .unwrap();
        assert_eq!(kind, JobKind::Restart);
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().kind,
            JobKind::Restart
        );
    }

    #[tokio::test]
    async fn test_enqueue_job_type_reload_or_start_of_inactive_unit_is_start() {
        let alloc = alloc_with_state("inactive");
        register_service_worker(&mut alloc.write());
        let (job_id, kind) =
            enqueue_job_type(alloc.clone(), "demo.service", JobType::ReloadOrStart, false, JobMode::Replace)
                .await
                .unwrap();
        assert_eq!(kind, JobKind::Start);
        assert_eq!(alloc.read().jobs.get(&job_id).unwrap().kind, JobKind::Start);
    }

    #[tokio::test]
    async fn test_enqueue_job_type_verify_active_root_completes_by_state() {
        // Direct verify-active request: active unit → done; inactive → skipped.
        let alloc = alloc_with_state("active");
        let (job_id, _) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::VerifyActive,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        );

        let alloc = alloc_with_state("inactive");
        let (job_id, _) = enqueue_job_type(
            alloc.clone(),
            "demo.service",
            JobType::VerifyActive,
            false,
            JobMode::Replace,
        )
        .await
        .unwrap();
        assert_eq!(
            alloc.read().jobs.get(&job_id).unwrap().status,
            JobStatus::Done
        );
    }

    // =========================================================================
    // Serial execution helpers
    // =========================================================================

    #[test]
    fn test_enqueue_job_emit_job_new() {
        // Verify emit_job_new doesn't panic with None channel
        let mut state = AllocatorState::new();
        emit_job_new(&mut state, 1, "test.service", JobKind::Start);
        // No panic is the success case
    }

    // =========================================================================
    // Failure propagation (fail_dependents)
    // =========================================================================

    fn state_with_job(state: &mut AllocatorState, unit: &str, kind: JobKind) -> u64 {
        let jid = next_job_id();
        state.jobs.insert(
            jid,
            Job {
                id: jid,
                unit_name: unit.to_string(),
                kind,
                status: JobStatus::Running,
                timeout_abort: None,
            },
        );
        jid
    }

    fn unit_requires(name: &str, req: &[&str]) -> (String, UnitFile) {
        let mut u = make_unit(name);
        for dep in req {
            u.unit.requires.insert(dep.to_string());
        }
        (name.to_string(), u)
    }

    #[test]
    fn test_fail_dependents_start_failure_propagates_requires_chain() {
        let mut state = AllocatorState::new();
        state.units.insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        let (cn, c) = unit_requires("c.service", &["b.service"]);
        state.units.insert(cn, c);
        state_with_job(&mut state, "a.service", JobKind::Start);
        state_with_job(&mut state, "b.service", JobKind::Start);
        state_with_job(&mut state, "c.service", JobKind::Start);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "b.service").unwrap().status,
            JobStatus::Failed(_)
        ));
        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "c.service").unwrap().status,
            JobStatus::Failed(_)
        ));
        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "a.service").unwrap().status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_start_failure_ignores_non_start_jobs() {
        let mut state = AllocatorState::new();
        state.units.insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Start);
        // b has a stop job — must not be failed
        state_with_job(&mut state, "b.service", JobKind::Stop);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "b.service").unwrap().status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_start_failure_propagates_binds_to() {
        let mut state = AllocatorState::new();
        state.units.insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = {
            let (n, mut u) = unit_requires("b.service", &["a.service"]);
            u.unit.requires.remove("a.service");
            u.unit.binds_to.insert("a.service".to_string());
            (n, u)
        };
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Start);
        state_with_job(&mut state, "b.service", JobKind::Start);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "b.service").unwrap().status,
            JobStatus::Failed(_)
        ));
    }

    #[test]
    fn test_fail_dependents_stop_failure_propagates_conflicts() {
        let mut state = AllocatorState::new();
        state.units.insert("a.service".to_string(), make_unit("a.service"));
        let mut b = make_unit("b.service");
        b.unit.conflicts.insert("a.service".to_string());
        state.units.insert("b.service".to_string(), b);
        state_with_job(&mut state, "a.service", JobKind::Stop);
        state_with_job(&mut state, "b.service", JobKind::Start);

        fail_dependents(&mut state, "a.service", JobKind::Stop);

        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "b.service").unwrap().status,
            JobStatus::Failed(_)
        ));
    }

    #[test]
    fn test_fail_dependents_requires_does_not_propagate_on_stop_failure() {
        let mut state = AllocatorState::new();
        state.units.insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Stop);
        state_with_job(&mut state, "b.service", JobKind::Start);

        // A failed stop job only propagates through Conflicts=
        fail_dependents(&mut state, "a.service", JobKind::Stop);

        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "b.service").unwrap().status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_restart_failure_propagates_nothing() {
        let mut state = AllocatorState::new();
        state.units.insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Restart);
        state_with_job(&mut state, "b.service", JobKind::Start);

        // systemd: only JOB_START / JOB_VERIFY_ACTIVE failures propagate
        fail_dependents(&mut state, "a.service", JobKind::Restart);

        assert!(matches!(
            state.jobs.values().find(|j| j.unit_name == "b.service").unwrap().status,
            JobStatus::Running
        ));
    }

    #[test]
    fn test_fail_dependents_completion_result_is_dependency() {
        let mut state = AllocatorState::new();
        state.units.insert("a.service".to_string(), make_unit("a.service"));
        let (bn, b) = unit_requires("b.service", &["a.service"]);
        state.units.insert(bn, b);
        state_with_job(&mut state, "a.service", JobKind::Start);
        state_with_job(&mut state, "b.service", JobKind::Start);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.job_completion_tx = Some(tx);

        fail_dependents(&mut state, "a.service", JobKind::Start);

        let completion = rx.try_recv().expect("completion emitted");
        assert_eq!(completion.unit_name, "b.service");
        assert_eq!(completion.result, JobResultKind::Dependency);
    }

    // =========================================================================
    // BindsTo start propagation tests
    // =========================================================================

    fn unit_binds_to(name: &str, dep: &str) -> (String, UnitFile) {
        let mut u = make_unit(name);
        u.unit.binds_to.insert(dep.to_string());
        (name.to_string(), u)
    }

    fn cached_active(state: &mut AllocatorState, name: &str) {
        state.unit_states.insert(
            name.to_string(),
            CachedUnitState {
                active_state: "active".to_string(),
                sub_state: String::new(),
                main_pid: 0,
                invocation_id: String::new(),
                active_enter_timestamp: 0,
                inactive_enter_timestamp: 0,
                extensions: HashMap::new(),
                pids: Vec::new(),
                controller: String::new(),
            },
        );
    }

    #[test]
    fn binds_to_start_propagates_when_dependency_active() {
        let mut state = AllocatorState::new();
        state.units.insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert_eq!(targets, vec!["consumer.service".to_string()]);
    }

    #[test]
    fn binds_to_start_propagation_skips_inactive_dependency() {
        let mut state = AllocatorState::new();
        state.units.insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_skips_unit_with_running_job() {
        let mut state = AllocatorState::new();
        state.units.insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");
        state_with_job(&mut state, "consumer.service", JobKind::Start);

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_skips_restart_job_targets() {
        // An in-flight Restart also counts as running (gate B).
        let mut state = AllocatorState::new();
        state.units.insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");
        state_with_job(&mut state, "consumer.service", JobKind::Restart);

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Start);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_skips_already_active_target() {
        let mut state = AllocatorState::new();
        state.units.insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");
        cached_active(&mut state, "consumer.service");

        let targets = binds_to_start_propagation(&state, "dep.service", true, JobKind::Restart);
        assert!(targets.is_empty());
    }

    #[test]
    fn binds_to_start_propagation_ignores_failed_and_stop_jobs() {
        let mut state = AllocatorState::new();
        state.units.insert("dep.service".to_string(), make_unit("dep.service"));
        let (cn, c) = unit_binds_to("consumer.service", "dep.service");
        state.units.insert(cn, c);
        cached_active(&mut state, "dep.service");

        assert!(binds_to_start_propagation(&state, "dep.service", false, JobKind::Start).is_empty());
        assert!(binds_to_start_propagation(&state, "dep.service", true, JobKind::Stop).is_empty());
    }
}
