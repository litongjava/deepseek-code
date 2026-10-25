//! Recorded usage, configured and built-in list price estimates. Unknown prices or
//! missing provider counters remain unknown; they are never reported as free.
use super::anthropic_auth as auth;
use crate::{config::AppConfig, db::DbManager};
use anyhow::{ensure, Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, time::Duration};

#[derive(Default, Serialize, Deserialize)]
pub struct Window {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
pub struct ExtraUsage {
    pub is_enabled: bool,
    pub monthly_limit: Option<f64>,
    pub used_credits: Option<f64>,
    pub utilization: Option<f64>,
}
#[derive(Default, Serialize, Deserialize)]
pub struct PlanUsage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub seven_day_oauth_apps: Option<Window>,
    pub seven_day_opus: Option<Window>,
    pub seven_day_sonnet: Option<Window>,
    pub extra_usage: Option<ExtraUsage>,
}

async fn read_plan(request: reqwest::RequestBuilder) -> Result<PlanUsage> {
    let response = request
        .send()
        .await
        .context("Fetching Anthropic plan usage")?;
    ensure!(
        response.status().is_success(),
        "Plan usage returned HTTP {}",
        response.status().as_u16()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|len| len <= 128 * 1024),
        "Plan usage response is too large"
    );
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            bytes.len() + chunk.len() <= 128 * 1024,
            "Plan usage response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let plan: PlanUsage = serde_json::from_slice(&bytes).context("Invalid plan usage response")?;
    for window in [
        &plan.five_hour,
        &plan.seven_day,
        &plan.seven_day_oauth_apps,
        &plan.seven_day_opus,
        &plan.seven_day_sonnet,
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            window
                .utilization
                .is_none_or(|value| value.is_finite() && value >= 0.0),
            "Invalid plan utilization"
        );
        if let Some(reset) = &window.resets_at {
            chrono::DateTime::parse_from_rfc3339(reset).context("Invalid plan reset time")?;
        }
    }
    if let Some(extra) = &plan.extra_usage {
        for value in [extra.monthly_limit, extra.used_credits, extra.utilization]
            .into_iter()
            .flatten()
        {
            ensure!(
                value.is_finite() && value >= 0.0,
                "Invalid extra usage value"
            );
        }
    }
    Ok(plan)
}

pub async fn plan(root: &Path) -> Result<Option<PlanUsage>> {
    let status = auth::status(root)?;
    if !status.authenticated || !status.scopes.iter().any(|scope| scope == "user:profile") {
        return Ok(None);
    }
    let base = "https://api.anthropic.com";
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let request = http.get(format!("{base}/api/oauth/usage")).header(
        "User-Agent",
        concat!("claude-code-rust/", env!("CARGO_PKG_VERSION")),
    );
    let request = auth::authorize(request, &auth::credential_marker(root)?, base, false).await?;
    Ok(Some(read_plan(request).await?))
}

pub fn session_cost(db: &DbManager, session: &str, config: &AppConfig) -> Result<Value> {
    db.with_conn(|conn| -> Result<Value> {
        let mut statement=conn.prepare("SELECT provider_id,COALESCE(response_model,model),input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,usage_json,started_at_ms FROM model_requests WHERE conversation_id=?1")?;
        let rows=statement.query_map([session],|row|Ok((row.get::<_,Option<String>>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,Option<u64>>(2)?,row.get::<_,Option<u64>>(3)?,row.get::<_,Option<u64>>(4)?,row.get::<_,Option<u64>>(5)?,row.get::<_,Option<String>>(6)?,row.get::<_,Option<i64>>(7)?)))?;
        let mut totals=BTreeMap::<String,Value>::new();
        let mut known=0.0;
        let mut missing=0u64;
        let mut reported=0u64;
        let mut estimated=0u64;
        for row in rows {
            let (provider,model,input,output,cache_read,cache_write,usage,started)=row?;
            let provider=provider.unwrap_or_default();
            let model=model.unwrap_or_default();
            let rate=config.providers.iter().find(|candidate|candidate.id==provider).and_then(|provider|provider.models.iter().find(|candidate|candidate.id==model));
            let usage=usage.as_deref().and_then(|text|serde_json::from_str::<Value>(text).ok()).unwrap_or(Value::Null);
            let explicit=usage["cost_usd"].as_f64().filter(|value|value.is_finite() && *value>=0.0);
            let estimate=rate.and_then(|rate|Some((rate.cost_per_1k_input?,rate.cost_per_1k_output?))).filter(|(a,b)|a.is_finite() && b.is_finite() && *a>=0.0 && *b>=0.0).and_then(|(a,b)|Some((input? as f64*a+output? as f64*b)/1000.0)).filter(|value|value.is_finite()).filter(|_|cache_read.unwrap_or(0)==0 && cache_write.unwrap_or(0)==0);
            let estimate=estimate.or_else(||super::model_pricing::list_price(&model,started,input,output,cache_read,cache_write,&usage));
            let price=explicit.or(estimate);
            let row=totals.entry(format!("{provider}::{model}")).or_insert_with(||json!({"requests":0,"known_subtotal_usd":0.0,"unpriced_requests":0,"reported_requests":0,"estimated_requests":0}));
            row["requests"]=json!(row["requests"].as_u64().unwrap()+1);
            if let Some(price)=price {
                known+=price;
                row["known_subtotal_usd"]=json!(row["known_subtotal_usd"].as_f64().unwrap()+price);
                let key=if explicit.is_some() {reported+=1;"reported_requests"} else {estimated+=1;"estimated_requests"};
                row[key]=json!(row[key].as_u64().unwrap()+1);
            } else { missing+=1; row["unpriced_requests"]=json!(row["unpriced_requests"].as_u64().unwrap()+1); }
        }
        Ok(json!({"currency":"USD","total_usd":if missing==0 {Some(known)} else {None},"known_subtotal_usd":known,"unpriced_requests":missing,"reported_requests":reported,"estimated_requests":estimated,"models":totals,"basis":"Provider-reported cost_usd where available, otherwise configured per-1k prices, otherwise built-in list prices (DeepSeek, with cache-hit and off-peak rates). Other cached requests without explicit cost are unpriced. Estimates are not invoices; subscription quotas are reported separately."}))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn plan_transport_preserves_null_and_rejects_auth_failure() {
        use axum::{routing::get, Router};
        let app=Router::new().route("/usage",get(||async {axum::Json(json!({"five_hour":{"utilization":32.5,"resets_at":"2026-10-15T12:00:00Z"},"seven_day":null,"extra_usage":{"is_enabled":false,"monthly_limit":null,"used_credits":null,"utilization":null}}))})).route("/forbidden",get(||async {axum::http::StatusCode::FORBIDDEN}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let usage = read_plan(http.get(format!("http://{address}/usage")))
            .await
            .unwrap();
        assert_eq!(usage.five_hour.unwrap().utilization, Some(32.5));
        assert!(usage.seven_day.is_none());
        assert!(read_plan(http.get(format!("http://{address}/forbidden")))
            .await
            .is_err());
        server.abort();
    }

    #[test]
    fn unknown_and_cached_costs_are_not_fabricated_as_zero() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let session = store.create(temp.path(), "model", "provider").unwrap();
        let message = store.append(&session, "assistant", "reply").unwrap();
        store.db.with_conn(|conn|crate::db::request_repo::save_request(conn,&message,&json!({"id":"request1","providerId":"provider","model":"model","inputTokens":100,"outputTokens":50,"cacheReadTokens":20,"usage":{"cost_usd":0.125}}))).unwrap().unwrap();
        let config = AppConfig::default();
        assert_eq!(
            session_cost(&store.db, &session, &config).unwrap()["total_usd"],
            0.125
        );
        store.db.with_conn(|conn|crate::db::request_repo::save_request(conn,&message,&json!({"id":"request2","providerId":"provider","model":"unknown","inputTokens":100,"outputTokens":50}))).unwrap().unwrap();
        let result = session_cost(&store.db, &session, &config).unwrap();
        assert!(result["total_usd"].is_null());
        assert_eq!(result["known_subtotal_usd"], 0.125);
        assert_eq!(result["unpriced_requests"], 1);
    }

    #[test]
    fn deepseek_requests_use_built_in_list_prices_with_cache_hits() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let session = store.create(temp.path(), "deepseek-flash", "cli-runtime").unwrap();
        let message = store.append(&session, "assistant", "reply").unwrap();
        // Saturday 2026-10-24 is off-peak: half of 2000*0.30 + 10000*0.006 + 100*1.20 per million.
        let started = chrono::DateTime::parse_from_rfc3339("2026-10-24T12:00:00Z").unwrap().timestamp_millis();
        store.db.with_conn(|conn|crate::db::request_repo::save_request(conn,&message,&json!({"id":"request1","providerId":"cli-runtime","model":"deepseek-flash","startedAt":started,"inputTokens":2000,"outputTokens":100,"cacheReadTokens":10000,"cacheWriteTokens":0,"usage":{"input_tokens":2000,"output_tokens":100,"cache_read_input_tokens":10000}}))).unwrap().unwrap();
        let result = session_cost(&store.db, &session, &AppConfig::default()).unwrap();
        let total = result["total_usd"].as_f64().unwrap();
        assert!((total - (2000.0 * 0.30 + 10000.0 * 0.006 + 100.0 * 1.20) / 2_000_000.0).abs() < 1e-12, "{result}");
        assert_eq!(result["estimated_requests"], 1);
    }
}
