use anyhow::{anyhow, Context};
use axum::{
    async_trait, extract,
    headers::{authorization::Bearer, Authorization},
    routing::get,
    Router, TypedHeader,
};
use bollard::Docker;
use flate2::{write::GzEncoder, Compression};
use fqdn::FQDN;
use futures::{TryFutureExt, TryStreamExt};
use http::{uri::Scheme, Method, Request, Response, StatusCode, Uri};
use hyper::{client::HttpConnector, Body, Client as HyperClient};
use jsonwebtoken::EncodingKey;
use once_cell::sync::Lazy;
use rand::distributions::{Alphanumeric, DistString, Distribution, Uniform};
use ring::signature::{self, Ed25519KeyPair, KeyPair};
use shuttle_common::{
    backends::auth::ConvertResponse,
    claims::{AccountTier, Claim},
    models::{deployment::DeploymentRequest, project, service},
};
use shuttle_common_tests::postgres::DockerInstance;
use shuttle_gateway::{
    acme::AcmeClient,
    api::latest::ApiBuilder,
    args::{ContextArgs, StartArgs, UseTls},
    project::Project,
    service::{ContainerSettings, GatewayService, MIGRATIONS},
    task::BoxedTask,
    worker::Worker,
    DockerContext, Error,
};
use shuttle_proto::test_utils::resource_recorder::get_mocked_resource_recorder;
use sqlx::{query, PgPool};
use std::{
    collections::HashMap,
    env,
    fs::{canonicalize, read_dir},
    io,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use test_context::AsyncTestContext;
use tokio::{
    sync::mpsc::{channel, Sender},
    time::sleep,
};
use tower::Service;

static PG: Lazy<DockerInstance> = Lazy::new(DockerInstance::default);
#[ctor::dtor]
fn cleanup() {
    PG.cleanup();
}

/// Initialize the connection pool to a Postgres database at the given URI.
pub(crate) async fn pgpool_init(db_uri: &str) -> io::Result<PgPool> {
    let opts = db_uri
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let pool = PgPool::connect_with(opts)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    MIGRATIONS
        .run(&pool)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    Ok(pool)
}

macro_rules! value_block_helper {
    ($next:ident, $block:block) => {
        $block
    };
    ($next:ident,) => {
        $next
    };
}

macro_rules! assert_stream_matches {
    (
        $stream:ident,
        $(#[assertion = $assert:literal])?
            $($pattern:pat_param)|+ $(if $guard:expr)? $(=> $more:block)?,
    ) => {{
        let next = ::futures::stream::StreamExt::next(&mut $stream)
            .await
            .expect("Stream ended before the last of assertions");

        match &next {
            $($pattern)|+ $(if $guard)? => {
                print!("{}", ::colored::Colorize::green(::colored::Colorize::bold("[ok]")));
                $(print!(" {}", $assert);)?
                    print!("\n");
                crate::helpers::value_block_helper!(next, $($more)?)
            },
            _ => {
                eprintln!("{} {:#?}", ::colored::Colorize::red(::colored::Colorize::bold("[err]")), next);
                eprint!("{}", ::colored::Colorize::red(::colored::Colorize::bold("Assertion failed")));
                $(eprint!(": {}", $assert);)?
                    eprint!("\n");
                panic!("State mismatch")
            }
        }
    }};
    (
        $stream:ident,
        $(#[$($meta:tt)*])*
            $($pattern:pat_param)|+ $(if $guard:expr)? $(=> $more:block)?,
        $($(#[$($metas:tt)*])* $($patterns:pat_param)|+ $(if $guards:expr)? $(=> $mores:block)?,)+
    ) => {{
        assert_stream_matches!(
            $stream,
            $(#[$($meta)*])* $($pattern)|+ $(if $guard)? => {
                $($more)?
                    assert_stream_matches!(
                        $stream,
                        $($(#[$($metas)*])* $($patterns)|+ $(if $guards)? $(=> $mores)?,)+
                    )
            },
        )
    }};
}

macro_rules! assert_matches {
    {
        $ctx:ident,
        $state:expr,
        $($(#[$($meta:tt)*])* $($patterns:pat_param)|+ $(if $guards:expr)? $(=> $mores:block)?,)+
    } => {{
        let state = $state;
        let mut stream = StateExt::into_stream(state, &$ctx);
        assert_stream_matches!(
            stream,
            $($(#[$($meta)*])* $($patterns)|+ $(if $guards)? $(=> $mores)?,)+
        )
    }}
}

macro_rules! assert_err_kind {
    {
        $left:expr, ErrorKind::$right:ident
    } => {{
        let left: Result<_, shuttle_gateway::Error> = $left;
        assert_eq!(
            left.map_err(|err| err.kind()),
            Err(shuttle_common::models::error::ErrorKind::$right)
        );
    }};
}

macro_rules! timed_loop {
    (wait: $wait:literal$(, max: $max:literal)?, $block:block) => {{
        #[allow(unused_mut)]
        #[allow(unused_variables)]
        let mut tries = 0;
        loop {
            $block
                tries += 1;
            $(if tries > $max {
                panic!("timed out in the loop");
            })?
                ::tokio::time::sleep(::std::time::Duration::from_secs($wait)).await;
        }
    }};
}

pub(crate) use {
    assert_err_kind, assert_matches, assert_stream_matches, timed_loop, value_block_helper,
};

mod request_builder_ext {
    pub trait Sealed {}

    impl Sealed for axum::http::request::Builder {}

    impl<'r> Sealed for &'r mut axum::headers::HeaderMap {}

    impl<B> Sealed for axum::http::Request<B> {}
}

pub trait RequestBuilderExt: Sized + request_builder_ext::Sealed {
    fn with_header<H: axum::headers::Header>(self, header: &H) -> Self;
}

impl RequestBuilderExt for axum::http::request::Builder {
    fn with_header<H: axum::headers::Header>(mut self, header: &H) -> Self {
        self.headers_mut().unwrap().with_header(header);
        self
    }
}

impl<'r> RequestBuilderExt for &'r mut axum::headers::HeaderMap {
    fn with_header<H: axum::headers::Header>(self, header: &H) -> Self {
        let mut buf = vec![];
        header.encode(&mut buf);
        self.append(H::name(), buf.pop().unwrap());
        self
    }
}

impl<B> RequestBuilderExt for Request<B> {
    fn with_header<H: axum::headers::Header>(mut self, header: &H) -> Self {
        self.headers_mut().with_header(header);
        self
    }
}

pub struct Client<C = HttpConnector, B = Body> {
    target: SocketAddr,
    hyper: Option<HyperClient<C, B>>,
}

impl<C, B> Client<C, B> {
    pub fn new<A: Into<SocketAddr>>(target: A) -> Self {
        Self {
            target: target.into(),
            hyper: None,
        }
    }

    pub fn with_hyper_client(mut self, client: HyperClient<C, B>) -> Self {
        self.hyper = Some(client);
        self
    }
}

impl Client<HttpConnector, Body> {
    pub async fn request(&self, mut req: Request<Body>) -> Result<Response<Vec<u8>>, hyper::Error> {
        if req.uri().authority().is_none() {
            let mut uri = req.uri().clone().into_parts();
            uri.scheme = Some(Scheme::HTTP);
            uri.authority = Some(self.target.to_string().parse().unwrap());
            *req.uri_mut() = Uri::from_parts(uri).unwrap();
        }
        self.hyper
            .as_ref()
            .unwrap()
            .request(req)
            .and_then(|mut resp| async move {
                let body = resp
                    .body_mut()
                    .try_fold(Vec::new(), |mut acc, x| async move {
                        acc.extend(x);
                        Ok(acc)
                    })
                    .await?;
                let (parts, _) = resp.into_parts();
                Ok(Response::from_parts(parts, body))
            })
            .await
    }
}

pub struct World {
    docker: Docker,
    settings: ContainerSettings,
    pub args: StartArgs,
    pool: PgPool,
    acme_client: AcmeClient,
    auth_service: Arc<Mutex<AuthService>>,
    auth_uri: Uri,
}

#[derive(Clone)]
pub struct WorldContext {
    pub docker: Docker,
    pub container_settings: ContainerSettings,
    pub auth_uri: Uri,
}

impl World {
    pub async fn new() -> Self {
        let docker = Docker::connect_with_local_defaults().unwrap();

        docker
            .list_images::<&str>(None)
            .await
            .context(anyhow!("A docker daemon does not seem accessible",))
            .unwrap();

        let control: i16 = Uniform::from(9000..10000).sample(&mut rand::thread_rng());
        let user = control + 1;
        let bouncer = user + 1;
        let auth_port = bouncer + 1;
        let control = format!("127.0.0.1:{control}").parse().unwrap();
        let user = format!("127.0.0.1:{user}").parse().unwrap();
        let bouncer = format!("127.0.0.1:{bouncer}").parse().unwrap();
        let auth: SocketAddr = format!("0.0.0.0:{auth_port}").parse().unwrap();
        let auth_uri: Uri = format!("http://{auth}").parse().unwrap();
        let resource_recorder_port = get_mocked_resource_recorder().await;

        let auth_service = AuthService::new(auth);
        auth_service
            .lock()
            .unwrap()
            .users
            .insert("gateway".to_string(), AccountTier::Deployer);

        let prefix = format!(
            "shuttle_test_{}_",
            Alphanumeric.sample_string(&mut rand::thread_rng(), 4)
        );

        let image = env::var("SHUTTLE_TESTS_RUNTIME_IMAGE")
            .unwrap_or_else(|_| "public.ecr.aws/shuttle-dev/deployer:latest".to_string());

        let network_name =
            env::var("SHUTTLE_TESTS_NETWORK").unwrap_or_else(|_| "shuttle_default".to_string());

        let provisioner_host = "provisioner".to_string();
        let builder_host = "builder".to_string();

        let docker_host = "/var/run/docker.sock".to_string();

        let args = StartArgs {
            control,
            user,
            bouncer,
            use_tls: UseTls::Disable,
            context: ContextArgs {
                docker_host,
                image,
                prefix,
                provisioner_host,
                builder_host,
                // The started containers need to reach auth on the host.
                // For this to work, the firewall should not be blocking traffic on the `SHUTTLE_TEST_NETWORK` interface.
                // The following command can be used on NixOs to allow traffic on the interface.
                // ```
                // sudo iptables -I nixos-fw -i <interface> -j nixos-fw-accept
                // ```
                //
                // Something like this should work on other systems.
                // ```
                // sudo iptables -I INPUT -i <interface> -j ACCEPT
                // ```
                auth_uri: format!("http://host.docker.internal:{auth_port}")
                    .parse()
                    .unwrap(),
                resource_recorder_uri: format!(
                    "http://host.docker.internal:{resource_recorder_port}"
                )
                .parse()
                .unwrap(),
                network_name,
                proxy_fqdn: FQDN::from_str("test.shuttleapp.rs").unwrap(),
                admin_key: "dummykey".to_string(),
                deploys_api_key: "gateway".to_string(),
                cch_container_limit: 1,
                soft_container_limit: 2,
                hard_container_limit: 3,

                // Allow access to the auth on the host
                extra_hosts: vec!["host.docker.internal:host-gateway".to_string()],
            },
        };

        let settings = ContainerSettings::builder().from_args(&args.context).await;

        let pool = pgpool_init(PG.get_unique_uri().as_str()).await.unwrap();

        let acme_client = AcmeClient::new();

        Self {
            docker,
            settings,
            args,
            pool,
            acme_client,
            auth_service,
            auth_uri,
        }
    }

    pub fn args(&self) -> ContextArgs {
        self.args.context.clone()
    }

    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    pub fn fqdn(&self) -> FQDN {
        self.args().proxy_fqdn
    }

    pub fn acme_client(&self) -> AcmeClient {
        self.acme_client.clone()
    }

    /// Create user with a specific tier
    pub fn create_user(&self, user: &str, tier: AccountTier) -> String {
        self.auth_service
            .lock()
            .unwrap()
            .users
            .insert(user.to_string(), tier);

        user.to_string()
    }

    /// Create a user with the given name and tier and return the authorization bearer for the user
    pub fn create_authorization_bearer(
        &self,
        user: &str,
        tier: AccountTier,
    ) -> Authorization<Bearer> {
        let user_key = self.create_user(user, tier);
        Authorization::bearer(&user_key).unwrap()
    }

    pub fn set_super_user(&self, user: &str) {
        if let Some(tier) = self.auth_service.lock().unwrap().users.get_mut(user) {
            *tier = AccountTier::Admin;
        }
    }

    /// Create a service and sender to handle tasks. Also starts up a worker to create actual Docker containers for all requests
    pub async fn service(&self) -> (Arc<GatewayService>, Sender<BoxedTask>) {
        let service = Arc::new(
            GatewayService::init(self.args(), self.pool(), "".into())
                .await
                .unwrap(),
        );
        let worker = Worker::new();

        let (sender, mut receiver) = channel(256);
        tokio::spawn({
            let worker_sender = worker.sender();
            async move {
                while let Some(work) = receiver.recv().await {
                    // Forward tasks to an actual worker
                    worker_sender
                        .send(work)
                        .await
                        .map_err(|_| "could not send work")
                        .unwrap();
                }
            }
        });

        let _worker = tokio::spawn(async move {
            worker.start().await.unwrap();
        });

        // Allow the spawns to start
        tokio::time::sleep(Duration::from_secs(1)).await;

        (service, sender)
    }

    /// Create a router to make API calls against
    pub fn router(&self, service: Arc<GatewayService>, sender: Sender<BoxedTask>) -> Router {
        ApiBuilder::new()
            .with_service(Arc::clone(&service))
            .with_sender(sender)
            .with_default_routes()
            .with_auth_service(self.context().auth_uri, "dummykey".to_string())
            .into_router()
    }

    pub fn client<A: Into<SocketAddr>>(addr: A) -> Client {
        let hyper = HyperClient::builder().build(HttpConnector::new());
        Client::new(addr).with_hyper_client(hyper)
    }

    pub fn context(&self) -> WorldContext {
        WorldContext {
            docker: self.docker.clone(),
            container_settings: self.settings.clone(),
            auth_uri: self.auth_uri.clone(),
        }
    }
}

#[async_trait]
impl DockerContext for WorldContext {
    fn docker(&self) -> &Docker {
        &self.docker
    }

    fn container_settings(&self) -> &ContainerSettings {
        &self.container_settings
    }

    async fn get_stats(&self, _container_id: &str) -> Result<u64, Error> {
        Ok(0)
    }
}

struct AuthService {
    users: HashMap<String, AccountTier>,
    encoding_key: EncodingKey,
    public_key: Vec<u8>,
}

impl AuthService {
    fn new(address: SocketAddr) -> Arc<Mutex<Self>> {
        let doc =
            signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let encoding_key = EncodingKey::from_ed_der(doc.as_ref());
        let pair = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        let public_key = pair.public_key().as_ref().to_vec();

        let this = Arc::new(Mutex::new(Self {
            users: HashMap::new(),
            encoding_key,
            public_key,
        }));

        let router = Router::new()
            .route(
                "/public-key",
                get(
                    |extract::State(state): extract::State<Arc<Mutex<Self>>>| async move {
                        state.lock().unwrap().public_key.clone()
                    },
                ),
            )
            .route(
                "/auth/key",
                get(
                    |extract::State(state): extract::State<Arc<Mutex<Self>>>,
                     TypedHeader(bearer): TypedHeader<Authorization<Bearer>>| async move {
                        let state = state.lock().unwrap();

                        if let Some(tier) = state.users.get(bearer.token()) {
                            let claim = Claim::new(
                                bearer.token().to_string(),
                                (*tier).into(),
                                *tier,
                                *tier,
                            );
                            let token = claim.into_token(&state.encoding_key)?;
                            Ok(serde_json::to_vec(&ConvertResponse { token }).unwrap())
                        } else {
                            Err(StatusCode::NOT_FOUND)
                        }
                    },
                ),
            )
            .with_state(this.clone());

        tokio::spawn(async move {
            axum::Server::bind(&address)
                .serve(router.into_make_service())
                .await
                .unwrap();
        });

        this
    }
}

/// Helper struct to wrap a bunch of commands to run against gateway's API
pub struct TestGateway {
    router: Router,
    authorization: Authorization<Bearer>,
    service: Arc<GatewayService>,
    sender: Sender<BoxedTask>,
    world: World,
}

impl TestGateway {
    /// Try to create a project with a given user and return the request response
    pub async fn try_user_create_project(
        &mut self,
        project_name: &str,
        authorization: &Authorization<Bearer>,
    ) -> StatusCode {
        self.router
            .call(
                Request::builder()
                    .method("POST")
                    .uri(format!("/projects/{project_name}"))
                    .header("Content-Type", "application/json")
                    .body("{\"idle_minutes\": 3}".into())
                    .unwrap()
                    .with_header(authorization),
            )
            .await
            .unwrap()
            .status()
    }

    /// Try to create a project and return the request response
    pub async fn try_create_project(&mut self, project_name: &str) -> StatusCode {
        self.try_user_create_project(project_name, &self.authorization.clone())
            .await
    }

    /// Create a new project using the given user and return its helping wrapper
    pub async fn user_create_project(
        &mut self,
        project_name: &str,
        authorization: &Authorization<Bearer>,
    ) -> TestProject {
        let status_code = self
            .try_user_create_project(project_name, authorization)
            .await;

        assert_eq!(
            status_code,
            StatusCode::OK,
            "could not create {project_name}"
        );

        let mut this = TestProject {
            authorization: authorization.clone(),
            project_name: project_name.to_string(),
            router: self.router.clone(),
            pool: self.world.pool(),
            service: self.service.clone(),
            sender: self.sender.clone(),
        };

        this.wait_for_state(project::State::Ready).await;

        this
    }

    /// Create a new project in the test world and return its helping wrapper
    pub async fn create_project(&mut self, project_name: &str) -> TestProject {
        self.user_create_project(project_name, &self.authorization.clone())
            .await
    }

    /// Get authorization bearer for a new user
    pub fn new_authorization_bearer(&self, user: &str, tier: AccountTier) -> Authorization<Bearer> {
        self.world.create_authorization_bearer(user, tier)
    }
}

#[async_trait]
impl AsyncTestContext for TestGateway {
    async fn setup() -> Self {
        let world = World::new().await;

        let (service, sender) = world.service().await;

        let router = world.router(service.clone(), sender.clone());
        let authorization = world.create_authorization_bearer("neo", AccountTier::Basic);

        Self {
            router,
            authorization,
            service,
            sender,
            world,
        }
    }

    async fn teardown(mut self) {}
}

/// Helper struct to wrap a bunch of commands to run against a test project
pub struct TestProject {
    router: Router,
    authorization: Authorization<Bearer>,
    project_name: String,
    pool: PgPool,
    service: Arc<GatewayService>,
    sender: Sender<BoxedTask>,
}

impl TestProject {
    /// Wait a few seconds for the project to enter the desired state
    pub async fn wait_for_state(&mut self, state: project::State) {
        let mut tries = 0;
        let project_name = &self.project_name;

        loop {
            let resp = self
                .router
                .call(
                    Request::get(format!("/projects/{project_name}"))
                        .with_header(&self.authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            let body = hyper::body::to_bytes(resp.into_body()).await.unwrap();
            let project: project::Response = serde_json::from_slice(&body).unwrap();

            if project.state == state {
                break;
            }

            tries += 1;
            if tries > 12 {
                panic!("timed out waiting for state {state}");
            }

            sleep(Duration::from_secs(1)).await;
        }
    }

    /// Is this project still available - aka has it been deleted
    pub async fn is_missing(&mut self) -> bool {
        let project_name = &self.project_name;

        let resp = self
            .router
            .call(
                Request::get(format!("/projects/{project_name}"))
                    .with_header(&self.authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        resp.status() == StatusCode::NOT_FOUND
    }

    /// Destroy / stop a project. Like `cargo shuttle project stop`
    pub async fn destroy_project(&mut self) {
        let TestProject {
            router,
            authorization,
            project_name,
            ..
        } = self;

        router
            .call(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/projects/{project_name}"))
                    .body(Body::empty())
                    .unwrap()
                    .with_header(authorization),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        self.wait_for_state(project::State::Destroyed).await;
    }

    /// Send a request to the router for this project
    pub async fn router_call(&mut self, method: Method, sub_path: &str) -> StatusCode {
        let project_name = &self.project_name;

        self.router
            .call(
                Request::builder()
                    .method(method)
                    .uri(format!("/projects/{project_name}{sub_path}"))
                    .body(Body::empty())
                    .unwrap()
                    .with_header(&self.authorization),
            )
            .map_ok(|resp| resp.status())
            .await
            .unwrap()
    }

    /// Just deploy the code at the path and don't wait for it to finish
    pub async fn just_deploy(&mut self, path: &str) {
        let path = canonicalize(path).expect("deploy path to be valid");
        let name = path.file_name().unwrap().to_str().unwrap();
        let enc = GzEncoder::new(Vec::new(), Compression::fast());
        let mut tar = tar::Builder::new(enc);

        for dir_entry in read_dir(&path).unwrap() {
            let dir_entry = dir_entry.unwrap();
            if dir_entry.file_name() != "target" {
                let path = format!("{name}/{}", dir_entry.file_name().to_str().unwrap());

                if dir_entry.file_type().unwrap().is_dir() {
                    tar.append_dir_all(path, dir_entry.path()).unwrap();
                } else {
                    tar.append_path_with_name(dir_entry.path(), path).unwrap();
                }
            }
        }

        let enc = tar.into_inner().unwrap();
        let bytes = enc.finish().unwrap();

        println!("{name}: finished getting archive for test");

        let project_name = &self.project_name;
        let deployment_req = rmp_serde::to_vec(&DeploymentRequest {
            data: bytes,
            no_test: true,
            ..Default::default()
        })
        .expect("to serialize DeploymentRequest as a MessagePack byte vector");

        self.router
            .call(
                Request::builder()
                    .method(Method::POST)
                    .header("Transfer-Encoding", "chunked")
                    .uri(format!("/projects/{project_name}/services/{project_name}"))
                    .body(deployment_req.into())
                    .unwrap()
                    .with_header(&self.authorization),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();
    }

    /// Deploy the code at the path to the project and wait for it to finish
    pub async fn deploy(&mut self, path: &str) {
        self.just_deploy(path).await;

        let project_name = &self.project_name;

        // Wait for deployment to be up
        let mut tries = 0;

        loop {
            let resp = self
                .router
                .call(
                    Request::get(format!("/projects/{project_name}/services/{project_name}"))
                        .with_header(&self.authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            let body = hyper::body::to_bytes(resp.into_body()).await.unwrap();
            let service: service::Summary = serde_json::from_slice(&body).unwrap();

            if service.deployment.is_some() {
                break;
            }

            tries += 1;
            // We should consider making a mock deployer image to be able to "deploy" (aka fake deploy) things instantly for tests
            if tries > 240 {
                panic!("timed out waiting for deployment");
            }

            sleep(Duration::from_secs(1)).await;
        }
    }

    /// Stop a service running in a project
    pub async fn stop_service(&mut self) {
        let TestProject {
            router,
            authorization,
            project_name,
            ..
        } = self;

        router
            .call(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/projects/{project_name}/services/{project_name}"))
                    .body(Body::empty())
                    .unwrap()
                    .with_header(authorization),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();
    }

    /// Puts the project in a new state
    pub async fn update_state(&self, state: Project) {
        let TestProject {
            project_name, pool, ..
        } = self;

        let state = sqlx::types::Json(state);

        query("UPDATE projects SET project_state = ?1 WHERE project_name = ?2")
            .bind(&state)
            .bind(project_name)
            .execute(pool)
            .await
            .expect("test to update project state");
    }

    /// Run one iteration of health checks for this project
    pub async fn run_health_check(&self) {
        let handle = self
            .service
            .new_task()
            .project(self.project_name.parse().unwrap())
            .send(&self.sender)
            .await
            .expect("to send one ambulance task");

        // We wait for the check to be done before moving on
        handle.await
    }
}

#[async_trait]
impl AsyncTestContext for TestProject {
    async fn setup() -> Self {
        let mut world = TestGateway::setup().await;

        world.create_project("matrix").await
    }

    async fn teardown(mut self) {
        let dangling = !self.is_missing().await;

        if dangling {
            self.router_call(Method::DELETE, "/delete").await;
            eprintln!("test left a dangling project which you might need to clean manually");
        }
    }
}
