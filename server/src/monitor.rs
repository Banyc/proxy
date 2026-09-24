use axum::{
    Router,
    extract::{Query, State},
    routing::get,
};
use common::proxy_runtime::metrics::{stream::StreamSessionTable, udp::UdpSessionTable};
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
            let sql = &params.stream_query;
            text.push_str("Stream:\n");
            let sessions = session_table.stream.to_view(sql).map(|s| s.to_string())?;
            text.push_str(&sessions);
            text.push('\n');
        }
        {
            let sql = &params.udp_query;
            text.push_str("UDP:\n");
            let sessions = session_table.udp.to_view(sql).map(|s| s.to_string())?;
            text.push_str(&sessions);
            text.push('\n');
        }
        Ok(text)
    }
    let router = Router::new()
        .route("/metrics", get(metrics))
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
    #[serde(alias = "stream_sql")]
    stream_query: String,
    #[serde(default = "udp_default_sql")]
    #[serde(alias = "udp_sql")]
    udp_query: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default stream query targets the stream table's flattened
    /// `destination.addr.*` columns, and the default udp query the udp
    /// table's `destination.*` columns; swapping them silently queries the
    /// wrong table.
    #[test]
    fn the_default_queries_target_their_own_session_table() {
        let params: SessionsParams = toml::from_str("").unwrap();
        assert!(
            params.stream_query.contains("destination.addr.host"),
            "stream default query: {}",
            params.stream_query
        );
        assert!(
            params.udp_query.contains("destination.host"),
            "udp default query: {}",
            params.udp_query
        );
    }

    /// The `stream_sql`/`udp_sql` aliases are part of the query contract; if
    /// an alias is dropped a client using it silently falls back to the
    /// default query instead of the one it asked for.
    #[test]
    fn the_sql_aliases_override_the_default_queries() {
        let params: SessionsParams =
            toml::from_str("stream_sql = \"select 1\"\nudp_sql = \"select 2\"").unwrap();
        assert_eq!(params.stream_query, "select 1");
        assert_eq!(params.udp_query, "select 2");
    }

    /// The rest of the defaults is a rendering contract too: without a
    /// query the session view is read for the sessions' start order and
    /// their durations, so those must not silently drop out of either
    /// default.
    #[test]
    fn the_default_queries_select_the_ordering_and_duration_columns() {
        let params: SessionsParams = toml::from_str("").unwrap();
        for (table, query) in [("stream", &params.stream_query), ("udp", &params.udp_query)] {
            assert!(
                query.contains("sort start_ms"),
                "the {table} default query must order by the session start: {query}"
            );
            assert!(
                query.contains("duration"),
                "the {table} default query must select the session duration: {query}"
            );
        }
    }
}
