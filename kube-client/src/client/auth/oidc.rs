use std::collections::HashMap;

use base64::{
    alphabet,
    engine::{
        fast_portable::{FastPortable, FastPortableConfig},
        DecodePaddingMode,
    },
};
use chrono::{Duration, TimeZone, Utc};
use form_urlencoded::Serializer;
use http::{
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Method, StatusCode, Version,
};
use hyper::{
    body,
    client::HttpConnector,
    http::{uri::InvalidUri, Uri},
    Client, Request,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer};
use serde_json::Number;
use thiserror::Error;

/// Possible errors when extracting expiration time from an ID token.
#[derive(Error, Debug)]
pub enum IdTokenError {
    #[error("not a valid JWT token")]
    InvalidFormat,
    #[error("failed to decode base64: {0}")]
    InvalidBase64(
        #[source]
        #[from]
        base64::DecodeError,
    ),
    #[error("failed to unmarshal JSON: {0}")]
    InvalidJson(
        #[source]
        #[from]
        serde_json::Error,
    ),
    #[error("invalid expiration timestamp")]
    InvalidExpirationTimestamp,
}

/// Possible error when initializing the [`Refresher`].
#[derive(Error, Debug, Clone)]
pub enum RefreshInitError {
    #[error("missing field {0}")]
    MissingField(&'static str),
    #[cfg(feature = "openssl-tls")]
    #[cfg_attr(docsrs, doc(cfg(feature = "openssl-tls")))]
    #[error("failed to create OpenSSL HTTPS connector: {0}")]
    CreateOpensslHttpsConnector(
        #[source]
        #[from]
        openssl::error::ErrorStack,
    ),
}

/// Possible errors when using the refresh token.
#[derive(Error, Debug)]
pub enum RefreshError {
    #[error("invalid URI: {0}")]
    InvalidURI(
        #[source]
        #[from]
        InvalidUri,
    ),
    #[error("hyper error: {0}")]
    HyperError(
        #[source]
        #[from]
        hyper::Error,
    ),
    #[error("invalid metadata received from the provider: {0}")]
    InvalidMetadata(#[source] serde_json::Error),
    #[error("request failed with status code: {0}")]
    RequestFailed(StatusCode),
    #[error("http error: {0}")]
    HttpError(
        #[source]
        #[from]
        http::Error,
    ),
    #[error("failed to authorize with the provider using any of known authorization styles")]
    AuthorizationFailure,
    #[error("invalid token response received from the provider: {0}")]
    InvalidTokenResponse(#[source] serde_json::Error),
    #[error("no ID token received from the provider")]
    NoIdTokenReceived,
}

/// Possible errors when dealing with OIDC.
#[derive(Error, Debug)]
pub enum Error {
    #[error("missing field {}", Oidc::CONFIG_ID_TOKEN)]
    IdTokenMissing,
    #[error("invalid ID token: {0}")]
    IdToken(
        #[source]
        #[from]
        IdTokenError,
    ),
    #[error("ID token expired and refreshing is not possible: {0}")]
    RefreshInit(
        #[source]
        #[from]
        RefreshInitError,
    ),
    #[error("ID token expired and refreshing failed: {0}")]
    Refresh(
        #[source]
        #[from]
        RefreshError,
    ),
}

const BASE64_ENGINE: FastPortable = FastPortable::from(
    &alphabet::URL_SAFE,
    FastPortableConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Debug)]
pub struct Oidc {
    id_token: SecretString,
    refresher: Result<Refresher, RefreshInitError>,
}

impl Oidc {
    /// Config key for the ID token.
    const CONFIG_ID_TOKEN: &str = "id-token";
    /// How many seconds before ID token expiration we want to refresh it.
    const EXPIRY_DELTA_SECONDS: i64 = 10;

    /// Check whether the stored ID token can still be used.
    fn token_valid(&self) -> Result<bool, IdTokenError> {
        let part = self
            .id_token
            .expose_secret()
            .split('.')
            .skip(1)
            .next()
            .ok_or(IdTokenError::InvalidFormat)?;
        let payload = base64::decode_engine(part, &BASE64_ENGINE)?;
        let expiry = serde_json::from_slice::<Claims>(&payload)?.expiry;
        let timestamp = Utc
            .timestamp_opt(expiry, 0)
            .earliest()
            .ok_or(IdTokenError::InvalidExpirationTimestamp)?;

        let valid = Utc::now() + Duration::seconds(Self::EXPIRY_DELTA_SECONDS) < timestamp;

        Ok(valid)
    }

    /// Retrieve the ID token. If the stored ID token is or will soon be expired, try refreshing it first.
    pub async fn id_token(&mut self) -> Result<String, Error> {
        if self.token_valid()? {
            return Ok(self.id_token.expose_secret().clone());
        }

        let id_token = self.refresher.as_mut().map_err(|e| e.clone())?.id_token().await?;

        self.id_token = id_token.clone().into();

        Ok(id_token)
    }

    /// Create an instance of this struct from the auth provider config.
    pub fn from_config(config: &HashMap<String, String>) -> Result<Self, Error> {
        let id_token = config
            .get(Self::CONFIG_ID_TOKEN)
            .ok_or(Error::IdTokenMissing)?
            .clone()
            .into();
        let refresher = Refresher::from_config(config);

        Ok(Self { id_token, refresher })
    }
}

/// Claims extracted from the ID token. Only expiration time here is important.
#[derive(Deserialize)]
struct Claims {
    #[serde(rename = "exp", deserialize_with = "deserialize_expiry")]
    expiry: i64,
}

/// Deserialize expiration time from a JSON number.
fn deserialize_expiry<'de, D: Deserializer<'de>>(deserializer: D) -> core::result::Result<i64, D::Error> {
    let json_number = Number::deserialize(deserializer)?;

    json_number
        .as_i64()
        .or_else(|| Some(json_number.as_f64()? as i64))
        .ok_or(serde::de::Error::custom("cannot be casted to i64"))
}

/// Metadata retrieved from the provider. Only token endpoint here is important.
#[derive(Deserialize)]
struct Metadata {
    token_endpoint: String,
}

/// Authorization styles used by different providers.
/// Some providers require the authorization info in the header, some in the request body.
/// Some providers reject requests when authorization info is passed in both.
#[derive(Debug, Clone, Copy)]
enum AuthStyle {
    Header,
    Params,
}

impl AuthStyle {
    /// All known authorization styles.
    const ALL: [Self; 2] = [Self::Header, Self::Params];
}

/// Token response from the provider. Only refresh token and id token here are important.
#[derive(Deserialize)]
struct TokenResponse {
    refresh_token: Option<String>,
    id_token: Option<String>,
}

#[cfg(not(any(feature = "rustls-tls", feature = "openssl-tls")))]
compile_error!(
    "At least one of rustls-tls or openssl-tls feature must be enabled to use refresh-oidc feature"
);
// Current TLS feature precedence when more than one are set:
// 1. openssl-tls
// 2. rustls-tls
#[cfg(feature = "openssl-tls")]
type HttpsConnector = hyper_openssl::HttpsConnector<HttpConnector>;
#[cfg(all(not(feature = "openssl-tls"), feature = "rustls-tls"))]
type HttpsConnector = hyper_rustls::HttpsConnector<HttpConnector>;

/// Struct for refreshing the ID token with the refresh token.
#[derive(Debug)]
struct Refresher {
    issuer: String,
    /// Token endpoint exposed by the provider.
    /// Retrieved from the provider metadata with the first refresh request.
    token_endpoint: Option<String>,
    /// Refresh token used in the refresh requests.
    /// Updated when a new refresh token is returned by the provider.
    refresh_token: SecretString,
    client_id: SecretString,
    client_secret: SecretString,
    https_client: Client<HttpsConnector, String>,
    /// Authorization style used by the provider.
    /// Determined with the first refresh request by trying all known styles.
    auth_style: Option<AuthStyle>,
}

impl Refresher {
    /// Config key for the client ID.
    const CONFIG_CLIENT_ID: &str = "client-id";
    /// Config key for the client secret.
    const CONFIG_CLIENT_SECRET: &str = "client-secret";
    /// Config key for the issuer url.
    const CONFIG_ISSUER_URL: &str = "idp-issuer-url";
    /// Config key for the refresh token.
    const CONFIG_REFRESH_TOKEN: &str = "refresh-token";

    /// Create a new instance of this struct from the provider config.
    fn from_config(config: &HashMap<String, String>) -> Result<Self, RefreshInitError> {
        let get_field = |name: &'static str| {
            config
                .get(name)
                .cloned()
                .ok_or(RefreshInitError::MissingField(name))
        };

        let issuer = get_field(Self::CONFIG_ISSUER_URL)?;
        let refresh_token = get_field(Self::CONFIG_REFRESH_TOKEN)?.into();
        let client_id = get_field(Self::CONFIG_CLIENT_ID)?.into();
        let client_secret = get_field(Self::CONFIG_CLIENT_SECRET)?.into();


        #[cfg(feature = "openssl-tls")]
        let https = hyper_openssl::HttpsConnector::new()?;
        #[cfg(all(not(feature = "openssl-tls"), feature = "rustls-tls"))]
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .https_only()
            .enable_http1()
            .build();

        let https_client = hyper::Client::builder().build(https);

        Ok(Self {
            issuer,
            token_endpoint: None,
            refresh_token,
            client_id,
            client_secret,
            https_client,
            auth_style: None,
        })
    }

    /// If the token endpoint is not yet cached in this struct, extract it from the provider metadata and store in the cache.
    /// Provider metadata is retrieved from a well-known path.
    async fn token_endpoint(&mut self) -> Result<String, RefreshError> {
        if let Some(endpoint) = self.token_endpoint.clone() {
            return Ok(endpoint);
        }

        let discovery = format!("{}/.well-known/openid-configuration", self.issuer).parse::<Uri>()?;
        let response = self.https_client.get(discovery).await?;

        if response.status().is_success() {
            let body = body::to_bytes(response.into_body()).await?;
            let metadata =
                serde_json::from_slice::<Metadata>(body.as_ref()).map_err(RefreshError::InvalidMetadata)?;

            self.token_endpoint.replace(metadata.token_endpoint.clone());

            Ok(metadata.token_endpoint)
        } else {
            Err(RefreshError::RequestFailed(response.status()))
        }
    }

    /// Prepare a token request to the provider.
    fn token_request(&self, endpoint: &str, auth_style: AuthStyle) -> Result<Request<String>, RefreshError> {
        let mut builder = Request::builder()
            .uri(endpoint)
            .method(Method::POST)
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            )
            .version(Version::HTTP_11);
        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", self.refresh_token.expose_secret()),
        ];

        match auth_style {
            AuthStyle::Header => {
                builder = builder.header(
                    AUTHORIZATION,
                    format!(
                        "Basic {}",
                        base64::encode(format!(
                            "{}:{}",
                            self.client_id.expose_secret(),
                            self.client_secret.expose_secret()
                        ))
                    ),
                );
            }
            AuthStyle::Params => {
                params.extend([
                    ("client_id", self.client_id.expose_secret().as_str()),
                    ("client_secret", self.client_secret.expose_secret().as_str()),
                ]);
            }
        };

        let body = Serializer::new(String::new()).extend_pairs(params).finish();

        builder.body(body).map_err(Into::into)
    }

    /// Fetch a new ID token from the provider.
    async fn id_token(&mut self) -> Result<String, RefreshError> {
        let token_endpoint = self.token_endpoint().await?;

        let response = match self.auth_style.clone() {
            Some(style) => {
                let request = self.token_request(&token_endpoint, style)?;
                self.https_client.request(request).await?
            }
            None => {
                let mut ok_response = None;

                for style in AuthStyle::ALL {
                    let request = self.token_request(&token_endpoint, style)?;
                    let response = self.https_client.request(request).await?;
                    if response.status().is_success() {
                        ok_response.replace(response);
                        self.auth_style.replace(style);
                        break;
                    }
                }

                ok_response.ok_or(RefreshError::AuthorizationFailure)?
            }
        };

        if !response.status().is_success() {
            return Err(RefreshError::RequestFailed(response.status()));
        }

        let body = body::to_bytes(response.into_body()).await?;
        let token_response = serde_json::from_slice::<TokenResponse>(body.as_ref())
            .map_err(RefreshError::InvalidTokenResponse)?;

        if let Some(token) = token_response.refresh_token {
            self.refresh_token = token.into();
        }

        token_response.id_token.ok_or(RefreshError::NoIdTokenReceived)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_valid() {
        let mut oidc = Oidc {
            id_token: String::new().into(),
            refresher: Err(RefreshInitError::MissingField(Refresher::CONFIG_REFRESH_TOKEN)),
        };

        // Proper JWT expiring at 2123-06-28T15:18:12.629Z
        let token_valid = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9\
.eyJpc3MiOiJPbmxpbmUgSldUIEJ1aWxkZXIiLCJpYXQiOjE2ODc5NjU0NTIsImV4cCI6NDg0MzYzOTA5MiwiYXVkIjoid3d3LmV4YW1wbGUuY29tIiwic3ViIjoianJvY2tldEBleGFtcGxlLmNvbSIsIkVtYWlsIjoiYmVlQGV4YW1wbGUuY29tIn0\
.GKTkPMywcNQv0n01iBfv_A6VuCCCcAe72RhP0OrZsQM";
        // Proper JWT expired at 2023-06-28T15:19:53.421Z
        let token_expired = "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9\
.eyJpc3MiOiJPbmxpbmUgSldUIEJ1aWxkZXIiLCJpYXQiOjE2ODc5NjU0NTIsImV4cCI6MTY4Nzk2NTU5MywiYXVkIjoid3d3LmV4YW1wbGUuY29tIiwic3ViIjoianJvY2tldEBleGFtcGxlLmNvbSIsIkVtYWlsIjoiYmVlQGV4YW1wbGUuY29tIn0\
.zTDnfI_zXIa6yPKY_ZE8r6GoLK7Syj-URcTU5_ryv1M";

        oidc.id_token = token_valid.to_string().into();
        assert!(oidc.token_valid().unwrap());

        oidc.id_token = token_expired.to_string().into();
        assert!(!oidc.token_valid().unwrap());

        let malformed_token = token_expired.split_once('.').unwrap().0.to_string();
        oidc.id_token = malformed_token.into();
        oidc.token_valid().unwrap_err();

        let invalid_base64_token = token_valid
            .split_once('.')
            .map(|(prefix, suffix)| format!("{}.?{}", prefix, suffix))
            .unwrap();
        oidc.id_token = invalid_base64_token.into();
        oidc.token_valid().unwrap_err();

        let invalid_claims = [("sub", "jrocket@example.com"), ("aud", "www.example.com")]
            .into_iter()
            .collect::<HashMap<_, _>>();
        let invalid_claims_token = format!(
            "{}.{}.{}",
            token_valid.split_once('.').unwrap().0,
            base64::encode(serde_json::to_string(&invalid_claims).unwrap()),
            token_valid.rsplit_once('.').unwrap().1,
        );
        oidc.id_token = invalid_claims_token.into();
        oidc.token_valid().unwrap_err();
    }
}
