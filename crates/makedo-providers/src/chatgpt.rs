//! `ChatGPT` plan usage («Sign in with `ChatGPT`» для локальных приложений): свой
//! OAuth-вход makedo и Responses API в счёт подписки пользователя.
//! Документация: <https://developers.openai.com/siwc/token-sharing-open-source>
//!
//! Вход — authorization code + PKCE с callback на `127.0.0.1`. Первый вход
//! регистрирует makedo как `dynamic_agent_client` и получает собственный `client_id`.
//! Токены лежат в `~/.makedo/chatgpt.json` (0600, атомарная запись). Access-токен
//! живёт час, refresh-токен ротируется: обновление сериализовано файловой
//! блокировкой, чтобы два процесса makedo не сожгли один и тот же refresh-токен.
// ponytail: выход с отзывом токена (`revocation_endpoint`) — когда понадобится
// смена аккаунта; сейчас достаточно удалить файл и войти заново.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SecureRandom as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::http::{event_stream, send_with_retries};
use crate::{CompletionRequest, EventStream, Provider, ProviderError};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const CALLBACK_PATH: &str = "/auth/callback";
/// Порт из примера документации; занят — берём любой свободный (меняться может только порт).
const DEFAULT_PORT: u16 = 1455;
const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
/// Обновляем access-токен за две минуты до истечения.
const REFRESH_MARGIN_S: u64 = 120;

fn auth(message: impl Into<String>) -> ProviderError {
    ProviderError::Auth(message.into())
}

fn not_signed_in() -> ProviderError {
    auth("ChatGPT plan: not signed in — run `makedo login chatgpt`")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random<const N: usize>() -> Result<[u8; N], ProviderError> {
    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| auth("system random generator is unavailable"))?;
    Ok(bytes)
}

/// PKCE S256: base64url(SHA-256(verifier)) без паддинга.
fn challenge(verifier: &str) -> String {
    B64.encode(ring::digest::digest(
        &ring::digest::SHA256,
        verifier.as_bytes(),
    ))
}

/// Сохранённая учётная запись. Токены наружу не отдаются и в `Debug` не попадают.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub email: String,
    pub subject: String,
    pub client_id: String,
    pub ext_agent_host_id: String,
    id_token: String,
    access_token: String,
    refresh_token: String,
    scopes: Vec<String>,
    /// Unix-секунды истечения access-токена.
    expires_at: u64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("email", &self.email)
            .field("client_id", &self.client_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    fn fresh(&self) -> bool {
        now() + REFRESH_MARGIN_S < self.expires_at
    }
}

fn path() -> Result<PathBuf, ProviderError> {
    makedo_config::global_dir()
        .map(|d| d.join("chatgpt.json"))
        .ok_or_else(|| auth("no ~/.makedo directory for ChatGPT credentials"))
}

fn load() -> Option<Credentials> {
    serde_json::from_str(&std::fs::read_to_string(path().ok()?).ok()?).ok()
}

/// E-mail сохранённой учётной записи (для `makedo doctor` и `/models`).
#[must_use]
pub fn account() -> Option<String> {
    load().map(|c| c.email)
}

/// Атомарно и только для владельца: временный файл 0600 → rename.
fn save(creds: &Credentials) -> Result<(), ProviderError> {
    let write = || -> std::io::Result<()> {
        let path = path().map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension(format!("json.{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(creds)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    };
    write().map_err(|e| auth(format!("cannot save ChatGPT credentials: {e}")))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

/// Обмен кода или refresh-токена. Тело ответа с ошибкой укорачиваем: в логи
/// не должно попасть ничего, кроме кода ошибки.
async fn exchange(
    client: &reqwest::Client,
    form: &[(&str, &str)],
) -> Result<TokenResponse, ProviderError> {
    let response = client.post(TOKEN).form(form).send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        let code = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| Some(v.get("error")?.as_str()?.to_owned()))
            .unwrap_or_else(|| text.chars().take(120).collect());
        return Err(auth(format!(
            "ChatGPT sign-in failed ({status}: {code}) — run `makedo login chatgpt`"
        )));
    }
    serde_json::from_str(&text).map_err(|e| ProviderError::Protocol(format!("token response: {e}")))
}

/// Одна попытка входа: её `state`, `nonce` и PKCE-verifier живут только здесь.
struct Attempt {
    url: reqwest::Url,
    redirect: String,
    state: String,
    nonce: String,
    verifier: String,
    host_id: String,
    /// Выданный ранее `client_id`; `None` — первая регистрация.
    issued: Option<String>,
}

impl Attempt {
    fn new(saved: Option<&Credentials>, port: u16) -> Result<Self, ProviderError> {
        let verifier = B64.encode(random::<32>()?);
        let state = B64.encode(random::<24>()?);
        let nonce = B64.encode(random::<24>()?);
        let host_id = if let Some(c) = saved {
            c.ext_agent_host_id.clone()
        } else {
            let mut b = random::<16>()?;
            b[6] = (b[6] & 0x0f) | 0x40; // UUID v4
            b[8] = (b[8] & 0x3f) | 0x80;
            let hex = b.iter().fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
            format!(
                "urn:uuid:{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            )
        };
        let issued = saved.map(|c| c.client_id.clone());
        let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
        let code_challenge = challenge(&verifier);
        let mut params = vec![
            ("client_id", issued.as_deref().unwrap_or(DYNAMIC_CLIENT)),
            ("ext_agent_host_id", &host_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("scope", SCOPE),
            ("resource", RESOURCE),
            ("state", &state),
            ("nonce", &nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &code_challenge),
        ];
        match saved {
            // имя — только при первой регистрации; при повторном входе подсказываем аккаунт
            None => params.push(("agent_name_hint", "makedo")),
            Some(c) => params.push(("login_hint", &c.email)),
        }
        let url = reqwest::Url::parse_with_params(AUTHORIZE, &params)
            .map_err(|e| auth(format!("authorize url: {e}")))?;
        Ok(Self {
            url,
            redirect,
            state,
            nonce,
            verifier,
            host_id,
            issued,
        })
    }

    /// Проверяет callback и возвращает `(code, выданный client_id)`.
    fn check(&self, query: &HashMap<String, String>) -> Result<(String, String), ProviderError> {
        if query.get("state") != Some(&self.state) {
            return Err(auth("sign-in callback has a wrong state; start again"));
        }
        if let Some(error) = query.get("error") {
            return Err(auth(format!("sign-in was not completed: {error}")));
        }
        let code = query
            .get("code")
            .ok_or_else(|| auth("sign-in callback has no code"))?;
        let client_id = match (&self.issued, query.get("client_id")) {
            (Some(saved), Some(got)) if saved != got => {
                return Err(auth("sign-in returned a different client id; rejected"));
            }
            (Some(saved), _) => saved.clone(),
            (None, Some(got)) if got != DYNAMIC_CLIENT => got.clone(),
            (None, _) => return Err(auth("registration is incomplete: no client id issued")),
        };
        Ok((code.clone(), client_id))
    }
}

/// Ждёт запрос на `/auth/callback` и возвращает его query; прочие пути (favicon) — 404.
async fn wait_callback(
    listener: &tokio::net::TcpListener,
) -> Result<HashMap<String, String>, ProviderError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| auth(format!("callback listener: {e}")))?;
        let mut buffer = vec![0u8; 8192];
        let n = stream.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..n]);
        let target = request.split_whitespace().nth(1).unwrap_or("");
        let callback = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .ok()
            .filter(|url| url.path() == CALLBACK_PATH);
        let (status, body) = if callback.is_some() {
            (
                "200 OK",
                "makedo: sign-in response received. You can close this tab.",
            )
        } else {
            ("404 Not Found", "not found")
        };
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        if let Some(url) = callback {
            return Ok(url.query_pairs().into_owned().collect());
        }
    }
}

/// Проверка ID-токена: подпись RS256 по JWKS, издатель, аудитория, срок, nonce.
/// Возвращает claims.
fn verify_id_token(
    token: &str,
    jwks: &Value,
    client_id: &str,
    nonce: &str,
) -> Result<Value, ProviderError> {
    let bad = |what: &str| auth(format!("ID token rejected: {what}"));
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("not a JWT"));
    };
    let json =
        |part: &str| -> Option<Value> { serde_json::from_slice(&B64.decode(part).ok()?).ok() };
    let header_json = json(header).ok_or_else(|| bad("header"))?;
    if header_json["alg"] != "RS256" {
        return Err(bad("unexpected algorithm"));
    }
    let key = jwks["keys"]
        .as_array()
        .and_then(|keys| {
            keys.iter()
                .find(|k| k["kty"] == "RSA" && k["kid"] == header_json["kid"])
        })
        .ok_or_else(|| bad("unknown signing key"))?;
    let component = |name: &str| key[name].as_str().and_then(|v| B64.decode(v).ok());
    let (Some(n), Some(e), Ok(signature)) = (component("n"), component("e"), B64.decode(signature))
    else {
        return Err(bad("malformed key or signature"));
    };
    ring::signature::RsaPublicKeyComponents { n, e }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            &token.as_bytes()[..header.len() + 1 + payload.len()],
            &signature,
        )
        .map_err(|_| bad("signature"))?;
    let claims = json(payload).ok_or_else(|| bad("payload"))?;
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(list) => list.iter().any(|aud| aud == client_id),
        _ => false,
    };
    if claims["iss"] != ISSUER {
        return Err(bad("issuer"));
    }
    if !audience_ok {
        return Err(bad("audience"));
    }
    if claims["exp"].as_u64().is_none_or(|exp| exp <= now()) {
        return Err(bad("expired"));
    }
    if claims["nonce"] != nonce {
        return Err(bad("nonce"));
    }
    Ok(claims)
}

/// Полный вход. `open` получает URL авторизации: вызывающая сторона открывает
/// браузер и печатает ссылку. Учётная запись сохраняется только после всех проверок.
pub async fn login(open: impl FnOnce(&str)) -> Result<Credentials, ProviderError> {
    let saved = load();
    let client = reqwest::Client::new();
    // слушатель поднимается ДО открытия браузера
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", DEFAULT_PORT)).await {
        Ok(listener) => listener,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| auth(format!("cannot listen on 127.0.0.1: {e}")))?,
    };
    let port = listener
        .local_addr()
        .map_err(|e| auth(format!("callback listener: {e}")))?
        .port();
    let attempt = Attempt::new(saved.as_ref(), port)?;
    open(attempt.url.as_str());
    let query = tokio::time::timeout(LOGIN_TIMEOUT, wait_callback(&listener))
        .await
        .map_err(|_| auth("sign-in timed out: no response from the browser in 5 minutes"))??;
    let (code, client_id) = attempt.check(&query)?;
    let tokens = exchange(
        &client,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &client_id),
            ("code", &code),
            ("code_verifier", &attempt.verifier),
            ("redirect_uri", &attempt.redirect),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    let (Some(id_token), Some(refresh_token)) = (tokens.id_token, tokens.refresh_token) else {
        return Err(auth("sign-in response has no ID or refresh token"));
    };
    let jwks: Value = client
        .get(JWKS)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let claims = verify_id_token(&id_token, &jwks, &client_id, &attempt.nonce)?;
    let scopes: Vec<String> = tokens
        .scope
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    // валидный ID-токен сам по себе не даёт права тратить квоту плана
    if !scopes.iter().any(|s| s == PLAN_SCOPE) {
        return Err(auth(
            "ChatGPT plan usage was not granted for this account (scope missing)",
        ));
    }
    let creds = Credentials {
        email: claims["email"].as_str().unwrap_or_default().to_owned(),
        subject: claims["sub"].as_str().unwrap_or_default().to_owned(),
        client_id,
        ext_agent_host_id: attempt.host_id,
        id_token,
        access_token: tokens.access_token,
        refresh_token,
        scopes,
        expires_at: now() + tokens.expires_in,
    };
    save(&creds)?;
    Ok(creds)
}

/// Действующий access-токен; при скором истечении обновляет его.
pub async fn access_token(client: &reqwest::Client) -> Result<String, ProviderError> {
    let creds = load().ok_or_else(not_signed_in)?;
    if creds.fresh() {
        return Ok(creds.access_token);
    }
    // Refresh-токен ротируется: обновление — под межпроцессной блокировкой,
    // после которой состояние перечитывается (другой makedo мог успеть раньше).
    let lock_path = path()?.with_extension("lock");
    let _lock = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)?;
        file.lock()?;
        Ok(file)
    })
    .await
    .map_err(|e| auth(format!("credentials lock: {e}")))?
    .map_err(|e| auth(format!("credentials lock: {e}")))?;
    let mut creds = load().ok_or_else(not_signed_in)?;
    if creds.fresh() {
        return Ok(creds.access_token);
    }
    let tokens = exchange(
        client,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &creds.client_id),
            ("refresh_token", &creds.refresh_token),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    // access-токен, срок, права и новый refresh-токен заменяются вместе
    creds.access_token = tokens.access_token;
    creds.expires_at = now() + tokens.expires_in;
    if let Some(refresh_token) = tokens.refresh_token {
        creds.refresh_token = refresh_token;
    }
    if let Some(scope) = tokens.scope {
        creds.scopes = scope.split_whitespace().map(str::to_owned).collect();
    }
    save(&creds)?;
    Ok(creds.access_token)
}

/// Модели, доступные вошедшему аккаунту (`slug` для поля `model`).
pub async fn models() -> Result<Vec<String>, ProviderError> {
    let client = reqwest::Client::new();
    let token = access_token(&client).await?;
    let catalog: Value =
        send_with_retries(|| client.get(format!("{RESOURCE}/models")).bearer_auth(&token))
            .await?
            .json()
            .await?;
    Ok(catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .filter_map(|m| m["slug"].as_str().map(str::to_owned))
        .collect())
}

/// Тело запроса для маршрута подписки: обычный Responses без `max_output_tokens`
/// (на этом маршруте поле не принимается).
fn plan_body(req: &CompletionRequest) -> Value {
    let mut body = crate::responses::body(req);
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
    }
    body
}

/// Провайдер `kind = "chatgpt"`: Responses API в счёт подписки `ChatGPT`.
#[derive(Debug)]
pub struct ChatGpt {
    client: reqwest::Client,
}

impl ChatGpt {
    pub fn new() -> Result<Self, ProviderError> {
        load().ok_or_else(not_signed_in)?;
        Ok(Self {
            client: reqwest::Client::new(),
        })
    }
}

impl Provider for ChatGpt {
    fn stream(&self, req: CompletionRequest) -> EventStream {
        let client = self.client.clone();
        let body = plan_body(&req);
        let request = Box::pin(async move {
            let token = access_token(&client).await?;
            send_with_retries(|| {
                client
                    .post(format!("{RESOURCE}/responses"))
                    .bearer_auth(&token)
                    .json(&body)
            })
            .await
        });
        event_stream(request, crate::responses::Mapper::default())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn saved() -> Credentials {
        Credentials {
            email: "user@example.com".into(),
            subject: "sub".into(),
            client_id: "oaiapp_issued".into(),
            ext_agent_host_id: "urn:uuid:host".into(),
            id_token: "ID-SECRET".into(),
            access_token: "ACCESS-SECRET".into(),
            refresh_token: "REFRESH-SECRET".into(),
            scopes: vec![PLAN_SCOPE.into()],
            expires_at: now() + 3600,
        }
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn first_sign_in_registers_and_later_ones_reuse_the_issued_client() {
        let query = |url: &reqwest::Url| -> HashMap<String, String> {
            url.query_pairs().into_owned().collect()
        };
        let first = Attempt::new(None, 1455).unwrap();
        let q = query(&first.url);
        assert_eq!(q["client_id"], DYNAMIC_CLIENT);
        assert_eq!(q["agent_name_hint"], "makedo");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["resource"], RESOURCE);
        assert_eq!(q["code_challenge"], challenge(&first.verifier));
        assert!(q["ext_agent_host_id"].starts_with("urn:uuid:"));

        let again = Attempt::new(Some(&saved()), 54321).unwrap();
        let q = query(&again.url);
        assert_eq!(q["client_id"], "oaiapp_issued");
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:host");
        assert_eq!(q["login_hint"], "user@example.com");
        assert!(!q.contains_key("agent_name_hint"));
        assert_ne!(first.state, again.state);
    }

    #[test]
    fn callback_is_accepted_only_with_matching_state_and_an_issued_client() {
        let first = Attempt::new(None, 1455).unwrap();
        let callback = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let ok = callback(&[
            ("state", &first.state),
            ("code", "c"),
            ("client_id", "oaiapp_new"),
        ]);
        assert_eq!(first.check(&ok).unwrap(), ("c".into(), "oaiapp_new".into()));
        for rejected in [
            callback(&[
                ("state", "forged"),
                ("code", "c"),
                ("client_id", "oaiapp_new"),
            ]),
            callback(&[("state", &first.state), ("error", "access_denied")]),
            callback(&[("state", &first.state), ("code", "c")]),
            callback(&[
                ("state", &first.state),
                ("code", "c"),
                ("client_id", DYNAMIC_CLIENT),
            ]),
        ] {
            assert!(first.check(&rejected).is_err(), "{rejected:?}");
        }
        let again = Attempt::new(Some(&saved()), 1455).unwrap();
        let other = callback(&[
            ("state", &again.state),
            ("code", "c"),
            ("client_id", "oaiapp_other"),
        ]);
        assert!(again.check(&other).is_err());
        let same = callback(&[("state", &again.state), ("code", "c")]);
        assert_eq!(again.check(&same).unwrap().1, "oaiapp_issued");
    }

    #[tokio::test]
    async fn listener_ignores_other_paths_and_returns_the_callback_query() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let browser = tokio::spawn(async move {
            for target in ["/favicon.ico", "/auth/callback?code=abc&state=s%20t"] {
                let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
                stream
                    .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut reply = String::new();
                stream.read_to_string(&mut reply).await.unwrap();
                assert!(reply.starts_with("HTTP/1.1 "));
            }
        });
        let query = wait_callback(&listener).await.unwrap();
        assert_eq!(query["code"], "abc");
        assert_eq!(query["state"], "s t");
        browser.await.unwrap();
    }

    /// Одноразовый тестовый ключ RSA-2048 (приватная часть уничтожена) и подписанный им
    /// токен: iss/aud/nonce как в бою, срок — 2100 год.
    const TEST_KEY_N: &str = "n7yEZ1MFEkL_PVcV_o8hAg3YOzBgc4z9n6G2fY_6GfoIXmAZDAXMlsaYYbETTXL4Gra43b5oEJ6ACO_Fw6rqNx1n\
         7If__RkWi3W7k_y_SCioUtUl9M53nt-BZBgK65Drxe3sUMr-E5FSrsjfUzj_WX_psEfI-xPJoHYEeXonjddd1kQS\
         4lHj4xtV_VHBISXkMwgIDmnHZdLmcZI3Ta49K5-_BSw-mREu0JPg0yogxxV3zIe13Rg6Z5qL0UbH-IutcCQi-tAA\
         hVHxwXT62PVS8hODHjn-DPjVBuzz92SpdEZfHqJx2duTWeK-CsVnqDoUBu2irl20Tjsp4EIr5-xJSw";
    const TEST_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InRlc3Qta2V5In0.eyJpc3MiOiJodHRwczovL2F1dGgub3BlbmFpLmNvbSIs\
         ImF1ZCI6Im9haWFwcF9pc3N1ZWQiLCJleHAiOjQxMDI0NDQ4MDAsIm5vbmNlIjoibjEiLCJzdWIiOiJzdWItMSIs\
         ImVtYWlsIjoidXNlckBleGFtcGxlLmNvbSJ9.kTKdA2n5nw7YAuXZzBSATswEA_eGRxjxEzafoOo-OWFJYA8-QjV\
         Uyy2h6HSt7rUqFAAaDH8u2sgm0fR_Ya8OTXMEOg5lYqPhfqCbPLmO5zZI5PahD6VgAW43HoKzf1NvkqAkJBYT2hG\
         0cBe_ltZbFqdmwtmLJhCntbuPaSee6lu2Efjzzc_JZ4LhfndWfz4rgSe45RjLWwfnPziIhmiPr1qiN0ybRZ4mfP5\
         dX2Hm90uVAx0h5TkxByJvb9RQbpGyljES8L3Fd4xq6va5ZFJM8JuX0xmJQabxR2TFbdN9JCTQ8rWqWBS8KWj4HAX\
         omrQe4EHTofqUCL0bL6UQOhz5gw";

    #[test]
    fn id_token_needs_a_valid_signature_and_matching_claims() {
        let jwks = serde_json::json!({"keys": [
            {"kty": "RSA", "kid": "other", "n": "AQAB", "e": "AQAB"},
            {"kty": "RSA", "kid": "test-key", "n": TEST_KEY_N, "e": "AQAB"},
        ]});
        let claims = verify_id_token(TEST_TOKEN, &jwks, "oaiapp_issued", "n1").unwrap();
        assert_eq!(claims["email"], "user@example.com");

        // тот же токен с подменённым payload: подпись больше не сходится
        let mut parts: Vec<&str> = TEST_TOKEN.split('.').collect();
        let forged = B64.encode(br#"{"iss":"https://auth.openai.com","aud":"oaiapp_issued","exp":4102444800,"nonce":"n1","email":"evil@example.com"}"#);
        parts[1] = &forged;
        assert!(verify_id_token(&parts.join("."), &jwks, "oaiapp_issued", "n1").is_err());

        assert!(verify_id_token(TEST_TOKEN, &jwks, "oaiapp_other", "n1").is_err());
        assert!(verify_id_token(TEST_TOKEN, &jwks, "oaiapp_issued", "other-nonce").is_err());
        let no_key = serde_json::json!({"keys": []});
        assert!(verify_id_token(TEST_TOKEN, &no_key, "oaiapp_issued", "n1").is_err());
    }

    #[test]
    fn plan_route_omits_the_output_limit_and_secrets_stay_out_of_debug() {
        let body = plan_body(&CompletionRequest {
            model: "m".into(),
            system: "s".into(),
            messages: vec![crate::Message::User {
                content: "hi".into(),
            }],
            tools: Vec::new(),
            max_tokens: 100,
            reasoning_effort: None,
        });
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "s");

        let creds = saved();
        assert!(creds.fresh());
        let printed = format!("{creds:?}");
        assert!(printed.contains("user@example.com") && !printed.contains("SECRET"));
    }
}
