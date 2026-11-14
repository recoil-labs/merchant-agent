//! Health and metrics, on the health port (127.0.0.1:8741 by default).
//!
//! - `/healthz`: the process is alive. For a container's liveness probe.
//! - `/readyz`: the agent is fit to pay: it holds the lease, the database
//!   takes writes, RecoilPay and the RPCs answer, and payouts aren't
//!   paused. 503 with each condition when not. For readiness probes and
//!   load balancers.
//! - `/metrics`: Prometheus.
//!
//! None of them needs a login, and none shows a trade or a person: only
//! yes/no conditions and counts. That is why they live apart from the
//! console.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::console::AgentOps;

pub fn router(ops: Arc<dyn AgentOps>) -> Router {
	Router::new()
		.route("/healthz", get(|| async { (StatusCode::OK, "ok\n") }))
		.route("/readyz", get(readyz))
		.route("/metrics", get(metrics))
		.with_state(ops)
}

async fn readyz(State(ops): State<Arc<dyn AgentOps>>) -> impl IntoResponse {
	let r = ops.readiness().await;
	let status = if r.ready() {
		StatusCode::OK
	} else {
		StatusCode::SERVICE_UNAVAILABLE
	};
	let body = serde_json::json!({ "ready": r.ready(), "checks": r });
	(status, axum::Json(body))
}

async fn metrics(State(ops): State<Arc<dyn AgentOps>>) -> impl IntoResponse {
	(
		[(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
		ops.metrics_text().await,
	)
}
