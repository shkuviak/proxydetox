#![allow(clippy::type_complexity)]

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::{
    collections::HashMap,
    task::{self, Poll},
};

use http::header::{HOST, VIA};
use http::HeaderValue;
use http::Uri;
use http::{Request, Response};
use hyper::service::Service;
use hyper::Body;
use parking_lot::Mutex;
use proxy_client::HttpProxyConnector;
use tracing_attributes::instrument;
use tracing_futures::Instrument;

use crate::auth::AuthenticatorFactory;
use paclib::proxy::ProxyDesc;
use paclib::Evaluator;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("authentication mechanism error: {0}")]
    Auth(#[from] crate::auth::Error),
    #[error("invalid URI")]
    InvalidUri,
    #[error("upstream proxy ({0}) requires authentication")]
    ProxyAuthenticationRequired(ProxyDesc),
}

impl From<crate::client::Error> for Error {
    fn from(cause: crate::client::Error) -> Error {
        use crate::client;
        match cause {
            client::Error::Auth(cause) => Error::Auth(cause),
            client::Error::Hyper(cause) => Error::Hyper(cause),
            client::Error::InvalidUri => Error::InvalidUri,
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

type ProxyClient = crate::client::Client;

#[derive(Debug)]
pub struct Builder {
    pac_script: Option<String>,
    auth: Option<AuthenticatorFactory>,
    pool_max_idle_per_host: usize,
    pool_idle_timeout: Option<Duration>,
    always_use_connect: bool,
}

impl std::default::Default for Builder {
    fn default() -> Self {
        Self {
            pac_script: None,
            auth: None,
            pool_max_idle_per_host: usize::MAX,
            pool_idle_timeout: None,
            always_use_connect: false,
        }
    }
}

impl Builder {
    /// PAC script used for evaluation
    /// If `None`, FindProxy will evaluate to DIRECT
    pub fn pac_script(mut self, pac_script: Option<String>) -> Self {
        self.pac_script = pac_script;
        self
    }
    /// Authenticator factory (Basic or Negotiate)
    /// If `None`, use no authentication toward the proxy.
    pub fn authenticator_factory(mut self, factory: Option<AuthenticatorFactory>) -> Self {
        self.auth = factory;
        self
    }
    /// sets the maximum idle connection per host allowed in the pool
    pub fn pool_max_idle_per_host(mut self, max: usize) -> Self {
        self.pool_max_idle_per_host = max;
        self
    }
    /// set an optional timeout for idle sockets being kept-aliv.
    pub fn pool_idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.pool_idle_timeout = timeout;
        self
    }
    /// use the CONNECT method even for HTTP requests.
    pub fn always_use_connect(mut self, yesno: bool) -> Self {
        self.always_use_connect = yesno;
        self
    }

    pub fn build(self) -> Session {
        let pac_script = self
            .pac_script
            .unwrap_or_else(|| "function FindProxyForURL(url, host) { return \"DIRECT\"; }".into());
        let eval = Mutex::new(Evaluator::new(&pac_script).unwrap());
        let auth = self.auth.unwrap_or(AuthenticatorFactory::None);
        let direct_client = hyper::Client::builder()
            .pool_max_idle_per_host(self.pool_max_idle_per_host)
            .pool_idle_timeout(self.pool_idle_timeout)
            .build_http();
        Session(Arc::new(Inner {
            eval,
            direct_client,
            proxy_clients: Default::default(),
            auth,
            pool_idle_timeout: self.pool_idle_timeout,
            pool_max_idle_per_host: self.pool_max_idle_per_host,
            always_use_connect: self.always_use_connect,
        }))
    }
}

#[derive(Clone)]
pub struct Session(Arc<Inner>);

struct Inner {
    eval: Mutex<paclib::Evaluator>,
    direct_client: hyper::Client<hyper::client::HttpConnector>,
    proxy_clients: Mutex<HashMap<Uri, ProxyClient>>,
    auth: AuthenticatorFactory,
    pool_max_idle_per_host: usize,
    pool_idle_timeout: Option<Duration>,
    always_use_connect: bool,
}

impl Inner {
    fn find_proxy(&self, uri: &Uri) -> paclib::Proxies {
        self.eval.lock().find_proxy(uri).unwrap_or_else(|cause| {
            tracing::error!("failed to find_proxy: {:?}", cause);
            paclib::Proxies::direct()
        })
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish()
    }
}

impl Session {
    pub fn builder() -> Builder {
        Default::default()
    }

    // For now just use the first reportet proxy
    async fn find_proxy(&mut self, uri: &http::Uri) -> paclib::proxy::ProxyDesc {
        let inner = self.0.clone();
        let uri = uri.clone();
        let proxy = tokio::task::block_in_place(move || inner.find_proxy(&uri));
        proxy.first()
    }

    #[instrument(level = "trace", skip(self))]
    fn proxy_client(&mut self, uri: http::Uri) -> Result<ProxyClient> {
        let mut proxies = self.0.proxy_clients.lock();
        match proxies.get(&uri) {
            Some(proxy) => Ok(proxy.clone()),
            None => {
                tracing::trace!("new proxy client");
                let auth = self.0.auth.make(&uri);
                let auth = match auth {
                    Ok(auth) => auth,
                    Err(ref cause) => {
                        tracing::warn!("error makeing authenticator: {}", cause);
                        Box::new(crate::auth::NoneAuthenticator)
                    }
                };
                let client = hyper::Client::builder()
                    .pool_max_idle_per_host(self.0.pool_max_idle_per_host)
                    .pool_idle_timeout(self.0.pool_idle_timeout)
                    .build(HttpProxyConnector::new(uri.clone()));
                let client = ProxyClient::new(client, auth);
                proxies.insert(uri, client.clone());
                Ok(client)
            }
        }
    }

    pub async fn process(&mut self, req: hyper::Request<Body>) -> Result<Response<Body>> {
        let res = if req.uri().authority().is_some() {
            self.dispatch(req).await
        } else if req.method() == hyper::Method::GET {
            self.management(req).await
        } else {
            let mut res = Response::new(Body::from(String::from("Invalid Requst")));
            *res.status_mut() = http::StatusCode::BAD_REQUEST;
            Ok(res)
        };

        tracing::debug!("response: {:?}", res.as_ref().map(|r| r.status()));
        res
    }

    pub async fn dispatch(&mut self, mut req: hyper::Request<Body>) -> Result<Response<Body>> {
        // Remove hop-by-hop headers which are meant for the proxy.
        // "proxy-connection" is not an official header, but used by many clients.
        let _proxy_connection = req
            .headers_mut()
            .remove(http::header::HeaderName::from_static("proxy-connection"));
        let _proxy_auth = req.headers_mut().remove(http::header::PROXY_AUTHORIZATION);

        let proxy = self.find_proxy(req.uri()).await;
        self.dispatch_with_proxy(proxy, req).await
    }

    #[instrument(level = "debug", skip(self, req))]
    pub async fn dispatch_with_proxy(
        &mut self,
        proxy: ProxyDesc,
        req: hyper::Request<Body>,
    ) -> Result<Response<Body>> {
        let proxy_client;
        let client: &(dyn crate::client::ForwardClient + Send + Sync) = match proxy {
            ProxyDesc::Direct => &self.0.direct_client,
            ProxyDesc::Proxy(ref proxy) => {
                proxy_client = self.proxy_client(proxy.clone())?;
                &proxy_client
            }
        };

        let is_connect = req.method() == hyper::Method::CONNECT || self.0.always_use_connect;
        let mut res = if is_connect {
            client.connect(req).await
        } else {
            client.http(req).await
        };

        if let Ok(ref mut res) = res {
            if res.status() == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
                tracing::error!("407 proxy authentication required for {}", &proxy);
                return Err(Error::ProxyAuthenticationRequired(proxy));
            }

            let via = HeaderValue::from_str(&format!(
                "{}; {}/{}",
                &proxy,
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))
            .unwrap();
            res.headers_mut().insert(VIA, via);
        }
        Ok(res?)
    }

    pub async fn management(&mut self, req: hyper::Request<Body>) -> Result<Response<Body>> {
        let resp = if req.uri() == "/proxy.pac" {
            let body = format!(
                "function FindProxyForURL(url, host) {{ return \"PROXY {}\"; }}\n",
                req.headers()
                    .get(HOST)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("127.0.0.1:3128")
            );
            let mut resp = Response::new(Body::from(body));
            resp.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::header::HeaderValue::from_static("application/x-ns-proxy-autoconfig"),
            );
            resp
        } else {
            let body = format!(
                "<!DOCTYPE html><html><h1>{}/{}</h1><h2>DNS Cache</h2><code>{:?}</code></html>",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                self.0.eval.lock().cache()
            );
            let mut resp = Response::new(Body::from(body));
            resp.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::header::HeaderValue::from_static("text/html"),
            );
            resp
        };
        Ok(resp)
    }
}

fn make_error_response<E>(error: &E) -> Response<Body>
where
    E: std::error::Error + Send + Sync,
{
    let body = format!(
        include_str!("502.html"),
        error,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    let mut resp = Response::new(Body::from(body));
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::header::HeaderValue::from_static("text/html"),
    );
    *resp.status_mut() = http::StatusCode::BAD_GATEWAY;

    resp
}

impl Service<Request<Body>> for Session {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut task::Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut detox = self.clone();
        let method = req.method().clone();
        let uri = req.uri().clone();
        let version = req.version();

        let res = async move {
            tracing::trace!("request: {:?}", &req);
            let res = detox.process(req).await;
            tracing::trace!("response: {:?}", &res);
            let out = match res {
                Err(ref error) => make_error_response(error),
                Ok(res) => res,
            };
            Ok(out)
        };
        let res = res
            .instrument(tracing::debug_span!("call", method=%method, uri=%uri, version=?version));
        Box::pin(res)
    }
}

impl<'a> Service<&'a hyper::server::conn::AddrStream> for Session {
    type Response = Self;
    type Error = std::convert::Infallible;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut task::Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Ok(()).into()
    }

    fn call(&mut self, socket: &hyper::server::conn::AddrStream) -> Self::Future {
        let this = self.clone();
        let addr = socket.remote_addr();
        let res = async move {
            tracing::debug!("new connection");
            Ok(this)
        };
        let res = res.instrument(tracing::debug_span!("call", addr=%addr));
        Box::pin(res)
    }
}