use anyhow::bail;
use arroyo_rpc::grpc::node_grpc_client::NodeGrpcClient;
use arroyo_rpc::grpc::{
    HeartbeatNodeReq, RegisterNodeReq, StartWorkerData, StartWorkerHeader, StartWorkerReq,
    StopWorkerReq, StopWorkerStatus, WorkerFinishedReq,
};
use arroyo_types::{
    NodeId, WorkerId, JOB_ID_ENV, NODE_ID_ENV, RUN_ID_ENV, TASK_SLOTS_ENV, WORKER_ID_ENV,
};
use lazy_static::lazy_static;
use prometheus::{register_gauge, Gauge};
use std::collections::HashMap;
use std::os::unix::prelude::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{oneshot, Mutex};
use tonic::{Request, Status};
use tracing::{info, warn};

use crate::get_from_object_store;

#[cfg(feature = "k8s")]
pub mod kubernetes;

pub mod nomad;

lazy_static! {
    static ref FREE_SLOTS: Gauge =
        register_gauge!("arroyo_controller_free_slots", "number of free task slots").unwrap();
    static ref REGISTERED_SLOTS: Gauge = register_gauge!(
        "arroyo_controller_registered_slots",
        "total number of registered task slots"
    )
    .unwrap();
    static ref REGISTERED_NODES: Gauge = register_gauge!(
        "arroyo_controller_registered_nodes",
        "total number of registered nodes"
    )
    .unwrap();
}

const NODE_PART_SIZE: usize = 2 * 1024 * 1024;

#[async_trait::async_trait]
pub trait Scheduler: Send + Sync {
    async fn start_workers(
        &self,
        start_pipeline_req: StartPipelineReq,
    ) -> Result<(), SchedulerError>;

    async fn register_node(&self, req: RegisterNodeReq);
    async fn heartbeat_node(&self, req: HeartbeatNodeReq) -> Result<(), Status>;
    async fn worker_finished(&self, req: WorkerFinishedReq);
    async fn stop_workers(
        &self,
        job_id: &str,
        run_id: Option<i64>,
        force: bool,
    ) -> anyhow::Result<()>;
    async fn workers_for_job(
        &self,
        job_id: &str,
        run_id: Option<i64>,
    ) -> anyhow::Result<Vec<WorkerId>>;
}

pub struct ProcessWorker {
    job_id: String,
    run_id: i64,
    shutdown_tx: oneshot::Sender<()>,
}

/// This Scheduler starts new processes to run the worker nodes
pub struct ProcessScheduler {
    workers: Arc<Mutex<HashMap<WorkerId, ProcessWorker>>>,
    worker_counter: AtomicU64,
}

impl ProcessScheduler {
    pub fn new() -> Self {
        Self {
            workers: Arc::new(Mutex::new(HashMap::new())),
            worker_counter: AtomicU64::new(100),
        }
    }
}

const SLOTS_PER_NODE: usize = 16;

pub struct StartPipelineReq {
    pub name: String,
    pub pipeline_path: String,
    pub wasm_path: String,
    pub job_id: String,
    pub hash: String,
    pub run_id: i64,
    pub slots: usize,
    pub env_vars: HashMap<String, String>,
}

async fn get_binaries(req: &StartPipelineReq) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let pipeline = get_from_object_store(&req.pipeline_path).await?;
    let wasm = get_from_object_store(&req.wasm_path).await?;

    Ok((pipeline, wasm))
}

#[async_trait::async_trait]
impl Scheduler for ProcessScheduler {
    async fn start_workers(
        &self,
        start_pipeline_req: StartPipelineReq,
    ) -> Result<(), SchedulerError> {
        let workers = (start_pipeline_req.slots as f32 / SLOTS_PER_NODE as f32).ceil() as usize;

        let mut slots_scheduled = 0;

        let base_path = PathBuf::from_str(&format!(
            "/tmp/arroyo-process/{}",
            start_pipeline_req.job_id
        ))
        .unwrap();
        tokio::fs::create_dir_all(&base_path).await.unwrap();

        let (pipeline, wasm) = get_binaries(&start_pipeline_req)
            .await
            .map_err(|_| SchedulerError::CompilationNeeded)?;

        let pipeline_path = base_path.join("pipeline");

        if !pipeline_path.exists() {
            tokio::fs::write(&pipeline_path, pipeline).await.unwrap();
            let file = tokio::fs::File::open(&pipeline_path).await.unwrap();

            let mut perms = file.metadata().await.unwrap().permissions();
            perms.set_mode(0o776);
            file.set_permissions(perms).await.unwrap();

            tokio::fs::write(&base_path.join("wasm_fns_bg.wasm"), wasm)
                .await
                .unwrap();
        }

        for _ in 0..workers {
            let path = base_path.clone();

            let slots_here = (start_pipeline_req.slots - slots_scheduled).min(SLOTS_PER_NODE);

            let worker_id = self.worker_counter.fetch_add(1, Ordering::SeqCst);

            let (tx, rx) = oneshot::channel();

            {
                let mut workers = self.workers.lock().await;
                workers.insert(
                    WorkerId(worker_id),
                    ProcessWorker {
                        job_id: start_pipeline_req.job_id.clone(),
                        run_id: start_pipeline_req.run_id,
                        shutdown_tx: tx,
                    },
                );
            }

            slots_scheduled += slots_here;
            let job_id = start_pipeline_req.job_id.clone();
            println!("Starting in path {:?}", path);
            let workers = self.workers.clone();
            let env_map = start_pipeline_req.env_vars.clone();
            tokio::spawn(async move {
                let mut command = Command::new("./pipeline");
                for (env, value) in env_map {
                    command.env(env, value);
                }
                let mut child = command
                    .current_dir(&path)
                    .env("RUST_LOG", "info")
                    .env(TASK_SLOTS_ENV, format!("{}", slots_here))
                    .env(WORKER_ID_ENV, format!("{}", worker_id)) // start at 100 to make same length
                    .env(JOB_ID_ENV, &job_id)
                    .env(NODE_ID_ENV, format!("{}", 1))
                    .env(RUN_ID_ENV, format!("{}", start_pipeline_req.run_id))
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();

                tokio::select! {
                    status = child.wait() => {
                        info!("Child ({:?}) exited with status {:?}", path, status);
                    }
                    _ = rx => {
                        info!(message = "Killing child", worker_id = worker_id, job_id = job_id);
                        child.kill().await.unwrap();
                    }
                }

                let mut state = workers.lock().await;
                state.remove(&WorkerId(worker_id));
            });
        }

        Ok(())
    }

    async fn register_node(&self, _: RegisterNodeReq) {}
    async fn heartbeat_node(&self, _: HeartbeatNodeReq) -> Result<(), Status> {
        Ok(())
    }
    async fn worker_finished(&self, _: WorkerFinishedReq) {}

    async fn workers_for_job(
        &self,
        job_id: &str,
        run_id: Option<i64>,
    ) -> anyhow::Result<Vec<WorkerId>> {
        Ok(self
            .workers
            .lock()
            .await
            .iter()
            .filter(|(_, w)| {
                w.job_id == job_id && (run_id.is_none() || w.run_id == run_id.unwrap())
            })
            .map(|(k, _)| *k)
            .collect())
    }

    async fn stop_workers(
        &self,
        job_id: &str,
        run_id: Option<i64>,
        _force: bool,
    ) -> anyhow::Result<()> {
        for worker_id in self.workers_for_job(job_id, run_id).await? {
            let worker = {
                let mut state = self.workers.lock().await;
                let Some(worker) = state.remove(&worker_id) else {
                    return Ok(());
                };
                worker
            };

            let _ = worker.shutdown_tx.send(());
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct NodeStatus {
    id: NodeId,
    free_slots: usize,
    scheduled_slots: HashMap<WorkerId, usize>,
    addr: String,
    last_heartbeat: Instant,
}

impl NodeStatus {
    fn new(id: NodeId, slots: usize, addr: String) -> NodeStatus {
        FREE_SLOTS.add(slots as f64);
        REGISTERED_SLOTS.add(slots as f64);

        NodeStatus {
            id,
            free_slots: slots,
            scheduled_slots: HashMap::new(),
            addr,
            last_heartbeat: Instant::now(),
        }
    }

    fn take_slots(&mut self, worker: WorkerId, slots: usize) {
        if let Some(v) = self.free_slots.checked_sub(slots) {
            FREE_SLOTS.sub(slots as f64);
            self.free_slots = v;
            self.scheduled_slots.insert(worker, slots);
        } else {
            panic!(
                "Attempted to schedule more slots than are available on node {} ({} < {})",
                self.addr, self.free_slots, slots
            );
        }
    }

    fn release_slots(&mut self, worker_id: WorkerId, slots: usize) {
        if let Some(freed) = self.scheduled_slots.remove(&worker_id) {
            assert_eq!(freed, slots,
                "Controller and node disagree about how many slots are scheduled for worker {:?} ({} != {})",
                worker_id, freed, slots);

            self.free_slots += slots;

            FREE_SLOTS.add(slots as f64);
        } else {
            warn!(
                "Received release request for unknown worker {:?}",
                worker_id
            );
        }
    }
}

#[derive(Clone)]
struct NodeWorker {
    job_id: String,
    node_id: NodeId,
    run_id: i64,
    running: bool,
}

#[derive(Default)]
pub struct NodeSchedulerState {
    nodes: HashMap<NodeId, NodeStatus>,
    workers: HashMap<WorkerId, NodeWorker>,
}

impl NodeSchedulerState {
    fn expire_nodes(&mut self, expiration_time: Instant) {
        let expired_nodes: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(node_id, status)| {
                if status.last_heartbeat >= expiration_time {
                    None
                } else {
                    Some(*node_id)
                }
            })
            .collect();
        for node_id in expired_nodes {
            warn!("expiring node {:?} from scheduler state", node_id);
            self.nodes.remove(&node_id);
        }
    }
}

pub struct NodeScheduler {
    state: Arc<Mutex<NodeSchedulerState>>,
}

pub enum SchedulerError {
    NotEnoughSlots { slots_needed: usize },
    Other(String),
    CompilationNeeded,
}

impl NodeScheduler {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(NodeSchedulerState::default())),
        }
    }

    async fn stop_worker(
        &self,
        job_id: &str,
        worker_id: WorkerId,
        force: bool,
    ) -> anyhow::Result<Option<WorkerId>> {
        let state = self.state.lock().await;

        let Some(worker) = state.workers.get(&worker_id) else {
            // assume it's already finished
            return Ok(Some(worker_id));
        };

        let Some(node) = state.nodes.get(&worker.node_id) else {
            warn!(message = "node not found for stop worker", node_id = worker.node_id.0);
            return Ok(Some(worker_id));
        };

        let worker = worker.clone();
        let node = node.clone();
        drop(state);

        info!(
            message = "stopping worker",
            job_id = worker.job_id,
            node_id = worker.node_id.0,
            node_addr = node.addr,
            worker_id = worker_id.0
        );

        let Ok(mut client) = NodeGrpcClient::connect(format!("http://{}", node.addr)).await else {
            warn!("Failed to connect to worker to stop; this likely means it is dead");
            return Ok(Some(worker_id));
        };

        let Ok(resp) = client
            .stop_worker(Request::new(StopWorkerReq {
                job_id: job_id.to_string(),
                worker_id: worker_id.0,
                force,
            }))
            .await else {
                warn!("Failed to connect to worker to stop; this likely means it is dead");
                return Ok(Some(worker_id));
            };

        match (resp.get_ref().status(), force) {
            (StopWorkerStatus::NotFound, false) => {
                bail!("couldn't find worker, will only continue if force")
            }
            (StopWorkerStatus::StopFailed, _) => bail!("tried to kill and couldn't"),
            _ => Ok(None),
        }
    }
}

#[async_trait::async_trait]
impl Scheduler for NodeScheduler {
    async fn register_node(&self, req: RegisterNodeReq) {
        let mut state = self.state.lock().await;
        if let std::collections::hash_map::Entry::Vacant(e) = state.nodes.entry(NodeId(req.node_id))
        {
            e.insert(NodeStatus::new(
                NodeId(req.node_id),
                req.task_slots as usize,
                req.addr,
            ));
        }
    }

    async fn heartbeat_node(&self, req: HeartbeatNodeReq) -> Result<(), Status> {
        let mut state = self.state.lock().await;
        if let Some(node) = state.nodes.get_mut(&NodeId(req.node_id)) {
            node.last_heartbeat = Instant::now();
            Ok(())
        } else {
            warn!(
                "Received heartbeat for unregistered node {}, failing request",
                req.node_id
            );
            Err(Status::not_found(format!(
                "node {} not in scheduler's collection of nodes",
                req.node_id
            )))
        }
    }

    async fn worker_finished(&self, req: WorkerFinishedReq) {
        let mut state = self.state.lock().await;
        let worker_id = WorkerId(req.worker_id);

        if let Some(node) = state.nodes.get_mut(&NodeId(req.node_id)) {
            node.release_slots(worker_id, req.slots as usize);
        } else {
            warn!(
                "Got worker finished message for unknown node {}",
                req.node_id
            );
        }

        if state.workers.remove(&worker_id).is_none() {
            warn!(
                "Got worker finished message for unknown worker {}",
                worker_id.0
            );
        }
    }

    async fn workers_for_job(
        &self,
        job_id: &str,
        run_id: Option<i64>,
    ) -> anyhow::Result<Vec<WorkerId>> {
        let state = self.state.lock().await;
        Ok(state
            .workers
            .iter()
            .filter(|(_, v)| {
                v.job_id == job_id && v.running && (run_id.is_none() || v.run_id == run_id.unwrap())
            })
            .map(|(w, _)| *w)
            .collect())
    }

    async fn start_workers(
        &self,
        start_pipeline_req: StartPipelineReq,
    ) -> Result<(), SchedulerError> {
        let (binary, wasm) = get_binaries(&start_pipeline_req)
            .await
            .map_err(|_| SchedulerError::CompilationNeeded)?;

        let binary = Arc::new(binary);

        // TODO: make this locking more fine-grained
        let mut state = self.state.lock().await;

        state.expire_nodes(Instant::now() - Duration::from_secs(30));

        let free_slots = state.nodes.values().map(|n| n.free_slots).sum::<usize>();
        let slots = start_pipeline_req.slots;
        if slots > free_slots {
            return Err(SchedulerError::NotEnoughSlots {
                slots_needed: slots - free_slots,
            });
        }

        let mut to_schedule = slots;
        let mut slots_assigned = vec![];
        while to_schedule > 0 {
            // find the node with the most free slots and fill it
            let node = {
                if let Some(status) = state
                    .nodes
                    .values()
                    .filter(|n| {
                        n.free_slots > 0 && n.last_heartbeat.elapsed() < Duration::from_secs(30)
                    })
                    .max_by_key(|n| n.free_slots)
                    .cloned()
                {
                    status
                } else {
                    unreachable!();
                }
            };

            let slots_for_this_one = node.free_slots.min(to_schedule);
            info!(
                "Scheduling {} slots on node {}",
                slots_for_this_one, node.addr
            );

            let mut client = NodeGrpcClient::connect(format!("http://{}", node.addr))
                .await
                // TODO: handle this issue more gracefully by moving trying other nodes
                .map_err(|e| {
                    // release back slots already scheduled.
                    slots_assigned
                        .iter()
                        .for_each(|(node_id, worker_id, slots)| {
                            state
                                .nodes
                                .get_mut(node_id)
                                .unwrap()
                                .release_slots(*worker_id, *slots);
                        });
                    SchedulerError::Other(format!(
                        "Failed to connect to node {}: {:?}",
                        node.addr, e
                    ))
                })?;

            let header = StartWorkerReq {
                msg: Some(arroyo_rpc::grpc::start_worker_req::Msg::Header(
                    StartWorkerHeader {
                        name: start_pipeline_req.name.clone(),
                        job_id: start_pipeline_req.job_id.clone(),
                        wasm: wasm.clone(),
                        slots: slots_for_this_one as u64,
                        node_id: node.id.0,
                        run_id: start_pipeline_req.run_id as u64,
                        env_vars: start_pipeline_req.env_vars.clone(),
                        binary_size: binary.len() as u64,
                    },
                )),
            };

            let binary = binary.clone();
            let outbound = async_stream::stream! {
                yield header;

                let mut part = 0;
                let mut sent = 0;

                for chunk in binary.chunks(NODE_PART_SIZE) {
                    sent += chunk.len();

                    yield StartWorkerReq {
                        msg: Some(arroyo_rpc::grpc::start_worker_req::Msg::Data(StartWorkerData {
                            part,
                            data: chunk.to_vec(),
                            has_more: sent < binary.len(),
                        }))
                    };

                    part += 1;
                }
            };

            let res = client
                .start_worker(Request::new(outbound))
                .await
                .map_err(|e| {
                    // release back slots already scheduled.
                    slots_assigned
                        .iter()
                        .for_each(|(node_id, worker_id, slots)| {
                            state
                                .nodes
                                .get_mut(node_id)
                                .unwrap()
                                .release_slots(*worker_id, *slots);
                        });
                    SchedulerError::Other(format!(
                        "Failed to start worker on node {}: {:?}",
                        node.addr, e
                    ))
                })?
                .into_inner();

            state
                .nodes
                .get_mut(&node.id)
                .unwrap()
                .take_slots(WorkerId(res.worker_id), slots_for_this_one);

            state.workers.insert(
                WorkerId(res.worker_id),
                NodeWorker {
                    job_id: start_pipeline_req.job_id.clone(),
                    run_id: start_pipeline_req.run_id,
                    node_id: node.id,
                    running: true,
                },
            );

            slots_assigned.push((node.id, WorkerId(res.worker_id), slots_for_this_one));

            to_schedule -= slots_for_this_one;
        }
        Ok(())
    }

    async fn stop_workers(
        &self,
        job_id: &str,
        run_id: Option<i64>,
        force: bool,
    ) -> anyhow::Result<()> {
        // iterate through all of the workers from workers_for_job and stop them in parallel
        let workers = self.workers_for_job(job_id, run_id).await?;
        let mut futures = vec![];
        for worker_id in workers {
            futures.push(self.stop_worker(job_id, worker_id, force));
        }

        for f in futures {
            match f.await? {
                Some(worker_id) => {
                    let mut state = self.state.lock().await;
                    if let Some(worker) = state.workers.get_mut(&worker_id) {
                        worker.running = false;
                    }
                }
                None => {
                    bail!("Failed to stop worker");
                }
            }
        }

        Ok(())
    }
}
