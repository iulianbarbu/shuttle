#[macro_use]
extern crate async_trait;

use std::convert::Infallible;
use std::error::Error as StdError;
use std::fmt::Formatter;
use std::io;
use std::pin::Pin;
use std::str::FromStr;

use acme::AcmeClientError;

use axum::response::{IntoResponse, Response};

use bollard::Docker;
use futures::prelude::*;
use hyper::client::HttpConnector;
use hyper::Client;
use once_cell::sync::Lazy;
use serde::{Deserialize, Deserializer, Serialize};
use service::ContainerSettings;
use shuttle_common::models::error::{ApiError, ErrorKind};
use shuttle_common::models::project::ProjectName;
use strum::Display;
use tokio::sync::mpsc::error::SendError;

pub mod acme;
pub mod api;
pub mod args;
pub mod auth;
pub mod project;
pub mod proxy;
pub mod service;
pub mod task;
pub mod tls;
pub mod worker;

pub const DOCKER_STATS_PATH_CGROUP_V1: &str = "/sys/fs/cgroup/cpuacct/docker";
pub const DOCKER_STATS_PATH_CGROUP_V2: &str = "/sys/fs/cgroup/system.slice";

#[derive(Clone, Display, PartialEq, Eq)]
pub enum DockerStatsSource {
    CgroupV1,
    CgroupV2,
    Bollard,
}
static AUTH_CLIENT: Lazy<Client<HttpConnector>> = Lazy::new(Client::new);

/// Server-side errors that do not have to do with the user runtime
/// should be [`Error`]s.
///
/// All [`Error`] have an [`ErrorKind`] and an (optional) source.

/// [`Error] is safe to be used as error variants to axum endpoints
/// return types as their [`IntoResponse`] implementation does not
/// leak any sensitive information.
#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    source: Option<Box<dyn StdError + Sync + Send + 'static>>,
}

impl Error {
    pub fn source<E: StdError + Sync + Send + 'static>(kind: ErrorKind, err: E) -> Self {
        Self {
            kind,
            source: Some(Box::new(err)),
        }
    }

    pub fn custom<S: AsRef<str>>(kind: ErrorKind, message: S) -> Self {
        Self {
            kind,
            source: Some(Box::new(io::Error::new(
                io::ErrorKind::Other,
                message.as_ref().to_string(),
            ))),
        }
    }

    pub fn from_kind(kind: ErrorKind) -> Self {
        Self { kind, source: None }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind.clone()
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self::from_kind(kind)
    }
}

impl<T> From<SendError<T>> for Error {
    fn from(_: SendError<T>) -> Self {
        Self::from(ErrorKind::ServiceUnavailable)
    }
}

impl From<io::Error> for Error {
    fn from(_: io::Error) -> Self {
        Self::from(ErrorKind::Internal)
    }
}

impl From<AcmeClientError> for Error {
    fn from(error: AcmeClientError) -> Self {
        Self::source(ErrorKind::Internal, error)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let error: ApiError = self.kind.clone().into();

        if error.status_code >= 500 {
            tracing::error!(
                error = &self as &dyn std::error::Error,
                "control plane request error"
            );
        }

        error.into_response()
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind)?;
        if let Some(source) = self.source.as_ref() {
            write!(f, ": ")?;
            source.fmt(f)?;
        }
        Ok(())
    }
}

impl StdError for Error {}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type, Serialize)]
#[sqlx(transparent)]
pub struct AccountName(String);

impl FromStr for AccountName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for AccountName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for AccountName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|_err| todo!())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDetails {
    pub project_name: ProjectName,
    pub account_name: AccountName,
}

impl From<ProjectDetails> for shuttle_common::models::admin::ProjectResponse {
    fn from(project: ProjectDetails) -> Self {
        Self {
            project_name: project.project_name.to_string(),
            account_name: project.account_name.to_string(),
        }
    }
}

#[async_trait]
pub trait DockerContext: Send + Sync {
    fn docker(&self) -> &Docker;

    fn container_settings(&self) -> &ContainerSettings;

    async fn get_stats(&self, container_id: &str) -> Result<u64, Error>;
}

/// A generic state which can, when provided with a [`Context`], do
/// some work and advance itself
#[async_trait]
pub trait State<Ctx>: Send {
    type Next;

    type Error;

    async fn next(self, ctx: &Ctx) -> Result<Self::Next, Self::Error>;
}

pub type StateTryStream<'c, St, Err> = Pin<Box<dyn Stream<Item = Result<St, Err>> + Send + 'c>>;

pub trait StateExt<Ctx>: TryState + State<Ctx, Error = Infallible, Next = Self>
where
    Ctx: Sync,
    Self: Clone,
{
    /// Convert the state into a [`TryStream`] that yields
    /// the generated states.
    ///
    /// This stream will not end.
    fn into_stream<'c>(self, ctx: &'c Ctx) -> StateTryStream<'c, Self, Self::ErrorVariant>
    where
        Self: 'c,
    {
        Box::pin(stream::try_unfold((self, ctx), |(state, ctx)| async move {
            state
                .next(ctx)
                .await
                .unwrap() // EndState's `next` is Infallible
                .into_result()
                .map(|state| Some((state.clone(), (state, ctx))))
        }))
    }
}

impl<Ctx, S> StateExt<Ctx> for S
where
    S: Clone + TryState + State<Ctx, Error = Infallible, Next = Self>,
    Ctx: Send + Sync,
{
}

/// A [`State`] which contains all its transitions, including
/// failures
pub trait TryState: Sized {
    type ErrorVariant;

    fn into_result(self) -> Result<Self, Self::ErrorVariant>;
}

pub trait IntoTryState<S>
where
    S: TryState,
{
    fn into_try_state(self) -> Result<S, Infallible>;
}

impl<S, F, Err> IntoTryState<S> for Result<F, Err>
where
    S: TryState + From<F> + From<Err>,
{
    fn into_try_state(self) -> Result<S, Infallible> {
        self.map(|s| S::from(s)).or_else(|err| Ok(S::from(err)))
    }
}

#[async_trait]
pub trait Refresh<Ctx>: Sized {
    type Error: StdError;

    async fn refresh(self, ctx: &Ctx) -> Result<Self, Self::Error>;
}
