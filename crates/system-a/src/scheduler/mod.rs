//! Task scheduler — generates and dispatches tasks to System Workers.
//!
//! When System A receives a request to start or stop a unit, the scheduler:
//! 1. Determines which units must be started/stopped (dependency expansion).
//! 2. Creates Job records for each operation.
//! 3. Dispatches WorkerTask messages to the appropriate workers.

use anyhow::Result;
use prost::Message;
use tracing::{debug, info, warn};

use common::proto::{ServiceConfig, TaskDispatch, TaskKind, UnitConfig};
use crate::state::{
    ActiveState, AllocatorHandle, Job, JobCompletion, JobKind, JobResult, JobResultKind,
    JobStatus, UnitRuntimeInfo, WorkerTask, next_job_id, next_task_id,
};
use crate::unit::types::{UnitFile, UnitKind};

/// Enqueue a start job for the named unit, expanding dependencies.
/// Returns the primary job ID.
pub async fn enqueue_start(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Start).await
}

/// Enqueue a stop job for the named unit.
pub async fn enqueue_stop(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Stop).await
}

/// Enqueue a restart job for the named unit.
pub async fn enqueue_restart(allocator: AllocatorHandle, unit_name: &str) -> Result<u64> {
    enqueue_job(allocator, unit_name, JobKind::Restart).await
}

/// Core job enqueueing logic.
pub async fn enqueue_job(
    allocator: AllocatorHandle,
    unit_name: &str,
    kind: JobKind,
) -> Result<u64> {
    info!("Scheduling {:?} for {}", kind, unit_name);

    // --- Requisite check: all Requisite= deps must already be active ---
    if matches!(kind, JobKind::Start | JobKind::Restart) {
        let requisite_check = {
            let state = allocator.read();
            if let Some(unit) = state.units.get(unit_name) {
                let mut failed_requisites = Vec::new();
                for req in &unit.unit.requisite {
                    let is_active = state.runtime.get(req.as_str())
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
            return Ok(job_id);
        }
    }

    // --- Conflicts handling: stop conflicting units when starting ---
    if matches!(kind, JobKind::Start | JobKind::Restart) {
        let conflicts: Vec<String> = {
            let state = allocator.read();
            state.units.get(unit_name)
                .map(|u| u.unit.conflicts.iter().cloned().collect())
                .unwrap_or_default()
        };
        for conflict in conflicts {
            let is_active = {
                let state = allocator.read();
                state.runtime.get(conflict.as_str())
                    .map(|rt| matches!(rt.active_state, ActiveState::Active | ActiveState::Activating))
                    .unwrap_or(false)
            };
            if is_active {
                info!("Stopping conflicting unit {} before starting {}", conflict, unit_name);
                // Use Box::pin to handle the recursive async call.
                Box::pin(enqueue_job(allocator.clone(), &conflict, JobKind::Stop)).await?;
            }
        }
    }

    // Determine which units to activate in dependency order.
    let units_to_process = {
        let state = allocator.read();
        match kind {
            JobKind::Start | JobKind::Restart => {
                compute_start_order(&state.units, unit_name)
            }
            JobKind::Stop => {
                // For stop, compute reverse dependencies to propagate the stop.
                compute_stop_order(&state.units, &state.runtime, unit_name)
            }
            JobKind::Reload => {
                // Propagate reload to units declared in PropagatesReloadTo=.
                compute_reload_order(&state.units, unit_name)
            }
        }
    };

    debug!("Processing order: {:?}", units_to_process);

    let primary_job_id = next_job_id();

    // Create job records and dispatch tasks.
    for name in units_to_process.iter() {
        // The primary job ID belongs to the unit that was directly requested.
        let is_root = name.as_str() == unit_name;
        let job_id = if is_root { primary_job_id } else { next_job_id() };

        // Idempotency: skip if the unit is already in the desired state,
        // or if there is already an in-flight job of the same kind for it.
        {
            let state = allocator.read();
            let already_in_desired_state = match kind {
                JobKind::Start => {
                    state.runtime.get(name.as_str())
                        .map(|rt| matches!(rt.active_state, ActiveState::Active | ActiveState::Activating))
                        .unwrap_or(false)
                }
                // Restart always re-executes: stop then start, regardless of current state.
                JobKind::Restart => false,
                JobKind::Stop => {
                    state.runtime.get(name.as_str())
                        .map(|rt| matches!(rt.active_state, ActiveState::Inactive | ActiveState::Deactivating))
                        .unwrap_or(true) // treat unknown as inactive for stop
                }
                JobKind::Reload => false,
            };

            // Check for an existing running/waiting job of the same kind.
            let existing_running_job_id: Option<u64> = state.jobs.values()
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

        // Target units are handled internally (no external worker needed).
        // Check under a short-lived read lock and drop it before calling
        // activate_target_internally, which needs a write lock on the same handle.
        let is_target = {
            let state = allocator.read();
            state.units.get(name.as_str()).map_or(false, |u| matches!(u.kind, UnitKind::Target))
        };
        if is_target {
            activate_target_internally(allocator.clone(), name);
            // If this is the directly-requested unit, immediately notify the D-Bus
            // layer so callers (e.g. systemctl) receive JobRemoved and don't hang.
            if is_root {
                if let Some(ref tx) = allocator.read().job_completion_tx {
                    let _ = tx.send(JobCompletion {
                        job_id: primary_job_id,
                        unit_name: unit_name.to_string(),
                        result: JobResultKind::Done,
                    });
                }
            }
            continue;
        }

        // Find the appropriate worker.
        let (worker_task_tx, task_id, unit_type) = {
            let state = allocator.read();
            let unit = state.units.get(name.as_str());
            let unit_type = unit
                .map(|u| u.kind.worker_type().to_string())
                .unwrap_or_else(|| "service".to_string());

            let worker = state
                .workers
                .values()
                .find(|w| w.unit_types.contains(&unit_type));

            match worker {
                Some(w) => {
                    let tx = w.task_tx.clone();
                    let tid = next_task_id();
                    (tx, tid, unit_type)
                }
                None => {
                    warn!("No worker registered for unit type '{}' (unit: {})", unit_type, name);
                    if is_root {
                        // No worker available — mark the unit as failed and emit
                        // a failure completion so callers (e.g. systemctl) are
                        // not left waiting forever for a JobRemoved signal.
                        let mut state = allocator.write();
                        let rt = state.runtime.entry(name.clone()).or_default();
                        rt.active_state = ActiveState::Failed;
                        rt.sub_state = "failed".to_string();
                        if let Some(ref tx) = state.job_completion_tx {
                            let _ = tx.send(JobCompletion {
                                job_id: primary_job_id,
                                unit_name: name.clone(),
                                result: JobResultKind::Failed,
                            });
                        }
                    }
                    continue;
                }
            }
        };

        let unit_file = {
            let state = allocator.read();
            state.units.get(name.as_str()).cloned()
        };

        // Record the job and the task_id → job_kind mapping.
        {
            let mut state = allocator.write();
            let rt = state.runtime.entry(name.clone()).or_default();
            rt.load_state = "loaded".to_string();
            // Mark as activating/deactivating so the disconnect-cleanup code
            // can transition the unit to Failed if the worker disconnects.
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
                    completion_tx: None, // set by D-Bus layer if waiting
                },
            );
            state.task_kinds.insert(task_id, kind);
        }

        let task = WorkerTask {
            task_id,
            unit_name: name.clone(),
            unit_type,
            kind,
            unit_file,
        };

        if worker_task_tx.send(task).await.is_err() {
            warn!("Worker channel closed for unit {}", name);
            let mut state = allocator.write();
            if let Some(job) = state.jobs.get_mut(&job_id) {
                job.status = JobStatus::Failed("Worker disconnected".to_string());
            }
            let rt = state.runtime.entry(name.clone()).or_default();
            rt.active_state = ActiveState::Failed;
            rt.sub_state = "failed".to_string();
            // Emit failure so the caller (e.g. systemctl) doesn't hang
            // waiting for a JobRemoved signal that will never arrive.
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
    }

    Ok(primary_job_id)
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
                let is_active = runtime.get(other_name.as_str())
                    .map(|rt| matches!(rt.active_state, ActiveState::Active | ActiveState::Activating))
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
            for dep in unit.unit.requires.iter()
                .chain(unit.unit.wants.iter())
                .chain(unit.unit.requisite.iter())
                .chain(unit.unit.binds_to.iter())
            {
                queue.push_back(dep.clone());
            }
        }
    }

    // Simple DFS-based topological sort respecting After ordering.
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
        // Visit After-dependencies first.
        if let Some(unit) = units.get(name) {
            let afters: Vec<String> = unit.unit.after.iter().cloned().collect();
            for dep in afters {
                visit(&dep, units, visited, result);
            }
        }
        result.push(name.to_string());
    }

    for name in &reachable {
        visit(name, units, &mut visited, &mut result);
    }

    // Ensure the root unit appears last.
    if let Some(pos) = result.iter().position(|n| n == root) {
        let last = result.len() - 1;
        result.swap(pos, last);
    }

    result
}

/// Activate a target unit inline in System A (no external worker needed).
fn activate_target_internally(allocator: AllocatorHandle, name: &str) {
    let mut state = allocator.write();
    let rt = state.runtime.entry(name.to_string()).or_default();
    rt.active_state = ActiveState::Active;
    rt.sub_state = "active".to_string();
    rt.load_state = "loaded".to_string();
    debug!("Target {} activated internally", name);
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
    // Collect dependency-related actions that need async scheduling BEFORE
    // taking the write lock.  We'll collect them and spawn them afterwards.
    let mut post_actions: Vec<PostAction> = Vec::new();

    {
        let mut state = allocator.write();

        // Clean up the task_id → kind mapping.
        state.task_kinds.remove(&task_id);

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
            let result = JobResult {
                job_id: jid,
                unit_name: unit_name.to_string(),
                result: result_kind.clone(),
            };
            if let Some(job) = state.jobs.get_mut(&jid) {
                job.status = if success {
                    JobStatus::Done
                } else {
                    JobStatus::Failed(message.to_string())
                };
                // Notify D-Bus waiter if present.
                if let Some(tx) = job.completion_tx.take() {
                    let _ = tx.send(result);
                }
            }
            // Notify the D-Bus signal emitter so it can send JobRemoved to subscribers
            // (e.g. systemctl waits for this signal before returning to the user).
            if let Some(ref tx) = state.job_completion_tx {
                let _ = tx.send(JobCompletion {
                    job_id: jid,
                    unit_name: unit_name.to_string(),
                    result: result_kind,
                });
            }
        }

        // --- BindsTo= lifecycle binding ---
        // If a unit that others BindsTo stops or fails, stop those units.
        if matches!(kind, JobKind::Stop) || !success {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.binds_to.contains(unit_name) {
                    let is_active = state.runtime.get(other_name.as_str())
                        .map(|rt| matches!(rt.active_state, ActiveState::Active | ActiveState::Activating))
                        .unwrap_or(false);
                    if is_active {
                        post_actions.push(PostAction::Stop(other_name.clone()));
                    }
                }
            }
        }

        // --- PartOf= stop propagation ---
        // If a unit stops, stop all units that have PartOf= pointing to it.
        if matches!(kind, JobKind::Stop) {
            for (other_name, other_unit) in &state.units {
                if other_unit.unit.part_of.contains(unit_name) {
                    let is_active = state.runtime.get(other_name.as_str())
                        .map(|rt| matches!(rt.active_state, ActiveState::Active | ActiveState::Activating))
                        .unwrap_or(false);
                    if is_active {
                        post_actions.push(PostAction::Stop(other_name.clone()));
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
        // If a unit that someone Upholds= becomes inactive/failed, restart it.
        if (!success || matches!(kind, JobKind::Stop))
            && matches!(
                state.runtime.get(unit_name).map(|rt| &rt.active_state),
                Some(ActiveState::Inactive) | Some(ActiveState::Failed)
            )
        {
            for (_other_name, other_unit) in &state.units {
                if other_unit.unit.upholds.contains(unit_name) {
                    // The upholder wants this unit to stay active — restart it.
                    let upholder_active = state.runtime.get(_other_name.as_str())
                        .map(|rt| matches!(rt.active_state, ActiveState::Active))
                        .unwrap_or(false);
                    if upholder_active {
                        post_actions.push(PostAction::Start(unit_name.to_string()));
                        break; // one restart is enough
                    }
                }
            }
        }
    } // drop write lock

    // Execute post-actions asynchronously (these need the lock released).
    if !post_actions.is_empty() {
        let alloc = allocator.clone();
        tokio::spawn(async move {
            for action in post_actions {
                match action {
                    PostAction::Stop(name) => {
                        info!("Propagating stop to {}", name);
                        if let Err(e) = enqueue_job(alloc.clone(), &name, JobKind::Stop).await {
                            warn!("Failed to propagate stop to {}: {}", name, e);
                        }
                    }
                    PostAction::Start(name) => {
                        info!("Triggering start for {}", name);
                        if let Err(e) = enqueue_job(alloc.clone(), &name, JobKind::Start).await {
                            warn!("Failed to trigger start for {}: {}", name, e);
                        }
                    }
                }
            }
        });
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
        exec_start: svc.exec_start.first().cloned().unwrap_or_default(),
        exec_stop: svc.exec_stop.first().cloned().unwrap_or_default(),
        exec_reload: svc.exec_reload.first().cloned().unwrap_or_default(),
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

    UnitConfig {
        unit_name: uf.name.clone(),
        description: uf.unit.description.clone(),
        service,
    }
}
