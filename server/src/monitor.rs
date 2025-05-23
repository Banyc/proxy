use axum::{
    Router,
    extract::{Query, State},
    routing::get,
};
use common::proto::metrics::{stream::StreamSessionTable, udp::UdpSessionTable};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct SessionTables {
    pub stream: StreamSessionTable,
    pub udp: UdpSessionTable,
}

pub fn monitor_router() -> (SessionTables, Router) {
    let metrics_handle = PrometheusBuilder::new().install_recorder().unwrap();
    let session_tables = SessionTables {
        stream: StreamSessionTable::new(),
        udp: UdpSessionTable::new(),
    };

    async fn metrics(metrics_handle: State<PrometheusHandle>) -> String {
        metrics_handle.render()
    }
    fn sessions(
        Query(params): Query<SessionsParams>,
        State(session_table): State<SessionTables>,
    ) -> anyhow::Result<String> {
        let mut text = String::new();
        {
            let sql = &params.stream_sql;
            text.push_str("Stream:\n");
            let sessions = session_table.stream.to_view(sql).map(|s| s.to_string())?;
            text.push_str(&sessions);
            text.push('\n');
        }
        {
            let sql = &params.udp_sql;
            text.push_str("UDP:\n");
            let sessions = session_table.udp.to_view(sql).map(|s| s.to_string())?;
            text.push_str(&sessions);
            text.push('\n');
        }
        Ok(text)
    }
    let router = Router::new()
        .route("/", get(metrics))
        .with_state(metrics_handle)
        .route(
            "/sessions",
            get(|params, state| async { sessions(params, state).map_err(|e| format!("{e:#?}")) }),
        )
        .with_state(session_tables.clone())
        .route("/health", get(|| async { Ok::<_, ()>(()) }));

    (session_tables, router)
}

fn stream_default_sql() -> String {
    const SQL: &str = r#"
sort start_ms
select (col "destination.addr.host") (col "destination.addr.port") duration (col "upstream_remote.addr.host") (col "upstream_remote.addr.port")
"#;
    SQL.to_string()
}
fn udp_default_sql() -> String {
    const SQL: &str = r#"
sort start_ms
select (col "destination.host") (col "destination.port") duration (col "upstream_remote.host") (col "upstream_remote.port")
"#;
    SQL.to_string()
}
#[derive(Debug, Deserialize)]
struct SessionsParams {
    #[serde(default = "stream_default_sql")]
    stream_sql: String,
    #[serde(default = "udp_default_sql")]
    udp_sql: String,
}
