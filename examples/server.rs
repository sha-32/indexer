//! Serves a small collection of tasks from an in-memory SQLite database.
//!
//! ```text
//! cargo run --example server
//! curl "http://localhost:8000/tasks?q=p>=high+l:bug+or+is:overdue&order=created"
//! curl "http://localhost:8000/tasks/syntax"      # the query vocabulary, as JSON
//! curl "http://localhost:8000/tasks/syntax.md"   # the same, as a guide to read
//! ```

use std::sync::Arc;

use axum::Router;
use axum::extract::{OriginalUri, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use indexer::backend::sqlite::{SqliteBackend, sqlite_backend};
use indexer::http::{HandlerOptions, respond};
use indexer::syntax::{describe, guide};
use indexer::{Collection, Field, Order};

struct App {
    tasks: Collection,
    backend: SqliteBackend,
    options: HandlerOptions,
}

fn tasks() -> Result<Collection, indexer::DefinitionError> {
    Collection::builder("tasks")
        .field("id", Field::number())
        .field("title", Field::text().regex().describe("The one-line summary of the task."))
        .field("status", Field::keyword().values(["open", "closed"]).alias("s"))
        .field(
            "priority",
            Field::ordinal(["low", "normal", "high", "urgent"])
                .alias("p")
                .describe("How soon it needs doing."),
        )
        .field("labels", Field::set(["bug", "feature", "docs", "infra"]).alias("l"))
        .field("estimate", Field::number().alias("est").nullable().describe("Days of work expected."))
        .field("spent", Field::number().nullable().describe("Days of work done so far."))
        .field("created", Field::date())
        .field("assignee", Field::keyword().alias("a").nullable())
        .id("id")
        .default_field("title")
        .predicate("open", "status:open")
        .predicate("overdue", "is:open spent>estimate")
        .order("created", Order::by(["created"]).desc())
        .order("priority", Order::by(["priority", "created"]).desc())
        .describe("is:overdue", "Open tasks that have taken longer than estimated.")
        .describe("order:created", "Newest first.")
        .describe("order:priority", "Most urgent first, newest first within a priority.")
        .build()
}

fn database() -> rusqlite::Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open_in_memory()?;
    connection.execute_batch(
        "CREATE TABLE tasks (
            id INTEGER PRIMARY KEY, title TEXT NOT NULL, status TEXT NOT NULL, priority TEXT NOT NULL,
            labels TEXT NOT NULL, estimate REAL, spent REAL, created TEXT NOT NULL, assignee TEXT
        );
        INSERT INTO tasks VALUES
            (1, 'Login fails on Safari', 'open', 'urgent', '[\"bug\"]', 3, 5, '2026-01-05', 'kim'),
            (2, 'Add SSO login', 'open', 'high', '[\"feature\"]', 8, 2, '2026-01-12', 'lee'),
            (3, 'Document the search syntax', 'closed', 'low', '[\"docs\"]', 2, 2, '2026-02-01', NULL),
            (4, 'Rotate TLS certificates', 'open', 'high', '[\"infra\"]', NULL, 1, '2026-02-14', 'kim'),
            (5, 'Flaky deploy', 'open', 'normal', '[\"bug\",\"infra\"]', 5, 9, '2026-03-02', 'ana'),
            (6, 'Dark mode', 'open', 'low', '[\"feature\"]', 13, NULL, '2026-03-20', NULL);",
    )?;
    Ok(connection)
}

async fn search(State(app): State<Arc<App>>, OriginalUri(uri): OriginalUri) -> impl IntoResponse {
    // The database call blocks, so it runs off the async workers.
    let target = uri.path_and_query().map_or_else(|| uri.path().to_string(), ToString::to_string);
    let answered =
        tokio::task::spawn_blocking(move || respond(&app.tasks, &app.backend, &target, &app.options)).await;
    match answered {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(response.body)).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn syntax_json(State(app): State<Arc<App>>) -> impl IntoResponse {
    Json(describe(&app.tasks))
}

async fn syntax_guide(State(app): State<Arc<App>>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], guide(&app.tasks))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tasks = tasks()?;
    let backend = sqlite_backend(&tasks, database()?)?;
    let options = HandlerOptions { page_size: 3, ..HandlerOptions::default() };
    let app = Arc::new(App { tasks, backend, options });

    let router = Router::new()
        .route("/tasks", get(search))
        .route("/tasks/syntax", get(syntax_json))
        .route("/tasks/syntax.md", get(syntax_guide))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8000").await?;
    println!("listening on http://{}/tasks", listener.local_addr()?);
    axum::serve(listener, router).await?;
    Ok(())
}
