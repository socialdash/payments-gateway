use super::error::*;
use client::{HttpClient, StoriqaClient};
use futures;
use futures::prelude::*;
use hyper::{header::HeaderValue, Body, HeaderMap, Method, Response, Uri};
use models::Auth;
use std::sync::Arc;

mod fallback;
mod users;

pub use self::fallback::*;
pub use self::users::*;

pub type ControllerFuture = Box<Future<Item = Response<Body>, Error = Error> + Send>;

#[derive(Clone)]
pub struct Context {
    pub body: Vec<u8>,
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap<HeaderValue>,
    pub auth: Option<Auth>,
    pub client: Arc<HttpClient>,
    pub storiqa_client: Arc<StoriqaClient>,
}

fn authorize<F>(ctx: &Context, f: F) -> ControllerFuture
where
    F: FnOnce() -> ControllerFuture,
{
    if ctx.auth.is_none() {
        Box::new(futures::future::err(ErrorKind::Unauthorized.into()))
    } else {
        f()
    }
}
