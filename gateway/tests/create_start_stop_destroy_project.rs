mod helpers;
mod needs_docker {

    use std::time::Duration;

    use bollard::secret::ContainerStateStatusEnum;
    use bollard::service::NetworkSettings;
    use bollard::{models::ContainerState, secret::ContainerInspectResponse};
    use futures::prelude::*;
    use hyper::{Body, Request, StatusCode};
    use shuttle_gateway::project::{
        Project, ProjectAttaching, ProjectCreating, ProjectDestroyed, ProjectReady, ProjectStarted,
        ProjectStarting, ProjectStopped,
    };
    use shuttle_gateway::StateExt;
    use tokio::time::sleep;
    use ulid::Ulid;

    use crate::helpers::assert_stream_matches;

    use super::helpers::{assert_matches, World};

    #[tokio::test]
    async fn create_start_stop_destroy_project() -> anyhow::Result<()> {
        let world = World::new().await;

        let ctx = world.context();

        let project_started = assert_matches!(
            ctx,
            Project::Creating(ProjectCreating::new("my-project-test".parse().unwrap(),
            Ulid::new(),
            "test".to_string(),
            0)),
            #[assertion = "Container created, attach network"]
            Ok(Project::Attaching(ProjectAttaching {
                container: ContainerInspectResponse {
                    state: Some(ContainerState {
                        status: Some(ContainerStateStatusEnum::CREATED),
                        ..
                    }),
                    network_settings: Some(NetworkSettings {
                        networks: Some(networks),
                        ..
                    }),
                    ..
                },
                ..
            })) if networks.keys().collect::<Vec<_>>() == vec!["bridge"],
            #[assertion = "Container attached, assigned an `id`"]
            Ok(Project::Starting(ProjectStarting {
                container: ContainerInspectResponse {
                    id: Some(container_id),
                    state: Some(ContainerState {
                        status: Some(ContainerStateStatusEnum::CREATED),
                        ..
                    }),
                    network_settings: Some(NetworkSettings {
                        networks: Some(networks),
                        ..
                    }),
                    ..
                },
                ..
            })) if networks.keys().collect::<Vec<_>>() == vec![&ctx.container_settings.network_name],
            #[assertion = "Container started, in a running state"]
            Ok(Project::Started(ProjectStarted {
                container: ContainerInspectResponse {
                    id: Some(id),
                    state: Some(ContainerState {
                        status: Some(ContainerStateStatusEnum::RUNNING),
                        ..
                    }),
                    ..
                },
                ..
            })) if id == container_id,
        );

        let delay = sleep(Duration::from_secs(10));
        futures::pin_mut!(delay);
        let mut project_readying = project_started
            .unwrap()
            .into_stream(&ctx)
            .take_until(delay)
            .try_skip_while(|state| future::ready(Ok(!matches!(state, Project::Ready(_)))));

        let project_ready = assert_stream_matches!(
            project_readying,
            #[assertion = "Container is ready"]
            Ok(Project::Ready(ProjectReady {
                container: ContainerInspectResponse {
                    state: Some(ContainerState {
                        status: Some(ContainerStateStatusEnum::RUNNING),
                        ..
                    }),
                    ..
                },
                ..
            })),
        );

        let target_addr = project_ready
            .as_ref()
            .unwrap()
            .target_addr()
            .unwrap()
            .unwrap();

        let client = World::client(target_addr);

        client
            .request(
                Request::get("/projects/my-project-test/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .map_ok(|resp| assert_eq!(resp.status(), StatusCode::OK))
            .await
            .unwrap();

        let project_stopped = assert_matches!(
            ctx,
            project_ready.unwrap().stop().unwrap(),
            #[assertion = "Container is stopped"]
            Ok(Project::Stopped(ProjectStopped {
                container: ContainerInspectResponse {
                    state: Some(ContainerState {
                        status: Some(ContainerStateStatusEnum::EXITED),
                        ..
                    }),
                    ..
                },
            })),
        );

        assert_matches!(
            ctx,
            project_stopped.unwrap().destroy().unwrap(),
            #[assertion = "Container is destroyed"]
            Ok(Project::Destroyed(ProjectDestroyed { destroyed: _ })),
        )
        .unwrap();

        Ok(())
    }
}
