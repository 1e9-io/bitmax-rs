use chrono::Utc;
use failure::Fallible;
use hmac::{Hmac, Mac, NewMac};
use log::debug;
use reqwest::{Client, Method, Response};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::from_str;
use sha2::Sha256;
use url::Url;

pub mod request;
mod util;
pub mod websocket;

use request::Request;
use util::{HeaderBuilder, ToUrlQuery};

const HTTP_URL: &str = "bitmax.io";
const API_URL: &str = "/api/pro/v1";

#[derive(Debug, Clone)]
struct Auth {
    pub public_key: String,
    pub private_key_bytes: Vec<u8>,
    pub account_group: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct BitMaxClient {
    client: Client,
    auth: Option<Auth>,
}

#[derive(Deserialize, Debug)]
struct ResponseSchema<T> {
    code: u32,
    data: T,
}

impl BitMaxClient {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with_auth(
        public_key: &str,
        private_key: &str,
        account_group: Option<u32>,
    ) -> Fallible<Self> {
        Ok(Self {
            auth: Some(Auth {
                private_key_bytes: base64::decode(private_key)?,
                public_key: public_key.into(),
                account_group,
            }),
            client: Default::default(),
        })
    }

    /// Account group is needed for most of the account requests.
    /// Make the `Account` request to get your account group.
    pub fn set_account_group(&mut self, account_group: u32) -> Fallible<()> {
        self.auth
            .as_mut()
            .ok_or_else(|| failure::format_err!("missing auth"))?
            .account_group = Some(account_group);

        Ok(())
    }

    fn attach_auth_headers<B: HeaderBuilder>(&self, builder: B, api_path: &str) -> Fallible<B> {
        let auth = self
            .auth
            .as_ref()
            .ok_or_else(|| failure::format_err!("missing auth keys"))?;

        let timestamp = Utc::now().timestamp_millis();

        let prehash = format!("{}+{}", timestamp, &api_path[1..]); // skip the first `/`
        let mut mac = Hmac::<Sha256>::new_varkey(&auth.private_key_bytes)
            .map_err(|e| failure::format_err!("{}", e))?;
        mac.update(prehash.as_bytes());
        let signature = base64::encode(mac.finalize().into_bytes());

        Ok(builder
            .add_header("x-auth-key", &auth.public_key)
            .add_header("x-auth-timestamp", &timestamp.to_string())
            .add_header("x-auth-signature", &signature))
    }

    fn render_url(
        &self,
        protocol: &str,
        endpoint: &str,
        add_account_group: bool,
    ) -> Fallible<String> {
        Ok(if add_account_group {
            format!(
                "{}://{}/{}{}{}",
                protocol,
                HTTP_URL,
                self.auth
                    .as_ref()
                    .and_then(|a| a.account_group.as_ref())
                    .ok_or_else(|| failure::format_err!("missing account group"))?,
                API_URL,
                endpoint
            )
        } else {
            format!("{}://{}{}{}", protocol, HTTP_URL, API_URL, endpoint)
        })
    }

    pub async fn request<Q: Request>(&self, request: Q) -> Fallible<Q::Response> {
        let url = self.render_url("https", &request.render_endpoint(), Q::NEEDS_ACCOUNT_GROUP)?;

        let req = match Q::METHOD {
            Method::GET => {
                let url = Url::parse_with_params(&url, request.to_url_query())?;
                debug!("sending GET message: {:?}", url.as_str());
                self.client.request(Q::METHOD, url.as_str())
            }
            Method::POST | Method::DELETE => {
                debug!(
                    "sending POST message: {:?}",
                    serde_json::to_string(&request)
                );
                self.client
                    .request(Q::METHOD, url.as_str())
                    .body(serde_json::to_string(&request)?)
                    .header("content-type", "application/json")
            }
            _ => failure::bail!("unsupported method {}", Q::METHOD),
        };

        let req = req.header("user-agent", "bitmax-rs");

        let req = if Q::NEEDS_AUTH {
            self.attach_auth_headers(req, Q::API_PATH)?
        } else {
            req
        };

        self.handle_response(req.send().await?).await
    }

    async fn handle_response<T: DeserializeOwned + std::fmt::Debug>(
        &self,
        resp: Response,
    ) -> Fallible<T> {
        if resp.status().is_success() {
            let resp = resp.text().await?;
            debug!("got message: {}", &resp);
            match from_str::<ResponseSchema<T>>(&resp) {
                Ok(resp) => {
                    if resp.code == 0 {
                        Ok(resp.data)
                    } else {
                        Err(failure::format_err!("Non zero response code: {:?}", resp))
                    }
                }
                Err(e) => Err(failure::format_err!(
                    "error {} while deserializing {}",
                    e,
                    resp
                )),
            }
        } else {
            let resp_e = resp.error_for_status_ref().unwrap_err();
            Err(failure::format_err!(
                "error: {}; body: {};",
                resp_e,
                resp.text().await?
            ))
        }
    }
}
