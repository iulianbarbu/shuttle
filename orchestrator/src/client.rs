use k8s_openapi::api::{apps::v1::Deployment, core::v1::Pod};
use kube::{api::PostParams, client::ConfigExt, Api, Client, Config};
use serde_json::json;
use shuttle_common::project::ProjectName;
use tracing::{error, info};

use crate::error::{Error, Result};

struct K8sClient {
    inner: Client,
}

impl K8sPodsClient {
    pub async fn new() -> Self {
        // Starts a kube cluster client based on the local kubernetes configuration
        // (from ~/.kube/config or from KUBECONFIG env).
        let config = Config::infer()
            .await
            .expect("to be able to infer a K8s client");
        // Uses HTTPs for the Kubernetes client.
        let https = config
            .rustls_https_connector()
            .expect("to be able to get the rustls connector");
        let service = tower::ServiceBuilder::new()
            .layer(config.base_uri_layer())
            .option_layer(config.auth_layer().unwrap())
            .service(hyper::Client::builder().build(https));
        K8sPodsClient {
            inner: Client::new(service, config.default_namespace),
        }
    }

    pub async fn create_pod(
        &mut self,
        project_name: ProjectName,
        image_name: &String,
    ) -> Result<()> {
        info!("Creating a deployment from {}", image_name);
        let pods: Api<Pod> = Api::default_namespaced(self.inner.clone());
        let p: Pod = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": project_name},
            "spec": {
                "containers": [{
                "name": project_name,
                "image": image_name
                }],
            }
        }))?;

        // Create the pod.
        let pp = PostParams::default();
        match pods.create(&pp, &p).await {
            Ok(o) => {
                let name = o.name_any();
                assert_eq!(p.name_any(), name);
                info!("Created the pod for {}", name);
            }
            Err(err) => Error::Kube(err),
        }?;
    }

    pub async fn pod_status() {
        // Watch it phase for a few seconds.
        let establish = await_condition(
            pods.clone(),
            shuttle_service_name.as_str(),
            is_pod_running(),
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(60), establish).await?;

        // Verify if the pods is present.
        let p1cpy = pods.get(shuttle_service_name.as_str()).await.unwrap();
        if let Some(spec) = &p1cpy.spec {
            info!(
                "Got shuttle-service pod with containers: {:?}",
                spec.containers
            );
            assert_eq!(spec.containers[0].name, shuttle_service_name.as_str());
        }
    }
}
