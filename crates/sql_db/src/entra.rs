//! Entra ID sign-in (browser with PKCE, or device code) with a refreshing token cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::QueryError;

/// The public client the Azure SDKs use for interactive sign-in by default.
/// It accepts `http://localhost` redirects and device codes in every tenant.
pub const DEFAULT_CLIENT_ID: &str = "04b07795-8ddb-461a-bbee-02f9e1bf7b46";
/// Any work or school account, when the connection names no tenant.
pub const DEFAULT_TENANT: &str = "organizations";
const AUTHORITY: &str = "https://login.microsoftonline.com";
const SCOPES: &str = "https://database.windows.net/.default offline_access openid profile";
/// How long the browser has to come back to the loopback port.
const BROWSER_TIMEOUT: Duration = Duration::from_secs(300);
/// A token this close to expiry is refreshed before use.
const EXPIRY_MARGIN: Duration = Duration::from_secs(300);
const MAX_REDIRECT_BYTES: usize = 16 * 1024;
/// A loopback connection that sends nothing in this time is dropped, so it cannot block the redirect.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Token endpoint errors that mean the refresh token is no good; others may pass.
const REJECTED_REFRESH: [&str; 4] = [
    "invalid_grant",
    "interaction_required",
    "invalid_client",
    "unauthorized_client",
];

/// Something the user must do to finish signing in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInPrompt {
    /// Open this URL in the browser.
    OpenBrowser { url: String },
    /// Enter `user_code` at `verification_uri`.
    DeviceCode {
        verification_uri: String,
        user_code: String,
    },
    /// Sign-in ended, either way; any prompt shown can go.
    Finished,
}

type PromptHook = Box<dyn Fn(SignInPrompt) + Send + Sync>;
type LoadHook = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;
type SaveHook = Box<dyn Fn(&str, Option<&str>) + Send + Sync>;

/// Without hooks, interactive sign-in fails and refresh tokens are not persisted.
pub struct SignInHooks {
    pub prompt: PromptHook,
    /// Called on a blocking thread, since a keychain lookup can block.
    pub load_refresh_token: LoadHook,
    /// `None` deletes the stored token.
    pub save_refresh_token: SaveHook,
}

static HOOKS: OnceLock<SignInHooks> = OnceLock::new();

pub fn set_sign_in_hooks(hooks: SignInHooks) {
    if HOOKS.set(hooks).is_err() {
        tracing::warn!("Entra sign-in hooks were already set");
    }
}

/// Who signs in, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInRequest {
    /// Tenant ID or domain; empty means [`DEFAULT_TENANT`].
    pub tenant_id: String,
    /// Account to preselect; empty lets the user pick.
    pub user: String,
    /// App registration; empty means [`DEFAULT_CLIENT_ID`].
    pub client_id: String,
    /// Show a code to enter on another device instead of opening the browser.
    pub device_code: bool,
}

impl SignInRequest {
    fn tenant(&self) -> &str {
        non_empty(&self.tenant_id).unwrap_or(DEFAULT_TENANT)
    }

    fn client_id(&self) -> &str {
        non_empty(&self.client_id).unwrap_or(DEFAULT_CLIENT_ID)
    }

    /// Where the refresh token is stored; the device-code choice does not change the account.
    fn cache_key(&self) -> String {
        format!(
            "entra:{}:{}:{}",
            self.tenant().to_lowercase(),
            self.client_id().to_lowercase(),
            self.user.trim().to_lowercase()
        )
    }
}

fn is_tenant(tenant: &str) -> bool {
    tenant
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'))
}

fn non_empty(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty()).then_some(text)
}

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    /// Wall-clock: `Instant` stands still while the machine sleeps.
    expires_at: SystemTime,
}

impl CachedToken {
    fn is_fresh(&self, now: SystemTime) -> bool {
        self.expires_at > now + EXPIRY_MARGIN
    }
}

#[derive(Default)]
struct Account {
    token: Option<CachedToken>,
    refresh_token: Option<String>,
    /// The stored refresh token was read already, so a miss does not hit the keychain again.
    loaded: bool,
}

/// One lock per account, so a tab and the explorer connecting at once share one sign-in.
type Accounts = Mutex<HashMap<String, Arc<tokio::sync::Mutex<Account>>>>;

fn account(key: &str) -> Arc<tokio::sync::Mutex<Account>> {
    static ACCOUNTS: OnceLock<Accounts> = OnceLock::new();
    let mut accounts = ACCOUNTS
        .get_or_init(Accounts::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    accounts.entry(key.to_owned()).or_default().clone()
}

/// An access token for Azure SQL and Fabric: cached, refreshed, or from a new sign-in.
pub async fn access_token(request: &SignInRequest) -> Result<String, QueryError> {
    // The tenant becomes a path segment of the sign-in URL.
    if !is_tenant(request.tenant()) {
        return Err(QueryError::Auth(
            "the tenant must be a tenant ID or a domain name".to_owned(),
        ));
    }
    let key = request.cache_key();
    let account = account(&key);
    let mut account = account.lock().await;
    if let Some(token) = &account.token
        && token.is_fresh(SystemTime::now())
    {
        return Ok(token.access_token.clone());
    }
    if !account.loaded {
        account.refresh_token = load_refresh_token(&key).await;
        // Set after the load, so a connect cancelled mid-lookup reads it again next time.
        account.loaded = true;
    }
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| QueryError::Auth(error.to_string()))?;
    if let Some(refresh_token) = account.refresh_token.clone() {
        match refresh(&client, request, &refresh_token).await {
            Ok(tokens) => return Ok(store(&mut account, &key, tokens).await),
            Err((code, _)) if REJECTED_REFRESH.contains(&code.as_str()) => {
                tracing::info!(%code, "Entra refresh token rejected; signing in again");
                account.refresh_token = None;
                save_refresh_token(&key, None).await;
            }
            // Offline or a server error: keep the token for the next try.
            Err((code, description)) => {
                return Err(QueryError::Auth(format!(
                    "could not renew the Entra sign-in ({code}): {description}"
                )));
            }
        }
    }
    let tokens = {
        let _finished = FinishedOnDrop;
        sign_in(&client, request).await?
    };
    Ok(store(&mut account, &key, tokens).await)
}

async fn store(account: &mut Account, key: &str, tokens: TokenResponse) -> String {
    account.token = Some(CachedToken {
        access_token: tokens.access_token.clone(),
        // Capped, so an absurd value cannot overflow the clock.
        expires_at: SystemTime::now() + Duration::from_secs(tokens.expires_in.min(86_400)),
    });
    if let Some(refresh_token) = tokens.refresh_token
        && account.refresh_token.as_deref() != Some(refresh_token.as_str())
    {
        save_refresh_token(key, Some(&refresh_token)).await;
        account.refresh_token = Some(refresh_token);
    }
    tokens.access_token
}

async fn load_refresh_token(key: &str) -> Option<String> {
    let key = key.to_owned();
    tokio::task::spawn_blocking(move || {
        HOOKS
            .get()
            .and_then(|hooks| (hooks.load_refresh_token)(&key))
    })
    .await
    .unwrap_or_default()
}

async fn save_refresh_token(key: &str, token: Option<&str>) {
    let (key, token) = (key.to_owned(), token.map(str::to_owned));
    let saved = tokio::task::spawn_blocking(move || {
        if let Some(hooks) = HOOKS.get() {
            (hooks.save_refresh_token)(&key, token.as_deref());
        }
    })
    .await;
    if let Err(error) = saved {
        tracing::warn!(%error, "could not save the Entra refresh token");
    }
}

/// Clears the prompt also when the connect is cancelled mid sign-in.
struct FinishedOnDrop;

impl Drop for FinishedOnDrop {
    fn drop(&mut self) {
        prompt(SignInPrompt::Finished);
    }
}

fn prompt(prompt: SignInPrompt) {
    if let Some(hooks) = HOOKS.get() {
        (hooks.prompt)(prompt);
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expiry")]
    expires_in: u64,
    refresh_token: Option<String>,
}

fn default_expiry() -> u64 {
    3_600
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: String,
}

fn endpoint(request: &SignInRequest, path: &str) -> String {
    format!("{AUTHORITY}/{}/oauth2/v2.0/{path}", request.tenant())
}

/// The token endpoint's answer, or its `error` code and description.
async fn post_token(
    client: &reqwest::Client,
    request: &SignInRequest,
    form: &[(&str, &str)],
) -> Result<TokenResponse, (String, String)> {
    let network = |error: reqwest::Error| ("network".to_owned(), error.to_string());
    let response = client
        .post(endpoint(request, "token"))
        .form(form)
        .send()
        .await
        .map_err(network)?;
    let status = response.status();
    let body = response.bytes().await.map_err(network)?;
    if status.is_success()
        && let Ok(tokens) = serde_json::from_slice::<TokenResponse>(&body)
    {
        return Ok(tokens);
    }
    match serde_json::from_slice::<ErrorResponse>(&body) {
        Ok(error) => Err((error.error, first_line(&error.error_description))),
        Err(_) => Err(("http".to_owned(), format!("Entra answered {status}"))),
    }
}

/// Entra descriptions append trace and correlation IDs on later lines.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

async fn refresh(
    client: &reqwest::Client,
    request: &SignInRequest,
    refresh_token: &str,
) -> Result<TokenResponse, (String, String)> {
    post_token(
        client,
        request,
        &[
            ("client_id", request.client_id()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", SCOPES),
        ],
    )
    .await
}

async fn sign_in(
    client: &reqwest::Client,
    request: &SignInRequest,
) -> Result<TokenResponse, QueryError> {
    if HOOKS.get().is_none() {
        return Err(QueryError::Auth(
            "interactive Entra sign-in needs the app's sign-in prompt".to_owned(),
        ));
    }
    if request.device_code {
        return device_code(client, request).await;
    }
    match LoopbackListener::bind().await {
        Ok(listener) => browser(client, request, listener).await,
        Err(error) => {
            tracing::info!(%error, "no loopback port for the browser sign-in; using a device code");
            device_code(client, request).await
        }
    }
}

/// Listens on 127.0.0.1 and, when free, the same port on `::1`, since browsers
/// may resolve `localhost` to either.
struct LoopbackListener {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl LoopbackListener {
    async fn bind() -> std::io::Result<Self> {
        let v4 = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = v4.local_addr()?.port();
        let v6 = TcpListener::bind(("::1", port)).await.ok();
        Ok(Self { v4, v6, port })
    }

    async fn accept(&self) -> std::io::Result<tokio::net::TcpStream> {
        match &self.v6 {
            Some(v6) => tokio::select! {
                accepted = self.v4.accept() => accepted.map(|(stream, _)| stream),
                accepted = v6.accept() => accepted.map(|(stream, _)| stream),
            },
            None => self.v4.accept().await.map(|(stream, _)| stream),
        }
    }
}

async fn browser(
    client: &reqwest::Client,
    request: &SignInRequest,
    listener: LoopbackListener,
) -> Result<TokenResponse, QueryError> {
    let verifier = random_token()?;
    let state = random_token()?;
    let redirect_uri = format!("http://localhost:{}", listener.port);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse(&endpoint(request, "authorize"))
        .map_err(|error| QueryError::Auth(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", request.client_id())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("scope", SCOPES)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    match non_empty(&request.user) {
        Some(user) => url.query_pairs_mut().append_pair("login_hint", user),
        None => url
            .query_pairs_mut()
            .append_pair("prompt", "select_account"),
    };
    prompt(SignInPrompt::OpenBrowser { url: url.into() });
    let code = tokio::time::timeout(BROWSER_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| {
            QueryError::Auth("the browser sign-in did not finish within 5 minutes".to_owned())
        })??;
    post_token(
        client,
        request,
        &[
            ("client_id", request.client_id()),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
            ("scope", SCOPES),
        ],
    )
    .await
    .map_err(|(code, description)| QueryError::Auth(format!("{code}: {description}")))
}

/// The `code` from the first redirect carrying our `state`; other requests,
/// such as a browser asking for `/favicon.ico`, get a 404.
async fn wait_for_code(listener: &LoopbackListener, state: &str) -> Result<String, QueryError> {
    loop {
        let mut stream = listener
            .accept()
            .await
            .map_err(|error| QueryError::Auth(format!("loopback listener failed: {error}")))?;
        let Ok(Some(target)) =
            tokio::time::timeout(REQUEST_READ_TIMEOUT, read_request_target(&mut stream)).await
        else {
            continue;
        };
        let outcome = redirect_outcome(&target, state);
        let (status, page) = match &outcome {
            Some(Ok(_)) => (
                "200 OK",
                "Signed in. You can close this tab and return to Querent.",
            ),
            Some(Err(_)) => ("200 OK", "Sign-in failed. Return to Querent for details."),
            None => ("404 Not Found", "Not found."),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        if let Err(error) = stream.write_all(response.as_bytes()).await {
            tracing::debug!(%error, "could not answer the browser");
        }
        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    while !buffer.windows(2).any(|pair| pair == b"\r\n") {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || buffer.len() > MAX_REDIRECT_BYTES {
            return None;
        }
        buffer.extend_from_slice(chunk.get(..read)?);
    }
    let line = String::from_utf8_lossy(&buffer);
    let mut parts = line.lines().next()?.split(' ');
    (parts.next()? == "GET").then(|| parts.next().map(str::to_owned))?
}

/// `None` for a request that is not the redirect; otherwise the code or the error Entra sent.
fn redirect_outcome(target: &str, state: &str) -> Option<Result<String, QueryError>> {
    let url = url::Url::parse(&format!("http://localhost{target}")).ok()?;
    if url.path() != "/" {
        return None;
    }
    let pairs: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if pairs.get("state").map(String::as_str) != Some(state) {
        return None;
    }
    if let Some(code) = pairs.get("code") {
        return Some(Ok(code.clone()));
    }
    let error = pairs.get("error").cloned().unwrap_or_default();
    let description = pairs
        .get("error_description")
        .map(|text| first_line(text))
        .unwrap_or_default();
    Some(Err(QueryError::Auth(format!("{error}: {description}"))))
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

async fn device_code(
    client: &reqwest::Client,
    request: &SignInRequest,
) -> Result<TokenResponse, QueryError> {
    let auth = |error: reqwest::Error| QueryError::Auth(error.to_string());
    let response = client
        .post(endpoint(request, "devicecode"))
        .form(&[("client_id", request.client_id()), ("scope", SCOPES)])
        .send()
        .await
        .map_err(auth)?;
    let body = response.bytes().await.map_err(auth)?;
    let Ok(device) = serde_json::from_slice::<DeviceCodeResponse>(&body) else {
        let message = serde_json::from_slice::<ErrorResponse>(&body).map_or_else(
            |_| "Entra did not return a device code".to_owned(),
            |error| format!("{}: {}", error.error, first_line(&error.error_description)),
        );
        return Err(QueryError::Auth(message));
    };
    prompt(SignInPrompt::DeviceCode {
        verification_uri: device.verification_uri.clone(),
        user_code: device.user_code.clone(),
    });
    let deadline = Instant::now() + Duration::from_secs(device.expires_in.min(3_600));
    let mut interval = Duration::from_secs(device.interval.max(1));
    loop {
        tokio::time::sleep(interval).await;
        if Instant::now() > deadline {
            return Err(QueryError::Auth("the device code expired".to_owned()));
        }
        let polled = post_token(
            client,
            request,
            &[
                ("client_id", request.client_id()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &device.device_code),
            ],
        )
        .await;
        match polled {
            Ok(tokens) => return Ok(tokens),
            Err((code, _)) if code == "authorization_pending" => {}
            Err((code, _)) if code == "slow_down" => interval += Duration::from_secs(5),
            Err((code, description)) => {
                return Err(QueryError::Auth(format!("{code}: {description}")));
            }
        }
    }
}

/// 32 random bytes as base64url: a PKCE verifier or a `state` value.
fn random_token() -> Result<String, QueryError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| QueryError::Auth(format!("no random source: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_redirect() {
        assert_eq!(
            redirect_outcome("/?code=abc&state=s1", "s1")
                .unwrap()
                .unwrap(),
            "abc"
        );
        assert!(redirect_outcome("/?code=abc&state=other", "s1").is_none());
        assert!(redirect_outcome("/favicon.ico", "s1").is_none());
        let denied = redirect_outcome(
            "/?error=access_denied&error_description=User%20cancelled%0D%0ATrace%20ID%3A%201&state=s1",
            "s1",
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(
            denied.to_string(),
            "sign-in failed: access_denied: User cancelled"
        );
    }

    #[test]
    fn a_token_near_expiry_is_not_fresh() {
        let now = SystemTime::now();
        let token = |seconds| CachedToken {
            access_token: String::new(),
            expires_at: now + Duration::from_secs(seconds),
        };
        assert!(token(3_600).is_fresh(now));
        assert!(!token(60).is_fresh(now));
        // The clock moved on while the machine slept.
        assert!(!token(3_600).is_fresh(now + Duration::from_secs(7_200)));
    }

    #[test]
    fn a_tenant_is_an_id_or_a_domain() {
        assert!(is_tenant(DEFAULT_TENANT));
        assert!(is_tenant("contoso.onmicrosoft.com"));
        assert!(is_tenant("72f988bf-86f1-41af-91ab-2d7cd011db47"));
        assert!(!is_tenant("x/../common"));
        assert!(!is_tenant("a?b=c"));
    }

    #[test]
    fn defaults_tenant_and_client() {
        let request = SignInRequest {
            tenant_id: " ".to_owned(),
            user: "Ana@Example.com".to_owned(),
            client_id: String::new(),
            device_code: true,
        };
        assert_eq!(
            request.cache_key(),
            format!("entra:organizations:{DEFAULT_CLIENT_ID}:ana@example.com")
        );
    }

    #[tokio::test]
    async fn the_loopback_listener_returns_the_code() {
        let listener = LoopbackListener::bind().await.unwrap();
        let port = listener.port;
        let browser = tokio::spawn(async move {
            let mut favicon = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            favicon
                .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut answer = String::new();
            favicon.read_to_string(&mut answer).await.unwrap();
            assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
            let mut redirect = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            redirect
                .write_all(b"GET /?code=c1&state=s1 HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut answer = String::new();
            redirect.read_to_string(&mut answer).await.unwrap();
            assert!(answer.contains("Signed in"), "{answer}");
        });
        assert_eq!(wait_for_code(&listener, "s1").await.unwrap(), "c1");
        browser.await.unwrap();
    }
}
