#[cfg(feature = "gssapi")]
pub mod gssapi;
pub mod netrc;
#[cfg(feature = "sspi")]
pub mod sspi;

use futures::future;

use self::netrc::BasicAuthenticator;

#[cfg(feature = "gssapi")]
use self::gssapi::NegotiateAuthenticator;

#[cfg(feature = "sspi")]
use self::sspi::NegotiateAuthenticator;

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    error: Box<dyn std::error::Error + Send + Sync>,
}

impl Error {
    pub fn new(kind: ErrorKind, error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self { kind, error }
    }

    pub fn temporary(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self {
            kind: ErrorKind::Temporary,
            error,
        }
    }

    pub fn permanent(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self {
            kind: ErrorKind::Permanent,
            error,
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ErrorKind {
    Temporary,
    Permanent,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            ErrorKind::Temporary => write!(f, "temporary"),
            ErrorKind::Permanent => write!(f, "permanent"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait Authenticator: Send + Sync {
    fn step<'async_trait>(
        &'async_trait self,
        last_headers: Option<hyper::HeaderMap>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<hyper::HeaderMap>> + Send + 'async_trait>,
    >;
}

struct NoneAuthenticator;

impl Authenticator for NoneAuthenticator {
    fn step<'async_trait>(
        &'async_trait self,
        last_headers: Option<hyper::HeaderMap>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<hyper::HeaderMap>> + Send + 'async_trait>,
    > {
        Box::pin(future::ok(last_headers.unwrap_or_default()))
    }
}

#[derive(Clone, Debug)]
pub enum AuthenticatorFactory {
    None,
    Basic,
    #[cfg(any(feature = "gssapi", feature = "sspi"))]
    Negotiate,
}

impl AuthenticatorFactory {
    pub fn basic() -> Self {
        AuthenticatorFactory::Basic
    }

    #[cfg(any(feature = "gssapi", feature = "sspi"))]
    pub fn negotiate() -> Self {
        AuthenticatorFactory::Negotiate
    }

    pub fn make(&self, proxy_url: &http::Uri) -> Result<Box<dyn Authenticator>> {
        match self {
            Self::None => Ok(Box::new(NoneAuthenticator)),
            Self::Basic => Ok(Box::new(BasicAuthenticator::new(&proxy_url)?)),
            #[cfg(any(feature = "gssapi", feature = "sspi"))]
            Self::Negotiate => Ok(Box::new(NegotiateAuthenticator::new(&proxy_url)?)),
        }
    }
}

impl std::fmt::Display for AuthenticatorFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        let name = match *self {
            Self::None => "none",
            Self::Basic => "basic",
            #[cfg(feature = "gssapi")]
            Self::Negotiate => "negotiate",
            #[cfg(feature = "sspi")]
            Self::Negotiate => "negotiate",
        };
        f.write_str(name)
    }
}