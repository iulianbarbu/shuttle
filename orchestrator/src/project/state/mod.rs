use std::{convert::Infallible, pin::Pin};

use async_trait::async_trait;
use futures::{stream, Stream};

pub mod machine;

macro_rules! safe_unwrap {
    {$fst:ident$(.$attr:ident$(($ex:expr))?)+} => {
        $fst$(
            .$attr$(($ex))?
                .as_ref()
                .ok_or_else(|| ProjectError::internal(
                    concat!("container state object is malformed at attribute: ", stringify!($attr))
                ))?
        )+
    }
}

macro_rules! deserialize_json {
    {$ty:ty: $($json:tt)+} => {{
        let __ty_json = serde_json::json!($($json)+);
        serde_json::from_value::<$ty>(__ty_json).unwrap()
    }};
    {$($json:tt)+} => {{
        let __ty_json = serde_json::json!($($json)+);
        serde_json::from_value(__ty_json).unwrap()
    }}
}

macro_rules! impl_from_variant {
    {$e:ty: $($s:ty => $v:ident $(,)?)+} => {
        $(
            impl From<$s> for $e {
                fn from(s: $s) -> $e {
                    <$e>::$v(s)
                }
            }
        )+
    };
}

const RUNTIME_API_PORT: u16 = 8001;
const MAX_RECREATES: usize = 5;
const MAX_RESTARTS: usize = 5;
const MAX_REBOOTS: usize = 3;

// Client used for health checks
static CLIENT: Lazy<Client<HttpConnector>> = Lazy::new(Client::new);
// Health check must succeed within 10 seconds
pub static IS_HEALTHY_TIMEOUT: Duration = Duration::from_secs(10);

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
    type Error: std::error::Error;

    async fn refresh(self, ctx: &Ctx) -> Result<Self, Self::Error>;
}
