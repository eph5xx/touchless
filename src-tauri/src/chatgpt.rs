//! Sign in with ChatGPT, so OpenAI requests can use the user's ChatGPT plan
//! instead of an API key. Follows OpenAI's flow for open-source apps: the
//! first sign-in registers Touchless as a client, every sign-in uses PKCE with
//! a loopback callback, and the session renews itself with rotating refresh
//! tokens.

use std::{
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::{SecureRandom, SystemRandom},
    signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

use crate::{
    db::Db,
    settings::{self, keychain},
};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const REVOKE_URL: &str = "https://auth.openai.com/api/accounts/oauth/revoke";
const JWKS_URL: &str = "https://auth.openai.com/.well-known/jwks.json";
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
const RESOURCE: &str = "https://api.openai.com/v1";
/// The entry point for a first sign-in; never saved or used for tokens.
const REGISTRATION_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "Touchless";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Without this scope the tokens cannot use the ChatGPT plan.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const REFRESH_MARGIN_SECS: i64 = 120;
const CLOCK_LEEWAY_SECS: i64 = 60;
const USAGE_LIMIT_PAUSE: Duration = Duration::from_secs(15 * 60);
const REVOKE_RETRY_DELAY: Duration = Duration::from_secs(1);
const REGISTRATION_KEY: &str = "chatgpt";
const KEYCHAIN_ACCOUNT: &str = "chatgpt-session";
/// Refresh errors after which the session cannot be renewed.
const UNUSABLE_REFRESH_CODES: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

pub const SIGN_IN_AGAIN: &str = "Sign in with ChatGPT again in Settings → Services.";
pub const NOT_SIGNED_IN: &str = "Sign in with ChatGPT in Settings → Services.";
pub const USAGE_LIMIT: &str = "You've reached the usage limit for Touchless on your ChatGPT plan. Manage usage in ChatGPT settings.";
pub const CANCELLED: &str = "Sign-in cancelled";

/// What stays in SQLite, even after signing out: none of it is secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Registration {
    /// `urn:uuid:…`, created once for this Mac.
    host_id: Option<String>,
    /// Issued by the first sign-in and reused afterwards.
    client_id: Option<String>,
    email: Option<String>,
    /// The verified ID-token subject the registration belongs to.
    subject: Option<String>,
    welcome_shown: bool,
}

/// The tokens, kept in their own Keychain item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Session {
    client_id: String,
    access_token: String,
    refresh_token: String,
    id_token: String,
    scopes: Vec<String>,
    /// Unix seconds.
    expires_at: i64,
    earliest_refresh_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatGptView {
    signed_in: bool,
    email: Option<String>,
    /// The one-time "You're using your ChatGPT plan" message.
    show_welcome: bool,
}

pub struct ChatGpt {
    registration: Mutex<Registration>,
    session: Mutex<Option<Session>>,
    /// Serializes refreshes so a rotated refresh token is never used twice.
    renewal: tokio::sync::Mutex<()>,
    attempt: AtomicU64,
    cancel: Mutex<Option<(u64, oneshot::Sender<()>)>>,
    paused_until: Mutex<Option<Instant>>,
    on_lost: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl ChatGpt {
    pub fn load(db: &Db) -> Self {
        let session = match keychain::load(KEYCHAIN_ACCOUNT) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes)
                .map_err(|error| eprintln!("could not read the ChatGPT session: {error}"))
                .ok(),
            Ok(None) => None,
            Err(error) => {
                eprintln!("could not read the ChatGPT session from the Keychain: {error}");
                None
            }
        };
        Self::new(settings::load_json(db, REGISTRATION_KEY), session)
    }

    fn new(registration: Registration, session: Option<Session>) -> Self {
        Self {
            registration: Mutex::new(registration),
            session: Mutex::new(session),
            renewal: tokio::sync::Mutex::new(()),
            attempt: AtomicU64::new(0),
            cancel: Mutex::new(None),
            paused_until: Mutex::new(None),
            on_lost: OnceLock::new(),
        }
    }

    /// Called when a refresh shows the session can no longer be used.
    pub fn on_lost(&self, callback: impl Fn() + Send + Sync + 'static) {
        let _ = self.on_lost.set(Box::new(callback));
    }

    pub fn view(&self) -> ChatGptView {
        let registration = self.registration.lock().unwrap().clone();
        let signed_in = self.signed_in();
        ChatGptView {
            signed_in,
            email: registration.email.filter(|_| signed_in),
            show_welcome: signed_in && !registration.welcome_shown,
        }
    }

    pub fn signed_in(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    pub fn email(&self) -> Option<String> {
        self.registration.lock().unwrap().email.clone()
    }

    pub fn dismiss_welcome(&self, db: &Db) -> Result<(), String> {
        self.update_registration(db, |registration| registration.welcome_shown = true)
    }

    /// Opens the browser and waits until the user finishes signing in.
    pub async fn sign_in(&self, db: &Db) -> Result<(), String> {
        let id = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        if let Some((_, previous)) = self.cancel.lock().unwrap().replace((id, cancel_tx)) {
            let _ = previous.send(());
        }
        let result = tokio::select! {
            result = tokio::time::timeout(SIGN_IN_TIMEOUT, self.authorize(db)) => {
                result.unwrap_or_else(|_| Err("Sign-in timed out. Try again.".into()))
            }
            _ = cancel_rx => Err(CANCELLED.into()),
        };
        let mut cancel = self.cancel.lock().unwrap();
        if cancel.as_ref().is_some_and(|(current, _)| *current == id) {
            *cancel = None;
        }
        result
    }

    pub fn cancel_sign_in(&self) {
        if let Some((_, cancel)) = self.cancel.lock().unwrap().take() {
            let _ = cancel.send(());
        }
    }

    async fn authorize(&self, db: &Db) -> Result<(), String> {
        let registration = self.ensure_host_id(db)?;
        let listener = bind_callback().await?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("could not start the sign-in listener: {error}"))?
            .port();
        let attempt = Attempt::new(format!("http://127.0.0.1:{port}{CALLBACK_PATH}"))?;
        let id_token_hint = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .map(|session| session.id_token.clone());
        open_url(&authorize_url(
            &registration,
            &attempt,
            id_token_hint.as_deref(),
        ))?;

        let (mut stream, query) = wait_for_callback(&listener).await?;
        let result = self.finish(db, &registration, &attempt, &query).await;
        let page = match &result {
            Ok(()) => callback_page(
                "You're signed in",
                "You can close this tab and go back to Touchless.",
            ),
            Err(error) => callback_page("Sign-in didn't finish", error),
        };
        let _ = respond(&mut stream, "200 OK", &page).await;
        result
    }

    async fn finish(
        &self,
        db: &Db,
        registration: &Registration,
        attempt: &Attempt,
        query: &str,
    ) -> Result<(), String> {
        let callback = parse_callback(query, &attempt.state, registration.client_id.as_deref())?;
        let tokens = post_token(&[
            ("grant_type", "authorization_code"),
            ("client_id", &callback.client_id),
            ("code", &callback.code),
            ("code_verifier", &attempt.verifier),
            ("redirect_uri", &attempt.redirect_uri),
            ("resource", RESOURCE),
        ])
        .await
        .map_err(|error| error.message)?;

        let id_token = tokens
            .id_token
            .clone()
            .ok_or("ChatGPT didn't return an ID token. Try again.")?;
        let jwks = fetch_jwks().await?;
        let claims = validate_id_token(
            &id_token,
            &jwks,
            &callback.client_id,
            &attempt.nonce,
            now_secs(),
        )?;
        if registration
            .subject
            .as_ref()
            .is_some_and(|subject| registration.client_id.is_some() && *subject != claims.sub)
        {
            return Err("That's a different ChatGPT account from the one Touchless is registered with. Sign in with the same account.".into());
        }

        // The registration is kept even when plan use was declined, so the
        // next attempt reuses it.
        self.update_registration(db, |saved| {
            saved.client_id = Some(callback.client_id.clone());
            saved.subject = Some(claims.sub.clone());
            if claims.email.is_some() {
                saved.email = claims.email.clone();
            }
        })?;

        let scopes = split_scopes(tokens.scope.as_deref().unwrap_or_default());
        if !scopes.iter().any(|scope| scope == PLAN_SCOPE) {
            return Err("Touchless wasn't allowed to use your ChatGPT plan. Sign in again and allow it, or use an API key.".into());
        }
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or("ChatGPT didn't return a refresh token. Try again.")?;
        let now = now_secs();
        // A refresh still running with the old tokens must not overwrite these.
        let _renewal = self.renewal.lock().await;
        self.store_session(Session {
            client_id: callback.client_id,
            access_token: tokens.access_token,
            refresh_token,
            id_token,
            scopes,
            expires_at: now + tokens.expires_in.unwrap_or(3600),
            earliest_refresh_at: tokens.earliest_refresh_at.as_ref().and_then(parse_time),
        })?;
        *self.paused_until.lock().unwrap() = None;
        Ok(())
    }

    /// A bearer token for `api.openai.com`, renewed when it is about to expire.
    pub async fn access_token(&self) -> Result<String, String> {
        self.check_paused()?;
        let _renewal = self.renewal.lock().await;
        let session = self.session.lock().unwrap().clone().ok_or(NOT_SIGNED_IN)?;
        let now = now_secs();
        if !should_refresh(&session, now) {
            return Ok(session.access_token);
        }
        match refresh(&session, now).await {
            Ok(next) => {
                let token = next.access_token.clone();
                if let Err(error) = self.store_session(next) {
                    eprintln!("could not save the renewed ChatGPT session: {error}");
                }
                Ok(token)
            }
            Err(error) if error.is_unusable() => {
                eprintln!("ChatGPT session ended: {}", error.message);
                if let Err(error) = self.clear_session() {
                    eprintln!("could not clear the ChatGPT session: {error}");
                }
                if let Some(on_lost) = self.on_lost.get() {
                    on_lost();
                }
                Err(SIGN_IN_AGAIN.into())
            }
            // The current token still works; renewal is retried next time.
            Err(error) if now < session.expires_at => {
                eprintln!("could not renew the ChatGPT session yet: {}", error.message);
                Ok(session.access_token)
            }
            Err(error) => Err(error.message),
        }
    }

    /// Ends the session at OpenAI, then forgets the tokens. Returns whether
    /// OpenAI confirmed the revocation.
    pub async fn sign_out(&self) -> Result<bool, String> {
        self.cancel_sign_in();
        let _renewal = self.renewal.lock().await;
        let Some(session) = self.session.lock().unwrap().clone() else {
            return Ok(true);
        };
        let mut revoked = false;
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(REVOKE_RETRY_DELAY).await;
            }
            match revoke(&session).await {
                Ok(()) => {
                    revoked = true;
                    break;
                }
                Err(Revoke::Retry(error)) => {
                    eprintln!("could not end the ChatGPT session: {error}")
                }
                Err(Revoke::Failed(error)) => {
                    eprintln!("could not end the ChatGPT session: {error}");
                    break;
                }
            }
        }
        self.clear_session()?;
        Ok(revoked)
    }

    /// Stops sending requests for a while after the plan's usage limit.
    pub fn pause_for_usage_limit(&self) {
        *self.paused_until.lock().unwrap() = Some(Instant::now() + USAGE_LIMIT_PAUSE);
    }

    fn check_paused(&self) -> Result<(), String> {
        let mut paused = self.paused_until.lock().unwrap();
        match *paused {
            Some(until) if Instant::now() < until => Err(USAGE_LIMIT.into()),
            Some(_) => {
                *paused = None;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn ensure_host_id(&self, db: &Db) -> Result<Registration, String> {
        if self.registration.lock().unwrap().host_id.is_none() {
            let host_id = format!("urn:uuid:{}", uuid_v4()?);
            self.update_registration(db, |registration| {
                registration.host_id.get_or_insert(host_id);
            })?;
        }
        Ok(self.registration.lock().unwrap().clone())
    }

    fn update_registration(
        &self,
        db: &Db,
        change: impl FnOnce(&mut Registration),
    ) -> Result<(), String> {
        let mut current = self.registration.lock().unwrap();
        let mut next = current.clone();
        change(&mut next);
        settings::save_json(db, REGISTRATION_KEY, &next)
            .map_err(|error| format!("could not save the ChatGPT sign-in: {error}"))?;
        *current = next;
        Ok(())
    }

    fn store_session(&self, session: Session) -> Result<(), String> {
        let json = serde_json::to_vec(&session).map_err(|error| error.to_string())?;
        // Kept in memory even if the Keychain refuses, so this run still works.
        *self.session.lock().unwrap() = Some(session);
        keychain::save(KEYCHAIN_ACCOUNT, &json)
            .map_err(|error| format!("could not save the ChatGPT sign-in to the Keychain: {error}"))
    }

    fn clear_session(&self) -> Result<(), String> {
        *self.session.lock().unwrap() = None;
        *self.paused_until.lock().unwrap() = None;
        keychain::delete(KEYCHAIN_ACCOUNT)
            .map_err(|error| format!("could not update the Keychain: {error}"))
    }

    #[cfg(test)]
    pub fn for_test(signed_in: bool) -> Self {
        let session = signed_in.then(|| Session {
            client_id: "oaiapp_test".into(),
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: "id".into(),
            scopes: vec![PLAN_SCOPE.into()],
            expires_at: i64::MAX,
            earliest_refresh_at: None,
        });
        Self::new(
            Registration {
                email: Some("user@example.com".into()),
                ..Default::default()
            },
            session,
        )
    }
}

/// Random values bound to one sign-in attempt.
struct Attempt {
    redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
}

impl Attempt {
    fn new(redirect_uri: String) -> Result<Self, String> {
        Ok(Self {
            redirect_uri,
            state: random_token()?,
            nonce: random_token()?,
            verifier: random_token()?,
        })
    }
}

fn authorize_url(
    registration: &Registration,
    attempt: &Attempt,
    id_token_hint: Option<&str>,
) -> String {
    let mut params: Vec<(&str, &str)> = Vec::new();
    match &registration.client_id {
        Some(client_id) => {
            params.push(("client_id", client_id));
            if let Some(hint) = id_token_hint {
                params.push(("id_token_hint", hint));
            }
            if let Some(email) = &registration.email {
                params.push(("login_hint", email));
            }
        }
        None => {
            params.push(("client_id", REGISTRATION_CLIENT));
            params.push(("agent_name_hint", AGENT_NAME));
        }
    }
    if let Some(host_id) = &registration.host_id {
        params.push(("ext_agent_host_id", host_id));
    }
    let challenge = code_challenge(&attempt.verifier);
    params.extend([
        ("response_type", "code"),
        ("redirect_uri", attempt.redirect_uri.as_str()),
        ("scope", SCOPE),
        ("resource", RESOURCE),
        ("state", attempt.state.as_str()),
        ("nonce", attempt.nonce.as_str()),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge.as_str()),
    ]);
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

/// Percent-encodes a query value, with spaces as `%20`.
fn encode(value: &str) -> String {
    // byte_serialize writes spaces as `+` and a literal `+` as `%2B`.
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(Debug, PartialEq)]
struct Callback {
    code: String,
    client_id: String,
}

/// Checks the browser's redirect. `expected_client` is the saved client ID for
/// a returning sign-in, or `None` for a first registration.
fn parse_callback(
    query: &str,
    state: &str,
    expected_client: Option<&str>,
) -> Result<Callback, String> {
    let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let param = |key: &str| {
        params
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    if param("state") != Some(state) {
        return Err("The sign-in response didn't match this attempt. Try again.".into());
    }
    if let Some(error) = param("error") {
        return Err(if error == "access_denied" {
            "You didn't allow Touchless to use your ChatGPT plan.".into()
        } else {
            let detail = param("error_description").unwrap_or(error);
            format!("ChatGPT sign-in failed: {detail}")
        });
    }
    let code = param("code").ok_or("ChatGPT didn't return a sign-in code. Try again.")?;
    let client_id = match (expected_client, param("client_id")) {
        (None, Some(issued)) if issued != REGISTRATION_CLIENT => issued,
        (None, _) => return Err("ChatGPT didn't finish registering Touchless. Try again.".into()),
        (Some(expected), Some(issued)) if issued != expected => {
            return Err("ChatGPT returned a different app registration. Try again.".into());
        }
        (Some(expected), _) => expected,
    };
    Ok(Callback {
        code: code.to_owned(),
        client_id: client_id.to_owned(),
    })
}

#[derive(Debug, PartialEq)]
struct IdClaims {
    sub: String,
    email: Option<String>,
}

/// Verifies the RS256 signature against the JWKS and the standard claims.
fn validate_id_token(
    token: &str,
    jwks: &Value,
    client_id: &str,
    nonce: &str,
    now: i64,
) -> Result<IdClaims, String> {
    let invalid = |reason: &str| {
        format!("Touchless couldn't verify the ChatGPT sign-in ({reason}). Try again.")
    };
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("malformed ID token"));
    };
    let header_json = decode_json(header).ok_or_else(|| invalid("unreadable header"))?;
    if header_json["alg"] != "RS256" {
        return Err(invalid("unexpected signing algorithm"));
    }
    let signature = decode_base64(signature).ok_or_else(|| invalid("unreadable signature"))?;
    let message = format!("{header}.{payload}");
    let kid = header_json["kid"].as_str();
    let verified = jwks["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|key| key["kty"] == "RSA" && kid.is_none_or(|kid| key["kid"] == kid))
        .any(|key| verify_rs256(key, message.as_bytes(), &signature));
    if !verified {
        return Err(invalid("bad signature"));
    }

    let claims = decode_json(payload).ok_or_else(|| invalid("unreadable claims"))?;
    if claims["iss"] != ISSUER {
        return Err(invalid("wrong issuer"));
    }
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(auds) => auds.iter().any(|aud| aud == client_id),
        _ => false,
    };
    if !audience_ok {
        return Err(invalid("wrong audience"));
    }
    match claims["exp"].as_i64() {
        Some(exp) if now < exp + CLOCK_LEEWAY_SECS => {}
        _ => return Err(invalid("expired")),
    }
    if claims["nonce"] != nonce {
        return Err(invalid("nonce mismatch"));
    }
    let sub = claims["sub"]
        .as_str()
        .filter(|sub| !sub.is_empty())
        .ok_or_else(|| invalid("missing subject"))?;
    Ok(IdClaims {
        sub: sub.to_owned(),
        email: claims["email"].as_str().map(str::to_owned),
    })
}

fn verify_rs256(key: &Value, message: &[u8], signature: &[u8]) -> bool {
    let (Some(n), Some(e)) = (
        key["n"].as_str().and_then(decode_base64),
        key["e"].as_str().and_then(decode_base64),
    ) else {
        return false;
    };
    RsaPublicKeyComponents { n, e }
        .verify(&RSA_PKCS1_2048_8192_SHA256, message, signature)
        .is_ok()
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value.trim_end_matches('=')).ok()
}

fn decode_json(value: &str) -> Option<Value> {
    serde_json::from_slice(&decode_base64(value)?).ok()
}

/// Renews shortly before expiry, but not before the server allows it.
fn should_refresh(session: &Session, now: i64) -> bool {
    if now >= session.expires_at {
        return true;
    }
    now >= session.expires_at - REFRESH_MARGIN_SECS
        && session
            .earliest_refresh_at
            .is_none_or(|earliest| now >= earliest)
}

fn split_scopes(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_owned).collect()
}

/// Accepts Unix seconds, Unix milliseconds or an RFC 3339 timestamp.
fn parse_time(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().map(|time| {
            if time > 100_000_000_000 {
                time / 1000
            } else {
                time
            }
        }),
        Value::String(text) => chrono::DateTime::parse_from_rfc3339(text)
            .map(|time| time.timestamp())
            .ok()
            .or_else(|| text.parse().ok()),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    earliest_refresh_at: Option<Value>,
}

#[derive(Debug)]
struct TokenError {
    code: Option<String>,
    message: String,
}

impl TokenError {
    fn is_unusable(&self) -> bool {
        self.code
            .as_deref()
            .is_some_and(|code| UNUSABLE_REFRESH_CODES.contains(&code))
    }

    fn from_body(status: u16, body: &Value) -> Self {
        let code = body["error"]
            .as_str()
            .or_else(|| body["error"]["code"].as_str())
            .or_else(|| body["code"].as_str())
            .map(str::to_owned);
        let detail = body["error_description"]
            .as_str()
            .or_else(|| body["error"]["message"].as_str())
            .or_else(|| body["detail"].as_str())
            .or(code.as_deref());
        let message = match detail {
            Some(detail) => format!("ChatGPT sign-in failed ({status}): {detail}"),
            None => format!("ChatGPT sign-in failed ({status})"),
        };
        Self { code, message }
    }
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| format!("could not set up the ChatGPT sign-in: {error}"))
}

async fn post_form(url: &str, pairs: &[(&str, &str)]) -> Result<reqwest::Response, String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    http()?
        .post(url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| format!("could not reach ChatGPT: {error}"))
}

async fn post_token(pairs: &[(&str, &str)]) -> Result<TokenResponse, TokenError> {
    let response = post_form(TOKEN_URL, pairs)
        .await
        .map_err(|message| TokenError {
            code: None,
            message,
        })?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(TokenError::from_body(status.as_u16(), &body));
    }
    serde_json::from_value(body).map_err(|error| TokenError {
        code: None,
        message: format!("could not read the ChatGPT sign-in response: {error}"),
    })
}

async fn refresh(session: &Session, now: i64) -> Result<Session, TokenError> {
    let tokens = post_token(&[
        ("grant_type", "refresh_token"),
        ("client_id", &session.client_id),
        ("refresh_token", &session.refresh_token),
        ("resource", RESOURCE),
    ])
    .await?;
    Ok(Session {
        client_id: session.client_id.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .unwrap_or_else(|| session.refresh_token.clone()),
        id_token: tokens.id_token.unwrap_or_else(|| session.id_token.clone()),
        scopes: tokens
            .scope
            .as_deref()
            .map(split_scopes)
            .unwrap_or_else(|| session.scopes.clone()),
        expires_at: now + tokens.expires_in.unwrap_or(3600),
        earliest_refresh_at: tokens.earliest_refresh_at.as_ref().and_then(parse_time),
    })
}

enum Revoke {
    Retry(String),
    Failed(String),
}

async fn revoke(session: &Session) -> Result<(), Revoke> {
    let response = post_form(
        REVOKE_URL,
        &[
            ("token", &session.refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", &session.client_id),
        ],
    )
    .await
    .map_err(Revoke::Retry)?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else if status.is_server_error() {
        Err(Revoke::Retry(format!("OpenAI responded with {status}")))
    } else {
        Err(Revoke::Failed(format!("OpenAI responded with {status}")))
    }
}

async fn fetch_jwks() -> Result<Value, String> {
    let response = http()?
        .get(JWKS_URL)
        .send()
        .await
        .map_err(|error| format!("could not reach ChatGPT: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "could not load ChatGPT's signing keys ({})",
            response.status()
        ));
    }
    response
        .json()
        .await
        .map_err(|error| format!("could not read ChatGPT's signing keys: {error}"))
}

/// Port 1455 when free, otherwise any port; only the port may vary.
async fn bind_callback() -> Result<TcpListener, String> {
    match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|error| format!("could not start the sign-in listener: {error}")),
    }
}

/// Waits for the browser to reach the callback path; returns its query.
async fn wait_for_callback(listener: &TcpListener) -> Result<(TcpStream, String), String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|error| format!("the sign-in listener stopped: {error}"))?;
        let Ok(Some(target)) =
            tokio::time::timeout(REQUEST_READ_TIMEOUT, read_target(&mut stream)).await
        else {
            continue;
        };
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        if path == CALLBACK_PATH {
            return Ok((stream, query.to_owned()));
        }
        let _ = respond(&mut stream, "404 Not Found", "Not found").await;
    }
}

/// Reads the request line of an HTTP request and returns its target.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || buffer.len() > 16 * 1024 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut parts = text.lines().next()?.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_owned()),
        _ => None,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn callback_page(title: &str, message: &str) -> String {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Touchless</title>\
<style>body{{font-family:-apple-system,BlinkMacSystemFont,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;color:#18191c;background:#f5f5f2}}\
main{{text-align:center;max-width:420px;padding:24px}}h1{{font-size:22px;margin:0 0 8px}}p{{color:#686b73;margin:0}}\
@media (prefers-color-scheme: dark){{body{{color:#f5f5f3;background:#0c0d0f}}p{{color:#9a9ea7}}}}</style></head>\
<body><main><h1>{}</h1><p>{}</p></main></body></html>",
        escape(title),
        escape(message)
    )
}

pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("could not open the browser: {error}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        Err("opening the browser is only available on macOS".into())
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "could not generate a random value".to_owned())?;
    Ok(bytes)
}

fn random_token() -> Result<String, String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<32>()?))
}

fn uuid_v4() -> Result<String, String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A throwaway 2048-bit RSA key (PKCS#1 DER) used only to sign test tokens.
    const TEST_KEY: &str = "MIIEogIBAAKCAQEAjlHz1D/EIWqsfLjr5/ZaRVa+1ZiDUsbVtfZTel4la5zuLSB0Plg+MbLqjzjskbRdb6nGoD/89RfAm+SOL+nsTmCbhGZvLzaXNs4dLatlr52H7Vgy+NlmX8qABUwKycZV9ijp+dEuWSyKOq+P64uJ5F2+ZsesdlRx+Q2M1kVppB1p3KkhQrW3EAgTybg+09krzNrL8zVGJPnCwQgvPJF4W6yYmb40IWG6B01yhruRAEWcgVgSzqrrfmvrzq3B7D7GkiqkkKCKT7qu0BH6l9tsFKekNvpw9HyCzsuzFtofVd0gQib/k/Yn0PJ3H/eWkV8WCwlJNhNIAC7hbXXNOmm6cQIDAQABAoIBAAEcxl9fFTkepJT//AYO4KNAm5K/U61ytOpsb7gZ4sh9zEXJhmcTWP8Z8yfl7cFwy7cq+Ey521AOHLkLNwpMVYbCTgzhoRZZIqLwxv0bpRqTjGOwSAu2ZUyz4D7KWp47oOHg7X1Ao5BaXwU46JLSfyQsEGgswN4W6GImNzbYiYwZFMYcTvXoz1TfGYyoR0umMPnaTAOgRQGI1S56oghz7bxisM8k4WXaSn9Bm/xPh+1J6ym8FZ8uGyDGO0qXVT0q8EFnqoYxg/Mte2Ke++ZnQMxMdx7bzGtrFQ+XJHGgrmygmJVnZ0F5BOLwYnlbBPITtvYR8968KJylEzrogC/dxwECgYEAv5tH4ToDXyXmyypYF2clYUdglct0gplDpxYUv3wT3DIRaswSRvwBM+PIkxaNLzibAlFvNI/X8aLidrqVQnPDMgzgMMHF90s84Y4w/826Zu2CSFFDictK08hgEUx9Qnrkb1fPChwJ4pCVg+KBmdf3G5GwQij33Fd4amlUX32bxzECgYEAviZaF9HWlItV7CoE2N/HGBYlMdInJdiBJqGsVxiXgMTRLiLJBCSocGAvqyD2KmWqoFuDvUEVihjTw2D++GlGxPNGvVekZ3E47dCf4DvfM5q1NEE106/gm9K4E9mK8HEDVJXkL4CIA83ojOuBFMoRYGgmNQ9Xd8BbkmLYCroy10ECgYAoCIyrLVDZB3VCS7WeJin0OlKe2aSdSOKOvZlWLNmFb1yeQMcs16h0rAoTWWnkTOJuSI+AnvFShmLkxbU77i4Bfhk1drcjJTAyPGBWgiiJKuTGsVv4JWab+0i/NIkTY9Pow6L5jMQv91+V77R+bVYvB3CZUi//dLMvIJTFYtwycQKBgCBKMPhkiNTWyHN/Oy186rk7YXuRXVZ+6oed4r8zjN6wDXmRLKOIehYDL3UW9taCRzRN9hQYTkkSKDvAZ+vVLNBbI4MZs/Bg+LLhv5R7dk60Edtay4I0Fqc/ZETypzZnapJdlc8/O1myCLCNeXEHU2YUqZBPF6JdRu+X22oO7n1BAoGAXmP4fWb0/Seh+wlxznEWOszi4AknFqU5C/LGAdbktcJFdMlBg4LsCGBzaGvpCZTKvQ4aqoXdI4naDmuTYY4FaaUixRL/wjS30cx5N8HiSMUaAYJw/GgC2mbx5zHe6eI5VJaZ/GQ4f0yvX4N+TKj9QhPqm2pjedzaxiPBJnSGlrY=";
    const NOW: i64 = 1_790_000_000;

    fn key_pair() -> ring::rsa::KeyPair {
        let der = base64::engine::general_purpose::STANDARD
            .decode(TEST_KEY)
            .unwrap();
        ring::rsa::KeyPair::from_der(&der).unwrap()
    }

    fn jwks() -> Value {
        let components: ring::rsa::PublicKeyComponents<Vec<u8>> = key_pair().public().into();
        json!({ "keys": [
            { "kty": "RSA", "kid": "other", "n": "AQAB", "e": "AQAB" },
            {
                "kty": "RSA",
                "kid": "test",
                "alg": "RS256",
                "n": URL_SAFE_NO_PAD.encode(&components.n),
                "e": URL_SAFE_NO_PAD.encode(&components.e),
            }
        ]})
    }

    fn sign(header: &Value, claims: &Value) -> String {
        let message = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let pair = key_pair();
        let mut signature = vec![0; pair.public().modulus_len()];
        pair.sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            message.as_bytes(),
            &mut signature,
        )
        .unwrap();
        format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn claims() -> Value {
        json!({
            "iss": ISSUER,
            "aud": "oaiapp_123",
            "sub": "user-1",
            "email": "user@example.com",
            "nonce": "nonce-1",
            "exp": NOW + 3600,
            "iat": NOW,
        })
    }

    fn header() -> Value {
        json!({ "alg": "RS256", "kid": "test", "typ": "JWT" })
    }

    fn validate(token: &str) -> Result<IdClaims, String> {
        validate_id_token(token, &jwks(), "oaiapp_123", "nonce-1", NOW)
    }

    #[test]
    fn computes_the_pkce_challenge() {
        // RFC 7636, appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    fn attempt() -> Attempt {
        Attempt {
            redirect_uri: "http://127.0.0.1:1455/auth/callback".into(),
            state: "state-1".into(),
            nonce: "nonce-1".into(),
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
        }
    }

    fn query(url: &str) -> HashMap<String, String> {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn builds_the_first_sign_in_url() {
        let registration = Registration {
            host_id: Some("urn:uuid:1234".into()),
            ..Default::default()
        };
        let url = authorize_url(&registration, &attempt(), Some("ignored"));
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("scope=openid%20profile%20email"));
        let params = query(&url);
        assert_eq!(params["client_id"], "dynamic_agent_client");
        assert_eq!(params["agent_name_hint"], "Touchless");
        assert_eq!(params["ext_agent_host_id"], "urn:uuid:1234");
        assert_eq!(params["response_type"], "code");
        assert_eq!(
            params["redirect_uri"],
            "http://127.0.0.1:1455/auth/callback"
        );
        assert_eq!(params["scope"], SCOPE);
        assert_eq!(params["resource"], "https://api.openai.com/v1");
        assert_eq!(params["state"], "state-1");
        assert_eq!(params["nonce"], "nonce-1");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(
            params["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(!params.contains_key("id_token_hint"));
        assert!(!params.contains_key("login_hint"));
    }

    #[test]
    fn builds_the_returning_sign_in_url() {
        let registration = Registration {
            host_id: Some("urn:uuid:1234".into()),
            client_id: Some("oaiapp_123".into()),
            email: Some("user+1@example.com".into()),
            ..Default::default()
        };
        let params = query(&authorize_url(
            &registration,
            &attempt(),
            Some("id.token.hint"),
        ));
        assert_eq!(params["client_id"], "oaiapp_123");
        assert_eq!(params["id_token_hint"], "id.token.hint");
        assert_eq!(params["login_hint"], "user+1@example.com");
        assert_eq!(params["ext_agent_host_id"], "urn:uuid:1234");
        assert!(!params.contains_key("agent_name_hint"));

        let params = query(&authorize_url(&registration, &attempt(), None));
        assert!(!params.contains_key("id_token_hint"));
    }

    #[test]
    fn checks_the_callback() {
        assert_eq!(
            parse_callback(
                "code=abc&state=s&client_id=oaiapp_1&scope=openid",
                "s",
                None
            ),
            Ok(Callback {
                code: "abc".into(),
                client_id: "oaiapp_1".into()
            })
        );
        // A returning sign-in may omit the client ID.
        assert_eq!(
            parse_callback("code=abc&state=s", "s", Some("oaiapp_1")),
            Ok(Callback {
                code: "abc".into(),
                client_id: "oaiapp_1".into()
            })
        );
        assert!(parse_callback("code=abc&state=other&client_id=oaiapp_1", "s", None).is_err());
        assert!(parse_callback("code=abc&client_id=oaiapp_1", "s", None).is_err());
        let denied = parse_callback("error=access_denied&state=s", "s", None).unwrap_err();
        assert!(denied.contains("didn't allow"), "{denied}");
        // A denial with the wrong state is still a mismatch.
        assert!(
            parse_callback("error=access_denied&state=x", "s", None)
                .unwrap_err()
                .contains("didn't match")
        );
        assert!(parse_callback("code=abc&state=s", "s", None).is_err());
        assert!(
            parse_callback("code=abc&state=s&client_id=dynamic_agent_client", "s", None).is_err()
        );
        assert!(
            parse_callback("code=abc&state=s&client_id=oaiapp_2", "s", Some("oaiapp_1")).is_err()
        );
        assert!(parse_callback("state=s&client_id=oaiapp_1", "s", None).is_err());
    }

    #[test]
    fn accepts_a_valid_id_token() {
        assert_eq!(
            validate(&sign(&header(), &claims())),
            Ok(IdClaims {
                sub: "user-1".into(),
                email: Some("user@example.com".into())
            })
        );
        let mut listed = claims();
        listed["aud"] = json!(["other", "oaiapp_123"]);
        assert!(validate(&sign(&header(), &listed)).is_ok());
    }

    #[test]
    fn rejects_bad_id_tokens() {
        let with = |key: &str, value: Value| {
            let mut claims = claims();
            claims[key] = value;
            sign(&header(), &claims)
        };
        assert!(validate(&with("iss", json!("https://evil.example"))).is_err());
        assert!(validate(&with("aud", json!("oaiapp_other"))).is_err());
        assert!(validate(&with("exp", json!(NOW - 3600))).is_err());
        assert!(validate(&with("nonce", json!("nonce-2"))).is_err());
        assert!(validate(&with("sub", json!(""))).is_err());

        let token = sign(&header(), &claims());
        let (signed, signature) = token.rsplit_once('.').unwrap();
        let (head, _) = signed.split_once('.').unwrap();
        let mut forged_claims = claims();
        forged_claims["sub"] = json!("user-2");
        let forged = format!(
            "{head}.{}.{signature}",
            URL_SAFE_NO_PAD.encode(forged_claims.to_string())
        );
        assert!(validate(&forged).unwrap_err().contains("bad signature"));

        let unsigned = sign(&json!({ "alg": "none", "kid": "test" }), &claims());
        assert!(validate(&unsigned).is_err());
        let unknown_key = sign(&json!({ "alg": "RS256", "kid": "missing" }), &claims());
        assert!(validate(&unknown_key).is_err());
        assert!(validate("not-a-token").is_err());
    }

    fn session(expires_at: i64, earliest_refresh_at: Option<i64>) -> Session {
        Session {
            client_id: "oaiapp_1".into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: "i".into(),
            scopes: vec![],
            expires_at,
            earliest_refresh_at,
        }
    }

    #[test]
    fn refreshes_near_expiry() {
        assert!(!should_refresh(&session(NOW + 3600, None), NOW));
        assert!(!should_refresh(&session(NOW + 121, None), NOW));
        assert!(should_refresh(&session(NOW + 120, None), NOW));
        assert!(!should_refresh(&session(NOW + 60, Some(NOW + 10)), NOW));
        assert!(should_refresh(&session(NOW + 60, Some(NOW)), NOW));
        // An expired token renews even before the earliest refresh time.
        assert!(should_refresh(&session(NOW, Some(NOW + 10)), NOW));
    }

    #[test]
    fn reads_refresh_times() {
        assert_eq!(parse_time(&json!(NOW)), Some(NOW));
        assert_eq!(parse_time(&json!(NOW * 1000)), Some(NOW));
        assert_eq!(
            parse_time(&json!("2026-09-21T14:13:20Z")),
            Some(1_790_000_000)
        );
        assert_eq!(parse_time(&json!(null)), None);
    }

    #[test]
    fn spots_unusable_refresh_errors() {
        let error = |body: Value| TokenError::from_body(400, &body);
        assert!(error(json!({ "error": "invalid_grant" })).is_unusable());
        assert!(error(json!({ "error": { "code": "refresh_token_reused" } })).is_unusable());
        assert!(!error(json!({ "error": "invalid_client" })).is_unusable());
        assert!(!error(json!({ "detail": "busy" })).is_unusable());
        assert_eq!(
            error(json!({ "error": "invalid_grant", "error_description": "expired" })).message,
            "ChatGPT sign-in failed (400): expired"
        );
    }

    #[test]
    fn shows_the_welcome_once_signed_in() {
        let signed_in = ChatGpt::for_test(true);
        assert_eq!(
            signed_in.view(),
            ChatGptView {
                signed_in: true,
                email: Some("user@example.com".into()),
                show_welcome: true,
            }
        );
        signed_in.registration.lock().unwrap().welcome_shown = true;
        assert!(!signed_in.view().show_welcome);

        let signed_out = ChatGpt::for_test(false);
        assert_eq!(
            signed_out.view(),
            ChatGptView {
                signed_in: false,
                email: None,
                show_welcome: false,
            }
        );
    }

    #[test]
    fn pauses_after_the_usage_limit() {
        let chatgpt = ChatGpt::for_test(true);
        assert!(chatgpt.check_paused().is_ok());
        chatgpt.pause_for_usage_limit();
        assert_eq!(chatgpt.check_paused(), Err(USAGE_LIMIT.into()));
        *chatgpt.paused_until.lock().unwrap() = Some(Instant::now());
        assert!(chatgpt.check_paused().is_ok());
    }

    #[test]
    fn makes_uuid_host_ids() {
        let id = uuid_v4().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn saves_the_registration_but_not_tokens_in_sqlite() {
        let db = Db::open_in_memory().unwrap();
        let chatgpt = ChatGpt::for_test(false);
        let registration = chatgpt.ensure_host_id(&db).unwrap();
        let host_id = registration.host_id.unwrap();
        assert!(host_id.starts_with("urn:uuid:"));
        // The host ID is created once.
        assert_eq!(
            chatgpt.ensure_host_id(&db).unwrap().host_id.unwrap(),
            host_id
        );
        let saved: Registration = settings::load_json(&db, REGISTRATION_KEY);
        assert_eq!(saved.host_id.as_deref(), Some(host_id.as_str()));

        chatgpt.dismiss_welcome(&db).unwrap();
        let saved: Registration = settings::load_json(&db, REGISTRATION_KEY);
        assert!(saved.welcome_shown);
    }
}
