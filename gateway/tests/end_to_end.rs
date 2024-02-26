mod helpers;
mod needs_docker {
    use axum::headers::Authorization;
    use futures::TryFutureExt;
    use http::{Request, StatusCode};
    use hyper::Body;
    use shuttle_common::{claims::AccountTier, models::project};
    use shuttle_gateway::{
        api::latest::ApiBuilder, proxy::UserServiceBuilder, service::GatewayService, worker::Worker,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::sync::mpsc::channel;

    use crate::helpers::{timed_loop, RequestBuilderExt, World};

    #[tokio::test]
    async fn end_to_end() {
        let world = World::new().await;
        let service = Arc::new(
            GatewayService::init(world.args(), world.pool(), "".into())
                .await
                .unwrap(),
        );
        let worker = Worker::new();

        let (log_out, mut log_in) = channel(256);
        tokio::spawn({
            let sender = worker.sender();
            async move {
                while let Some(work) = log_in.recv().await {
                    sender
                        .send(work)
                        .await
                        .map_err(|_| "could not send work")
                        .unwrap();
                }
            }
        });

        let api_client = World::client(world.args.control);
        let ph_client_options = async_posthog::ClientOptions::new(
            "dummy".to_string(),
            "https://eu.posthog.com".to_string(),
            Duration::from_millis(800),
        );
        let posthog_client = async_posthog::client(ph_client_options);
        let api = ApiBuilder::new()
            .with_service(Arc::clone(&service))
            .with_sender(log_out.clone())
            .with_default_routes()
            .with_auth_service(world.context().auth_uri, "dummykey".to_string())
            .binding_to(world.args.control)
            .with_posthog_client(posthog_client);

        let user = UserServiceBuilder::new()
            .with_service(Arc::clone(&service))
            .with_task_sender(log_out)
            .with_public(world.fqdn())
            .with_user_proxy_binding_to(world.args.user);

        let _gateway = tokio::spawn(async move {
            tokio::select! {
                _ = worker.start() => {},
                _ = api.serve() => {},
                _ = user.serve() => {}
            }
        });

        // Allow the spawns to start
        tokio::time::sleep(Duration::from_secs(1)).await;

        let neo_key = world.create_user("neo", AccountTier::Basic);

        let authorization = Authorization::bearer(&neo_key).unwrap();

        println!("Creating the matrix project");
        api_client
            .request(
                Request::post("/projects/matrix")
                    .with_header(&authorization)
                    .header("Content-Type", "application/json")
                    .body("{\"idle_minutes\": 3}".into())
                    .unwrap(),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
            })
            .await
            .unwrap();

        timed_loop!(wait: 1, max: 12, {
            let project: project::Response = api_client
                .request(
                    Request::get("/projects/matrix")
                        .with_header(&authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .map_ok(|resp| {
                    assert_eq!(resp.status(), StatusCode::OK);
                    serde_json::from_slice(resp.body()).unwrap()
                })
                .await
                .unwrap();

            if project.state == project::State::Ready {
                break;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        println!("get matrix project status");
        api_client
            .request(
                Request::get("/projects/matrix/status")
                    .with_header(&authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::OK))
            .await
            .unwrap();

        println!("delete matrix project");
        api_client
            .request(
                Request::delete("/projects/matrix")
                    .with_header(&authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::OK))
            .await
            .unwrap();

        timed_loop!(wait: 1, max: 20, {
            let resp = api_client
                .request(
                    Request::get("/projects/matrix")
                        .with_header(&authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let resp = serde_json::from_slice::<project::Response>(resp.body().as_slice()).unwrap();
            if matches!(resp.state, project::State::Destroyed) {
                break;
            }
        });

        // Attempting to delete already Destroyed project will return Destroyed
        api_client
            .request(
                Request::delete("/projects/matrix")
                    .with_header(&authorization)
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| {
                assert_eq!(resp.status(), StatusCode::OK);
                let resp =
                    serde_json::from_slice::<project::Response>(resp.body().as_slice()).unwrap();
                assert_eq!(resp.state, project::State::Destroyed);
            })
            .await
            .unwrap();
    }
}
