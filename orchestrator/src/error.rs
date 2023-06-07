#[derive(Debug, thiserror::Error)]
pub enum Error {
    Kube(kube::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
