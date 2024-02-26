use std::io::Cursor;
use std::net::SocketAddr;
use std::ops::Sub;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::handler::Handler;
use axum::http::Request;
use axum::middleware::{self, from_extractor};
use axum::response::Response;
use axum::routing::{any, delete, get, post};
use axum::{Json as AxumJson, Router};
use fqdn::FQDN;
use futures::Future;
use http::{StatusCode, Uri};
use instant_acme::{AccountCredentials, ChallengeType};
use serde::{Deserialize, Serialize};
use shuttle_common::backends::auth::{AuthPublicKey, JwtAuthenticationLayer, ScopedLayer};
use shuttle_common::backends::cache::CacheManager;
use shuttle_common::backends::metrics::{Metrics, TraceLayer};
use shuttle_common::backends::ClaimExt;
use shuttle_common::claims::{Scope, EXP_MINUTES};
use shuttle_common::models::error::axum::CustomErrorPath;
use shuttle_common::models::error::ErrorKind;
use shuttle_common::models::service;
use shuttle_common::models::{
    admin::ProjectResponse,
    project::{self, ProjectName},
    stats,
};
use shuttle_common::{deployment, request_span, VersionInfo};
use shuttle_proto::provisioner::provisioner_client::ProvisionerClient;
use shuttle_proto::provisioner::Ping;
use tokio::sync::mpsc::Sender;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceBuilder;
use tracing::{error, field, instrument, trace};
use ttl_cache::TtlCache;
use ulid::Ulid;
use uuid::Uuid;
use x509_parser::nom::AsBytes;
use x509_parser::parse_x509_certificate;
use x509_parser::pem::parse_x509_pem;
use x509_parser::time::ASN1Time;

use crate::acme::{AccountWrapper, AcmeClient, CustomDomain};
use crate::api::tracing::project_name_tracing_layer;
use crate::auth::{ScopedUser, User};
use crate::service::{ContainerSettings, GatewayService};
use crate::task::{self, BoxedTask};
use crate::tls::{GatewayCertResolver, RENEWAL_VALIDITY_THRESHOLD_IN_DAYS};
use crate::worker::WORKER_QUEUE_SIZE;
use crate::{DockerContext, Error, AUTH_CLIENT};

use super::auth_layer::ShuttleAuthLayer;
use super::project_caller::ProjectCaller;

pub const SVC_DEGRADED_THRESHOLD: usize = 128;
pub const SHUTTLE_GATEWAY_VARIANT: &str = "shuttle-gateway";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    status: ComponentStatus,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PaginationDetails {
    /// Page to fetch, starting from 0.
    pub page: Option<u32>,
    /// Number of results per page.
    pub limit: Option<u32>,
}

impl StatusResponse {
    pub fn healthy() -> Self {
        Self {
            status: ComponentStatus::Healthy,
        }
    }

    pub fn degraded() -> Self {
        Self {
            status: ComponentStatus::Degraded,
        }
    }

    pub fn unhealthy() -> Self {
        Self {
            status: ComponentStatus::Unhealthy,
        }
    }
}

#[instrument(skip(service))]
async fn get_project(
    State(RouterState { service, .. }): State<RouterState>,
    ScopedUser { scope, .. }: ScopedUser,
) -> Result<AxumJson<project::Response>, Error> {
    let project = service.find_project(&scope).await?;
    let idle_minutes = project.state.idle_minutes();

    let response = project::Response {
        id: project.project_id.to_uppercase(),
        name: scope.to_string(),
        state: project.state.into(),
        idle_minutes,
    };

    Ok(AxumJson(response))
}

#[instrument(skip(service))]
async fn check_project_name(
    State(RouterState { service, .. }): State<RouterState>,
    CustomErrorPath(project_name): CustomErrorPath<ProjectName>,
) -> Result<AxumJson<bool>, Error> {
    service
        .project_name_exists(&project_name)
        .await
        .map(AxumJson)
}

async fn get_projects_list(
    State(RouterState { service, .. }): State<RouterState>,
    User { name, .. }: User,
    Query(PaginationDetails { page, limit }): Query<PaginationDetails>,
) -> Result<AxumJson<Vec<project::Response>>, Error> {
    let limit = limit.unwrap_or(u32::MAX) as i64;
    let page = page.unwrap_or(0) as i64;
    let projects = service
        // The `offset` is page size * amount of pages
        .iter_user_projects_detailed(&name, limit * page, limit)
        .await?
        .map(|project| project::Response {
            id: project.0.to_uppercase(),
            name: project.1.to_string(),
            idle_minutes: project.2.idle_minutes(),
            state: project.2.into(),
        })
        .collect();

    Ok(AxumJson(projects))
}

#[instrument(skip_all, fields(shuttle.project.name = %project_name))]
async fn create_project(
    State(RouterState {
        service, sender, ..
    }): State<RouterState>,
    User { name, claim, .. }: User,
    CustomErrorPath(project_name): CustomErrorPath<ProjectName>,
    AxumJson(config): AxumJson<project::Config>,
) -> Result<AxumJson<project::Response>, Error> {
    let is_cch_project = project_name.is_cch_project();

    // Check that the user is within their project limits.
    let can_create_project = claim.can_create_project(
        service
            .get_project_count(&name)
            .await?
            .saturating_sub(is_cch_project as i64)
            .try_into()
            .unwrap(),
    );

    if !claim.is_admin() {
        service.has_capacity(is_cch_project, &claim.tier).await?;
    }

    let project = service
        .create_project(
            project_name.clone(),
            name.clone(),
            claim.is_admin(),
            can_create_project,
            if is_cch_project {
                5
            } else {
                config.idle_minutes
            },
        )
        .await?;
    let idle_minutes = project.state.idle_minutes();

    service
        .new_task()
        .project(project_name.clone())
        .and_then(task::run_until_done())
        .and_then(task::start_idle_deploys())
        .send(&sender)
        .await?;

    let response = project::Response {
        id: project.project_id.to_string().to_uppercase(),
        name: project_name.to_string(),
        state: project.state.into(),
        idle_minutes,
    };

    Ok(AxumJson(response))
}

#[instrument(skip_all, fields(shuttle.project.name = %project_name))]
async fn destroy_project(
    State(RouterState {
        service, sender, ..
    }): State<RouterState>,
    ScopedUser {
        scope: project_name,
        ..
    }: ScopedUser,
) -> Result<AxumJson<project::Response>, Error> {
    let project = service.find_project(&project_name).await?;
    let idle_minutes = project.state.idle_minutes();

    let mut response = project::Response {
        id: project.project_id.to_uppercase(),
        name: project_name.to_string(),
        state: project.state.into(),
        idle_minutes,
    };

    if response.state == shuttle_common::models::project::State::Destroyed {
        return Ok(AxumJson(response));
    }

    // if project exists and isn't `Destroyed`, send destroy task
    service
        .new_task()
        .project(project_name)
        .and_then(task::destroy())
        .send(&sender)
        .await?;

    response.state = shuttle_common::models::project::State::Destroying;

    Ok(AxumJson(response))
}

#[derive(Deserialize)]
struct DeleteProjectParams {
    // Was added in v0.30.0
    // We have not needed it since 0.34.1, but have to keep in for any old CLI users
    #[allow(dead_code)]
    dry_run: Option<bool>,
}

#[instrument(skip_all, fields(shuttle.project.name = %scoped_user.scope))]
async fn delete_project(
    State(state): State<RouterState>,
    scoped_user: ScopedUser,
    Query(DeleteProjectParams { dry_run }): Query<DeleteProjectParams>,
    req: Request<Body>,
) -> Result<AxumJson<String>, Error> {
    // Don't do the dry run that might come from older CLIs
    if dry_run.is_some_and(|d| d) {
        return Ok(AxumJson("dry run is no longer supported".to_owned()));
    }

    let project_name = scoped_user.scope.clone();
    let project = state.service.find_project(&project_name).await?;
    let project_id =
        Ulid::from_string(&project.project_id).expect("stored project id to be a valid ULID");

    // Try to startup destroyed or errored projects
    let project_deletable = project.state.is_ready() || project.state.is_stopped();
    if !(project_deletable) {
        let handle = state
            .service
            .new_task()
            .project(project_name.clone())
            .and_then(task::restart(project_id))
            .send(&state.sender)
            .await?;

        // Wait for the project to be ready
        handle.await;

        let new_state = state.service.find_project(&project_name).await?;

        if !new_state.state.is_ready() {
            return Err(Error::from_kind(ErrorKind::ProjectCorrupted));
        }
    }

    let service = state.service.clone();
    let sender = state.sender.clone();

    let project_caller =
        ProjectCaller::new(state.clone(), scoped_user.clone(), req.headers()).await?;

    // check that a deployment is not running
    let mut deployments = project_caller.get_deployment_list().await?;
    deployments.sort_by_key(|d| d.last_update);

    // Make sure no deployment is in the building pipeline
    let has_bad_state = deployments.iter().any(|d| {
        !matches!(
            d.state,
            deployment::State::Running
                | deployment::State::Completed
                | deployment::State::Crashed
                | deployment::State::Stopped
        )
    });

    if has_bad_state {
        return Err(Error::from_kind(ErrorKind::ProjectHasBuildingDeployment));
    }

    let running_deployments = deployments
        .into_iter()
        .filter(|d| d.state == deployment::State::Running);

    for running_deployment in running_deployments {
        let res = project_caller
            .stop_deployment(&running_deployment.id)
            .await?;

        if res.status() != StatusCode::OK {
            return Err(Error::from_kind(ErrorKind::ProjectHasRunningDeployment));
        }
    }

    // check if any resources exist
    let resources = project_caller.get_resources().await?;
    let mut delete_fails = Vec::new();

    for resource in resources {
        let resource_type = resource.r#type.to_string();
        let res = project_caller.delete_resource(&resource_type).await?;

        if res.status() != StatusCode::OK {
            delete_fails.push(resource_type)
        }
    }

    if !delete_fails.is_empty() {
        return Err(Error::from_kind(ErrorKind::ProjectHasResources(
            delete_fails,
        )));
    }

    let task = service
        .new_task()
        .project(project_name.clone())
        .and_then(task::delete_project())
        .send(&sender)
        .await?;
    task.await;

    service.delete_project(&project_name).await?;

    Ok(AxumJson("project successfully deleted".to_owned()))
}

#[instrument(skip_all, fields(shuttle.project.name = %scoped_user.scope))]
async fn override_create_service(
    state: State<RouterState>,
    scoped_user: ScopedUser,
    req: Request<Body>,
) -> Result<Response<Body>, Error> {
    let account_name = scoped_user.user.claim.sub.clone();
    let posthog_client = state.posthog_client.clone();
    tokio::spawn(async move {
        let event = async_posthog::Event::new("shuttle_api_start_deployment", &account_name);

        if let Err(err) = posthog_client.capture(event).await {
            error!(
                error = &err as &dyn std::error::Error,
                "failed to send event to posthog"
            )
        };
    });

    route_project(state, scoped_user, req).await
}

#[instrument(skip_all, fields(shuttle.project.name = %scoped_user.scope))]
async fn override_get_delete_service(
    state: State<RouterState>,
    scoped_user: ScopedUser,
    req: Request<Body>,
) -> Result<Response<Body>, Error> {
    let project_name = scoped_user.scope.to_string();
    let service = state.service.clone();
    let ctx = state.service.context().clone();
    let ContainerSettings { fqdn: public, .. } = ctx.container_settings();
    let mut res = route_project(state, scoped_user, req).await?;

    // inject the (most relevant) URI that this project is being served on
    let uri = service
        .find_custom_domain_for_project(&project_name)
        .await
        .unwrap_or_default() // use project name if domain lookup fails
        .map(|c| format!("https://{}", c.fqdn))
        .unwrap_or_else(|| format!("https://{project_name}.{public}"));
    let body = hyper::body::to_bytes(res.body_mut()).await.unwrap();
    let mut json: service::Summary =
        serde_json::from_slice(body.as_bytes()).expect("valid service response from deployer");
    json.uri = uri;

    let bytes = serde_json::to_vec(&json).unwrap();
    let len = res
        .headers_mut()
        .entry("content-length")
        .or_insert(0.into());
    *len = bytes.len().into();
    *res.body_mut() = bytes.into();

    Ok(res)
}

#[instrument(skip_all, fields(shuttle.project.name = %scoped_user.scope))]
async fn route_project(
    State(RouterState {
        service, sender, ..
    }): State<RouterState>,
    scoped_user: ScopedUser,
    req: Request<Body>,
) -> Result<Response<Body>, Error> {
    let project_name = scoped_user.scope;
    let is_cch_project = project_name.is_cch_project();

    if !scoped_user.user.claim.is_admin() {
        service
            .has_capacity(is_cch_project, &scoped_user.user.claim.tier)
            .await?;
    }

    let project = service.find_or_start_project(&project_name, sender).await?;
    service
        .route(&project.state, &project_name, &scoped_user.user.name, req)
        .await
}

async fn get_status(
    State(RouterState {
        sender, service, ..
    }): State<RouterState>,
) -> Response<Body> {
    let mut statuses = Vec::new();
    // Compute gateway status.
    if sender.is_closed() || sender.capacity() == 0 {
        statuses.push((SHUTTLE_GATEWAY_VARIANT, StatusResponse::unhealthy()));
    } else if sender.capacity() < WORKER_QUEUE_SIZE - SVC_DEGRADED_THRESHOLD {
        statuses.push((SHUTTLE_GATEWAY_VARIANT, StatusResponse::degraded()));
    } else {
        statuses.push((SHUTTLE_GATEWAY_VARIANT, StatusResponse::healthy()));
    };

    // Compute provisioner status.
    let provisioner_status = if let Ok(channel) = service.provisioner_host().connect().await {
        let channel = ServiceBuilder::new().service(channel);
        let mut provisioner_client = ProvisionerClient::new(channel);
        if provisioner_client.health_check(Ping {}).await.is_ok() {
            StatusResponse::healthy()
        } else {
            StatusResponse::unhealthy()
        }
    } else {
        StatusResponse::unhealthy()
    };

    statuses.push(("shuttle-provisioner", provisioner_status));

    // Compute auth status.
    let auth_status = {
        let response = AUTH_CLIENT.get(service.auth_uri().clone()).await;
        match response {
            Ok(response) if response.status() == 200 => StatusResponse::healthy(),
            Ok(_) | Err(_) => StatusResponse::unhealthy(),
        }
    };

    statuses.push(("shuttle-auth", auth_status));

    let body = serde_json::to_vec(&statuses).expect("could not make a json out of the statuses");
    Response::builder()
        .body(body.into())
        .expect("could not make a response with the status check response")
}

#[instrument(skip_all)]
async fn post_load(
    State(RouterState { running_builds, .. }): State<RouterState>,
    AxumJson(build): AxumJson<stats::LoadRequest>,
) -> Result<AxumJson<stats::LoadResponse>, Error> {
    let mut running_builds = running_builds.lock().await;

    trace!(id = %build.id, "checking build queue");
    let mut load = calculate_capacity(&mut running_builds);

    if load.has_capacity
        && running_builds
            .insert(build.id, (), Duration::from_secs(60 * EXP_MINUTES as u64))
            .is_none()
    {
        // Only increase when an item was not already in the queue
        load.builds_count += 1;
    }

    Ok(AxumJson(load))
}

#[instrument(skip_all)]
async fn delete_load(
    State(RouterState { running_builds, .. }): State<RouterState>,
    AxumJson(build): AxumJson<stats::LoadRequest>,
) -> Result<AxumJson<stats::LoadResponse>, Error> {
    let mut running_builds = running_builds.lock().await;
    running_builds.remove(&build.id);

    trace!(id = %build.id, "removing from build queue");
    let load = calculate_capacity(&mut running_builds);

    Ok(AxumJson(load))
}

#[instrument(skip_all)]
async fn get_load_admin(
    State(RouterState { running_builds, .. }): State<RouterState>,
) -> Result<AxumJson<stats::LoadResponse>, Error> {
    let mut running_builds = running_builds.lock().await;

    let load = calculate_capacity(&mut running_builds);

    Ok(AxumJson(load))
}

#[instrument(skip_all)]
async fn delete_load_admin(
    State(RouterState { running_builds, .. }): State<RouterState>,
) -> Result<AxumJson<stats::LoadResponse>, Error> {
    let mut running_builds = running_builds.lock().await;
    running_builds.clear();

    let load = calculate_capacity(&mut running_builds);

    Ok(AxumJson(load))
}

fn calculate_capacity(running_builds: &mut MutexGuard<TtlCache<Uuid, ()>>) -> stats::LoadResponse {
    let active = running_builds.iter().count();
    let capacity = running_builds.capacity();
    let has_capacity = active < capacity;

    stats::LoadResponse {
        builds_count: active,
        has_capacity,
    }
}

#[instrument(skip_all)]
async fn revive_projects(
    State(RouterState {
        service, sender, ..
    }): State<RouterState>,
) -> Result<(), Error> {
    crate::project::exec::revive(service, sender)
        .await
        .map_err(|_| Error::from_kind(ErrorKind::Internal))
}

#[instrument(skip_all)]
async fn idle_cch_projects(
    State(RouterState {
        service, sender, ..
    }): State<RouterState>,
) -> Result<(), Error> {
    crate::project::exec::idle_cch(service, sender)
        .await
        .map_err(|_| Error::from_kind(ErrorKind::Internal))
}

#[instrument(skip_all)]
async fn destroy_projects(
    State(RouterState {
        service, sender, ..
    }): State<RouterState>,
) -> Result<(), Error> {
    crate::project::exec::destroy(service, sender)
        .await
        .map_err(|_| Error::from_kind(ErrorKind::Internal))
}

#[instrument(skip_all, fields(%email, ?acme_server))]
async fn create_acme_account(
    Extension(acme_client): Extension<AcmeClient>,
    Path(email): Path<String>,
    AxumJson(acme_server): AxumJson<Option<String>>,
) -> Result<AxumJson<serde_json::Value>, Error> {
    let res = acme_client.create_account(&email, acme_server).await?;

    Ok(AxumJson(res))
}

#[instrument(skip_all, fields(shuttle.project.name = %project_name, %fqdn))]
async fn request_custom_domain_acme_certificate(
    State(RouterState { service, .. }): State<RouterState>,
    Extension(acme_client): Extension<AcmeClient>,
    Extension(resolver): Extension<Arc<GatewayCertResolver>>,
    CustomErrorPath((project_name, fqdn)): CustomErrorPath<(ProjectName, String)>,
    AxumJson(credentials): AxumJson<AccountCredentials<'_>>,
) -> Result<String, Error> {
    let fqdn: FQDN = fqdn
        .parse()
        .map_err(|_err| Error::from(ErrorKind::InvalidCustomDomain))?;

    let (certs, private_key) = service
        .create_custom_domain_certificate(&fqdn, &acme_client, &project_name, credentials)
        .await?;

    let mut buf = Vec::new();
    buf.extend(certs.as_bytes());
    buf.extend(private_key.as_bytes());
    resolver
        .serve_pem(&fqdn.to_string(), Cursor::new(buf))
        .await?;
    Ok(format!(
        r#""New certificate created for {} project.""#,
        project_name
    ))
}

#[instrument(skip_all, fields(shuttle.project.name = %project_name, %fqdn))]
async fn renew_custom_domain_acme_certificate(
    State(RouterState { service, .. }): State<RouterState>,
    Extension(acme_client): Extension<AcmeClient>,
    Extension(resolver): Extension<Arc<GatewayCertResolver>>,
    CustomErrorPath((project_name, fqdn)): CustomErrorPath<(ProjectName, String)>,
    AxumJson(credentials): AxumJson<AccountCredentials<'_>>,
) -> Result<String, Error> {
    let fqdn: FQDN = fqdn
        .parse()
        .map_err(|_err| Error::from(ErrorKind::InvalidCustomDomain))?;
    // Try retrieve the current certificate if any.
    match service.project_details_for_custom_domain(&fqdn).await {
        Ok(CustomDomain {
            mut certificate,
            private_key,
            ..
        }) => {
            certificate.push('\n');
            certificate.push('\n');
            certificate.push_str(private_key.as_str());
            let (_, pem) = parse_x509_pem(certificate.as_bytes()).map_err(|err| {
                Error::custom(
                    ErrorKind::Internal,
                    format!("Error while parsing the pem certificate for {project_name}: {err}"),
                )
            })?;

            let (_, x509_cert_chain) =
                parse_x509_certificate(pem.contents.as_bytes()).map_err(|err| {
                    Error::custom(
                        ErrorKind::Internal,
                        format!(
                            "Error while parsing the certificate chain for {project_name}: {err}"
                        ),
                    )
                })?;

            let diff = x509_cert_chain
                .validity()
                .not_after
                .sub(ASN1Time::now())
                .unwrap_or_default();

            // Renew only when the difference is `None` (meaning certificate expired) or we're within the last 30 days of validity.
            if diff.whole_days() <= RENEWAL_VALIDITY_THRESHOLD_IN_DAYS {
                return match acme_client
                    .create_certificate(&fqdn.to_string(), ChallengeType::Http01, credentials)
                    .await
                {
                    // If successfully created, save the certificate in memory to be
                    // served in the future.
                    Ok((certs, private_key)) => {
                        service
                            .create_custom_domain(&project_name, &fqdn, &certs, &private_key)
                            .await?;

                        let mut buf = Vec::new();
                        buf.extend(certs.as_bytes());
                        buf.extend(private_key.as_bytes());
                        resolver
                            .serve_pem(&fqdn.to_string(), Cursor::new(buf))
                            .await?;
                        Ok(format!(
                            r#""Certificate renewed for {} project.""#,
                            project_name
                        ))
                    }
                    Err(err) => Err(err.into()),
                };
            } else {
                Ok(format!(
                    r#""Certificate renewal skipped, {} project certificate still valid for {} days.""#,
                    project_name, diff
                ))
            }
        }
        Err(err) => Err(err),
    }
}

#[instrument(skip_all)]
async fn renew_gateway_acme_certificate(
    State(RouterState { service, .. }): State<RouterState>,
    Extension(acme_client): Extension<AcmeClient>,
    Extension(resolver): Extension<Arc<GatewayCertResolver>>,
    AxumJson(credentials): AxumJson<AccountCredentials<'_>>,
) -> Result<String, Error> {
    let account = AccountWrapper::from(credentials).0;
    let certs = service
        .fetch_certificate(&acme_client, account.credentials())
        .await;
    // Safe to unwrap because a 'ChainAndPrivateKey' is built from a PEM.
    let chain_and_pk = certs.into_pem().unwrap();

    let (_, pem) = parse_x509_pem(chain_and_pk.as_bytes())
        .unwrap_or_else(|_| panic!("Malformed existing PEM certificate for the gateway."));
    let (_, x509_cert) = parse_x509_certificate(pem.contents.as_bytes())
        .unwrap_or_else(|_| panic!("Malformed existing X509 certificate for the gateway."));

    // We compute the difference between the certificate expiry date and current timestamp because we want to trigger the
    // gateway certificate renewal only during it's last 30 days of validity or if the certificate is expired.
    let diff = x509_cert.validity().not_after.sub(ASN1Time::now());

    // Renew only when the difference is `None` (meaning certificate expired) or we're within the last 30 days of validity.
    if diff.is_none()
        || diff
            .expect("to be Some given we checked for None previously")
            .whole_days()
            <= RENEWAL_VALIDITY_THRESHOLD_IN_DAYS
    {
        let tls_path = service.state_location.join("ssl.pem");
        let certs = service
            .create_certificate(&acme_client, account.credentials())
            .await;
        resolver
            .serve_default_der(certs.clone())
            .await
            .expect("Failed to serve the default certs");
        certs
            .save_pem(&tls_path)
            .expect("to save the certificate locally");
        return Ok(r#""Renewed the gateway certificate.""#.to_string());
    }

    Ok(format!(
        "\"Gateway certificate was not renewed. There are {} days until the certificate expires.\"",
        diff.expect("to be Some given we checked for None previously")
            .whole_days()
    ))
}

async fn get_projects(
    State(RouterState { service, .. }): State<RouterState>,
) -> Result<AxumJson<Vec<ProjectResponse>>, Error> {
    let projects = service
        .iter_projects_detailed()
        .await?
        .map(Into::into)
        .collect();

    Ok(AxumJson(projects))
}

#[derive(Clone)]
pub(crate) struct RouterState {
    pub service: Arc<GatewayService>,
    pub sender: Sender<BoxedTask>,
    pub running_builds: Arc<Mutex<TtlCache<Uuid, ()>>>,
    pub posthog_client: async_posthog::Client,
}

pub struct ApiBuilder {
    router: Router<RouterState>,
    service: Option<Arc<GatewayService>>,
    sender: Option<Sender<BoxedTask>>,
    posthog_client: Option<async_posthog::Client>,
    bind: Option<SocketAddr>,
}

impl Default for ApiBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiBuilder {
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            service: None,
            sender: None,
            posthog_client: None,
            bind: None,
        }
    }

    pub fn with_acme(mut self, acme: AcmeClient, resolver: Arc<GatewayCertResolver>) -> Self {
        self.router = self
            .router
            .route(
                "/admin/acme/:email",
                post(create_acme_account.layer(ScopedLayer::new(vec![Scope::AcmeCreate]))),
            )
            .route(
                "/admin/acme/request/:project_name/:fqdn",
                post(
                    request_custom_domain_acme_certificate
                        .layer(ScopedLayer::new(vec![Scope::CustomDomainCreate])),
                ),
            )
            .route(
                "/admin/acme/renew/:project_name/:fqdn",
                post(
                    renew_custom_domain_acme_certificate
                        .layer(ScopedLayer::new(vec![Scope::CustomDomainCertificateRenew])),
                ),
            )
            .route(
                "/admin/acme/gateway/renew",
                post(
                    renew_gateway_acme_certificate
                        .layer(ScopedLayer::new(vec![Scope::GatewayCertificateRenew])),
                ),
            )
            .layer(Extension(acme))
            .layer(Extension(resolver));
        self
    }

    pub fn with_service(mut self, service: Arc<GatewayService>) -> Self {
        self.service = Some(service);
        self
    }

    pub fn with_sender(mut self, sender: Sender<BoxedTask>) -> Self {
        self.sender = Some(sender);
        self
    }

    pub fn with_posthog_client(mut self, posthog_client: async_posthog::Client) -> Self {
        self.posthog_client = Some(posthog_client);
        self
    }

    pub fn binding_to(mut self, addr: SocketAddr) -> Self {
        self.bind = Some(addr);
        self
    }

    pub fn with_default_traces(mut self) -> Self {
        self.router = self.router.route_layer(from_extractor::<Metrics>()).layer(
            TraceLayer::new(|request| {
                request_span!(
                    request,
                    account.name = field::Empty,
                    request.params.project_name = field::Empty,
                    request.params.account_name = field::Empty,
                )
            })
            .with_propagation()
            .build(),
        );
        self
    }

    pub fn with_default_routes(mut self) -> Self {
        let admin_routes = Router::new()
            .route("/projects", get(get_projects))
            .route("/revive", post(revive_projects))
            .route("/destroy", post(destroy_projects))
            .route("/idle-cch", post(idle_cch_projects))
            .route("/stats/load", get(get_load_admin).delete(delete_load_admin))
            .layer(ScopedLayer::new(vec![Scope::Admin]));

        const CARGO_SHUTTLE_VERSION: &str = env!("CARGO_PKG_VERSION");

        let project_routes = Router::new()
            .route(
                "/projects/:project_name",
                get(get_project.layer(ScopedLayer::new(vec![Scope::Project])))
                    .delete(destroy_project.layer(ScopedLayer::new(vec![Scope::ProjectWrite])))
                    .post(create_project.layer(ScopedLayer::new(vec![Scope::ProjectWrite]))),
            )
            .route(
                "/projects/:project_name/delete",
                delete(delete_project.layer(ScopedLayer::new(vec![Scope::ProjectWrite]))),
            )
            .route("/projects/name/:project_name", get(check_project_name))
            .route(
                // catch these deployer endpoints for extra metrics or processing before/after being proxied
                "/projects/:project_name/services/:service_name",
                post(override_create_service)
                    .get(override_get_delete_service)
                    .delete(override_get_delete_service),
            )
            .route("/projects/:project_name/*any", any(route_project))
            .route_layer(middleware::from_fn(project_name_tracing_layer));

        self.router = self
            .router
            .route("/", get(get_status))
            .merge(project_routes)
            .route(
                "/versions",
                get(|| async {
                    axum::Json(VersionInfo {
                        gateway: env!("CARGO_PKG_VERSION").parse().unwrap(),
                        // For now, these use the same version as gateway (we release versions in lockstep).
                        // Only one version is officially compatible, but more are in reality.
                        cargo_shuttle: env!("CARGO_PKG_VERSION").parse().unwrap(),
                        deployer: env!("CARGO_PKG_VERSION").parse().unwrap(),
                        runtime: CARGO_SHUTTLE_VERSION.parse().unwrap(),
                    })
                }),
            )
            .route(
                "/version/cargo-shuttle",
                get(|| async { CARGO_SHUTTLE_VERSION }),
            )
            .route(
                "/projects",
                get(get_projects_list.layer(ScopedLayer::new(vec![Scope::Project]))),
            )
            .route("/stats/load", post(post_load).delete(delete_load))
            .nest("/admin", admin_routes);

        self
    }

    pub fn with_auth_service(mut self, auth_uri: Uri, gateway_admin_key: String) -> Self {
        let auth_public_key = AuthPublicKey::new(auth_uri.clone());

        let jwt_cache_manager = CacheManager::new(1000);

        self.router = self
            .router
            .layer(JwtAuthenticationLayer::new(auth_public_key))
            .layer(ShuttleAuthLayer::new(
                auth_uri,
                gateway_admin_key,
                Arc::new(Box::new(jwt_cache_manager)),
            ));

        self
    }

    pub fn into_router(self) -> Router {
        let service = self.service.expect("a GatewayService is required");
        let sender = self.sender.expect("a task Sender is required");
        let posthog_client = self.posthog_client.expect("a task Sender is required");

        // Allow about 4 cores per build, but use at most 75% (* 3 / 4) of all cores and at least 1 core
        // Assumes each builder (deployer) is assigned 4 cores
        let concurrent_builds: usize = (num_cpus::get() * 3 / 4 / 4).max(1);

        let running_builds = Arc::new(Mutex::new(TtlCache::new(concurrent_builds)));

        self.router.with_state(RouterState {
            service,
            sender,
            posthog_client,
            running_builds,
        })
    }

    pub fn serve(self) -> impl Future<Output = Result<(), hyper::Error>> {
        let bind = self.bind.expect("a socket address to bind to is required");
        let router = self.into_router();
        axum::Server::bind(&bind).serve(router.into_make_service())
    }
}
