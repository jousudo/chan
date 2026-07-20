//! Integration tests for the `/admin/v1/users/*` account contract:
//! user-wide access revocation and user deletion for the tier account
//! service. Both routes are bearer-gated by IDENTITY_ADMIN_TOKEN,
//! idempotent per step, and answer 502 on any step failure so the
//! caller retries.
//!
//! Each test gets its own throwaway Postgres schema; profile-service
//! and the devserver-proxy admin API are wiremocks.

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
use tower_sessions::session::{Id as SessionId, Record};
use tower_sessions::session_store::SessionStore;
use tower_sessions_sqlx_store::PostgresStore;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use identity::api_tokens::{ApiTokenService, NewToken, RequestMeta, TokenOrigin};
use identity::config::Config;
use identity::devserver_control_client::DevserverControlClient;
use identity::http;
use identity::profile_client::ProfileClient;

const ADMIN_TOKEN: &str = "test-identity-admin-token";
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
    api_tokens: ApiTokenService,
    store: PostgresStore,
    schema: String,
    admin_url: String,
    profile: MockServer,
    proxy_admin: MockServer,
}

impl TestApp {
    /// `admin_token` becomes IDENTITY_ADMIN_TOKEN (empty = surface
    /// disabled); `with_proxy_admin` toggles the workspace_admin
    /// client the eviction step needs.
    async fn new(admin_token: &str, with_proxy_admin: bool) -> Self {
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
        let proxy_admin = MockServer::start().await;

        let profile_client =
            ProfileClient::new(profile.uri().parse().unwrap(), PROFILE_TOKEN.into())
                .expect("profile client");
        // The client is always constructed (runtime config fails
        // closed without it); `with_proxy_admin = false` points it at
        // an unreachable address so the eviction step errors and the
        // route answers 502, exercising the no-silent-skip contract.
        let workspace_admin = if with_proxy_admin {
            DevserverControlClient::new(proxy_admin.uri().parse().unwrap(), "test-admin".into())
        } else {
            DevserverControlClient::new("http://127.0.0.1:1".parse().unwrap(), "test-admin".into())
        }
        .expect("workspace admin client");

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
            internal_auth_token: "test-internal".to_string(),
            identity_admin_token: admin_token.to_string(),
            workspace_admin,
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
            providers: vec![],
        });

        let router = http::router(
            cfg,
            store.clone(),
            pool.clone(),
            api_tokens.clone(),
            identity::token_throttle::TokenThrottle::new(),
        );

        Self {
            router,
            api_tokens,
            store,
            schema,
            admin_url: url,
            profile,
            proxy_admin,
        }
    }

    async fn cleanup(self) {
        let admin = admin_pool(&self.admin_url).await;
        let _ = sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", self.schema))
            .execute(&admin)
            .await;
        admin.close().await;
    }

    /// Insert a users row directly (profile-service owns it in prod;
    /// identity and profile share one database).
    async fn insert_user(&self, id: Uuid, email: &str) {
        let url = self.admin_url.clone();
        let s = self.schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(1)
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
            .expect("connect for insert_user");
        sqlx::query(
            "INSERT INTO users (id, email, username) VALUES \
             ($1, $2, 'u' || substr(replace($1::text, '-', ''), 1, 12))",
        )
        .bind(id)
        .bind(email)
        .execute(&pool)
        .await
        .expect("insert user row");
        pool.close().await;
    }

    /// Mint a real PAT row through the service; returns (id, secret).
    async fn mint_pat(&self, uid: Uuid, label: &str) -> (Uuid, String) {
        let created = self
            .api_tokens
            .create(
                NewToken {
                    user_id: uid,
                    label,
                    expires_at: None,
                    scopes: &["tunnel".to_string()],
                    origin: TokenOrigin::Spa,
                },
                &RequestMeta::default(),
            )
            .await
            .expect("mint pat");
        (created.token.id, created.secret)
    }

    /// Write a session record for `uid` directly and return its id in
    /// cookie-value form.
    async fn mint_session(&self, uid: Uuid) -> String {
        let mut data = HashMap::new();
        data.insert("user_id".to_string(), json!(uid));
        let record = Record {
            id: Default::default(),
            data,
            expiry_date: OffsetDateTime::now_utc() + Duration::days(1),
        };
        self.store.save(&record).await.expect("save session");
        record.id.to_string()
    }

    async fn session_exists(&self, raw: &str) -> bool {
        let id: SessionId = raw.parse().expect("session id");
        self.store.load(&id).await.expect("load session").is_some()
    }
}

/// The placeholder username `insert_user` seeds, mirrored in Rust so
/// mocks can pin the exact path.
fn username_of(uid: Uuid) -> String {
    format!("u{}", &uid.simple().to_string()[..12])
}

async fn mock_get_user(profile: &MockServer, uid: Uuid) {
    let now = chrono::Utc::now().to_rfc3339();
    Mock::given(method("GET"))
        .and(path(format!("/v1/users/{uid}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": uid,
            "email": "admin-contract@example.com",
            "display_name": "Admin Contract",
            "username": username_of(uid),
            "username_edits": 0,
            "created_at": now,
            "updated_at": now,
        })))
        .mount(profile)
        .await;
}

async fn mock_kill(proxy_admin: &MockServer, uid: Uuid, killed: usize) {
    Mock::given(method("POST"))
        .and(path(format!("/admin/v1/owners/{uid}/tunnels/kill")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "killed": killed })))
        .mount(proxy_admin)
        .await;
}

async fn mock_auth_audit(profile: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/auth-audit"))
        .respond_with(ResponseTemplate::new(204))
        .mount(profile)
        .await;
}

async fn send_admin(
    app: &TestApp,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(b) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_revoke(app: &TestApp, bearer: Option<&str>, uid: Uuid) -> (StatusCode, Value) {
    send_admin(
        app,
        Method::POST,
        &format!("/admin/v1/users/{uid}/access/revoke"),
        bearer,
    )
    .await
}

async fn delete_user(app: &TestApp, bearer: Option<&str>, uid: Uuid) -> (StatusCode, Value) {
    send_admin(
        app,
        Method::DELETE,
        &format!("/admin/v1/users/{uid}"),
        bearer,
    )
    .await
}

/// Count profile-service hits on one path (audit attribution
/// assertions without a database read).
async fn profile_hits(app: &TestApp, path: &str) -> usize {
    app.profile
        .received_requests()
        .await
        .expect("request recording")
        .iter()
        .filter(|r| r.url.path() == path)
        .count()
}

#[tokio::test]
async fn revoke_and_delete_require_the_exact_bearer() {
    let app = TestApp::new(ADMIN_TOKEN, true).await;
    let uid = Uuid::new_v4();

    for bearer in [None, Some("wrong-token")] {
        let (status, _) = post_revoke(&app, bearer, uid).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "revoke {bearer:?}");
        let (status, _) = delete_user(&app, bearer, uid).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "delete {bearer:?}");
    }

    app.cleanup().await;
}

#[tokio::test]
async fn routes_are_404_when_the_admin_surface_is_disabled() {
    let app = TestApp::new("", true).await;
    let uid = Uuid::new_v4();

    let (status, _) = post_revoke(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = delete_user(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    app.cleanup().await;
}

#[tokio::test]
async fn revoke_access_revokes_pats_and_evicts_tunnels() {
    let app = TestApp::new(ADMIN_TOKEN, true).await;
    let uid = Uuid::new_v4();
    app.insert_user(uid, "revoke@example.com").await;
    let (_, live1) = app.mint_pat(uid, "laptop").await;
    let (_, live2) = app.mint_pat(uid, "ci").await;
    // Already-revoked rows must not inflate the count.
    let (dead_id, _) = app.mint_pat(uid, "old").await;
    app.api_tokens
        .revoke(uid, dead_id, &RequestMeta::default())
        .await
        .expect("pre-revoke");

    mock_get_user(&app.profile, uid).await;
    mock_kill(&app.proxy_admin, uid, 2).await;
    mock_auth_audit(&app.profile).await;

    let (status, body) = post_revoke(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user_id"].as_str().unwrap(), uid.to_string());
    assert_eq!(body["username"].as_str().unwrap(), username_of(uid));
    assert_eq!(body["pats_revoked"], 2);
    assert_eq!(body["tunnels_evicted"], 2);

    // Both live tokens are dead at the validate path now.
    for secret in [&live1, &live2] {
        let err = app
            .api_tokens
            .validate(secret, &RequestMeta::default())
            .await
            .expect_err("revoked token must not validate");
        assert!(matches!(err, identity::error::Error::Unauthorized));
    }
    // Exactly one canonical auth_audit entry for the bulk revoke.
    assert_eq!(profile_hits(&app, "/v1/auth-audit").await, 1);

    // Retry: the revocation half converges to zero, no second audit
    // entry, eviction re-runs (idempotent upstream).
    let (status, body) = post_revoke(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pats_revoked"], 0);
    assert_eq!(body["tunnels_evicted"], 2);
    assert_eq!(profile_hits(&app, "/v1/auth-audit").await, 1);

    app.cleanup().await;
}

#[tokio::test]
async fn revoke_access_eviction_failure_is_502_and_retryable() {
    let app = TestApp::new(ADMIN_TOKEN, true).await;
    let uid = Uuid::new_v4();
    app.insert_user(uid, "retry@example.com").await;
    let (_, secret) = app.mint_pat(uid, "laptop").await;

    mock_get_user(&app.profile, uid).await;
    mock_auth_audit(&app.profile).await;
    // First eviction attempt fails; wiremock matches mocks in mount
    // order, so the one-shot 500 is consumed before the standing 200.
    Mock::given(method("POST"))
        .and(path(format!("/admin/v1/owners/{uid}/tunnels/kill")))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&app.proxy_admin)
        .await;
    mock_kill(&app.proxy_admin, uid, 1).await;

    let (status, _) = post_revoke(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // The PAT half is durable even though the eviction failed.
    let err = app
        .api_tokens
        .validate(&secret, &RequestMeta::default())
        .await
        .expect_err("revoked token must not validate");
    assert!(matches!(err, identity::error::Error::Unauthorized));

    let (status, body) = post_revoke(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pats_revoked"], 0);
    assert_eq!(body["tunnels_evicted"], 1);

    app.cleanup().await;
}

#[tokio::test]
async fn revoke_access_unknown_user_is_zeroed_success() {
    let app = TestApp::new(ADMIN_TOKEN, true).await;
    let uid = Uuid::new_v4();
    // No users row, and profile says the same: a completed delete
    // already cascaded the tokens away, so the goal state holds.
    Mock::given(method("GET"))
        .and(path(format!("/v1/users/{uid}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not found"})))
        .mount(&app.profile)
        .await;

    let (status, body) = post_revoke(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["username"].is_null(), "{body}");
    assert_eq!(body["pats_revoked"], 0);
    assert_eq!(body["tunnels_evicted"], 0);
    // The handler evicts only for a user that still exists; with no
    // user there is nothing to call.
    assert!(app
        .proxy_admin
        .received_requests()
        .await
        .expect("request recording")
        .is_empty());

    app.cleanup().await;
}

#[tokio::test]
async fn revoke_access_without_proxy_client_is_502() {
    let app = TestApp::new(ADMIN_TOKEN, false).await;
    let uid = Uuid::new_v4();
    app.insert_user(uid, "noproxy@example.com").await;
    let (_, secret) = app.mint_pat(uid, "laptop").await;
    mock_get_user(&app.profile, uid).await;
    mock_auth_audit(&app.profile).await;

    // No silent skip: the caller must stall loudly until the
    // deployment can honor the eviction half.
    let (status, _) = post_revoke(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let err = app
        .api_tokens
        .validate(&secret, &RequestMeta::default())
        .await
        .expect_err("revoked token must not validate");
    assert!(matches!(err, identity::error::Error::Unauthorized));

    app.cleanup().await;
}

#[tokio::test]
async fn delete_user_removes_sessions_and_profile() {
    let app = TestApp::new(ADMIN_TOKEN, true).await;
    let uid_a = Uuid::new_v4();
    let uid_b = Uuid::new_v4();
    app.insert_user(uid_a, "gone@example.com").await;
    app.insert_user(uid_b, "stays@example.com").await;
    let a1 = app.mint_session(uid_a).await;
    let a2 = app.mint_session(uid_a).await;
    let b1 = app.mint_session(uid_b).await;

    // First call records the durable pending-delete (202); a retry
    // sees the row gone (404). wiremock matches mocks in mount
    // order, so the one-shot 202 is consumed before the standing 404.
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/users/{uid_a}")))
        .respond_with(ResponseTemplate::new(202))
        .up_to_n_times(1)
        .mount(&app.profile)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/users/{uid_a}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error": "not found"})))
        .mount(&app.profile)
        .await;

    let (status, body) = delete_user(&app, Some(ADMIN_TOKEN), uid_a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sessions_deleted"], 2);
    assert_eq!(body["profile_existed"], true);

    assert!(!app.session_exists(&a1).await, "a1 swept");
    assert!(!app.session_exists(&a2).await, "a2 swept");
    assert!(app.session_exists(&b1).await, "other user untouched");

    let (status, body) = delete_user(&app, Some(ADMIN_TOKEN), uid_a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sessions_deleted"], 0);
    assert_eq!(body["profile_existed"], false);

    app.cleanup().await;
}

#[tokio::test]
async fn delete_user_profile_failure_is_502_and_retryable() {
    let app = TestApp::new(ADMIN_TOKEN, true).await;
    let uid = Uuid::new_v4();
    app.insert_user(uid, "flaky@example.com").await;
    let session = app.mint_session(uid).await;

    // First attempt fails; wiremock matches mocks in mount order, so
    // the one-shot 500 is consumed before the standing 202.
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/users/{uid}")))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&app.profile)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/users/{uid}")))
        .respond_with(ResponseTemplate::new(202))
        .mount(&app.profile)
        .await;

    let (status, _) = delete_user(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    // The sweep ran first and is durable across the retried call.
    assert!(!app.session_exists(&session).await, "session swept");

    let (status, body) = delete_user(&app, Some(ADMIN_TOKEN), uid).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sessions_deleted"], 0);
    assert_eq!(body["profile_existed"], true);

    app.cleanup().await;
}
