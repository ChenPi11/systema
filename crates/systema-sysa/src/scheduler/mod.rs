//! Task scheduler — generates and dispatches tasks to System Workers.
//!
//! When System A receives a request to start or stop a unit, the scheduler:
//! 1. Determines which units must be started/stopped (dependency expansion).
//! 2. Creates Job records for each operation.
//! 3. Dispatches WorkerTask messages to the appropriate workers.

use std::time::Duration;

use anyhow::{bail, Result};
use libsysa::l10n;
use prost::Message;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::state::{
    next_job_id, next_task_id, ActiveState, AllocatorHandle, AllocatorState, Job, JobCompletion,
    JobKind, JobMode, JobNewInfo, JobResult, JobResultKind, JobStatus, StartLimitState,
    UnitRuntimeInfo, WorkerTask,
};
use crate::unit::types::{ExitKind, RestartPolicy, StartLimitAction, UnitFile, UnitSection};
use libsysa::proto::{ServiceConfig, SocketAddress, SocketConfig, TaskDispatch, TaskKind, UnitConfig};

/// Enqueue a start job for the named unit, expanding dependencies.
/// Returns the primary job ID.
pub async fn enqueue_start(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Start, JobMode::Replace).await
}

/// Enqueue a stop job for the named unit.
pub async fn enqueue_stop(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Stop, JobMode::Replace).await
}

/// Enqueue a restart job for the named unit.
pub async fn enqueue_restart(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Restart, JobMode::Replace).await
}

/// Enqueue a start job with explicit mode.
pub async fn enqueue_start_with_mode(
    allocator: AllocatorHandle,
    unit_name: &str,
    mode: JobMode,
) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Start, mode).await
}

/// Core job enqueueing logic.
pub async fn enqueue_job(
    allocator: AllocatorHandle,
    unit_name: &str,
    kind: JobKind,
    mode: JobMode,
) -> Result<u64> {
    info!("Scheduling {:?} for {} (mode={:?})", kind, unit_name, mode);

    // --- Early check: ensure at least one worker exists for the root unit ---
    {
        let state = allocator.read();
        let unit = state.units.get(unit_name);
        let unit_type = unit
            .map(|u| u.kind.worker_type().to_string())
            .unwrap_or_else(|| "service".to_string());
        let has_worker = state.workers.values().any(|w| w.unit_types.contains(&unit_type));
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
                .filter(|j| matches!(j.status, JobStatus::Waiting | JobStatus::Running))
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
            let rt = state.runtime.entry(unit_name.to_string()).or_default();
            rt.active_state = ActiveState::Inactive;
            rt.sub_state = "dead".to_string();
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
                "Assert check failed for {}; marking unit as failed",
                unit_name
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            let rt = state.runtime.entry(unit_name.to_string()).or_default();
            rt.active_state = ActiveState::Failed;
            rt.sub_state = "failed".to_string();
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

    // --- Requisite check: all Requisite= deps must already be active ---
    if matches!(kind, JobKind::Start | JobKind::Restart) && mode != JobMode::IgnoreRequirements {
        let requisite_check = {
            let state = allocator.read();
            if let Some(unit) = state.units.get(unit_name) {
                let mut failed_requisites = Vec::new();
                for req in &unit.unit.requisite {
                    let is_active = state
                        .runtime
                        .get(req.as_str())
                        .map(|rt| matches!(rt.active_state, ActiveState::Active))
                        .unwrap_or(false);
                    if !is_active {
                        failed_requisites.push(req.clone());
                    }
                }
                if failed_requisites.is_empty() {
                    None
                } else {
                    Some(failed_requisites)
                }
            } else {
                None
            }
        };
        if let Some(failed) = requisite_check {
            warn!(
                "Requisite check failed for {}: required units not active: {:?}",
                unit_name, failed
            );
            let job_id = next_job_id();
            let mut state = allocator.write();
            let rt = state.runtime.entry(unit_name.to_string()).or_default();
            rt.active_state = ActiveState::Failed;
            rt.sub_state = "failed".to_string();
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

    // --- Conflicts handling: stop conflicting units when starting ---
    if matches!(kind, JobKind::Start | JobKind::Restart) {
        let conflicts: Vec<String> = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .map(|u| u.unit.conflicts.iter().cloned().collect())
                .unwrap_or_default()
        };
        for conflict in conflicts {
            let is_active = {
                let state = allocator.read();
                state
                    .runtime
                    .get(conflict.as_str())
                    .map(|rt| {
                        matches!(
                            rt.active_state,
                            ActiveState::Active | ActiveState::Activating
                        )
                    })
                    .unwrap_or(false)
            };
            if is_active {
                info!(
                    "Stopping conflicting unit {} before starting {}",
                    conflict, unit_name
                );
                Box::pin(enqueue_job(allocator.clone(), &conflict, JobKind::Stop, JobMode::Replace)).await?;
            }
        }
    }

    // --- Job conflict detection ---
    {
        let read_state = allocator.read();
        let existing: Option<(u64, String)> = read_state
            .jobs
            .values()
            .find(|j| {
                j.unit_name == unit_name
                    && j.kind == kind
                    && matches!(j.status, JobStatus::Running | JobStatus::Waiting)
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
                JobMode::Queue => {
                    return Ok(existing_id);
                }
                JobMode::Replace => {
                    let mut wstate = allocator.write();
                    if let Some(job) = wstate.jobs.get_mut(&existing_id) {
                        job.status = JobStatus::Cancelled;
                    }
                }
                _ => {}
            }
        }
    }

    // Determine which units to activate in dependency order.
    let units_to_process = {
        let state = allocator.read();
        match kind {
            JobKind::Start | JobKind::Restart
                if mode == JobMode::IgnoreDependencies || mode == JobMode::IgnoreRequirements =>
            {
                vec![unit_name.to_string()]
            }
            JobKind::Start | JobKind::Restart => compute_start_order(&state.units, unit_name),
            JobKind::Stop => {
                compute_stop_order(&state.units, &state.runtime, unit_name)
            }
            JobKind::Reload => {
                compute_reload_order(&state.units, unit_name)
            }
        }
    };

    // --- Isolate mode: stop all running units not in the dependency tree ---
    if mode == JobMode::Isolate && matches!(kind, JobKind::Start) {
        handle_isolate(allocator.clone(), &units_to_process).await;
    }

    debug!("Processing order: {:?}", units_to_process);

    // Before dispatching any dependencies, re-check that the root doesn't
    // already have a running job (handles races after the conflict detection above).
    {
        let state = allocator.read();
        if let Some(existing) = state.jobs.values().find(|j| {
            j.unit_name == unit_name
                && j.kind == kind
                && matches!(j.status, JobStatus::Running | JobStatus::Waiting)
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
    for name in units_to_process.iter() {
        // The primary job ID belongs to the unit that was directly requested.
        let is_root = name.as_str() == unit_name;
        let job_id = if is_root {
            primary_job_id
        } else {
            next_job_id()
        };
        created_job_ids.push((job_id, name.clone()));

        // Idempotency: skip if the unit is already in the desired state,
        // or if there is already an in-flight job of the same kind for it.
        {
            let state = allocator.read();
            let already_in_desired_state = match kind {
                JobKind::Start => state
                    .runtime
                    .get(name.as_str())
                    .map(|rt| {
                        matches!(
                            rt.active_state,
                            ActiveState::Active | ActiveState::Activating
                        )
                    })
                    .unwrap_or(false),
                // Restart always re-executes: stop then start, regardless of current state.
                JobKind::Restart => false,
                JobKind::Stop => {
                    state
                        .runtime
                        .get(name.as_str())
                        .map(|rt| {
                            matches!(
                                rt.active_state,
                                ActiveState::Inactive | ActiveState::Deactivating
                            )
                        })
                        .unwrap_or(true) // treat unknown as inactive for stop
                }
                JobKind::Reload => false,
            };

            // Check for an existing running/waiting job of the same kind.
            let existing_running_job_id: Option<u64> = state
                .jobs
                .values()
                .find(|j| {
                    j.unit_name == *name
                        && j.kind == kind
                        && matches!(j.status, JobStatus::Running | JobStatus::Waiting)
                })
                .map(|j| j.id);

            if already_in_desired_state {
                let reason = "already in desired state";
                debug!("Skipping {:?} for {} ({})", kind, name, reason);
                if is_root {
                    // Unit is already in the desired state — send an immediate
                    // completion so the caller (e.g. systemctl) doesn't wait.
                    if let Some(ref tx) = state.job_completion_tx {
                        let _ = tx.send(JobCompletion {
                            job_id: primary_job_id,
                            unit_name: unit_name.to_string(),
                            result: JobResultKind::Done,
                        });
                    }
                }
                continue;
            }

            if let Some(existing_jid) = existing_running_job_id {
                debug!("Skipping {:?} for {} (existing job running)", kind, name);
                if is_root {
                    // There is already an in-flight job for this operation.
                    // Return its ID so the caller waits for the REAL completion
                    // signal rather than a phantom new ID that will never fire.
                    return Ok(existing_jid);
                }
                continue;
            }
        }

        // Find the appropriate worker.
        // NOTE: read lock is dropped before match so the error path can
        // acquire the write lock without deadlocking.
        let (worker_chan, task_id, unit_type) = {
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
            (worker.map(|w| w.task_tx.clone()), tid, unit_type)
        };

        let (worker_task_tx, task_id) = match (worker_chan, task_id) {
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
                    let rt = state.runtime.entry(name.clone()).or_default();
                    rt.active_state = ActiveState::Failed;
                    rt.sub_state = "failed".to_string();
                    emit_job_new(&mut state, job_id, name, kind);

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

        // Create a serial chain entry for this task.
        let (next_serial_tx, next_serial_rx) = tokio::sync::oneshot::channel::<()>();

        // Record the job and the task_id → job_kind mapping.
        {
            let mut state = allocator.write();
            let rt = state.runtime.entry(name.clone()).or_default();
            rt.load_state = "loaded".to_string();
            match kind {
                JobKind::Start | JobKind::Restart => {
                    rt.active_state = ActiveState::Activating;
                    rt.sub_state = "start".to_string();
                }
                JobKind::Stop => {
                    rt.active_state = ActiveState::Deactivating;
                    rt.sub_state = "stop".to_string();
                }
                JobKind::Reload => {}
            }
            state.jobs.insert(
                job_id,
                Job {
                    id: job_id,
                    unit_name: name.clone(),
                    kind,
                    status: JobStatus::Running,
                    completion_tx: None,
                    timeout_abort: None,
                },
            );
            state.task_kinds.insert(task_id, kind);
            if serial_mode {
                state.serial_completion_txs.insert(task_id, next_serial_tx);
            }
        }

        // Emit JobNew signal for this job.
        emit_job_new_after_lock(allocator.clone(), job_id, name, kind);

        // --- Timeout monitoring ---
        let abort_handle = spawn_job_timeout(allocator.clone(), job_id, name, kind, &unit_file);
        if let Some(handle) = abort_handle {
            let mut state = allocator.write();
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.timeout_abort = Some(handle);
            }
        }

        let task = WorkerTask {
            task_id,
            unit_name: name.clone(),
            unit_type,
            kind,
            unit_file,
        };

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

        if worker_task_tx.send(task).await.is_err() {
            warn!("Worker channel closed for unit {}", name);
            let mut state = allocator.write();
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.status = JobStatus::Failed("Worker disconnected".to_string());
            }
            let rt = state.runtime.entry(name.clone()).or_default();
            rt.active_state = ActiveState::Failed;
            rt.sub_state = "failed".to_string();
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

/// Handle isolate mode: stop all running units not in the dependency tree.
async fn handle_isolate(allocator: AllocatorHandle, keep_units: &[String]) {
    let to_stop: Vec<String> = {
        let state = allocator.read();
        state
            .runtime
            .iter()
            .filter(|(name, rt)| {
                matches!(rt.active_state, ActiveState::Active | ActiveState::Activating)
                    && !keep_units.contains(name)
            })
            .map(|(name, _)| name.clone())
            .collect()
    };
    for name in to_stop {
        info!("Isolate mode: stopping unit {}", name);
        if let Err(e) = Box::pin(enqueue_job(allocator.clone(), &name, JobKind::Stop, JobMode::Replace)).await
        {
            warn!("Failed to stop {} during isolate: {}", name, e);
        }
    }
}

/// Emit a JobNew signal for a newly created job (must hold the write lock).
fn emit_job_new(state: &mut AllocatorState, job_id: u64, unit_name: &str, kind: JobKind) {
    if let Some(ref tx) = state.job_new_tx {
        let _ = tx.send(JobNewInfo {
            job_id,
            unit_name: unit_name.to_string(),
            kind,
        });
    }
}

/// Emit JobNew without holding the write lock (acquires it briefly).
fn emit_job_new_after_lock(allocator: AllocatorHandle, job_id: u64, unit_name: &str, kind: JobKind) {
    let state = allocator.read();
    if let Some(ref tx) = state.job_new_tx {
        let _ = tx.send(JobNewInfo {
            job_id,
            unit_name: unit_name.to_string(),
            kind,
        });
    }
}

/// Compute the order in which units should be stopped.
/// This includes the requested unit plus all units that have a hard dependency
/// on it (Requires=, BindsTo=, PartOf=).
fn compute_stop_order(
    units: &std::collections::HashMap<String, UnitFile>,
    runtime: &std::collections::HashMap<String, UnitRuntimeInfo>,
    root: &str,
) -> Vec<String> {
    use std::collections::HashSet;

    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![root.to_string()];

    while let Some(name) = stack.pop() {
        if visited.contains(&name) {
            continue;
        }
        visited.insert(name.clone());
        result.push(name.clone());

        // Find all units that have Requires=name, BindsTo=name, or PartOf=name
        // and are currently active — they must be stopped too.
        for (other_name, other_unit) in units {
            if visited.contains(other_name) {
                continue;
            }
            let depends_on_name = other_unit.unit.requires.contains(&name)
                || other_unit.unit.binds_to.contains(&name)
                || other_unit.unit.part_of.contains(&name);
            if depends_on_name {
                let is_active = runtime
                    .get(other_name.as_str())
                    .map(|rt| {
                        matches!(
                            rt.active_state,
                            ActiveState::Active | ActiveState::Activating
                        )
                    })
                    .unwrap_or(false);
                if is_active {
                    stack.push(other_name.clone());
                }
            }
        }
    }

    result
}

/// Compute the reload propagation order.
/// Includes the root unit plus all units listed in its PropagatesReloadTo=.
fn compute_reload_order(
    units: &std::collections::HashMap<String, UnitFile>,
    root: &str,
) -> Vec<String> {
    let mut result = vec![root.to_string()];
    if let Some(unit) = units.get(root) {
        for target in &unit.unit.propagates_reload_to {
            if !result.contains(target) {
                result.push(target.clone());
            }
        }
    }
    result
}

/// Compute the order in which units should be started, respecting After/Before.
fn compute_start_order(
    units: &std::collections::HashMap<String, UnitFile>,
    root: &str,
) -> Vec<String> {
    use std::collections::{HashSet, VecDeque};

    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    // Collect all units reachable via Requires/Wants/Requisite/BindsTo/Upholds from root.
    queue.push_back(root.to_string());
    let mut reachable = HashSet::new();
    while let Some(name) = queue.pop_front() {
        if reachable.contains(&name) {
            continue;
        }
        reachable.insert(name.clone());
        if let Some(unit) = units.get(&name) {
            for dep in unit
                .unit
                .requires
                .iter()
                .chain(unit.unit.wants.iter())
                .chain(unit.unit.requisite.iter())
                .chain(unit.unit.binds_to.iter())
            {
                queue.push_back(dep.clone());
            }
        }
    }

    // DFS-based topological sort respecting After and Before ordering.
    fn visit(
        name: &str,
        units: &std::collections::HashMap<String, UnitFile>,
        visited: &mut HashSet<String>,
        result: &mut Vec<String>,
    ) {
        if visited.contains(name) {
            return;
        }
        visited.insert(name.to_string());
        // Visit After-dependencies first (they must start before us).
        if let Some(unit) = units.get(name) {
            for dep in &unit.unit.after {
                visit(dep, units, visited, result);
            }
        }
        result.push(name.to_string());
        // Visit Before-targets after us (we must start before them).
        if let Some(unit) = units.get(name) {
            for dep in &unit.unit.before {
                visit(dep, units, visited, result);
            }
        }
    }

    for name in &reachable {
        visit(name, units, &mut visited, &mut result);
    }

    // Ensure the root unit appears exactly once, at the end.
    // The swap-based approach is incorrect when root appears multiple times
    // due to DFS traversal through After=/Before= edges.
    result.retain(|n| n != root);
    result.push(root.to_string());

    result
}

/// Update unit runtime state from a task result received from a worker.
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

        // Update active state based on task kind and success.
        let rt = state.runtime.entry(unit_name.to_string()).or_default();
        if success {
            rt.active_state = match kind {
                JobKind::Start | JobKind::Restart => ActiveState::Active,
                JobKind::Stop => ActiveState::Inactive,
                JobKind::Reload => ActiveState::Active,
            };
            rt.sub_state = if kind == JobKind::Stop {
                "dead".to_string()
            } else {
                "running".to_string()
            };
            // Reset rate-limit state on successful start.
            if matches!(kind, JobKind::Start | JobKind::Restart) {
                state.start_limit_state.remove(unit_name);
            }
        } else {
            rt.active_state = ActiveState::Failed;
            rt.sub_state = "failed".to_string();
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
            let comp_result = JobResult {
                job_id: jid,
                unit_name: unit_name.to_string(),
                result: result_kind.clone(),
            };
            if let Some(job) = state.jobs.get_mut(&jid) {
                // Cancel the timeout so it doesn't race with this completion.
                if let Some(abort) = job.timeout_abort.take() {
                    abort.abort();
                }
                job.status = if success {
                    JobStatus::Done
                } else {
                    JobStatus::Failed(message.to_string())
                };
                if let Some(tx) = job.completion_tx.take() {
                    let _ = tx.send(comp_result);
                }
            }
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id: jid,
                    unit_name: unit_name.to_string(),
                    result: result_kind,
                });
            }
        }

        // --- BindsTo= lifecycle binding ---
        if matches!(kind, JobKind::Stop) || !success {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.binds_to.contains(unit_name) {
                    let is_active = state
                        .runtime
                        .get(other_name.as_str())
                        .map(|rt| {
                            matches!(
                                rt.active_state,
                                ActiveState::Active | ActiveState::Activating
                            )
                        })
                        .unwrap_or(false);
                    if is_active {
                        post_actions.push(PostAction::Stop(other_name.clone()));
                    }
                }
            }
        }

        // --- PartOf= stop propagation ---
        if matches!(kind, JobKind::Stop) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.part_of.contains(unit_name) {
                    let is_active = state
                        .runtime
                        .get(other_name.as_str())
                        .map(|rt| {
                            matches!(
                                rt.active_state,
                                ActiveState::Active | ActiveState::Activating
                            )
                        })
                        .unwrap_or(false);
                    if is_active {
                        post_actions.push(PostAction::Stop(other_name.clone()));
                    }
                }
            }
        }

        // --- BindsTo= start propagation ---
        if success && matches!(kind, JobKind::Start | JobKind::Restart) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.binds_to.contains(unit_name) {
                    let is_inactive = state
                        .runtime
                        .get(other_name.as_str())
                        .map(|rt| {
                            matches!(
                                rt.active_state,
                                ActiveState::Inactive | ActiveState::Failed
                            )
                        })
                        .unwrap_or(true);
                    if is_inactive {
                        post_actions.push(PostAction::Start(other_name.clone()));
                    }
                }
            }
        }

        // --- PartOf= start propagation ---
        if success && matches!(kind, JobKind::Start | JobKind::Restart) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.part_of.contains(unit_name) {
                    let is_inactive = state
                        .runtime
                        .get(other_name.as_str())
                        .map(|rt| {
                            matches!(
                                rt.active_state,
                                ActiveState::Inactive | ActiveState::Failed
                            )
                        })
                        .unwrap_or(true);
                    if is_inactive {
                        post_actions.push(PostAction::Start(other_name.clone()));
                    }
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

        // --- Upholds= continuous activation ---
        if (!success || matches!(kind, JobKind::Stop))
            && matches!(
                state.runtime.get(unit_name).map(|rt| &rt.active_state),
                Some(ActiveState::Inactive) | Some(ActiveState::Failed)
            )
        {
            for (_other_name, other_unit) in &state.units {
                if other_unit.unit.upholds.contains(unit_name) {
                    let upholder_active = state
                        .runtime
                        .get(_other_name.as_str())
                        .map(|rt| matches!(rt.active_state, ActiveState::Active))
                        .unwrap_or(false);
                    if upholder_active {
                        post_actions.push(PostAction::Start(unit_name.to_string()));
                        break;
                    }
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
                        .trim()
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<i32>().ok())
                        .unwrap_or(1);
                    ExitKind::ExitCode(code)
                } else {
                    // Generic task failure — treat as non-zero exit.
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
                            enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace).await
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

/// Post-processing actions to be taken after a task result is handled.
enum PostAction {
    Stop(String),
    Start(String),
}

/// Build a `TaskDispatch` protobuf from a `WorkerTask`.
pub fn build_task_dispatch(task: &WorkerTask) -> TaskDispatch {
    let unit_config = task.unit_file.as_ref().map(|uf| build_unit_config(uf));
    let unit_config_bytes = unit_config
        .map(|c| {
            let mut buf = bytes::BytesMut::new();
            c.encode(&mut buf).unwrap_or_default();
            buf.to_vec()
        })
        .unwrap_or_default();

    TaskDispatch {
        task_id: task.task_id,
        unit_name: task.unit_name.clone(),
        unit_type: task.unit_type.clone(),
        kind: task_kind_to_proto(task.kind) as i32,
        unit_config: unit_config_bytes,
    }
}

fn task_kind_to_proto(kind: JobKind) -> TaskKind {
    match kind {
        JobKind::Start => TaskKind::Start,
        JobKind::Stop => TaskKind::Stop,
        JobKind::Restart => TaskKind::Restart,
        JobKind::Reload => TaskKind::Reload,
    }
}

fn build_unit_config(uf: &UnitFile) -> UnitConfig {
    let service = uf.service.as_ref().map(|svc| ServiceConfig {
        // Only the first ExecStart command is sent to the worker.
        // Multiple ExecStart directives (Type=oneshot) will be supported in Phase 2.
        exec_start: svc.exec_start.first().map(|c| c.raw.clone()).unwrap_or_default(),
        exec_stop: svc.exec_stop.first().map(|c| c.raw.clone()).unwrap_or_default(),
        exec_reload: svc.exec_reload.first().map(|c| c.raw.clone()).unwrap_or_default(),
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

    UnitConfig {
        unit_name: uf.name.clone(),
        description: uf.unit.description.clone(),
        service,
        socket,
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
                if t > 0 { Some(t as u64) } else { None }
            });
            if start_timeout.is_none() {
                // No start timeout configured; no watchdog either.
                return None;
            }
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
                        let rt = state.runtime.entry(name_clone.clone()).or_default();
                        rt.active_state = ActiveState::Failed;
                        rt.sub_state = "failed".to_string();
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
                    if t > 0 { Some(t as u64) } else { None }
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
                        let rt = state.runtime.entry(name_clone.clone()).or_default();
                        rt.active_state = ActiveState::Failed;
                        rt.sub_state = "failed".to_string();
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
                        let rt = state.runtime.entry(name_clone.clone()).or_default();
                        rt.active_state = ActiveState::Failed;
                        rt.sub_state = "failed".to_string();
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
            matches!(exit_kind, ExitCode(c) if *c != 0)
                || matches!(exit_kind, Signal(_) | Timeout)
        }
        RestartPolicy::OnAbnormal => matches!(exit_kind, Signal(_) | Timeout),
        RestartPolicy::OnWatchdog => matches!(exit_kind, Watchdog),
        RestartPolicy::OnAbort => matches!(exit_kind, Signal(_)),
    }
}

/// Check the start rate limit for `unit_name` and, if it passes, spawn an
/// async task that waits `RestartSec` and then enqueues a `Start` job.
///
/// Rate-limit parameters are read from the unit's `[Service]` section
/// (`StartLimitIntervalSec` / `StartLimitBurst`), falling back to 10 s / 5.
///
/// Returns `true` if the restart was scheduled, `false` if rate-limited.
pub fn schedule_automatic_restart(
    allocator: AllocatorHandle,
    unit_name: &str,
) -> bool {
    let (interval_sec, burst, restart_sec) = {
        let state = allocator.read();
        let svc = state.units.get(unit_name).and_then(|u| u.service.as_ref());
        (
            svc.map(|s| s.start_limit_interval_sec).unwrap_or(10),
            svc.map(|s| s.start_limit_burst).unwrap_or(5),
            svc.map(|s| s.restart_sec as u64).unwrap_or(0),
        )
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
            "Restart rate limit exceeded for {} (interval={}s burst={}), not auto-restarting",
            unit_name, interval_sec, burst
        );
        // Consult StartLimitAction.
        let action = {
            let state = allocator.read();
            state
                .units
                .get(unit_name)
                .and_then(|u| u.service.as_ref())
                .map(|svc| svc.start_limit_action.clone())
                .unwrap_or(StartLimitAction::None)
        };
        match action {
            StartLimitAction::None => {}
            StartLimitAction::Reboot
            | StartLimitAction::RebootForce
            | StartLimitAction::RebootImmediate => {
                warn!("StartLimitAction={:?}: rebooting system", action);
                let _ = std::process::Command::new("shutdown")
                    .args(["-r", "now", "StartLimitAction triggered by systema"])
                    .spawn();
            }
            StartLimitAction::Poweroff => {
                warn!("StartLimitAction=poweroff: powering off system");
                let _ = std::process::Command::new("shutdown")
                    .args(["-P", "now", "StartLimitAction triggered by systema"])
                    .spawn();
            }
            StartLimitAction::Exit => {
                warn!("StartLimitAction=exit: exiting (no-op, logging only)");
            }
        }
        return false;
    }

    let alloc = allocator.clone();
    let name = unit_name.to_string();
    tokio::spawn(async move {
        if restart_sec > 0 {
            tokio::time::sleep(Duration::from_secs(restart_sec)).await;
        }
        info!("Auto-restarting {} after exit/failure", name);
        if let Err(e) = enqueue_job(alloc.clone(), &name, JobKind::Start, JobMode::Replace).await
        {
            warn!("Failed to auto-restart {}: {}", name, e);
        }
    });

    true
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
        if !eval_condition_bool(glob, |g| path_glob_matches(g)) {
            return false;
        }
    }
    for path in &unit.condition_file_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::metadata(p)
                .map(|m| m.len() != 0)
                .unwrap_or(false)
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
        if !eval_condition_bool(glob, |g| path_glob_matches(g)) {
            return false;
        }
    }
    for path in &unit.assert_file_not_empty {
        if !eval_condition_bool(path, |p| {
            std::fs::metadata(p)
                .map(|m| m.len() != 0)
                .unwrap_or(false)
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
    if negate { !result } else { result }
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
    std::path::Path::new(libsysa::paths::instance().systemd_first_boot_file).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Allocator, StartLimitState};
    use std::collections::HashMap;
    use std::time::Duration;

    // =========================================================================
    // JobMode tests
    // =========================================================================

    #[test]
    fn test_job_mode_from_str_default() {
        assert_eq!(JobMode::from_str("unknown"), JobMode::Replace);
        assert_eq!(JobMode::from_str(""), JobMode::Replace);
    }

    #[test]
    fn test_job_mode_from_str_all() {
        assert_eq!(JobMode::from_str("replace"), JobMode::Replace);
        assert_eq!(JobMode::from_str("fail"), JobMode::Fail);
        assert_eq!(JobMode::from_str("queue"), JobMode::Queue);
        assert_eq!(JobMode::from_str("isolate"), JobMode::Isolate);
        assert_eq!(JobMode::from_str("flush"), JobMode::Flush);
        assert_eq!(JobMode::from_str("ignore-dependencies"), JobMode::IgnoreDependencies);
        assert_eq!(JobMode::from_str("ignore-requirements"), JobMode::IgnoreRequirements);
    }

    #[test]
    fn test_job_mode_as_str_roundtrip() {
        for mode in &[
            JobMode::Replace,
            JobMode::Fail,
            JobMode::Queue,
            JobMode::Isolate,
            JobMode::Flush,
            JobMode::IgnoreDependencies,
            JobMode::IgnoreRequirements,
        ] {
            assert_eq!(JobMode::from_str(mode.as_str()), *mode);
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
        state.timestamps.push(std::time::Instant::now() - Duration::from_secs(100));
        state.timestamps.push(std::time::Instant::now() - Duration::from_secs(100));
        // With short interval, they should be pruned
        assert!(state.check_rate_limit(Duration::from_secs(1), 5));
        assert_eq!(state.timestamps.len(), 1); // only the new one remains
    }

    // =========================================================================
    // compute_start_order tests
    // =========================================================================

    fn make_unit(name: &str) -> UnitFile {
        UnitFile::new(name)
    }

    fn make_unit_after(name: &str, after: &[&str]) -> UnitFile {
        let mut u = make_unit(name);
        for dep in after {
            u.unit.after.insert(dep.to_string());
        }
        u
    }

    fn make_unit_before(name: &str, before: &[&str]) -> UnitFile {
        let mut u = make_unit(name);
        for dep in before {
            u.unit.before.insert(dep.to_string());
        }
        u
    }

    #[test]
    fn test_compute_start_order_single_unit() {
        let mut units = HashMap::new();
        units.insert("foo.service".to_string(), make_unit("foo.service"));
        let order = compute_start_order(&units, "foo.service");
        assert_eq!(order, vec!["foo.service"]);
    }

    #[test]
    fn test_compute_start_order_with_after() {
        let mut units = HashMap::new();
        units.insert("a.service".to_string(), make_unit("a.service"));
        units.insert("b.service".to_string(), make_unit_after("b.service", &["a.service"]));
        units.insert("c.service".to_string(), make_unit_after("c.service", &["b.service"]));
        let order = compute_start_order(&units, "c.service");
        // a must come before b, b before c
        let pos_a = order.iter().position(|x| x == "a.service").unwrap();
        let pos_b = order.iter().position(|x| x == "b.service").unwrap();
        let pos_c = order.iter().position(|x| x == "c.service").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn test_compute_start_order_with_requires_expansion() {
        let mut units = HashMap::new();
        let a = make_unit("a.service");
        let mut b = make_unit("b.service");
        b.unit.requires.insert("a.service".to_string());
        let mut c = make_unit("c.service");
        c.unit.requires.insert("b.service".to_string());
        units.insert("a.service".to_string(), a);
        units.insert("b.service".to_string(), b);
        units.insert("c.service".to_string(), c);
        // compute_start_order BFS from root through requires/wants
        let order = compute_start_order(&units, "c.service");
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
        assert!(order.contains(&"c.service".to_string()));
    }

    #[test]
    fn test_compute_start_order_root_last() {
        let mut units = HashMap::new();
        units.insert("dep.service".to_string(), make_unit("dep.service"));
        units.insert("root.service".to_string(), make_unit_after("root.service", &["dep.service"]));
        let order = compute_start_order(&units, "root.service");
        assert_eq!(order.last().unwrap(), "root.service");
    }

    // =========================================================================
    // compute_stop_order tests
    // =========================================================================

    #[test]
    fn test_compute_stop_order_single_unit() {
        let units = HashMap::new();
        let mut runtime = HashMap::new();
        runtime.insert(
            "foo.service".to_string(),
            UnitRuntimeInfo {
                active_state: ActiveState::Active,
                ..Default::default()
            },
        );
        let order = compute_stop_order(&units, &runtime, "foo.service");
        assert_eq!(order, vec!["foo.service"]);
    }

    #[test]
    fn test_compute_stop_order_propagates_requires() {
        // B Requires=A, so stopping A should include B
        let mut units = HashMap::new();
        let mut b = make_unit("b.service");
        b.unit.requires.insert("a.service".to_string());
        units.insert("a.service".to_string(), make_unit("a.service"));
        units.insert("b.service".to_string(), b);

        let mut runtime = HashMap::new();
        runtime.insert(
            "a.service".to_string(),
            UnitRuntimeInfo {
                active_state: ActiveState::Active,
                ..Default::default()
            },
        );
        runtime.insert(
            "b.service".to_string(),
            UnitRuntimeInfo {
                active_state: ActiveState::Active,
                ..Default::default()
            },
        );

        let order = compute_stop_order(&units, &runtime, "a.service");
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
    }

    #[test]
    fn test_compute_stop_order_propagates_binds_to() {
        let mut units = HashMap::new();
        let mut b = make_unit("b.service");
        b.unit.binds_to.insert("a.service".to_string());
        units.insert("a.service".to_string(), make_unit("a.service"));
        units.insert("b.service".to_string(), b);

        let mut runtime = HashMap::new();
        runtime.insert("a.service".to_string(), UnitRuntimeInfo {
            active_state: ActiveState::Active,
            ..Default::default()
        });
        runtime.insert("b.service".to_string(), UnitRuntimeInfo {
            active_state: ActiveState::Active,
            ..Default::default()
        });

        let order = compute_stop_order(&units, &runtime, "a.service");
        assert!(order.contains(&"b.service".to_string()));
    }

    #[test]
    fn test_compute_stop_order_propagates_part_of() {
        let mut units = HashMap::new();
        let mut b = make_unit("b.service");
        b.unit.part_of.insert("a.service".to_string());
        units.insert("a.service".to_string(), make_unit("a.service"));
        units.insert("b.service".to_string(), b);

        let mut runtime = HashMap::new();
        runtime.insert("a.service".to_string(), UnitRuntimeInfo {
            active_state: ActiveState::Active,
            ..Default::default()
        });
        runtime.insert("b.service".to_string(), UnitRuntimeInfo {
            active_state: ActiveState::Active,
            ..Default::default()
        });

        let order = compute_stop_order(&units, &runtime, "a.service");
        assert!(order.contains(&"b.service".to_string()));
    }

    #[test]
    fn test_compute_stop_order_skips_inactive_dependents() {
        let mut units = HashMap::new();
        let mut b = make_unit("b.service");
        b.unit.requires.insert("a.service".to_string());
        units.insert("a.service".to_string(), make_unit("a.service"));
        units.insert("b.service".to_string(), b);

        let mut runtime = HashMap::new();
        runtime.insert("a.service".to_string(), UnitRuntimeInfo {
            active_state: ActiveState::Active,
            ..Default::default()
        });
        // B is not active, so it should NOT be in the stop list
        runtime.insert("b.service".to_string(), UnitRuntimeInfo {
            active_state: ActiveState::Inactive,
            ..Default::default()
        });

        let order = compute_stop_order(&units, &runtime, "a.service");
        assert_eq!(order, vec!["a.service"]);
    }

    // =========================================================================
    // compute_reload_order tests
    // =========================================================================

    #[test]
    fn test_compute_reload_order_single() {
        let units = HashMap::new();
        let order = compute_reload_order(&units, "foo.service");
        assert_eq!(order, vec!["foo.service"]);
    }

    #[test]
    fn test_compute_reload_order_propagates() {
        let mut units = HashMap::new();
        let mut a = make_unit("a.service");
        a.unit.propagates_reload_to.insert("b.service".to_string());
        a.unit.propagates_reload_to.insert("c.service".to_string());
        units.insert("a.service".to_string(), a);
        let order = compute_reload_order(&units, "a.service");
        assert!(order.contains(&"a.service".to_string()));
        assert!(order.contains(&"b.service".to_string()));
        assert!(order.contains(&"c.service".to_string()));
        assert_eq!(order.len(), 3);
    }

    // =========================================================================
    // Helper function tests
    // =========================================================================

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
    // build_task_dispatch / build_unit_config tests
    // =========================================================================

    #[test]
    fn test_build_task_dispatch_start() {
        let uf = make_unit("test.service");
        let task = WorkerTask {
            task_id: 42,
            unit_name: "test.service".to_string(),
            unit_type: "service".to_string(),
            kind: JobKind::Start,
            unit_file: Some(uf),
        };
        let dispatch = build_task_dispatch(&task);
        assert_eq!(dispatch.task_id, 42);
        assert_eq!(dispatch.unit_name, "test.service");
        assert_eq!(dispatch.unit_type, "service");
    }

    #[test]
    fn test_task_kind_to_proto() {
        use libsysa::proto::TaskKind;
        assert_eq!(task_kind_to_proto(JobKind::Start), TaskKind::Start);
        assert_eq!(task_kind_to_proto(JobKind::Stop), TaskKind::Stop);
        assert_eq!(task_kind_to_proto(JobKind::Restart), TaskKind::Restart);
        assert_eq!(task_kind_to_proto(JobKind::Reload), TaskKind::Reload);
    }

    // =========================================================================
    // Job conflict detection logic
    // =========================================================================

    #[test]
    fn test_job_mode_from_str_is_idempotent() {
        for s in &["replace", "fail", "queue", "isolate", "flush", "ignore-dependencies", "ignore-requirements"] {
            let mode = JobMode::from_str(s);
            assert_eq!(mode.as_str(), *s);
        }
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

    #[test]
    fn test_emit_job_new_after_lock() {
        let alloc = Allocator::new();
        emit_job_new_after_lock(alloc.clone(), 1, "test.service", JobKind::Start);
        // No panic is the success case
    }
}