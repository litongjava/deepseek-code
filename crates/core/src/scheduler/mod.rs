use crate::db::DbManager;
use crate::db::task_repo::ScheduledTaskRow;
use anyhow::Result;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::str::FromStr;
use tokio::sync::Mutex;
use tokio::time::{Duration, interval};
use tracing::{info, warn, error};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskExecutionResult {
    pub success: bool,
    pub output: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

pub struct ScheduledTaskScheduler {
    db_manager: Arc<DbManager>,
    running_tasks: Arc<Mutex<HashMap<String, TaskState>>>,
    should_stop: Arc<Mutex<bool>>,
    /// Where saved providers live; prompt and report tasks need a model.
    data_dir: Option<std::path::PathBuf>,
}

/// Longest a scheduled model request may run.
const GENERATION_TIMEOUT: Duration = Duration::from_secs(600);

/// A generation task's settings: plain text is the prompt; JSON may also name a
/// provider and model, otherwise the first enabled model is used.
fn generation_config(config: &str) -> (String, Option<String>, Option<String>) {
    match serde_json::from_str::<serde_json::Value>(config) {
        Ok(value) if value["prompt"].is_string() => {
            let text = |key: &str| value[key].as_str().filter(|text| !text.trim().is_empty()).map(str::to_owned);
            (value["prompt"].as_str().unwrap_or_default().trim().to_owned(), text("provider"), text("model"))
        }
        _ => (config.trim().to_owned(), None, None),
    }
}

/// A system task's command: plain text, or JSON with a `command` field.
fn system_command(config: &str) -> String {
    serde_json::from_str::<serde_json::Value>(config).ok()
        .and_then(|value| value["command"].as_str().map(str::to_owned))
        .unwrap_or_else(|| config.to_owned())
}

fn select_model(
    providers: &[crate::native_engine::provider_manager::Provider],
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(String, String)> {
    providers
        .iter()
        .filter(|candidate| candidate.enabled && provider.is_none_or(|id| candidate.id == id))
        .find_map(|candidate| {
            candidate.models.iter()
                .find(|option| option.enabled && model.is_none_or(|id| option.id == id))
                .map(|option| (candidate.id.clone(), option.id.clone()))
        })
        .ok_or_else(|| anyhow::anyhow!("No enabled model is available for this task; check the model settings"))
}

struct TaskState {
    run_id: String,
    started_at: std::time::Instant,
}

impl ScheduledTaskScheduler {
    pub fn new(db_manager: Arc<DbManager>) -> Self {
        Self {
            db_manager,
            running_tasks: Arc::new(Mutex::new(HashMap::new())),
            should_stop: Arc::new(Mutex::new(false)),
            data_dir: None,
        }
    }

    pub fn with_data_dir(mut self, data_dir: std::path::PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    pub async fn init(&self) -> Result<()> {
        info!("[Scheduler] Initializing task scheduler");
        let tasks = self.load_enabled_tasks().await?;
        info!("[Scheduler] Loaded {} enabled tasks from database", tasks.len());
        Ok(())
    }

    pub async fn add_task(&self, task: &ScheduledTaskRow) -> Result<()> {
        info!("[Scheduler] Adding task: {} ({})", task.name, task.id);
        Ok(())
    }

    pub async fn remove_task(&self, task_id: &str) -> Result<()> {
        info!("[Scheduler] Removing task: {}", task_id);
        let mut running = self.running_tasks.lock().await;
        running.remove(task_id);
        Ok(())
    }

    pub async fn execute_task(&self, task: &ScheduledTaskRow) -> TaskExecutionResult {
        let task_id = task.id.clone();
        let task_name = task.name.clone();
        let start_time = std::time::Instant::now();
        let run_id = uuid::Uuid::new_v4().to_string();

        {
            let mut running = self.running_tasks.lock().await;
            running.insert(task_id.clone(), TaskState {
                run_id: run_id.clone(),
                started_at: start_time,
            });
        }

        info!("[Scheduler] Executing task '{}' (run_id: {})", task_name, run_id);

        let result = self.do_execute_task(task, &run_id, start_time).await;

        {
            let mut running = self.running_tasks.lock().await;
            running.remove(&task_id);
        }

        result
    }

    async fn do_execute_task(
        &self,
        task: &ScheduledTaskRow,
        run_id: &str,
        start_time: std::time::Instant,
    ) -> TaskExecutionResult {
        let now = chrono::Utc::now().to_rfc3339();

        let db = self.db_manager.clone();
        let run_id_clone = run_id.to_string();
        let task_id_clone = task.id.clone();
        let insert_result = tokio::task::spawn_blocking(move || {
            db.with_conn(|conn| {
                crate::db::task_repo::insert_task_run(conn, &run_id_clone, &task_id_clone, &now)
            })
        }).await;

        if let Err(e) = insert_result {
            error!("[Scheduler] Failed to insert task run record: {}", e);
        }

        let execution_result = match task.task_type.as_str() {
            "prompt" => self.execute_prompt_task(task).await,
            "webhook" => self.execute_webhook_task(task).await,
            "system" => self.execute_system_task(task).await,
            "report" => self.execute_report_task(task).await,
            _ => TaskExecutionResult {
                success: false,
                output: None,
                error: Some(format!("Unknown task type: {}", task.task_type)),
                duration_ms: 0,
            },
        };

        let duration_ms = start_time.elapsed().as_millis() as u64;
        let finished_at = chrono::Utc::now().to_rfc3339();

        let status = if execution_result.success { "success" } else { "failed" };
        let output = execution_result.output.clone();
        let output2 = output.clone();
        let error_msg = execution_result.error.clone();

        let db2 = self.db_manager.clone();
        let run_id_clone2 = run_id.to_string();
        let finished_at_clone = finished_at.clone();
        let duration_clone = duration_ms as i64;

        let _ = tokio::task::spawn_blocking(move || {
            db2.with_conn(|conn| {
                crate::db::task_repo::update_task_run_status(
                    conn,
                    &run_id_clone2,
                    status,
                    output.as_deref(),
                    error_msg.as_deref(),
                    &finished_at_clone,
                    duration_clone,
                )
            })
        }).await;

        let next_run_at = crate::task::cron::calc_next_run_local(&task.cron_expression, &chrono::Utc::now())
            .ok();

        let db3 = self.db_manager.clone();
        let task_id_clone3 = task.id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            db3.with_conn(|conn| {
                crate::db::task_repo::update_task_run_result(
                    conn,
                    &task_id_clone3,
                    &finished_at,
                    status,
                    output2.as_deref(),
                    next_run_at.as_deref(),
                )
            })
        }).await;

        TaskExecutionResult {
            duration_ms,
            ..execution_result
        }
    }

    async fn execute_prompt_task(&self, task: &ScheduledTaskRow) -> TaskExecutionResult {
        self.generate(task, "Complete the scheduled user prompt. This scheduled run generates text only and cannot execute tools.").await
    }

    /// One model request with the task's prompt; the answer becomes the run's output.
    async fn generate(&self, task: &ScheduledTaskRow, system: &str) -> TaskExecutionResult {
        let start = std::time::Instant::now();
        let (output, error) = match self.request_generation(&task.task_config, system).await {
            Ok(output) => (Some(output), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        TaskExecutionResult { success: error.is_none(), output, error, duration_ms: start.elapsed().as_millis() as u64 }
    }

    async fn request_generation(&self, config: &str, system: &str) -> Result<String> {
        let (prompt, provider, model) = generation_config(config);
        anyhow::ensure!(!prompt.is_empty(), "This task has no prompt");
        let root = self.data_dir.clone().ok_or_else(|| anyhow::anyhow!("Model settings are unavailable"))?;
        let paths = crate::runtime::RuntimePaths::new(root)?;
        let providers = paths.providers()?;
        let (provider, model) = select_model(&providers, provider.as_deref(), model.as_deref())?;
        let generator = crate::services::generation::Generator::new(&paths, providers, &provider, &model)?;
        tokio::time::timeout(GENERATION_TIMEOUT, generator.generate(system, &prompt))
            .await
            .map_err(|_| anyhow::anyhow!("The model did not answer within {} seconds", GENERATION_TIMEOUT.as_secs()))?
    }

    async fn execute_webhook_task(&self, task: &ScheduledTaskRow) -> TaskExecutionResult {
        let start = std::time::Instant::now();

        match serde_json::from_str::<serde_json::Value>(&task.task_config) {
            Ok(config) => {
                let url = config.get("url").and_then(|v| v.as_str()).unwrap_or("");
                if url.is_empty() {
                    return TaskExecutionResult {
                        success: false,
                        output: None,
                        error: Some("Webhook URL not specified".to_string()),
                        duration_ms: start.elapsed().as_millis() as u64,
                    };
                }

                match reqwest::Client::new()
                    .post(url)
                    .json(&config.get("payload").unwrap_or(&serde_json::Value::Null))
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        TaskExecutionResult {
                            success: status.is_success(),
                            output: Some(format!("Status: {}, Body: {}", status, body)),
                            error: if status.is_success() { None } else { Some(format!("HTTP {}", status)) },
                            duration_ms: start.elapsed().as_millis() as u64,
                        }
                    }
                    Err(e) => TaskExecutionResult {
                        success: false,
                        output: None,
                        error: Some(format!("Request failed: {}", e)),
                        duration_ms: start.elapsed().as_millis() as u64,
                    },
                }
            }
            Err(e) => TaskExecutionResult {
                success: false,
                output: None,
                error: Some(format!("Invalid webhook config JSON: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
            },
        }
    }

    async fn execute_system_task(&self, task: &ScheduledTaskRow) -> TaskExecutionResult {
        let start = std::time::Instant::now();

        let command = system_command(&task.task_config);
        // Validate command before execution
        if crate::tools::is_dangerous_command(&command) {
            return TaskExecutionResult {
                success: false,
                output: None,
                error: Some("Command blocked by security filter: dangerous command detected".to_string()),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }

        #[cfg(target_os = "windows")]
        let output = tokio::process::Command::new("cmd")
            .args(["/C", &command])
            .output()
            .await;

        #[cfg(not(target_os = "windows"))]
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .await;

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                TaskExecutionResult {
                    success: out.status.success(),
                    output: if !stdout.is_empty() { Some(stdout) } else { None },
                    error: if !out.status.success() && !stderr.is_empty() { Some(stderr) } else { None },
                    duration_ms: start.elapsed().as_millis() as u64,
                }
            }
            Err(e) => TaskExecutionResult {
                success: false,
                output: None,
                error: Some(format!("Command execution failed: {}", e)),
                duration_ms: start.elapsed().as_millis() as u64,
            },
        }
    }

    async fn execute_report_task(&self, task: &ScheduledTaskRow) -> TaskExecutionResult {
        self.generate(task, "Write a concise, well-structured report for this scheduled request. Use only what the request provides; this run cannot execute tools.").await
    }

    pub async fn start(&self) {
        info!("[Scheduler] Starting task scheduler");
        *self.should_stop.lock().await = false;

        let db = self.db_manager.clone();
        let running = self.running_tasks.clone();
        let should_stop = self.should_stop.clone();
        let data_dir = self.data_dir.clone();

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(30));

            loop {
                ticker.tick().await;

                if *should_stop.lock().await {
                    info!("[Scheduler] Scheduler stopped");
                    break;
                }

                let now = chrono::Utc::now().to_rfc3339();
                let due_tasks = match db.with_conn(|conn| {
                    crate::db::task_repo::get_due_tasks(conn, &now)
                }) {
                    Ok(Ok(tasks)) => tasks,
                    Ok(Err(e)) | Err(e) => {
                        error!("[Scheduler] Failed to get due tasks: {}", e);
                        continue;
                    }
                };

                for task in due_tasks {
                    let task_id = task.id.clone();
                    if running.lock().await.contains_key(&task_id) {
                        warn!("[Scheduler] Task {} is already running, skipping", task_id);
                        continue;
                    }

                    info!("[Scheduler] Due task '{}' (cron: {})", task.name, task.cron_expression);

                    let db_clone = db.clone();
                    let running_clone = running.clone();
                    let task_clone = task.clone();
                    let data_dir_clone = data_dir.clone();

                    tokio::spawn(async move {
                        let scheduler = ScheduledTaskScheduler {
                            db_manager: db_clone,
                            running_tasks: running_clone,
                            should_stop: Arc::new(Mutex::new(false)),
                            data_dir: data_dir_clone,
                        };
                        let result = scheduler.execute_task(&task_clone).await;
                        info!(
                            "[Scheduler] Task '{}' completed: success={}, duration={}ms",
                            task_clone.name, result.success, result.duration_ms
                        );
                    });
                }
            }
        });
    }

    pub async fn stop(&self) {
        info!("[Scheduler] Stopping task scheduler");
        *self.should_stop.lock().await = true;
    }

    async fn load_enabled_tasks(&self) -> Result<Vec<ScheduledTaskRow>> {
        let tasks = self.db_manager.with_conn(|conn| {
            crate::db::task_repo::get_enabled_tasks(conn)
        })??;
        Ok(tasks)
    }

    pub fn calc_next_run(cron_expr: &str) -> Result<String> {
        let schedule = Schedule::from_str(cron_expr)
            .map_err(|e| anyhow::anyhow!("Invalid cron expression: {}", e))?;

        let now = chrono::Utc::now();
        let next = schedule
            .after(&now)
            .next()
            .ok_or_else(|| anyhow::anyhow!("No next run time found"))?;

        Ok(next.to_rfc3339())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(id: &str, enabled: bool, url: &str, models: &[(&str, bool)]) -> serde_json::Value {
        let models: Vec<_> = models.iter().map(|(id, enabled)| json!({"id":id,"name":id,"enabled":enabled,"max_tokens":4096,"supports_vision":false,"supports_web_search":false})).collect();
        json!({"id":id,"name":id,"base_url":url,"api_key":"fixture","api_format":"openai","enabled":enabled,"models":models})
    }

    #[test]
    fn system_commands_accept_plain_text_or_json() {
        assert_eq!(system_command("echo hi"), "echo hi");
        assert_eq!(system_command("{\"command\":\"echo hi\"}"), "echo hi");
        assert_eq!(system_command("{\"other\":1}"), "{\"other\":1}");
    }

    #[test]
    fn prompt_settings_accept_plain_text_or_json() {
        assert_eq!(generation_config(" check CI "), ("check CI".to_owned(), None, None));
        assert_eq!(generation_config("{\"prompt\":\"x\",\"provider\":\"p\",\"model\":\"m\"}"), ("x".to_owned(), Some("p".to_owned()), Some("m".to_owned())));
        assert_eq!(generation_config("{\"other\":1}").0, "{\"other\":1}", "JSON without a prompt is the prompt text");
    }

    #[test]
    fn the_first_enabled_model_is_used_unless_one_is_named() {
        let providers: Vec<crate::native_engine::provider_manager::Provider> = serde_json::from_value(json!([
            provider("off", false, "http://x", &[("a", true)]),
            provider("p1", true, "http://x", &[("b", false), ("c", true)]),
            provider("p2", true, "http://x", &[("d", true)]),
        ])).unwrap();
        assert_eq!(select_model(&providers, None, None).unwrap(), ("p1".into(), "c".into()));
        assert_eq!(select_model(&providers, Some("p2"), None).unwrap(), ("p2".into(), "d".into()));
        assert_eq!(select_model(&providers, None, Some("d")).unwrap(), ("p2".into(), "d".into()));
        assert!(select_model(&providers, Some("off"), None).is_err());
        assert!(select_model(&[], None, None).is_err());
    }

    /// A due task runs for real, records its output and moves to its next run.
    #[tokio::test]
    async fn due_prompt_tasks_call_the_model_and_failures_are_recorded() {
        use axum::{routing::post, Json, Router};
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/chat/completions", post(|Json(body): Json<serde_json::Value>| async move {
            let prompt = body["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
            assert!(prompt.contains("summarize the day"), "{prompt}");
            Json(json!({"id":"r","object":"chat.completion","created":0,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"REAL MODEL ANSWER"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        std::fs::write(dir.path().join("providers.json"), json!([provider("p", true, &format!("{url}/v1"), &[("m", true)])]).to_string()).unwrap();
        let paths = crate::runtime::RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let db = paths.open_database().unwrap();
        let past = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let insert = |id: &str, kind: &str, config: &str| {
            db.with_conn(|conn| crate::db::task_repo::insert_scheduled_task(conn, id, id, None, "0 9 * * *", kind, config, None, true, Some(&past), &past, &past)).unwrap().unwrap();
        };
        insert("good", "prompt", "summarize the day");
        insert("report", "report", "summarize the day");
        insert("empty", "prompt", "   ");
        let scheduler = ScheduledTaskScheduler::new(db.clone()).with_data_dir(dir.path().to_path_buf());
        for (id, expected) in [("good", "success"), ("report", "success"), ("empty", "failed")] {
            let task = db.with_conn(|conn| crate::db::task_repo::get_scheduled_task(conn, id)).unwrap().unwrap().unwrap();
            let result = scheduler.execute_task(&task).await;
            let task = db.with_conn(|conn| crate::db::task_repo::get_scheduled_task(conn, id)).unwrap().unwrap().unwrap();
            assert_eq!(task.last_run_status.as_deref(), Some(expected), "{result:?}");
            assert!(task.next_run_at.unwrap() > chrono::Utc::now().to_rfc3339(), "the task moved to its next run");
            if expected == "success" {
                assert_eq!(result.output.as_deref(), Some("REAL MODEL ANSWER"));
                assert_eq!(task.last_run_output.as_deref(), Some("REAL MODEL ANSWER"));
            } else {
                assert!(result.error.unwrap().contains("no prompt"));
            }
        }
        let due = db.with_conn(|conn| crate::db::task_repo::get_due_tasks(conn, &chrono::Utc::now().to_rfc3339())).unwrap().unwrap();
        assert!(due.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn anthropic_generation_asks_for_answer_text_without_reasoning() {
        use axum::{routing::post, Json, Router};
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/v1/messages", post(|Json(body): Json<serde_json::Value>| async move {
            assert_eq!(body["thinking"]["type"], "disabled", "{body}");
            Json(json!({"id":"m","type":"message","role":"assistant","model":"m","stop_reason":"end_turn","stop_sequence":null,"content":[{"type":"thinking","thinking":"HIDDEN","signature":"s"},{"type":"text","text":"ANSWER"}],"usage":{"input_tokens":1,"output_tokens":1}}))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut anthropic = provider("p", true, &url, &[("m", true)]);
        anthropic["api_format"] = json!("anthropic");
        std::fs::write(dir.path().join("providers.json"), json!([anthropic]).to_string()).unwrap();
        let paths = crate::runtime::RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let scheduler = ScheduledTaskScheduler::new(paths.open_database().unwrap()).with_data_dir(dir.path().to_path_buf());
        assert_eq!(scheduler.request_generation("hello", "system").await.unwrap(), "ANSWER");
        server.abort();
    }

    #[tokio::test]
    async fn a_task_without_model_settings_fails_instead_of_claiming_success() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(dir.path().to_path_buf()).unwrap();
        let scheduler = ScheduledTaskScheduler::new(paths.open_database().unwrap()).with_data_dir(dir.path().to_path_buf());
        let error = scheduler.request_generation("hello", "system").await.unwrap_err().to_string();
        assert!(error.contains("No enabled model"), "{error}");
    }
}
