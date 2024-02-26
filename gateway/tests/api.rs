mod helpers;

pub mod needs_docker {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::headers::Authorization;
    use axum::http::Request;
    use futures::TryFutureExt;
    use http::Method;
    use hyper::body::to_bytes;
    use hyper::StatusCode;
    use serde_json::Value;
    use shuttle_common::claims::AccountTier;
    use shuttle_common::constants::limits::{MAX_PROJECTS_DEFAULT, MAX_PROJECTS_EXTRA};
    use shuttle_common::models::project::{self, ProjectName};
    use shuttle_gateway::api::latest::ApiBuilder;
    use shuttle_gateway::project::{Project, ProjectError};
    use shuttle_gateway::service::GatewayService;
    use shuttle_gateway::task::BoxedTask;
    use test_context::test_context;
    use tokio::sync::mpsc::channel;
    use tokio::sync::oneshot;
    use tokio::time::sleep;
    use tower::Service;

    use crate::helpers::{RequestBuilderExt, TestGateway, TestProject, World};

    #[tokio::test]
    async fn api_create_get_delete_projects() -> anyhow::Result<()> {
        let world = World::new().await;
        let service = Arc::new(GatewayService::init(world.args(), world.pool(), "".into()).await?);

        let (sender, mut receiver) = channel::<BoxedTask>(256);
        tokio::spawn(async move {
            while receiver.recv().await.is_some() {
                // do not do any work with inbound requests
            }
        });

        let mut router = ApiBuilder::new()
            .with_service(Arc::clone(&service))
            .with_sender(sender)
            .with_default_routes()
            .with_auth_service(world.context().auth_uri, "dummykey".to_string())
            .into_router();

        let neo_key = world.create_user("neo", AccountTier::Basic);

        let create_project = |project: &str| {
            Request::builder()
                .method("POST")
                .uri(format!("/projects/{project}"))
                .header("Content-Type", "application/json")
                .body("{\"idle_minutes\": 3}".into())
                .unwrap()
        };

        let stop_project = |project: &str| {
            Request::builder()
                .method("DELETE")
                .uri(format!("/projects/{project}"))
                .body(Body::empty())
                .unwrap()
        };

        router
            .call(create_project("matrix"))
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::UNAUTHORIZED))
            .await
            .unwrap();

        let authorization = Authorization::bearer(&neo_key).unwrap();

        router
            .call(create_project("matrix").with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        router
            .call(create_project("matrix").with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            })
            .await
            .unwrap();

        let get_project = |project| {
            Request::builder()
                .method("GET")
                .uri(format!("/projects/{project}"))
                .body(Body::empty())
                .unwrap()
        };

        router
            .call(get_project("matrix"))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            })
            .await
            .unwrap();

        router
            .call(get_project("matrix").with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        router
            .call(stop_project("matrix").with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        router
            .call(create_project("reloaded").with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        let trinity_key = world.create_user("trinity", AccountTier::Basic);

        let authorization = Authorization::bearer(&trinity_key).unwrap();

        router
            .call(get_project("reloaded").with_header(&authorization))
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::NOT_FOUND))
            .await
            .unwrap();

        router
            .call(stop_project("reloaded").with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            })
            .await
            .unwrap();

        let get_load = || {
            Request::builder()
                .method("GET")
                .uri("/admin/stats/load")
                .body(Body::empty())
                .unwrap()
        };

        // Non-admin user cannot access admin routes
        router
            .call(get_load().with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            })
            .await
            .unwrap();

        // Create new admin user
        let admin_neo_key = world.create_user("admin-neo", AccountTier::Basic);
        world.set_super_user("admin-neo");

        let authorization = Authorization::bearer(&admin_neo_key).unwrap();

        // Admin user can access admin routes
        router
            .call(get_load().with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        // TODO: setting the user to admin here doesn't update the cached token, so the
        // commands will still fail. We need to add functionality for this or modify the test.
        // world.set_super_user("trinity");

        // router
        //     .call(get_project("reloaded").with_header(&authorization))
        //     .map_ok(|resp| assert_eq!(resp.status(), StatusCode::OK))
        //     .await
        //     .unwrap();

        // router
        //     .call(delete_project("reloaded").with_header(&authorization))
        //     .map_ok(|resp| {
        //         assert_eq!(resp.status(), StatusCode::OK);
        //     })
        //     .await
        //     .unwrap();

        // // delete returns 404 for project that doesn't exist
        // router
        //     .call(delete_project("resurrections").with_header(&authorization))
        //     .map_ok(|resp| {
        //         assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        //     })
        //     .await
        //     .unwrap();

        Ok(())
    }

    #[tokio::test]
    async fn api_create_project_limits() -> anyhow::Result<()> {
        let world = World::new().await;
        let service = Arc::new(GatewayService::init(world.args(), world.pool(), "".into()).await?);

        let (sender, mut receiver) = channel::<BoxedTask>(256);
        tokio::spawn(async move {
            while receiver.recv().await.is_some() {
                // do not do any work with inbound requests
            }
        });

        let mut router = ApiBuilder::new()
            .with_service(Arc::clone(&service))
            .with_sender(sender)
            .with_default_routes()
            .with_auth_service(world.context().auth_uri, "dummykey".to_string())
            .into_router();

        let neo_key = world.create_user("neo", AccountTier::Basic);

        let create_project = |project: &str| {
            Request::builder()
                .method("POST")
                .uri(format!("/projects/{project}"))
                .header("Content-Type", "application/json")
                .body("{\"idle_minutes\": 3}".into())
                .unwrap()
        };

        let authorization = Authorization::bearer(&neo_key).unwrap();

        // Creating three projects for a basic user succeeds.
        for i in 0..MAX_PROJECTS_DEFAULT {
            router
                .call(create_project(format!("matrix-{i}").as_str()).with_header(&authorization))
                .map_ok(|resp| {
                    assert_eq!(resp.status(), StatusCode::OK);
                })
                .await
                .unwrap();
        }

        // Creating one more project hits the project limit.
        router
            .call(create_project("resurrections").with_header(&authorization))
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            })
            .await
            .unwrap();

        // Create a new admin user. We can't simply make the previous user an admin, since their token
        // will live in the auth cache without the admin scope.
        let trinity_key = world.create_user("trinity", AccountTier::Basic);
        world.set_super_user("trinity");
        let authorization = Authorization::bearer(&trinity_key).unwrap();

        // Creating more than the basic and pro limit of projects for an admin user succeeds.
        for i in 0..MAX_PROJECTS_EXTRA + 1 {
            router
                .call(create_project(format!("reloaded-{i}").as_str()).with_header(&authorization))
                .map_ok(|resp| {
                    assert_eq!(resp.status(), StatusCode::OK);
                })
                .await
                .unwrap();
        }

        Ok(())
    }

    #[test_context(TestGateway)]
    #[tokio::test]
    async fn api_create_project_above_container_limit(gateway: &mut TestGateway) {
        let _ = gateway.create_project("matrix").await;
        let cch_code = gateway.try_create_project("cch23-project").await;

        assert_eq!(cch_code, StatusCode::SERVICE_UNAVAILABLE);

        // It should be possible to still create a normal project
        let _normal_project = gateway.create_project("project").await;

        let more_code = gateway.try_create_project("project-normal-2").await;

        assert_eq!(
            more_code,
            StatusCode::SERVICE_UNAVAILABLE,
            "more normal projects should not go over soft limit"
        );

        // A pro user can go over the soft limits
        let pro_user = gateway.new_authorization_bearer("trinity", AccountTier::Pro);
        let _long_running = gateway.user_create_project("reload", &pro_user).await;

        // A pro user cannot go over the hard limits
        let code = gateway
            .try_user_create_project("training-simulation", &pro_user)
            .await;

        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test_context(TestGateway)]
    #[tokio::test]
    async fn start_idle_project_when_above_container_limit(gateway: &mut TestGateway) {
        let mut cch_idle_project = gateway.create_project("cch23-project").await;
        // RUNNING PROJECTS = 1 [cch_idle_project]
        // Run four health checks to get the project to go into idle mode (cch projects always default to 5 min of idle time)
        cch_idle_project.run_health_check().await;
        cch_idle_project.run_health_check().await;
        cch_idle_project.run_health_check().await;
        cch_idle_project.run_health_check().await;

        cch_idle_project
            .wait_for_state(project::State::Stopped)
            .await;
        // RUNNING PROJECTS = 0 []
        let mut normal_idle_project = gateway.create_project("project").await;
        // RUNNING PROJECTS = 1 [normal_idle_project]
        // Run two health checks to get the project to go into idle mode
        normal_idle_project.run_health_check().await;
        normal_idle_project.run_health_check().await;

        normal_idle_project
            .wait_for_state(project::State::Stopped)
            .await;
        // RUNNING PROJECTS = 0 []
        let mut normal_idle_project2 = gateway.create_project("project-2").await;
        // RUNNING PROJECTS = 1 [normal_idle_project2]
        // Run two health checks to get the project to go into idle mode
        normal_idle_project2.run_health_check().await;
        normal_idle_project2.run_health_check().await;

        normal_idle_project2
            .wait_for_state(project::State::Stopped)
            .await;
        // RUNNING PROJECTS = 0 []
        let pro_user = gateway.new_authorization_bearer("trinity", AccountTier::Pro);
        let mut long_running = gateway.user_create_project("matrix", &pro_user).await;
        // RUNNING PROJECTS = 1 [long_running]
        // Now try to start the idle projects
        let cch_code = cch_idle_project
            .router_call(Method::GET, "/services/cch23-project")
            .await;
        // RUNNING PROJECTS = 1 [long_running]

        assert_eq!(cch_code, StatusCode::SERVICE_UNAVAILABLE);

        let normal_code = normal_idle_project
            .router_call(Method::GET, "/services/project")
            .await;
        // RUNNING PROJECTS = 2 [long_running, normal_idle_project]

        assert_eq!(
            normal_code,
            StatusCode::NOT_FOUND,
            "should not be able to find a service since nothing was deployed"
        );

        let normal_code2 = normal_idle_project2
            .router_call(Method::GET, "/services/project")
            .await;
        // RUNNING PROJECTS = 2 [long_running, normal_idle_project]

        assert_eq!(
            normal_code2,
            StatusCode::SERVICE_UNAVAILABLE,
            "should not be able to wake project that will go over soft limit"
        );

        // Now try to start a pro user's project
        // Have it idle so that we can wake it up
        long_running.run_health_check().await;
        long_running.run_health_check().await;

        long_running.wait_for_state(project::State::Stopped).await;
        // RUNNING PROJECTS = 1 [normal_idle_project]

        let normal_code2 = normal_idle_project2
            .router_call(Method::GET, "/services/project")
            .await;
        // RUNNING PROJECTS = 2 [normal_idle_project, normal_idle_project2]

        assert_eq!(
            normal_code2,
            StatusCode::NOT_FOUND,
            "should not be able to find a service since nothing was deployed"
        );

        let long_running_code = long_running
            .router_call(Method::GET, "/services/project")
            .await;
        // RUNNING PROJECTS = 3 [normal_idle_project, normal_idle_project2, long_running]

        assert_eq!(
            long_running_code,
            StatusCode::NOT_FOUND,
            "should be able to wake the project of a pro user. Even if we are over the soft limit"
        );

        // Now try to start a pro user's project when we are at the hard limit
        long_running.run_health_check().await;
        long_running.run_health_check().await;

        long_running.wait_for_state(project::State::Stopped).await;
        // RUNNING PROJECTS = 2 [normal_idle_project, normal_idle_project2]
        let _extra = gateway.user_create_project("reloaded", &pro_user).await;
        // RUNNING PROJECTS = 3 [normal_idle_project, normal_idle_project2, _extra]

        let long_running_code = long_running
            .router_call(Method::GET, "/services/project")
            .await;
        // RUNNING PROJECTS = 3 [normal_idle_project, normal_idle_project2, _extra]

        assert_eq!(
            long_running_code,
            StatusCode::SERVICE_UNAVAILABLE,
            "should be able to wake the project of a pro user. Even if we are over the soft limit"
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_is_ready(project: &mut TestProject) {
        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::OK
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_is_stopped(project: &mut TestProject) {
        // Run two health checks to get the project to go into idle mode
        project.run_health_check().await;
        project.run_health_check().await;

        project.wait_for_state(project::State::Stopped).await;

        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::OK
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_is_destroyed(project: &mut TestProject) {
        project.destroy_project().await;

        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::OK
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_has_resources(project: &mut TestProject) {
        project.deploy("../examples/rocket/secrets").await;
        project.stop_service().await;

        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::OK
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_has_resources_but_fails_to_remove_them(
        project: &mut TestProject,
    ) {
        project.deploy("../examples/axum/metadata").await;
        project.stop_service().await;

        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_has_running_deployment(project: &mut TestProject) {
        project.deploy("../examples/axum/hello-world").await;

        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::OK
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_is_building(project: &mut TestProject) {
        project.just_deploy("../examples/axum/hello-world").await;

        // Wait a bit to it to progress in the queue
        sleep(Duration::from_secs(2)).await;

        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[test_context(TestProject)]
    #[tokio::test]
    async fn api_delete_project_that_is_errored(project: &mut TestProject) {
        project
            .update_state(Project::Errored(ProjectError::internal(
                "Mr. Anderson is here",
            )))
            .await;

        assert_eq!(
            project.router_call(Method::DELETE, "/delete").await,
            StatusCode::OK
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status() {
        let world = World::new().await;
        let service = Arc::new(
            GatewayService::init(world.args(), world.pool(), "".into())
                .await
                .unwrap(),
        );

        let (sender, mut receiver) = channel::<BoxedTask>(1);
        let (ctl_send, ctl_recv) = oneshot::channel();
        let (done_send, done_recv) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut done_send = Some(done_send);
            // do not process until instructed
            ctl_recv.await.unwrap();

            while receiver.recv().await.is_some() {
                done_send.take().unwrap().send(()).unwrap();
                // do nothing
            }
        });

        let mut router = ApiBuilder::new()
            .with_service(Arc::clone(&service))
            .with_sender(sender)
            .with_default_routes()
            .with_auth_service(world.context().auth_uri, "dummykey".to_string())
            .into_router();

        let get_status = || {
            Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap()
        };

        let resp = router.call(get_status()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let matrix: ProjectName = "matrix".parse().unwrap();

        let neo_key = world.create_user("neo", AccountTier::Basic);
        let authorization = Authorization::bearer(&neo_key).unwrap();

        let create_project = Request::builder()
            .method("POST")
            .uri(format!("/projects/{matrix}"))
            .header("Content-Type", "application/json")
            .body("{\"idle_minutes\": 3}".into())
            .unwrap()
            .with_header(&authorization);

        router.call(create_project).await.unwrap();

        let resp = router.call(get_status()).await.unwrap();
        let body = to_bytes(resp.into_body()).await.unwrap();

        // The status check response will be a JSON array of objects.
        let resp: Value = serde_json::from_slice(&body).unwrap();

        // The gateway health status will always be the first element in the array.
        assert_eq!(resp[0][1]["status"], "unhealthy".to_string());

        ctl_send.send(()).unwrap();
        done_recv.await.unwrap();

        let resp = router.call(get_status()).await.unwrap();
        let body = to_bytes(resp.into_body()).await.unwrap();

        let resp: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp[0][1]["status"], "degraded".to_string());

        worker.abort();
        let _ = worker.await;

        let resp = router.call(get_status()).await.unwrap();
        let body = to_bytes(resp.into_body()).await.unwrap();

        let resp: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(resp[0][1]["status"], "unhealthy".to_string());
    }
}
