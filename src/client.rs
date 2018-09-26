use config::Config;
use futures::prelude::*;
use hyper;
use hyper::{client::HttpConnector, Body, Error, Request, Response};
use hyper_tls::HttpsConnector;

pub trait Client: Send + Sync {
    fn request(&self, req: Request<Body>) -> Box<Future<Item = Response<Body>, Error = Error>>;
}

#[derive(Clone)]
pub struct ClientImpl {
    cli: hyper::Client<HttpsConnector<HttpConnector>>,
}

impl ClientImpl {
    pub fn new(config: &Config) -> Self {
        let mut connector = HttpsConnector::new(config.client.dns_threads).unwrap();
        connector.https_only(true);
        let cli = hyper::Client::builder().build(connector);
        Self { cli }
    }
}

impl Client for ClientImpl {
    fn request(&self, req: Request<Body>) -> Box<Future<Item = Response<Body>, Error = Error>> {
        Box::new(self.cli.request(req))
    }
}
