use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{delete, get, post};
use forgejo_agent::api::ForgejoClient;
use forgejo_agent::config::AgentConfig;
use forgejo_agent::types::{IssueRef, RepoRef};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;
use url::Url;

#[derive(Debug, Deserialize)]
struct CreateIssue {
    title: String,
    body: String,
}

#[tokio::test]
async fn issue_create_follows_repo_rename_redirect_without_switching_to_get() -> Result<()> {
    let post_hits = Arc::new(AtomicUsize::new(0));
    let post_hits_clone = post_hits.clone();

    let app = axum::Router::new()
        .route(
            "/api/v1/repos/main/forgejo-work/issues",
            post(|| async {
                Redirect::to("/api/v1/repos/main/forgejo-agent/issues").into_response()
            }),
        )
        .route(
            "/api/v1/repos/main/forgejo-agent/issues",
            get(|| async { Json(json!([])).into_response() }).post(
                move |Json(req): Json<CreateIssue>| async move {
                    post_hits_clone.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "number": 123,
                            "state": "open",
                            "title": req.title,
                            "body": req.body,
                            "html_url": "http://example.invalid/main/forgejo-agent/issues/123",
                        })),
                    )
                        .into_response()
                },
            ),
        );

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let base_url = Url::parse(&format!("http://{addr}"))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let cfg = AgentConfig {
        base_url,
        default_repo: RepoRef::new("main", "scratch"),
        agent_name: "test".to_string(),
        lease_minutes: 90,
        token: "test-token".to_string(),
    };

    let created = tokio::task::spawn_blocking(move || {
        let api = ForgejoClient::new(&cfg)?;
        api.create_issue(&cfg, &RepoRef::new("main", "forgejo-work"), "t", "b")
    })
    .await??;

    shutdown_tx.send(()).ok();
    server_task.await.ok();

    assert_eq!(created.number, 123);
    assert_eq!(post_hits.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn issue_label_delete_follows_repo_rename_redirect_without_switching_to_get() -> Result<()> {
    let delete_hits = Arc::new(AtomicUsize::new(0));
    let delete_hits_clone = delete_hits.clone();

    let app = axum::Router::new()
        .route(
            "/api/v1/repos/main/forgejo-work/issues/123/labels/99",
            delete(|| async {
                Redirect::to("/api/v1/repos/main/forgejo-agent/issues/123/labels/99")
                    .into_response()
            }),
        )
        .route(
            "/api/v1/repos/main/forgejo-agent/issues/123/labels/99",
            delete(move || async move {
                delete_hits_clone.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT.into_response()
            }),
        );

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let base_url = Url::parse(&format!("http://{addr}"))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let cfg = AgentConfig {
        base_url,
        default_repo: RepoRef::new("main", "scratch"),
        agent_name: "test".to_string(),
        lease_minutes: 90,
        token: "test-token".to_string(),
    };

    tokio::task::spawn_blocking(move || {
        let api = ForgejoClient::new(&cfg)?;
        let issue = IssueRef {
            repo: RepoRef::new("main", "forgejo-work"),
            number: 123,
        };
        api.remove_issue_label(&cfg, &issue, 99)
    })
    .await??;

    shutdown_tx.send(()).ok();
    server_task.await.ok();

    assert_eq!(delete_hits.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn issue_label_delete_rejects_non_canonical_non_get_redirect() -> Result<()> {
    let redirected_hits = Arc::new(AtomicUsize::new(0));
    let redirected_hits_clone = redirected_hits.clone();

    let app = axum::Router::new()
        .route(
            "/api/v1/repos/main/forgejo-work/issues/123/labels/99",
            delete(|| async { Redirect::to("/api/v1/user").into_response() }),
        )
        .route(
            "/api/v1/user",
            delete(move || async move {
                redirected_hits_clone.fetch_add(1, Ordering::SeqCst);
                Json(json!({"ok": true})).into_response()
            })
            .get(|| async { Json(json!({"ok": true})).into_response() }),
        );

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let base_url = Url::parse(&format!("http://{addr}"))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    let cfg = AgentConfig {
        base_url,
        default_repo: RepoRef::new("main", "scratch"),
        agent_name: "test".to_string(),
        lease_minutes: 90,
        token: "test-token".to_string(),
    };

    let err = tokio::task::spawn_blocking(move || {
        let api = ForgejoClient::new(&cfg)?;
        let issue = IssueRef {
            repo: RepoRef::new("main", "forgejo-work"),
            number: 123,
        };
        api.remove_issue_label(&cfg, &issue, 99)
    })
    .await?
    .expect_err("non-canonical non-GET redirect should fail");

    shutdown_tx.send(()).ok();
    server_task.await.ok();

    assert!(err.to_string().contains("unexpected redirect"));
    assert_eq!(redirected_hits.load(Ordering::SeqCst), 0);
    Ok(())
}
