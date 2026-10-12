//! Shared server state: config, upstream HTTP client, console ticket store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::header::{COOKIE, HOST};
use axum::http::HeaderMap;
use base64::Engine as _;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest as _, Sha256};

use crate::auth::constant_time_eq;
use crate::config::{Agent, Config, Instance};
use crate::history::HistoryStore;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const TICKET_TTL: Duration = Duration::from_secs(30);
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60);
pub const PAIRING_TTL: Duration = Duration::from_secs(2 * 60);
pub const MAX_PAIRINGS_PER_SESSION: usize = 5;
pub const SESSION_COOKIE: &str = "muser_console_session";

pub type ProxyClient = Client<HttpsConnector<HttpConnector>, axum::body::Body>;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<StateInner>,
}

struct StateInner {
    config: Config,
    engine_clients: HashMap<String, ProxyClient>,
    agent_client: ProxyClient,
    tickets: Mutex<HashMap<String, Ticket>>,
    sessions: Mutex<HashMap<String, Session>>,
    pairings: Mutex<Vec<Pairing>>,
    /// `None` when the history plane is off. The state plane never reads
    /// this, so a console with no store proxies exactly as before.
    history: Option<HistoryStore>,
    /// Last scrape outcome per agent name. An agent with no entry has never
    /// been scraped, which reports as `unknown` — never as up or down.
    agent_states: Mutex<HashMap<String, AgentState>>,
}

/// What the console knows about an agent, which is only ever what its own
/// last scrape did. `Live` means the exporter answered that scrape, not that
/// its data source had anything to report: an exporter serving
/// `muser_agent_up 0` is reachable and stores nothing, and the gap in the
/// series is what says so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    /// No scrape has completed yet (including on a console whose history
    /// plane — and therefore its sampler — is switched off).
    Unknown,
    Live,
    Unreachable,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Unknown => "unknown",
            AgentState::Live => "live",
            AgentState::Unreachable => "unreachable",
        }
    }
}

/// A console-minted /stream ticket. Tickets are instance-scoped: one minted
/// for instance A is not a credential for instance B.
struct Ticket {
    expiry: Instant,
    instance: String,
}

struct Session {
    expiry: Instant,
    csrf: String,
    origin: String,
}

/// A one-use credential that can establish a dashboard session on another
/// device. Only the SHA-256 digest is retained: the QR bearer exists solely
/// in the mint response and on the operator's screen.
struct Pairing {
    id: String,
    token_hash: [u8; 32],
    issuer_hash: [u8; 32],
    expiry: Instant,
    origin: String,
}

pub struct MintedPairing {
    pub id: String,
    pub token: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MutationAuthError {
    Unauthorized,
    Csrf,
}

impl AppState {
    /// State-plane-only console: proxying and auth, no history store.
    pub fn new(config: Config) -> AppState {
        AppState::build(config, None)
    }

    /// Console with the history plane attached. The store is owned here so
    /// sampler tasks and the query API reach the same writer thread.
    pub fn with_history(config: Config, history: HistoryStore) -> AppState {
        AppState::build(config, Some(history))
    }

    fn build(config: Config, history: Option<HistoryStore>) -> AppState {
        let engine_clients = config
            .instances
            .iter()
            .map(|instance| {
                (
                    instance.name.clone(),
                    build_client(instance.tls_config.clone()),
                )
            })
            .collect();
        let agent_client = build_client(config.agent_tls_config.clone());
        AppState {
            inner: Arc::new(StateInner {
                config,
                engine_clients,
                agent_client,
                tickets: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                pairings: Mutex::new(Vec::new()),
                history,
                agent_states: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn history(&self) -> Option<&HistoryStore> {
        self.inner.history.as_ref()
    }

    pub fn default_instance(&self) -> &Instance {
        &self.inner.config.instances[0]
    }

    /// Exact-string lookup; instance names are validated `[A-Za-z0-9_-]{1,64}`
    /// at config load, so no normalization or decoding happens here.
    pub fn instance(&self, name: &str) -> Option<&Instance> {
        self.inner
            .config
            .instances
            .iter()
            .find(|instance| instance.name == name)
    }

    pub fn client(&self, instance: &Instance) -> &ProxyClient {
        self.inner
            .engine_clients
            .get(&instance.name)
            .expect("validated instance has a dedicated HTTP client")
    }

    pub fn agent_client(&self) -> &ProxyClient {
        &self.inner.agent_client
    }

    /// Agents attached to one instance, in config order.
    pub fn agents_for<'a>(&'a self, instance: &'a str) -> impl Iterator<Item = &'a Agent> {
        self.inner.config.agents_for(instance)
    }

    /// What the last scrape of `agent` did. Never scraped is `Unknown`; the
    /// console does not guess at an exporter it has not yet reached.
    pub fn agent_state(&self, agent: &str) -> AgentState {
        self.inner
            .agent_states
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(agent)
            .copied()
            .unwrap_or(AgentState::Unknown)
    }

    /// Records one scrape outcome. Called by the sampler only.
    pub fn set_agent_state(&self, agent: &str, state: AgentState) {
        self.inner
            .agent_states
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(agent.to_owned(), state);
    }

    pub fn mint_ticket(&self, instance: &str) -> Result<String, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        let ticket = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut tickets = self
            .inner
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        tickets.retain(|_, entry| entry.expiry > now);
        tickets.insert(
            ticket.clone(),
            Ticket {
                expiry: now + TICKET_TTL,
                instance: instance.to_owned(),
            },
        );
        Ok(ticket)
    }

    /// Browser API reads use a session cookie on TLS listeners. The in-memory
    /// bearer remains available only on literal loopback listeners; a remote
    /// browser therefore sends the access key once, to the login route, and
    /// never on telemetry or control requests.
    pub fn authorized_read(&self, headers: &HeaderMap) -> bool {
        if self.inner.config.listen.ip().is_loopback()
            && crate::auth::valid_bearer(&self.inner.config.access_key, headers)
        {
            return true;
        }
        self.valid_session(headers).is_some()
    }

    /// A bearer-authenticated loopback mutation needs no CSRF token because
    /// it has no ambient browser credential. Cookie-authenticated mutations
    /// are bound to the session's random CSRF value.
    pub fn authorized_mutation(&self, headers: &HeaderMap) -> Result<(), MutationAuthError> {
        if self.inner.config.listen.ip().is_loopback()
            && crate::auth::valid_bearer(&self.inner.config.access_key, headers)
        {
            return Ok(());
        }
        let Some(csrf) = self.valid_session(headers) else {
            return Err(MutationAuthError::Unauthorized);
        };
        let candidate = headers
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok());
        if candidate
            .is_some_and(|candidate| constant_time_eq(candidate.as_bytes(), csrf.as_bytes()))
        {
            Ok(())
        } else {
            Err(MutationAuthError::Csrf)
        }
    }

    pub fn mint_session(&self, origin: String) -> Result<(String, String), getrandom::Error> {
        let mut token_bytes = [0u8; 32];
        let mut csrf_bytes = [0u8; 32];
        getrandom::fill(&mut token_bytes)?;
        getrandom::fill(&mut csrf_bytes)?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
        let csrf = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(csrf_bytes);
        let now = Instant::now();
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        sessions.retain(|_, entry| entry.expiry > now);
        sessions.insert(
            token.clone(),
            Session {
                expiry: now + SESSION_TTL,
                csrf: csrf.clone(),
                origin,
            },
        );
        Ok((token, csrf))
    }

    /// Return the CSRF value for the current cookie session. This is safe to
    /// expose only through the exact-origin session bootstrap route: it lets
    /// a refreshed HttpOnly-cookie client remain capable of mutations.
    pub fn session_csrf(&self, headers: &HeaderMap) -> Option<String> {
        self.valid_session(headers)
    }

    /// Mint a bounded, one-use LAN pairing credential for an authenticated
    /// session. The caller has already enforced mutation auth and exact
    /// HTTPS Origin/Host; repeating session validation here provides the
    /// issuer identity used for the per-session cap and revoke operation.
    pub fn mint_pairing(
        &self,
        headers: &HeaderMap,
        origin: String,
    ) -> Result<Option<MintedPairing>, getrandom::Error> {
        let Some(session_token) = cookie_value(headers, SESSION_COOKIE) else {
            return Ok(None);
        };
        if self.valid_session(headers).is_none() {
            return Ok(None);
        }

        let mut raw_token = [0u8; 32];
        let mut raw_id = [0u8; 16];
        getrandom::fill(&mut raw_token)?;
        getrandom::fill(&mut raw_id)?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw_token);
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw_id);
        let issuer_hash = sha256(session_token.as_bytes());
        let now = Instant::now();
        let mut pairings = self
            .inner
            .pairings
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pairings.retain(|entry| entry.expiry > now);

        while pairings
            .iter()
            .filter(|entry| constant_time_eq(&entry.issuer_hash, &issuer_hash))
            .count()
            >= MAX_PAIRINGS_PER_SESSION
        {
            let Some(oldest) = pairings
                .iter()
                .position(|entry| constant_time_eq(&entry.issuer_hash, &issuer_hash))
            else {
                break;
            };
            pairings.remove(oldest);
        }
        pairings.push(Pairing {
            id: id.clone(),
            token_hash: sha256(&raw_token),
            issuer_hash,
            expiry: now + PAIRING_TTL,
            origin,
        });
        Ok(Some(MintedPairing { id, token }))
    }

    /// Spend a pairing credential exactly once. Expiry cleanup, comparison,
    /// and removal happen under one mutex so concurrent scans cannot both
    /// establish a session.
    pub fn consume_pairing(&self, raw_token: &[u8; 32], origin: &str) -> bool {
        let candidate_hash = sha256(raw_token);
        let mut pairings = self
            .inner
            .pairings
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        consume_pairing_at(&mut pairings, &candidate_hash, origin, Instant::now())
    }

    /// Revoke a still-pending pairing from the same session that minted it.
    pub fn revoke_pairing(&self, headers: &HeaderMap, id: &str) -> bool {
        let Some(session_token) = cookie_value(headers, SESSION_COOKIE) else {
            return false;
        };
        if self.valid_session(headers).is_none() {
            return false;
        }
        let issuer_hash = sha256(session_token.as_bytes());
        let now = Instant::now();
        let mut pairings = self
            .inner
            .pairings
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pairings.retain(|entry| entry.expiry > now);
        let Some(index) = pairings.iter().position(|entry| {
            constant_time_eq(entry.id.as_bytes(), id.as_bytes())
                && constant_time_eq(&entry.issuer_hash, &issuer_hash)
        }) else {
            return false;
        };
        pairings.remove(index);
        true
    }

    fn valid_session(&self, headers: &HeaderMap) -> Option<String> {
        self.inner.config.tls.as_ref()?;
        let candidate = cookie_value(headers, SESSION_COOKIE)?;
        let request_origin = request_origin(headers)?;
        let now = Instant::now();
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        sessions.retain(|_, entry| entry.expiry > now);
        sessions
            .iter()
            .find(|(token, entry)| {
                constant_time_eq(token.as_bytes(), candidate.as_bytes())
                    && constant_time_eq(entry.origin.as_bytes(), request_origin.as_bytes())
            })
            .map(|(_, entry)| entry.csrf.clone())
    }

    /// Consume `candidate` for `instance`. A ticket presented against the
    /// wrong instance is rejected with the same answer as a bogus ticket and
    /// is left unconsumed — its legitimate holder can still spend it where
    /// it was minted.
    pub fn consume_ticket(&self, candidate: &str, instance: &str) -> bool {
        let now = Instant::now();
        let mut tickets = self
            .inner
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        tickets.retain(|_, entry| entry.expiry > now);
        let Some(stored) = tickets
            .iter()
            .find(|(ticket, entry)| {
                constant_time_eq(ticket.as_bytes(), candidate.as_bytes())
                    && entry.instance == instance
            })
            .map(|(ticket, _)| ticket.clone())
        else {
            return false;
        };
        tickets.remove(&stored);
        true
    }
}

fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = single_header(headers, HOST.as_str())?;
    let authority: axum::http::uri::Authority = host.parse().ok()?;
    if authority.as_str() != host {
        return None;
    }
    Some(format!("https://{authority}"))
}

fn sha256(input: &[u8]) -> [u8; 32] {
    Sha256::digest(input).into()
}

fn consume_pairing_at(
    pairings: &mut Vec<Pairing>,
    candidate_hash: &[u8; 32],
    origin: &str,
    now: Instant,
) -> bool {
    pairings.retain(|entry| entry.expiry > now);
    let Some(index) = pairings.iter().position(|entry| {
        constant_time_eq(&entry.token_hash, candidate_hash)
            && constant_time_eq(entry.origin.as_bytes(), origin.as_bytes())
    }) else {
        return false;
    };
    pairings.remove(index);
    true
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(first)
}

fn build_client(tls_config: Arc<rustls::ClientConfig>) -> ProxyClient {
    let mut connector = HttpConnector::new();
    // `HttpsConnectorBuilder::build()` applies this to its own default
    // connector, but `wrap_connector()` cannot mutate an arbitrary generic
    // connector. Without it the inner connector rejects `https` before
    // rustls gets a chance to perform the handshake.
    connector.enforce_http(false);
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
    connector.set_nodelay(true);
    let connector = HttpsConnectorBuilder::new()
        .with_tls_config(tls_config.as_ref().clone())
        .https_or_http()
        .enable_http1()
        .wrap_connector(connector);
    Client::builder(TokioExecutor::new()).build(connector)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for header in headers.get_all(COOKIE) {
        let text = header.to_str().ok()?;
        for part in text.split(';') {
            let Some((candidate_name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if candidate_name == name {
                // Ambiguous duplicate cookies are rejected instead of relying
                // on browser or proxy ordering.
                if found.is_some() || value.is_empty() {
                    return None;
                }
                found = Some(value.to_owned());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;

    #[tokio::test]
    async fn wrapped_http_connector_allows_https_to_reach_transport() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let client = build_client(Arc::new(tls));
        let request = Request::builder()
            .uri("https://127.0.0.1:9/healthz")
            .body(Body::empty())
            .expect("request");
        let result = tokio::time::timeout(Duration::from_secs(2), client.request(request)).await;
        let error = match result {
            Ok(Err(error)) => error.to_string(),
            Ok(Ok(_)) => return, // an unexpected TLS test service still proves scheme passage
            Err(_) => return,    // a network timeout also proves the connector accepted HTTPS
        };
        assert!(
            !error.contains("scheme is not http"),
            "the inner HttpConnector rejected HTTPS before rustls: {error}"
        );
    }

    #[test]
    fn expired_pairing_is_removed_and_never_consumed() {
        let now = Instant::now();
        let raw = [7u8; 32];
        let mut pairings = vec![Pairing {
            id: "expired".to_owned(),
            token_hash: sha256(&raw),
            issuer_hash: [3u8; 32],
            expiry: now - Duration::from_millis(1),
            origin: "https://console.test".to_owned(),
        }];
        assert!(!consume_pairing_at(
            &mut pairings,
            &sha256(&raw),
            "https://console.test",
            now
        ));
        assert!(pairings.is_empty());
    }
}
