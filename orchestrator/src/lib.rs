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

pub mod client;
pub mod error;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt::init();

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
