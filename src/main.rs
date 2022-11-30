//! Simple in-memory key/value store showing features of axum.
//!
//! Run with:
//!
//! ```not_rust
//! cd examples && cargo run -p example-key-value-store
//! ```

use axum::{
    body::Bytes,
    error_handling::HandleErrorLayer,
    extract::{DefaultBodyLimit, Form, Path, State},
    handler::Handler,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{delete, get},
    Router,
};
use chrono::prelude::*;
use chrono::{Duration as chronDuration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::time::{Instant, SystemTime};
use std::{
    borrow::Cow,
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tokio::time::sleep;
use tower::{BoxError, ServiceBuilder};
use tower_http::{
    auth::RequireAuthorizationLayer, compression::CompressionLayer, limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "example_chat=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let shared_state = SharedState::default();
    let shared_state_clone = shared_state.clone();

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

            let keys_to_delete = shared_state_clone
                .db
                .iter()
                .filter(|r| r.value().expires <= Utc::now())
                .map(|r| r.key().clone())
                .collect::<Vec<String>>();

            for key in keys_to_delete.iter() {
                shared_state_clone.db.remove(key);
            }
        }
    });

    // Build our application by composing routes
    let app = Router::new()
        .route(
            "/",
            get(show_form).post_service(
                accept_form
                    .layer((
                        DefaultBodyLimit::disable(),
                        RequestBodyLimitLayer::new(1024 * 5_000 /* ~5mb */),
                    ))
                    .with_state(Arc::clone(&shared_state)),
            ),
        )
        .route(
            "/:key",
            // Add compression to `kv_get`
            get(kv_get)
                .layer(CompressionLayer::new())
                .with_state(Arc::clone(&shared_state)), // But don't compress `kv_set`
        )
        .route("/keys", get(list_keys))
        // Nest our admin routes under `/admin`
        .nest("/admin", admin_routes())
        // Add middleware to all routes
        .layer(
            ServiceBuilder::new()
                // Handle errors from middleware
                .layer(HandleErrorLayer::new(handle_error))
                .load_shed()
                .concurrency_limit(1024)
                .timeout(Duration::from_secs(10))
                .layer(TraceLayer::new_for_http()),
        )
        .with_state(Arc::clone(&shared_state));

    // Run our app with hyper
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

type SharedState = Arc<AppState>;

#[derive(Default)]
struct AppState {
    db: DashMap<String, Data>,
}

#[derive(Default)]
struct Data {
    expires: DateTime<Utc>,
    text: String,
}

async fn kv_get(
    Path(key): Path<String>,
    State(state): State<SharedState>,
) -> Result<String, StatusCode> {
    dbg!(&key);

    let ret = match state.db.try_get(&key) {
        dashmap::try_result::TryResult::Present(r) => {
            let text = r.value().text.clone();
            Ok(text)
        }
        dashmap::try_result::TryResult::Absent => {
            dbg!("absent");
            Err(StatusCode::NOT_FOUND)
        }
        dashmap::try_result::TryResult::Locked => {
            dbg!("Locked!!");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    };

    state.db.remove(&key);
    ret
}

async fn list_keys(State(state): State<SharedState>) -> String {
    state
        .db
        .iter()
        .map(|r| r.key().clone())
        .collect::<Vec<String>>()
        .join("\n")
}

fn admin_routes() -> Router<SharedState> {
    async fn delete_all_keys(State(state): State<SharedState>) {
        state.db.clear();
    }

    async fn remove_key(Path(key): Path<String>, State(state): State<SharedState>) {
        state.db.remove(&key);
    }

    Router::new()
        .route("/keys", delete(delete_all_keys))
        .route("/key/:key", delete(remove_key))
        // Require bearer auth for all admin routes
        .layer(RequireAuthorizationLayer::bearer("secret-token"))
}

async fn handle_error(error: BoxError) -> impl IntoResponse {
    if error.is::<tower::timeout::error::Elapsed>() {
        return (StatusCode::REQUEST_TIMEOUT, Cow::from("request timed out"));
    }

    if error.is::<tower::load_shed::error::Overloaded>() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Cow::from("service is overloaded, try again later"),
        );
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Cow::from(format!("Unhandled internal error: {}", error)),
    )
}

async fn show_form() -> Html<&'static str> {
    Html(
        r#"
        <!doctype html>
        <html>
            <head></head>
            <body>
                <label for="minutes">Expiring Minutes (Min: 0, Max: 59):</label>
                <input type="number" id="minutes" name="minutes" form="postSecret" min="0" max="59" value="0">
                <br/>
                <label for="hours">Expiring Hours (Min: 0, Max: 23):</label>
                <input type="number" id="hours" name="hours" form="postSecret" min="0" max="23" value="0">
                <br/>
                <label for="days">Expiring Days (Min: 0, Max: 7):</label>
                <input type="number" id="days" name="days" form="postSecret" min="0" max="7" value="0">
                <br/>
                <textarea form="postSecret" name="text" id="text" cols="35" wrap="soft"></textarea>
                <form action="/" method="post" id="postSecret">
                    <input type="submit" value="Create Secret">
                </form>
            </body>
        </html>
        "#,
    )
}

#[derive(Deserialize, Debug)]
pub struct Input {
    text: String,
    minutes: u32,
    hours: u32,
    days: u32,
}

async fn accept_form(State(state): State<SharedState>, Form(input): Form<Input>) {
    dbg!(&input);
    let expire = Utc::now()
        + chronDuration::days(input.days as i64)
        + chronDuration::hours(input.hours as i64)
        + chronDuration::minutes(input.minutes as i64);

    let mut hasher = DefaultHasher::new();
    input.text.hash(&mut hasher);

    let data = Data {
        expires: expire,
        text: input.text,
    };

    state.db.insert(hasher.finish().to_string(), data);
}
