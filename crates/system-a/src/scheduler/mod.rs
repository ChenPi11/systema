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
    ActiveState, AllocatorHandle, Job, JobKind, JobResult, JobResultKind, JobStatus, WorkerTask, next_job_id, next_task_id,
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

    // Determine which units to activate in dependency order.
    let units_to_process = {
        let state = allocator.read();
        match kind {
            JobKind::Start | JobKind::Restart => {
                compute_start_order(&state.units, unit_name)
            }
            JobKind::Stop => {
                // For stop, we just stop the requested unit (and dependents if needed).
                vec![unit_name.to_string()]
            }
            JobKind::Reload => {
                vec![unit_name.to_string()]
            }
        }
    };

    debug!("Processing order: {:?}", units_to_process);

    let primary_job_id = next_job_id();

    // Create job records and dispatch tasks.
    for (i, name) in units_to_process.iter().enumerate() {
        let job_id = if i == 0 { primary_job_id } else { next_job_id() };

        // Find the appropriate worker.
        let (worker_task_tx, task_id, unit_type) = {
            let state = allocator.read();
            let unit = state.units.get(name.as_str());
            let unit_type = unit
                .map(|u| u.kind.worker_type().to_string())
                .unwrap_or_else(|| "service".to_string());

            // Target units are handled internally (no external worker needed).
            if unit.map(|u| &u.kind) == Some(&UnitKind::Target) {
                activate_target_internally(allocator.clone(), name);
                continue;
            }

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
                    continue;
                }
            }
        };

        let unit_file = {
            let state = allocator.read();
            state.units.get(name.as_str()).cloned()
        };

        // Record the job.
        {
            let mut state = allocator.write();
            state.runtime.entry(name.clone()).or_default().load_state = "loaded".to_string();
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
        }
    }

    Ok(primary_job_id)
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

    // Collect all units reachable via Requires/Wants from root.
    queue.push_back(root.to_string());
    let mut reachable = HashSet::new();
    while let Some(name) = queue.pop_front() {
        if reachable.contains(&name) {
            continue;
        }
        reachable.insert(name.clone());
        if let Some(unit) = units.get(&name) {
            for dep in unit.unit.requires.iter().chain(unit.unit.wants.iter()) {
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
    _task_id: u64,
    success: bool,
    message: &str,
    unit_name: &str,
    kind: JobKind,
) {
    let mut state = allocator.write();

    // Update active state based on task kind and success.
    let rt = state.runtime.entry(unit_name.to_string()).or_default();
    if success {
        rt.active_state = match kind {
            JobKind::Start | JobKind::Restart => ActiveState::Active,
            JobKind::Stop => ActiveState::Inactive,
            JobKind::Reload => ActiveState::Active,
        };
        rt.sub_state = "running".to_string();
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
    }
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
