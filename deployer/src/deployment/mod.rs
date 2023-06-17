pub mod deploy_layer;
pub mod error;
pub mod persistence;

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use shuttle_common::{claims::Claim, storage_manager::ArtifactsStorageManager};
use sqlx::{sqlite::SqliteRow, FromRow, Row};
use tracing::instrument;
use ulid::Ulid;

use crate::{project::driver::DeploymentRun, runtime_manager::RuntimeManager};
use persistence::State;
use tokio::sync::mpsc;

use self::{deploy_layer::LogRecorder, persistence::dal::Dal};

const RUN_BUFFER_SIZE: usize = 100;

pub struct DeploymentManagerBuilder<LR, D: Dal + Sync + 'static> {
    build_log_recorder: Option<LR>,
    artifacts_path: Option<PathBuf>,
    runtime_manager: Option<RuntimeManager>,
    dal: Option<D>,
    claim: Option<Claim>,
}

impl<LR, D: Dal + Send + Sync + 'static> DeploymentManagerBuilder<LR, D>
where
    LR: LogRecorder,
{
    pub fn build_log_recorder(mut self, build_log_recorder: LR) -> Self {
        self.build_log_recorder = Some(build_log_recorder);

        self
    }

    pub fn dal(mut self, dal: D) -> Self {
        self.dal = Some(dal);

        self
    }

    pub fn artifacts_path(mut self, artifacts_path: PathBuf) -> Self {
        self.artifacts_path = Some(artifacts_path);

        self
    }

    pub fn claim(mut self, claim: Claim) -> Self {
        self.claim = Some(claim);
        self
    }

    pub fn runtime(mut self, runtime_manager: RuntimeManager) -> Self {
        self.runtime_manager = Some(runtime_manager);

        self
    }

    /// Creates two Tokio tasks, one for building queued services, the other for
    /// executing/deploying built services. Two multi-producer, single consumer
    /// channels are also created which are for moving on-going service
    /// deployments between the aforementioned tasks.
    pub fn build(self) -> DeploymentManager<D> {
        let artifacts_path = self.artifacts_path.expect("artifacts path to be set");
        let runtime_manager = self.runtime_manager.expect("a runtime manager to be set");
        let (run_send, run_recv) = mpsc::channel(RUN_BUFFER_SIZE);
        let storage_manager = ArtifactsStorageManager::new(artifacts_path);
        let dal = self.dal.expect("a DAL is required");

        tokio::spawn(crate::project::driver::task(
            run_recv,
            runtime_manager.clone(),
            storage_manager.clone(),
            dal.clone(),
            self.claim,
        ));

        DeploymentManager {
            run_send,
            runtime_manager,
            storage_manager,
            dal,
        }
    }
}

#[derive(Clone)]
pub struct DeploymentManager<D: Dal + Sync + 'static> {
    run_send: RunSender,
    runtime_manager: RuntimeManager,
    storage_manager: ArtifactsStorageManager,
    dal: D,
}

/// ```no-test
/// queue channel   all deployments here are State::Queued until the get a slot from gateway
///       |
///       v
///  run channel    all deployments here are State::Built
///       |
///       v
///    run task     tasks enter the State::Running state and begin
///                 executing
/// ```
impl<D: Dal + Sync + 'static> DeploymentManager<D> {
    /// Create a new deployment manager. Manages one or more 'pipelines' for
    /// processing service building, loading, and deployment.
    pub fn builder<LR>() -> DeploymentManagerBuilder<LR, D> {
        DeploymentManagerBuilder {
            build_log_recorder: None,
            artifacts_path: None,
            runtime_manager: None,
            dal: None,
            claim: None,
        }
    }

    async fn run_push(&self, run: DeploymentRun) -> Result<(), error::Error> {
        self.run_send
            .send(run)
            .await
            .map_err(|err| error::Error::Send(err.to_string()))
    }

    #[instrument(skip(self), fields(service_id = %service_id, state = %State::Built))]
    pub async fn run_deployment(
        &self,
        service_id: Ulid,
        deployment_id: Ulid,
        network_name: &str,
        claim: Option<Claim>,
        is_next: bool,
    ) -> Result<(), error::Error> {
        // Refreshing the container should restart it and persist a new associated address to it.
        let service = self
            .dal
            .service(&service_id)
            .await
            .map_err(error::Error::Dal)?;

        let run = DeploymentRun {
            deployment_id,
            service_name: service.name,
            service_id: service.id,
            tracing_context: Default::default(),
            claim,
            target_ip: service
                .state
                .target_ip(network_name)
                .map_err(|_| error::Error::MissingIpv4Address)?,
            is_next,
        };

        self.run_push(run).await
    }

    pub async fn kill(&mut self, id: Ulid) {
        self.runtime_manager.kill(&id).await;
    }

    pub fn storage_manager(&self) -> ArtifactsStorageManager {
        self.storage_manager.clone()
    }
}

type RunSender = mpsc::Sender<DeploymentRun>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deployment {
    pub id: Ulid,
    pub service_id: Ulid,
    pub state: State,
    pub last_update: DateTime<Utc>,
    pub is_next: bool,
    pub git_commit_hash: Option<String>,
    pub git_commit_message: Option<String>,
    pub git_branch: Option<String>,
    pub git_dirty: Option<bool>,
}

impl FromRow<'_, SqliteRow> for Deployment {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: Ulid::from_string(row.try_get("id")?)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            service_id: Ulid::from_string(row.try_get("service_id")?)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            state: row.try_get("state")?,
            last_update: row.try_get("last_update")?,
            is_next: row.try_get("is_next")?,
            git_commit_hash: row.try_get("git_commit_hash")?,
            git_commit_message: row.try_get("git_commit_message")?,
            git_branch: row.try_get("git_branch")?,
            git_dirty: row.try_get("git_dirty")?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DeploymentState {
    pub id: Ulid,
    pub state: State,
    pub last_update: DateTime<Utc>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DeploymentRunnable {
    pub id: Ulid,
    pub service_name: String,
    pub service_id: Ulid,
    pub is_next: bool,
}

impl FromRow<'_, SqliteRow> for DeploymentRunnable {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: Ulid::from_string(row.try_get("id")?)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            service_name: row.try_get("service_name")?,
            service_id: Ulid::from_string(row.try_get("service_id")?)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            is_next: row.try_get("is_next")?,
        })
    }
}
