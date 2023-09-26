mod container;

pub use self::container::ContainerSettings;

use bollard::Docker;

pub trait Context: Send + Sync {
    fn docker(&self) -> &Docker;

    fn container_settings(&self) -> &ContainerSettings;
}
