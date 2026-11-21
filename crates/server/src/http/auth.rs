//! Login flows, browser cookies and GitHub OAuth request validation.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use oauth2::basic::BasicClient;
use oauth2::{
    AsyncHttpClient, AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    EndpointNotSet, EndpointSet, HttpRequest, HttpResponse, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, TokenResponse, TokenUrl,
};
use rand::TryRng;
use rand::rngs::SysRng;
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ApiError, ApiState};
use crate::store::{SessionIdentity, User};

pub const SESSION_COOKIE: &str = "__Host-reticula_session";
const LOOPBACK_SESSION_COOKIE: &str = "reticula_session";
const STATE_COOKIE: &str = "reticula_oauth";
const CALLBACK: &str = "/api/auth/github/callback";
const STATE_SECONDS: i64 = 600;
const SESSION_SECONDS: i32 = 30 * 24 * 60 * 60;

pub struct AuthConfig {
    public_origin: String,
    secure: bool,
    provider: Provider,
}

enum Provider {
    Github(Box<Github>),
    DevMock,
}

type OAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

struct Github {
    oauth: OAuthClient,
    http: OAuthHttp,
    cookie_key: [u8; 32],
    profile_url: Url,
}

struct OAuthHttp(reqwest::Client);

impl<'c> AsyncHttpClient<'c> for OAuthHttp {
    type Error = reqwest::Error;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, Self::Error>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(oauth_request(&self.0, request))
    }
}

impl AuthConfig {
    pub fn from_env(bind_ip: IpAddr) -> Result<Self, String> {
        let origin = required("RETICULA_PUBLIC_ORIGIN")?;
        match std::env::var("RETICULA_AUTH_MODE")
            .as_deref()
            .unwrap_or("github")
        {
            "dev-mock" if bind_ip.is_loopback() => Self::dev_mock(&origin),
            "dev-mock" => Err("dev-mock requires a loopback BIND_ADDR".into()),
            "github" => Self::github(
                &origin,
                required("RETICULA_GITHUB_CLIENT_ID")?,
                required("RETICULA_GITHUB_CLIENT_SECRET")?,
            ),
            _ => Err("RETICULA_AUTH_MODE must be github or dev-mock".into()),
        }
    }

    pub fn dev_mock(origin: &str) -> Result<Self, String> {
        let origin = parse_origin(origin)?;
        if !cfg!(debug_assertions) || !is_loopback(&origin) {
            return Err("dev-mock requires a debug build and a loopback origin".into());
        }
        Ok(Self::new(&origin, Provider::DevMock))
    }

    pub fn github(origin: &str, client_id: String, client_secret: String) -> Result<Self, String> {
        let origin = parse_origin(origin)?;
        if origin.scheme() != "https" && !is_loopback(&origin) {
            return Err("GitHub login requires an HTTPS or loopback origin".into());
        }
        let callback = origin.join(CALLBACK).map_err(|e| e.to_string())?;
        let oauth = BasicClient::new(ClientId::new(client_id))
            .set_client_secret(ClientSecret::new(client_secret))
            .set_auth_type(AuthType::RequestBody)
            .set_auth_uri(
                AuthUrl::new("https://github.com/login/oauth/authorize".into())
                    .map_err(|e| e.to_string())?,
            )
            .set_token_uri(
                TokenUrl::new("https://github.com/login/oauth/access_token".into())
                    .map_err(|e| e.to_string())?,
            )
            .set_redirect_uri(RedirectUrl::from_url(callback));
        let http = reqwest::Client::builder()
            .user_agent("reticula-server")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        let cookie_key =
            random_secret().map_err(|_| "could not initialize OAuth state key".to_owned())?;
        Ok(Self::new(
            &origin,
            Provider::Github(Box::new(Github {
                oauth,
                http: OAuthHttp(http),
                cookie_key,
                profile_url: Url::parse("https://api.github.com/user")
                    .map_err(|e| e.to_string())?,
            })),
        ))
    }

    fn new(origin: &Url, provider: Provider) -> Self {
        Self {
            public_origin: origin.origin().ascii_serialization(),
            secure: origin.scheme() == "https",
            provider,
        }
    }

    pub(super) fn check_origin(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let mut origins = headers.get_all(header::ORIGIN).iter();
        if origins.next().and_then(|origin| origin.to_str().ok())
            == Some(self.public_origin.as_str())
            && origins.next().is_none()
        {
            Ok(())
        } else {
            Err(ApiError::Forbidden)
        }
    }

    fn github_provider(&self) -> Result<&Github, ApiError> {
        match &self.provider {
            Provider::Github(github) => Ok(github),
            Provider::DevMock => Err(ApiError::NotFound),
        }
    }

    fn session_cookie(&self) -> &'static str {
        if self.secure {
            SESSION_COOKIE
        } else {
            LOOPBACK_SESSION_COOKIE
        }
    }

    pub(super) fn is_dev_mock(&self) -> bool {
        matches!(self.provider, Provider::DevMock)
    }
}

fn required(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn parse_origin(raw: &str) -> Result<Url, String> {
    let url =
        Url::parse(raw).map_err(|_| "RETICULA_PUBLIC_ORIGIN must be an HTTP or HTTPS origin")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("RETICULA_PUBLIC_ORIGIN must be an HTTP or HTTPS origin".into());
    }
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

pub(super) fn random_secret() -> Result<[u8; 32], ApiError> {
    let mut bytes = [0; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| ApiError::Internal)?;
    Ok(bytes)
}

pub(super) fn encode_secret(value: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(value)
}

fn decode_secret(value: &str) -> Option<[u8; 32]> {
    if value.len() != 43 {
        return None;
    }
    URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .ok()?
        .try_into()
        .ok()
}

pub(super) fn token_hash(value: &str) -> Option<[u8; 32]> {
    Some(Sha256::digest(decode_secret(value)?).into())
}

pub(super) fn session_hash(config: &AuthConfig, jar: &CookieJar) -> Option<[u8; 32]> {
    token_hash(jar.get(config.session_cookie())?.value())
}

pub(super) async fn authenticate_optional(
    state: &ApiState,
    jar: &CookieJar,
) -> Result<Option<(User, [u8; 32])>, ApiError> {
    let Some(hash) = session_hash(&state.auth, jar) else {
        return Ok(None);
    };
    let Some(user) = state.store.session_user(&hash).await? else {
        return Ok(None);
    };
    Ok(Some((user, hash)))
}

pub(super) async fn authenticate(state: &ApiState, jar: &CookieJar) -> Result<User, ApiError> {
    let hash = session_hash(&state.auth, jar).ok_or(ApiError::Unauthorized)?;
    state
        .store
        .session_user(&hash)
        .await?
        .ok_or(ApiError::Unauthorized)
}

pub(super) fn clear_session_cookie(config: &AuthConfig, jar: CookieJar) -> CookieJar {
    jar.remove(cookie(
        config.session_cookie(),
        String::new(),
        "/",
        config.secure,
        0,
    ))
}

fn cookie(
    name: &'static str,
    value: String,
    path: &'static str,
    secure: bool,
    seconds: i64,
) -> Cookie<'static> {
    Cookie::build((name, value))
        .path(path)
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .max_age(time::Duration::seconds(seconds))
        .build()
}

async fn sign_in(
    state: &ApiState,
    jar: CookieJar,
    identity: SessionIdentity<'_>,
) -> Result<(CookieJar, User), ApiError> {
    let secret = random_secret()?;
    let hash: [u8; 32] = Sha256::digest(secret).into();
    let previous = session_hash(&state.auth, &jar);
    let user = state
        .store
        .sign_in(&identity, hash, previous, SESSION_SECONDS)
        .await?;
    if let Some(previous) = previous {
        let _ = state.revocations.send(previous);
    }
    let jar = jar.add(cookie(
        state.auth.session_cookie(),
        encode_secret(&secret),
        "/",
        state.auth.secure,
        i64::from(SESSION_SECONDS),
    ));
    Ok((jar, user))
}

#[derive(Deserialize, ToSchema)]
pub(super) struct DevLogin {
    account: DevAccount,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum DevAccount {
    Alice,
    Bob,
    Carol,
}

#[utoipa::path(post, path = "/api/auth/dev-login", request_body = DevLogin,
    responses((status = 200, body = User), (status = 403), (status = 404)))]
pub(super) async fn dev_login(
    State(state): State<ApiState>,
    jar: CookieJar,
    Json(login): Json<DevLogin>,
) -> Result<(CookieJar, Json<User>), ApiError> {
    let (subject, name) = match login.account {
        DevAccount::Alice => ("alice", "Alice"),
        DevAccount::Bob => ("bob", "Bob"),
        DevAccount::Carol => ("carol", "Carol"),
    };
    let (jar, user) = sign_in(
        &state,
        jar,
        SessionIdentity {
            provider: "dev-mock",
            subject,
            name,
            avatar: "",
        },
    )
    .await?;
    Ok((jar, Json(user)))
}

#[utoipa::path(get, path = "/api/auth/me", security(("session" = [])), responses((status = 200, body = User), (status = 401)))]
pub(super) async fn me(
    State(state): State<ApiState>,
    jar: CookieJar,
) -> Result<Json<User>, ApiError> {
    Ok(Json(authenticate(&state, &jar).await?))
}

#[utoipa::path(post, path = "/api/auth/logout", responses((status = 204), (status = 403)))]
pub(super) async fn logout(
    State(state): State<ApiState>,
    jar: CookieJar,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(hash) = session_hash(&state.auth, &jar) {
        state.store.end_session(&hash).await?;
        let _ = state.revocations.send(hash);
    }
    Ok((
        clear_session_cookie(&state.auth, jar),
        StatusCode::NO_CONTENT,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LoginQuery {
    return_to: Option<String>,
}

#[utoipa::path(get, path = "/api/auth/github/login", params(("returnTo" = Option<String>, Query)),
    responses((status = 303), (status = 400), (status = 404)))]
pub(super) async fn github_login(
    State(state): State<ApiState>,
    jar: CookieJar,
    Query(query): Query<LoginQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let github = state.auth.github_provider()?;
    let destination = query.return_to.as_deref().unwrap_or("/app");
    if !valid_destination(destination) {
        return Err(ApiError::Invalid("invalid login destination".into()));
    }
    let nonce = random_secret()?;
    let verifier = PkceCodeVerifier::new(encode_secret(&random_secret()?));
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
    let expiry = now_seconds().saturating_add(STATE_SECONDS.unsigned_abs());
    let signed = seal(
        &github.cookie_key,
        expiry,
        &nonce,
        verifier.secret(),
        destination,
    )?;
    let (url, _) = github
        .oauth
        .authorize_url(|| CsrfToken::new(encode_secret(&nonce)))
        .set_pkce_challenge(challenge)
        .url();
    Ok((
        jar.add(cookie(
            STATE_COOKIE,
            signed,
            CALLBACK,
            state.auth.secure,
            STATE_SECONDS,
        )),
        Redirect::to(url.as_str()),
    ))
}

#[derive(Deserialize)]
pub(super) struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
}

#[derive(Deserialize)]
struct Profile {
    id: u64,
    login: String,
    avatar_url: String,
}

#[utoipa::path(get, path = "/api/auth/github/callback", params(("state" = String, Query), ("code" = String, Query)),
    responses((status = 303), (status = 400), (status = 404), (status = 502)))]
pub(super) async fn github_callback(
    State(state): State<ApiState>,
    jar: CookieJar,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let response_jar = jar.clone().remove(cookie(
        STATE_COOKIE,
        String::new(),
        CALLBACK,
        state.auth.secure,
        0,
    ));
    let result = complete_github(&state, &jar, query).await;
    match result {
        Ok((profile, destination)) => {
            let identity = SessionIdentity {
                provider: "github",
                subject: &profile.id.to_string(),
                name: &profile.login,
                avatar: &profile.avatar_url,
            };
            match sign_in(&state, response_jar.clone(), identity).await {
                Ok((jar, _)) => (jar, Redirect::to(&destination)).into_response(),
                Err(error) => (response_jar, error).into_response(),
            }
        }
        Err(error) => (response_jar, error).into_response(),
    }
}

async fn complete_github(
    state: &ApiState,
    jar: &CookieJar,
    query: CallbackQuery,
) -> Result<(Profile, String), ApiError> {
    let github = state.auth.github_provider()?;
    let nonce = query
        .state
        .as_deref()
        .and_then(decode_secret)
        .ok_or_else(|| ApiError::Invalid("invalid OAuth state".into()))?;
    let signed = jar
        .get(STATE_COOKIE)
        .ok_or_else(|| ApiError::Invalid("missing OAuth state".into()))?
        .value();
    let (verifier, destination) = unseal(&github.cookie_key, &nonce, signed, now_seconds())?;
    let code = query
        .code
        .filter(|code| !code.is_empty() && code.len() <= 256)
        .ok_or_else(|| ApiError::Invalid("missing OAuth code".into()))?;
    let token = github
        .oauth
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(PkceCodeVerifier::new(verifier))
        .request_async(&github.http)
        .await
        .map_err(|_| ApiError::Provider)?;
    let profile = github
        .http
        .0
        .get(github.profile_url.clone())
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .bearer_auth(token.access_token().secret())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| ApiError::Provider)?
        .json()
        .await
        .map_err(|_| ApiError::Provider)?;
    Ok((profile, destination))
}

async fn oauth_request(
    client: &reqwest::Client,
    request: HttpRequest,
) -> Result<HttpResponse, reqwest::Error> {
    let (parts, body) = request.into_parts();
    let response = client
        .request(parts.method, parts.uri.to_string())
        .headers(parts.headers)
        .body(body)
        .send()
        .await?;
    let mut result = HttpResponse::new(Vec::new());
    *result.status_mut() = response.status();
    *result.headers_mut() = response.headers().clone();
    *result.body_mut() = response.bytes().await?.to_vec();
    Ok(result)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn new_mac(key: &[u8; 32], nonce: &[u8; 32], payload: &[u8]) -> Result<Hmac<Sha256>, ApiError> {
    let mut mac = Hmac::new_from_slice(key).map_err(|_| ApiError::Internal)?;
    mac.update(nonce);
    mac.update(payload);
    Ok(mac)
}

fn valid_destination(path: &str) -> bool {
    path == "/app"
        || path == "/workbooks"
        || path == "/account"
        || path == "/sim"
        || path
            .strip_prefix("/app?collab=")
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
        || path
            .strip_prefix("/join/")
            .is_some_and(|token| decode_secret(token).is_some())
}

fn seal(
    key: &[u8; 32],
    expiry: u64,
    nonce: &[u8; 32],
    verifier: &str,
    destination: &str,
) -> Result<String, ApiError> {
    let mut bytes = expiry.to_be_bytes().to_vec();
    bytes.extend_from_slice(verifier.as_bytes());
    bytes.extend_from_slice(destination.as_bytes());
    bytes.extend_from_slice(&new_mac(key, nonce, &bytes)?.finalize().into_bytes());
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn unseal(
    key: &[u8; 32],
    nonce: &[u8; 32],
    signed: &str,
    now: u64,
) -> Result<(String, String), ApiError> {
    let invalid = || ApiError::Invalid("invalid or expired OAuth state".into());
    if signed.len() > 256 {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD.decode(signed).map_err(|_| invalid())?;
    let (payload, sig) = bytes
        .split_at_checked(bytes.len().checked_sub(32).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    new_mac(key, nonce, payload)?
        .verify_slice(sig)
        .map_err(|_| invalid())?;
    let (expiry, data) = payload.split_at_checked(8).ok_or_else(invalid)?;
    if now >= u64::from_be_bytes(expiry.try_into().map_err(|_| invalid())?) {
        return Err(invalid());
    }
    let (verifier, destination) = data.split_at_checked(43).ok_or_else(invalid)?;
    let verifier = std::str::from_utf8(verifier).map_err(|_| invalid())?;
    let destination = std::str::from_utf8(destination).map_err(|_| invalid())?;
    if decode_secret(verifier).is_none() || !valid_destination(destination) {
        return Err(invalid());
    }
    Ok((verifier.to_owned(), destination.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Form;
    use axum::Router;
    use axum::routing::{get, post};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;
    use crate::http;
    use crate::store::Store;

    #[test]
    fn signed_state_binds_nonce_expiry_verifier_and_destination() {
        let key = [1; 32];
        let nonce = [2; 32];
        let verifier = encode_secret(&[3; 32]);
        let destination = format!("/join/{}", encode_secret(&[4; 32]));
        let state = seal(&key, 100, &nonce, &verifier, &destination).unwrap();
        assert_eq!(
            unseal(&key, &nonce, &state, 99).unwrap(),
            (verifier, destination)
        );
        assert!(unseal(&key, &nonce, &state, 100).is_err());
        assert!(unseal(&[5; 32], &nonce, &state, 99).is_err());
        assert!(unseal(&key, &[5; 32], &state, 99).is_err());
        assert!(!valid_destination("//example.com/app"));
        assert!(!valid_destination("/join/%2Fexample.com"));
        assert!(!valid_destination("/app?collab=bogus"));
    }

    #[tokio::test]
    #[ignore = "requires an isolated PostgreSQL database in RETICULA_TEST_DATABASE_URL"]
    async fn github_callback_exchanges_code_and_creates_https_session() {
        let db = std::env::var("RETICULA_TEST_DATABASE_URL").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::connect(&db, dir.path().to_owned()).await.unwrap();
        let token_requests = Arc::new(AtomicUsize::new(0));
        let requests = Arc::clone(&token_requests);
        let challenge = Arc::new(std::sync::Mutex::new(String::new()));
        let expected_challenge = Arc::clone(&challenge);
        let oauth_mock = Router::new()
            .route("/token", post(move |Form(params): Form<HashMap<String, String>>| async move {
                requests.fetch_add(1, Ordering::Relaxed);
                assert_eq!(params["client_id"], "example-client");
                assert_eq!(params["client_secret"], "example-secret");
                assert_eq!(params["code"], "example-code");
                assert_eq!(params["redirect_uri"], "https://reticula.example/api/auth/github/callback");
                let verifier = PkceCodeVerifier::new(params["code_verifier"].to_owned());
                assert_eq!(PkceCodeChallenge::from_code_verifier_sha256(&verifier).as_str(), expected_challenge.lock().unwrap().as_str());
                Json(json!({ "access_token": "example-token", "token_type": "bearer" }))
            }))
            .route("/user", get(|headers: HeaderMap| async move {
                assert_eq!(headers[header::AUTHORIZATION], "Bearer example-token");
                Json(json!({ "id": 123456789, "login": "Octocat", "avatar_url": "https://avatars.githubusercontent.com/test" }))
            }));
        let mock = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider = format!("http://{}", mock.local_addr().unwrap());
        let provider_task =
            tokio::spawn(async move { axum::serve(mock, oauth_mock).await.unwrap() });
        let mut auth = AuthConfig::github(
            "https://reticula.example",
            "example-client".into(),
            "example-secret".into(),
        )
        .unwrap();
        let Provider::Github(github) = &mut auth.provider else {
            panic!("expected GitHub mode")
        };
        github.oauth = github
            .oauth
            .clone()
            .set_token_uri(TokenUrl::new(format!("{provider}/token")).unwrap());
        github.profile_url = Url::parse(&format!("{provider}/user")).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().nest("/api", http::router(ApiState::new(store, auth)));
        let app_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let browser = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let destination = format!("/join/{}", encode_secret(&[7; 32]));
        let sign_in = browser
            .get(format!("{base}/api/auth/github/login"))
            .query(&[("returnTo", &destination)])
            .send()
            .await
            .unwrap();
        assert_eq!(sign_in.status(), StatusCode::SEE_OTHER);
        let authorization =
            Url::parse(sign_in.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        assert_eq!(authorization.host_str(), Some("github.com"));
        let params = authorization
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(params["code_challenge_method"], "S256");
        assert!(!params.contains_key("scope"));
        challenge
            .lock()
            .unwrap()
            .clone_from(&params["code_challenge"]);
        let state_cookie = sign_in.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let callback = format!("{base}{CALLBACK}");
        let bad = browser
            .get(&callback)
            .header(header::COOKIE, state_cookie)
            .query(&[
                ("state", &encode_secret(&[9; 32])),
                ("code", &"example-code".to_owned()),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        assert_eq!(token_requests.load(Ordering::Relaxed), 0);

        let good = browser
            .get(&callback)
            .header(header::COOKIE, state_cookie)
            .query(&[
                ("state", params["state"].as_str()),
                ("code", "example-code"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(good.status(), StatusCode::SEE_OTHER);
        assert_eq!(good.headers()[header::LOCATION], destination);
        let session = good
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(|value| {
                let cookie = value.to_str().unwrap();
                cookie.starts_with(SESSION_COOKIE).then_some(cookie)
            })
            .unwrap();
        assert!(
            session.contains("Secure")
                && session.contains("HttpOnly")
                && session.contains("SameSite=Lax")
                && session.contains("Path=/")
        );
        let me = browser
            .get(format!("{base}/api/auth/me"))
            .header(header::COOKIE, session.split(';').next().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);
        assert_eq!(
            me.json::<serde_json::Value>().await.unwrap()["name"],
            "Octocat"
        );
        app_task.abort();
        provider_task.abort();
    }
}
