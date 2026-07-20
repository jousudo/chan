//! Integration tests for `POST /internal/v1/sessions/whoami`.
//!
//! Each test gets its own throwaway Postgres schema; profile-service
//! and GitHub are wiremocks. One test drives a real OAuth login so
//! the cookie-value -> whoami round-trip is exercised end to end;
//! edge cases write session records straight into the store.

#[path = "../../../tests-shared/pg_reaper.rs"]
mod pg_reaper;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tower::ServiceExt;
use tower_sessions::cookie::time::{Duration, OffsetDateTime};
use tower_sessions::session::Record;
use tower_sessions::session_store::SessionStore;
use tower_sessions_sqlx_store::PostgresStore;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use identity::api_tokens::ApiTokenService;
use identity::config::Config;
use identity::http;
use identity::profile_client::ProfileClient;
use identity::providers::github::{GitHubEndpoints, GitHubProvider};

const INTERNAL_TOKEN: &str = "test-internal";
const PROFILE_TOKEN: &str = "test-profile-token";

/// Single-connection admin pool -- see profile-tests for the
/// rationale (default pool size * parallel tests blows past
/// Postgres' max_connections).
async fn admin_pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await
        .expect("connect admin")
}

struct TestApp {
    router: Router,
    store: PostgresStore,
    schema: String,
    admin_url: String,
    profile: MockServer,
    github: MockServer,
}

impl TestApp {
    async fn new() -> Self {
        let url = std::env::var("TEST_DATABASE_URL")
            .expect("TEST_DATABASE_URL must be set; e.g. postgres://localhost/chan_gateway_test");
        pg_reaper::reap_idle(&url).await;
        let schema = format!("t_{}", Uuid::new_v4().simple());

        let admin = admin_pool(&url).await;
        sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
            .execute(&admin)
            .await
            .expect("create schema");
        admin.close().await;

        let s = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |conn, _meta| {
                let s = s.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO \"{s}\", public"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("connect pool");

        let store = PostgresStore::new(pool.clone());
        store.migrate().await.expect("migrate sessions");

        let profile = MockServer::start().await;
        let github = MockServer::start().await;

        let profile_client =
            ProfileClient::new(profile.uri().parse().unwrap(), PROFILE_TOKEN.into())
                .expect("profile client");

        let github_endpoints = GitHubEndpoints {
            auth: format!("{}/login/oauth/authorize", github.uri()),
            token: format!("{}/login/oauth/access_token", github.uri()),
            user: format!("{}/user", github.uri()),
            emails: format!("{}/user/emails", github.uri()),
        };
        let provider = GitHubProvider::with_endpoints(
            "client-id".into(),
            "client-secret".into(),
            github_endpoints,
        )
        .expect("github provider");

        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrate identity tables");

        let api_tokens = ApiTokenService::new(pool.clone());

        let cfg = Arc::new(Config {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            internal_bind_addr: "127.0.0.1:0".parse().unwrap(),
            base_url: "http://localhost:7000/".parse().unwrap(),
            devserver_proxy_origin: "https://proxy.example.test".parse().unwrap(),
            devserver_tunnel_origin: "https://tunnel.example.test".parse().unwrap(),
            database_url: url.clone(),
            cookie_secure: false,
            profile_client,
            internal_auth_token: INTERNAL_TOKEN.to_string(),
            identity_admin_token: String::new(),
            workspace_admin: identity::devserver_control_client::DevserverControlClient::new(
                "http://127.0.0.1:1".parse().unwrap(),
                "test-identity-admin-token".into(),
            )
            .unwrap(),
            admission_lease_verifier: {
                let signer = devserver_control_proto::AdmissionLeaseSigner::from_base64(
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                )
                .unwrap();
                devserver_control_proto::AdmissionLeaseVerifier::from_base64(
                    &signer.verifying_key_base64(),
                )
                .unwrap()
            },
            entry_signer: gateway_common::devserver_gate::EntrySigner::from_base64(
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            )
            .unwrap(),
            providers: vec![Arc::new(provider)],
        });

        let router = http::router(
            cfg,
            store.clone(),
            pool.clone(),
            api_tokens,
            identity::token_throttle::TokenThrottle::new(),
        );

        Self {
            router,
            store,
            schema,
            admin_url: url,
            profile,
            github,
        }
    }

    async fn cleanup(self) {
        let admin = admin_pool(&self.admin_url).await;
        let _ = sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", self.schema))
            .execute(&admin)
            .await;
        admin.close().await;
    }

    /// Write a session record directly, bypassing the OAuth flow, and
    /// return the cookie value that would carry it.
    async fn mint_session(&self, user_id: Option<Uuid>, authenticated_at: Option<&str>) -> String {
        let mut data = HashMap::new();
        if let Some(uid) = user_id {
            data.insert("user_id".to_string(), json!(uid));
        }
        if let Some(ts) = authenticated_at {
            data.insert("authenticated_at".to_string(), json!(ts));
        }
        let record = Record {
            id: Default::default(),
            data,
            expiry_date: OffsetDateTime::now_utc() + Duration::days(1),
        };
        self.store.save(&record).await.expect("save session");
        record.id.to_string()
    }
}

/// Mock profile's `GET /v1/users/{uid}` for a live user row.
async fn mock_get_user(profile: &MockServer, uid: Uuid, blocked: bool) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut body = json!({
        "id": uid,
        "email": "whoami@example.com",
        "display_name": "Who Ami",
        "username": format!("u{}", &uid.simple().to_string()[..12]),
        "username_edits": 0,
        "created_at": now,
        "updated_at": now,
    });
    if blocked {
        body["blocked_at"] = json!(now);
        body["block_reason"] = json!("test block");
    }
    Mock::given(method("GET"))
        .and(path(format!("/v1/users/{uid}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(profile)
        .await;
}

async fn whoami(app: &TestApp, bearer: Option<&str>, session: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/internal/v1/sessions/whoami")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(b) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let req = builder
        .body(Body::from(json!({ "session": session }).to_string()))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn whoami_requires_the_internal_bearer() {
    let app = TestApp::new().await;
    let session = app.mint_session(Some(Uuid::new_v4()), None).await;

    for bearer in [None, Some("wrong-token")] {
        let (status, _) = whoami(&app, bearer, &session).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
    }

    app.cleanup().await;
}

#[tokio::test]
async fn whoami_malformed_and_unknown_sessions_are_401() {
    let app = TestApp::new().await;

    let (status, _) = whoami(&app, Some(INTERNAL_TOKEN), "not-a-session-id").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Parses as a session id but was never issued.
    let forged = tower_sessions::session::Id::default().to_string();
    let (status, _) = whoami(&app, Some(INTERNAL_TOKEN), &forged).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

#[tokio::test]
async fn whoami_pre_auth_session_is_401() {
    let app = TestApp::new().await;
    // Pending-OAuth state: a record with no user key.
    let session = app.mint_session(None, None).await;

    let (status, _) = whoami(&app, Some(INTERNAL_TOKEN), &session).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

#[tokio::test]
async fn whoami_resolves_user_and_authenticated_at() {
    let app = TestApp::new().await;
    let uid = Uuid::new_v4();
    let session = app
        .mint_session(Some(uid), Some("2026-07-20T12:00:00Z"))
        .await;
    mock_get_user(&app.profile, uid, false).await;

    let (status, body) = whoami(&app, Some(INTERNAL_TOKEN), &session).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["id"].as_str().unwrap(), uid.to_string());
    assert_eq!(
        body["user"]["username"].as_str().unwrap(),
        format!("u{}", &uid.simple().to_string()[..12])
    );
    assert_eq!(body["user"]["blocked"], false);
    let stamped = body["session"]["authenticated_at"]
        .as_str()
        .expect("authenticated_at present");
    let parsed = chrono::DateTime::parse_from_rfc3339(stamped).expect("rfc3339");
    assert_eq!(parsed.timestamp(), 1_784_548_800, "{stamped}");

    app.cleanup().await;
}

#[tokio::test]
async fn whoami_without_authentication_stamp_returns_null() {
    let app = TestApp::new().await;
    let uid = Uuid::new_v4();
    let session = app.mint_session(Some(uid), None).await;
    mock_get_user(&app.profile, uid, false).await;

    let (status, body) = whoami(&app, Some(INTERNAL_TOKEN), &session).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["session"]["authenticated_at"].is_null(), "{body}");

    app.cleanup().await;
}

#[tokio::test]
async fn whoami_blocked_user_reports_blocked() {
    let app = TestApp::new().await;
    let uid = Uuid::new_v4();
    let session = app.mint_session(Some(uid), None).await;
    mock_get_user(&app.profile, uid, true).await;

    let (status, body) = whoami(&app, Some(INTERNAL_TOKEN), &session).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["blocked"], true);

    app.cleanup().await;
}

#[tokio::test]
async fn whoami_deleted_user_is_401() {
    let app = TestApp::new().await;
    let uid = Uuid::new_v4();
    let session = app.mint_session(Some(uid), None).await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/users/{uid}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not found"})))
        .mount(&app.profile)
        .await;

    let (status, _) = whoami(&app, Some(INTERNAL_TOKEN), &session).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

#[tokio::test]
async fn whoami_expired_session_is_401() {
    let app = TestApp::new().await;
    let uid = Uuid::new_v4();
    let mut data = HashMap::new();
    data.insert("user_id".to_string(), json!(uid));
    let record = Record {
        id: Default::default(),
        data,
        // The store's load() filters expiry_date <= now, so an expired
        // record takes the same 401 path as an unknown one.
        expiry_date: OffsetDateTime::now_utc() - Duration::hours(1),
    };
    app.store.save(&record).await.expect("save session");
    let session = record.id.to_string();

    let (status, _) = whoami(&app, Some(INTERNAL_TOKEN), &session).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    app.cleanup().await;
}

/// Tiny cookie-jar client for the login flow: keeps the session
/// cookie between calls so the callback can find the pending state.
struct Client<'a> {
    app: &'a TestApp,
    cookie: Option<String>,
}

impl<'a> Client<'a> {
    fn new(app: &'a TestApp) -> Self {
        Self { app, cookie: None }
    }

    async fn send(&mut self, method: Method, uri: &str) -> (StatusCode, String) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(c) = &self.cookie {
            builder = builder.header(header::COOKIE, c.clone());
        }
        let req = builder.body(Body::empty()).unwrap();
        let res = self.app.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        if let Some(set_cookie) = res
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .next()
            .and_then(|v| v.to_str().ok())
        {
            let pair = set_cookie.split(';').next().unwrap_or("").to_string();
            self.cookie = Some(pair);
        }
        let location = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        (status, location)
    }
}

#[tokio::test]
async fn login_flow_stamps_and_resolves_end_to_end() {
    let app = TestApp::new().await;
    let mut c = Client::new(&app);
    let uid = Uuid::new_v4();
    let email = "octo@example.com";

    // /auth/github -> provider redirect with state.
    let (s, location) = c.send(Method::GET, "/auth/github").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let state = Url::parse(&location)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state param");

    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gh-access",
            "token_type": "Bearer",
            "scope": "read:user,user:email",
        })))
        .mount(&app.github)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 999,
            "login": "octocat",
            "name": "Octo Cat",
            "email": email,
        })))
        .mount(&app.github)
        .await;

    let now = chrono::Utc::now().to_rfc3339();
    let user_body = json!({
        "id": uid,
        "email": email,
        "display_name": "Octo Cat",
        "username": format!("u{}", &uid.simple().to_string()[..12]),
        "username_edits": 0,
        "created_at": now,
        "updated_at": now,
    });
    Mock::given(method("POST"))
        .and(path("/v1/users/upsert-by-identity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": user_body,
            "user_created": true,
            "identity_created": true,
        })))
        .mount(&app.profile)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/users/{uid}/grants/claim")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"claimed": 0})))
        .mount(&app.profile)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/users/{uid}/flags")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "oauth_login": true,
        })))
        .mount(&app.profile)
        .await;

    let (s, location) = c
        .send(
            Method::GET,
            &format!("/auth/github/callback?code=fake&state={state}"),
        )
        .await;
    assert_eq!(s, StatusCode::SEE_OTHER, "callback should redirect");
    assert_eq!(location, "/");

    // The jar holds `id_session=<value>`; whoami takes the bare value
    // exactly as a tier-local service would extract it.
    let cookie = c.cookie.clone().expect("session cookie set");
    let session_value = cookie
        .split_once('=')
        .map(|(_, v)| v.to_string())
        .expect("cookie value");
    mock_get_user(&app.profile, uid, false).await;

    let (status, body) = whoami(&app, Some(INTERNAL_TOKEN), &session_value).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["id"].as_str().unwrap(), uid.to_string());
    assert_eq!(body["user"]["blocked"], false);
    let stamped = body["session"]["authenticated_at"]
        .as_str()
        .expect("login stamps authenticated_at");
    let parsed = chrono::DateTime::parse_from_rfc3339(stamped).expect("rfc3339");
    let age = chrono::Utc::now().timestamp() - parsed.timestamp();
    assert!((0..60).contains(&age), "stamp {stamped} is fresh");

    app.cleanup().await;
}
