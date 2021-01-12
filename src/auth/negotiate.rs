use super::Result;
use http::{
    header::{PROXY_AUTHENTICATE, PROXY_AUTHORIZATION},
    HeaderValue,
};
use libgssapi::{
    context::{ClientCtx, CtxFlags},
    credential::{Cred, CredUsage},
    name::Name,
    oid::{OidSet, GSS_MECH_KRB5, GSS_NT_HOSTBASED_SERVICE},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::task;

#[derive(Debug, Clone)]
pub struct GssAuthenticator {
    proxy_url: http::Uri,
    //client: Arc<Mutex<ClientCtx>>,
    supports_auth: Arc<AtomicBool>,
}

impl GssAuthenticator {
    pub fn new(proxy_url: &http::Uri) -> Result<Self> {
        //let client = GssAuthenticator::make_client(proxy_url)?;
        Ok(Self {
            proxy_url: proxy_url.clone(),
            //client: Default::default(),
            supports_auth: Arc::new(AtomicBool::new(true)),
        })
    }

    fn make_client(proxy_url: &http::Uri) -> Result<ClientCtx> {
        let desired_mechs = {
            let mut s = OidSet::new().expect("OidSet::new");
            s.add(&GSS_MECH_KRB5).expect("GSS_MECH_KRB5");
            s
        };

        let service_name = format!("http@{}", proxy_url.host().expect("URL with host"));
        let service_name = service_name.as_bytes();

        let name = Name::new(service_name, Some(&GSS_NT_HOSTBASED_SERVICE))?;
        let name = name.canonicalize(Some(&GSS_MECH_KRB5))?;

        let client_cred = Cred::acquire(None, None, CredUsage::Initiate, Some(&desired_mechs))?;

        Ok(ClientCtx::new(
            client_cred,
            name,
            CtxFlags::GSS_C_MUTUAL_FLAG,
            Some(&GSS_MECH_KRB5),
        ))
    }

    // Extract the server token from "Proxy-Authenticate: Negotiate <base64>" header value
    fn server_token(response: &http::Response<hyper::Body>) -> Option<Vec<u8>> {
        let mut server_tok: Option<Vec<u8>> = None;

        for auth in response.headers().get_all(PROXY_AUTHENTICATE) {
            if let Ok(auth) = auth.to_str() {
                let mut split = auth.splitn(2, ' ');
                if let Some(method) = split.next() {
                    if method == "Negotiate" {
                        if let Some(token) = split.next() {
                            if let Ok(token) = base64::decode(token) {
                                server_tok = Some(token);
                            }
                        }
                    }
                }
            }
        }

        server_tok
    }

    // Call `step` `while request.status() == http::StatusCode::PROXY_AUTHENTICATION_REQUIRED {}`.
    pub async fn step(&self, response: Option<&http::Response<hyper::Body>>) -> hyper::HeaderMap {
        let mut headers = hyper::HeaderMap::new();

        if self.supports_auth.load(Ordering::Relaxed) == false {
            return headers;
        }

        let server_tok = response.map(|r| Self::server_token(&r)).flatten();

        // Get client token, and create new gss client context.
        let token = {
            let proxy_url = self.proxy_url.clone();
            //let client = self.client.clone();
            task::spawn_blocking(move || {
                //let mut stepper = client.lock().unwrap();
                let stepper = Self::make_client(&proxy_url).unwrap();
                let token = server_tok.as_ref().map(|b| &**b);
                let token = stepper.step(token);
                //*stepper = GssAuthenticator::make_client(&proxy_url).expect("make_client");
                token
            })
            .await
            .expect("spawn_blocking")
        };

        match token {
            Ok(Some(token)) => {
                let b64token = base64::encode(&*token);
                tracing::debug!("auth gss token: {}", &b64token);

                let auth_str = format!("Negotiate {}", b64token);
                headers.append(
                    PROXY_AUTHORIZATION,
                    HeaderValue::from_str(&auth_str).expect("valid header value"),
                );
            }
            Ok(None) => {
                // finished with setting up the token, cannot re-use ClinetCtx
            }
            Err(ref err) => {
                // When authentication is not supported, to not try again.
                if err
                    .major
                    .contains(libgssapi::error::MajorFlags::GSS_S_BAD_MECH)
                {
                    self.supports_auth.store(false, Ordering::Relaxed);
                } else {
                    tracing::error!(
                        "gss step error for {}: {} ({:?})",
                        &self.proxy_url,
                        &err,
                        &err
                    )
                }
            }
        }

        headers
    }
}
