use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{DeleteParams, ListParams, PostParams},
    client::ConfigExt,
    runtime::wait::{await_condition, conditions::is_pod_running},
    Api, Client, Config, ResourceExt,
};
use serde_json::json;
use std::io;
use tracing::info;

const PODS_COUNT: u16 = 2000;
const START_POD: u16 = 0;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt::init();

    // Starts a kube cluster client based on the local kubernetes configuration
    // (from ~/.kube/config or from KUBECONFIG env).
    let config = Config::infer().await.unwrap();

    // Uses HTTPs for the Kubernetes client.
    let https = config.rustls_https_connector().unwrap();
    let service = tower::ServiceBuilder::new()
        .layer(config.base_uri_layer())
        .option_layer(config.auth_layer().unwrap())
        .service(hyper::Client::builder().build(https));
    let client = Client::new(service, config.default_namespace);

    // Manage pods.
    let pods: Api<Pod> = Api::default_namespaced(client);

    // Create Pod blog spec. By default, the images are retrieved from DockerHub.
    // To specify a different registry we must use an image name as described here:
    // https://kubernetes.io/docs/concepts/containers/images/#image-names.
    info!("Creating 10 pod instances");
    for idx in START_POD..PODS_COUNT {
        let mut shuttle_service_name = "shuttle-service-".to_string();
        shuttle_service_name.push_str(&idx.to_string());
        let p: Pod = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": shuttle_service_name },
            "spec": {
                "containers": [{
                  "name": shuttle_service_name,
                  "image": "localhost:32000/shuttle-service:latest"
                }],
            }
        }))?;

        // Create the pod.
        let pp = PostParams::default();
        match pods.create(&pp, &p).await {
            Ok(o) => {
                let name = o.name_any();
                assert_eq!(p.name_any(), name);
                info!("Created {}", name);
            }
            Err(kube::Error::Api(ae)) => assert_eq!(ae.code, 409), // if you skipped delete, for instance
            Err(_e) => panic!("error"),                            // any other case is probably bad
        }

        // Watch it phase for a few seconds.
        let establish = await_condition(
            pods.clone(),
            shuttle_service_name.as_str(),
            is_pod_running(),
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(60), establish).await?;

        // Verify we can get it.
        info!("Get Pod shuttle-service-{}", idx);
        let p1cpy = pods.get(shuttle_service_name.as_str()).await.unwrap();
        if let Some(spec) = &p1cpy.spec {
            info!(
                "Got shuttle-service pod with containers: {:?}",
                spec.containers
            );
            assert_eq!(spec.containers[0].name, shuttle_service_name.as_str());
        }
    }

    // Delete it.
    for idx in 0..PODS_COUNT {
        let mut shuttle_service_name = "shuttle-service-".to_string();
        shuttle_service_name.push_str(&idx.to_string());
        let dp = DeleteParams::default();
        pods.delete(shuttle_service_name.as_str(), &dp)
            .await
            .unwrap()
            .map_left(|pdel| {
                assert_eq!(pdel.name_any(), shuttle_service_name);
                info!("Deleting shuttle-service-{} pod started: {:?}", idx, pdel);
            });
    }

    Ok(())
}
