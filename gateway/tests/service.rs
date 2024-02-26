mod helpers;
mod needs_docker {
    use std::sync::Arc;

    use fqdn::FQDN;
    use shuttle_common::models::{error::ErrorKind, project::ProjectName};
    use shuttle_gateway::{
        project::Project,
        service::{FindProjectPayload, GatewayService},
        task::{self, TaskResult},
        AccountName, DockerContext, Error, ProjectDetails,
    };

    use crate::helpers::{assert_err_kind, World};

    #[tokio::test]
    async fn service_create_find_stop_delete_project() -> anyhow::Result<()> {
        let world = World::new().await;
        let svc = Arc::new(GatewayService::init(world.args(), world.pool(), "".into()).await?);

        let neo: AccountName = "neo".parse().unwrap();
        let trinity: AccountName = "trinity".parse().unwrap();
        let matrix: ProjectName = "matrix".parse().unwrap();

        let admin: AccountName = "admin".parse().unwrap();

        let creating_same_project_name = |project: &Project, project_name: &ProjectName| {
            matches!(
                project,
                Project::Creating(creating) if creating.project_name() == project_name
            )
        };

        let project = svc
            .create_project(matrix.clone(), neo.clone(), false, true, 0)
            .await
            .unwrap();

        assert!(creating_same_project_name(&project.state, &matrix));

        assert_eq!(
            svc.find_project(&matrix).await.unwrap().state,
            project.state
        );
        assert_eq!(
            svc.iter_projects_detailed()
                .await
                .unwrap()
                .next()
                .expect("to get one project with its user"),
            ProjectDetails {
                project_name: matrix.clone(),
                account_name: neo.clone(),
            }
        );
        assert_eq!(
            svc.iter_user_projects_detailed(&neo, 0, u32::MAX as i64)
                .await
                .unwrap()
                .map(|item| item.1)
                .collect::<Vec<_>>(),
            vec![matrix.clone()]
        );

        // Test project pagination, first create 20 projects.
        for p in (0..20).map(|p| format!("matrix-{p}")) {
            svc.create_project(p.parse().unwrap(), admin.clone(), true, true, 0)
                .await
                .unwrap();
        }

        // Creating a project with can_create_project set to false should fail.
        assert_eq!(
            svc.create_project("final-one".parse().unwrap(), admin.clone(), false, false, 0)
                .await
                .err()
                .unwrap()
                .kind(),
            ErrorKind::TooManyProjects
        );

        // We need to fetch all of them from the DB since they are ordered by created_at (in the id) and project_name,
        // and created_at will be the same for some of them.
        let all_projects = svc
            .iter_user_projects_detailed(&admin, 0, u32::MAX as i64)
            .await
            .unwrap()
            .map(|item| item.0)
            .collect::<Vec<_>>();

        assert_eq!(all_projects.len(), 20);

        // Get first 5 projects.
        let paginated = svc
            .iter_user_projects_detailed(&admin, 0, 5)
            .await
            .unwrap()
            .map(|item| item.0)
            .collect::<Vec<_>>();

        assert_eq!(all_projects[..5], paginated);

        // Get 10 projects starting at an offset of 10.
        let paginated = svc
            .iter_user_projects_detailed(&admin, 10, 10)
            .await
            .unwrap()
            .map(|item| item.0)
            .collect::<Vec<_>>();
        assert_eq!(all_projects[10..20], paginated);

        // Get 20 projects starting at an offset of 200.
        let paginated = svc
            .iter_user_projects_detailed(&admin, 200, 20)
            .await
            .unwrap()
            .collect::<Vec<_>>();

        assert!(paginated.is_empty());

        let mut work = svc
            .new_task()
            .project(matrix.clone())
            .and_then(task::destroy())
            .build();

        while let TaskResult::Pending(_) = work.poll(()).await {}
        assert!(matches!(work.poll(()).await, TaskResult::Done(())));

        // After project has been destroyed...
        assert!(matches!(
            svc.find_project(&matrix).await,
            Ok(FindProjectPayload {
                project_id: _,
                state: Project::Destroyed(_),
            })
        ));

        // If recreated by a different user
        assert!(matches!(
            svc.create_project(matrix.clone(), trinity.clone(), false, true, 0)
                .await,
            Err(Error {
                kind: ErrorKind::ProjectAlreadyExists,
                ..
            })
        ));

        // If recreated by the same user
        assert!(matches!(
            svc.create_project(matrix.clone(), neo.clone(), false, true, 0)
                .await,
            Ok(FindProjectPayload {
                project_id: _,
                state: Project::Creating(_),
            })
        ));

        // If recreated by the same user again while it's running
        assert!(matches!(
            svc.create_project(matrix.clone(), neo.clone(), false, true, 0)
                .await,
            Err(Error {
                kind: ErrorKind::OwnProjectAlreadyExists(_),
                ..
            })
        ));

        let mut work = svc
            .new_task()
            .project(matrix.clone())
            .and_then(task::destroy())
            .build();

        while let TaskResult::Pending(_) = work.poll(()).await {}
        assert!(matches!(work.poll(()).await, TaskResult::Done(())));

        // After project has been destroyed again...
        assert!(matches!(
            svc.find_project(&matrix).await,
            Ok(FindProjectPayload {
                project_id: _,
                state: Project::Destroyed(_),
            })
        ));

        // If recreated by an admin
        assert!(matches!(
            svc.create_project(matrix.clone(), admin.clone(), true, true, 0)
                .await,
            Ok(FindProjectPayload {
                project_id: _,
                state: Project::Creating(_),
            })
        ));

        // If recreated by an admin again while it's running
        assert!(matches!(
            svc.create_project(matrix.clone(), admin.clone(), true, true, 0)
                .await,
            Err(Error {
                kind: ErrorKind::OwnProjectAlreadyExists(_),
                ..
            })
        ));

        // We can delete a project
        assert!(matches!(svc.delete_project(&matrix).await, Ok(())));

        // Project is gone
        assert!(matches!(
            svc.find_project(&matrix).await,
            Err(Error {
                kind: ErrorKind::ProjectNotFound(_),
                ..
            })
        ));

        // It can be re-created by anyone, with the same project name
        assert!(matches!(
            svc.create_project(matrix, trinity.clone(), false, true, 0)
                .await,
            Ok(FindProjectPayload {
                project_id: _,
                state: Project::Creating(_),
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn service_create_ready_kill_restart_docker() -> anyhow::Result<()> {
        let world = World::new().await;
        let svc = Arc::new(GatewayService::init(world.args(), world.pool(), "".into()).await?);

        let neo: AccountName = "neo".parse().unwrap();
        let matrix: ProjectName = "matrix".parse().unwrap();

        svc.create_project(matrix.clone(), neo.clone(), false, true, 0)
            .await
            .unwrap();

        let mut task = svc.new_task().project(matrix.clone()).build();

        while let TaskResult::Pending(_) = task.poll(()).await {
            // keep polling
        }

        let project = svc.find_project(&matrix).await.unwrap();
        println!("{:?}", project.state);
        assert!(project.state.is_ready());

        let container = project.state.container().unwrap();
        svc.context()
            .docker()
            .kill_container::<String>(container.name.unwrap().strip_prefix('/').unwrap(), None)
            .await
            .unwrap();

        println!("killed container");

        let mut ambulance_task = svc.new_task().project(matrix.clone()).build();

        // the first poll will trigger a refresh
        let _ = ambulance_task.poll(()).await;

        let project = svc.find_project(&matrix).await.unwrap();
        println!("{:?}", project.state);
        assert!(!project.state.is_ready());

        // the subsequent will trigger a restart task
        while let TaskResult::Pending(_) = ambulance_task.poll(()).await {
            // keep polling
        }

        let project = svc.find_project(&matrix).await.unwrap();
        println!("{:?}", project.state);
        assert!(project.state.is_ready());

        Ok(())
    }

    #[tokio::test]
    async fn service_create_find_custom_domain() -> anyhow::Result<()> {
        let world = World::new().await;
        let svc = Arc::new(GatewayService::init(world.args(), world.pool(), "".into()).await?);

        let account: AccountName = "neo".parse().unwrap();
        let project_name: ProjectName = "matrix".parse().unwrap();
        let domain: FQDN = "neo.the.matrix".parse().unwrap();
        let certificate = "dummy certificate";
        let private_key = "dummy private key";

        assert_err_kind!(
            svc.project_details_for_custom_domain(&domain).await,
            ErrorKind::CustomDomainNotFound
        );

        let _ = svc
            .create_project(project_name.clone(), account.clone(), false, true, 0)
            .await
            .unwrap();

        svc.create_custom_domain(&project_name, &domain, certificate, private_key)
            .await
            .unwrap();

        let custom_domain = svc
            .project_details_for_custom_domain(&domain)
            .await
            .unwrap();

        assert_eq!(custom_domain.project_name, project_name);
        assert_eq!(custom_domain.certificate, certificate);
        assert_eq!(custom_domain.private_key, private_key);

        // Should auto replace the domain details
        let certificate = "dummy certificate update";
        let private_key = "dummy private key update";

        svc.create_custom_domain(&project_name, &domain, certificate, private_key)
            .await
            .unwrap();

        let custom_domain = svc
            .project_details_for_custom_domain(&domain)
            .await
            .unwrap();

        assert_eq!(custom_domain.project_name, project_name);
        assert_eq!(custom_domain.certificate, certificate);
        assert_eq!(custom_domain.private_key, private_key);

        Ok(())
    }

    #[tokio::test]
    async fn service_create_custom_domain_destroy_recreate_project() -> anyhow::Result<()> {
        let world = World::new().await;
        let svc = Arc::new(GatewayService::init(world.args(), world.pool(), "".into()).await?);

        let account: AccountName = "neo".parse().unwrap();
        let project_name: ProjectName = "matrix".parse().unwrap();
        let domain: FQDN = "neo.the.matrix".parse().unwrap();
        let certificate = "dummy certificate";
        let private_key = "dummy private key";

        assert_err_kind!(
            svc.project_details_for_custom_domain(&domain).await,
            ErrorKind::CustomDomainNotFound
        );

        let _ = svc
            .create_project(project_name.clone(), account.clone(), false, true, 0)
            .await
            .unwrap();

        svc.create_custom_domain(&project_name, &domain, certificate, private_key)
            .await
            .unwrap();

        let mut work = svc
            .new_task()
            .project(project_name.clone())
            .and_then(task::destroy())
            .build();

        while let TaskResult::Pending(_) = work.poll(()).await {}
        assert!(matches!(work.poll(()).await, TaskResult::Done(())));

        let recreated_project = svc
            .create_project(project_name.clone(), account.clone(), false, true, 0)
            .await
            .unwrap();

        assert!(matches!(recreated_project.state, Project::Creating(_)));

        Ok(())
    }
}
