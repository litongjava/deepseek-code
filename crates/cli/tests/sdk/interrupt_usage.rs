use super::*;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Keep the first HTTP body open after reporting usage. Cancellation must finish
// the CLI turn without relying on provider EOF or a final request snapshot.
struct StreamingServer {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StreamingServer {
    fn start(anthropic: bool, known: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let (stopped, seen) = (stop.clone(), requests.clone());
        let thread = std::thread::spawn(move || {
            let mut held = Vec::<TcpStream>::new();
            'serve: while !stopped.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        continue 'serve;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
                let mut frames = Vec::new();
                if anthropic {
                    let mut message = json!({"id":"partial", "model":"fixture", "role":"assistant", "content":[]});
                    if first && known {
                        message["usage"] = json!({"input_tokens":11,"output_tokens":0});
                    }
                    frames.push(json!({"type":"message_start","message":message}));
                    frames.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                    frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":if first {"partial"} else {"next reply"}}}));
                    if first && known {
                        for output in [1, 2, 2] {
                            frames.push(json!({"type":"message_delta","delta":{},"usage":{"output_tokens":output}}));
                        }
                        frames.push(json!({"type":"message_delta","delta":{},"usage":{"input_tokens":null,"output_tokens":null}}));
                    }
                    if !first {
                        frames.push(json!({"type":"content_block_stop","index":0}));
                        frames.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":7,"output_tokens":1}}));
                        frames.push(json!({"type":"message_stop"}));
                    }
                } else {
                    frames.push(json!({"choices":[{"index":0,"delta":{"role":"assistant","content":if first {"partial"} else {"next reply"}},"finish_reason":null}]}));
                    if first && known {
                        for output in [1, 2, 2] {
                            frames.push(json!({"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":output,"total_tokens":11+output}}));
                        }
                        frames.push(json!({"choices":[],"usage":{"prompt_tokens":null,"completion_tokens":null,"total_tokens":null}}));
                    }
                    if !first {
                        frames.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}}));
                    }
                }
                let mut body = String::new();
                for frame in frames {
                    if anthropic {
                        body.push_str(&format!("event: {}\n", frame["type"].as_str().unwrap()));
                    }
                    body.push_str(&format!("data: {frame}\n\n"));
                }
                if !first && !anthropic {
                    body.push_str("data: [DONE]\n\n");
                }
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
                socket.write_all(body.as_bytes()).unwrap();
                if first {
                    held.push(socket);
                }
            }
        });
        Self {
            url,
            stop,
            requests,
            thread: Some(thread),
        }
    }
}

impl Drop for StreamingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

async fn next(client: &mut Client, events: &mut Vec<Value>) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(15), client.output.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("SDK exited before finalization");
    let event: Value = serde_json::from_str(&line).unwrap();
    events.push(event.clone());
    event
}

#[tokio::test]
async fn interrupt_preserves_reported_usage_and_finalizes_each_turn_once() {
    for anthropic in [false, true] {
        for known in [false, true] {
            let server = StreamingServer::start(anthropic, known);
            let root = tempfile::tempdir().unwrap();
            let format = if anthropic { "anthropic" } else { "openai" };
            let mut client = Client::launch(root.path(), Some((&server.url, format)), &[]);
            let mut events = Vec::new();
            client.send(user("Stream until interrupted")).await;
            loop {
                let event = next(&mut client, &mut events).await;
                assert_ne!(event["type"], "result", "{event}");
                if known {
                    let key = if anthropic {
                        "output_tokens"
                    } else {
                        "completion_tokens"
                    };
                    if event["type"] == "usage" && event["usage"][key] == 2 {
                        break;
                    }
                } else if event["type"] == "text" {
                    break;
                }
            }
            // A controlled interval belongs to the product's execution duration.
            tokio::time::sleep(Duration::from_millis(120)).await;
            client.send(json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}})).await;
            let result = loop {
                let event = next(&mut client, &mut events).await;
                if event["type"] == "result" {
                    break event;
                }
            };
            assert_eq!(
                result["turn"]["status"], "cancelled",
                "{format} {known}: {result}"
            );
            assert_eq!(result["is_error"], true);
            assert_eq!(result["subtype"], "error_during_execution");
            assert!(result["duration_ms"].as_u64().unwrap() >= 120, "{result}");
            assert_eq!(result["num_turns"], 1);
            for key in [
                "total_cost_usd",
                "permission_denials",
                "structured_output",
                "stop_reason",
            ] {
                assert!(result.get(key).is_some(), "missing {key}: {result}");
            }
            if known {
                let (input, output) = if anthropic {
                    ("input_tokens", "output_tokens")
                } else {
                    ("prompt_tokens", "completion_tokens")
                };
                assert_eq!(result["usage"][input], 11, "{result}");
                assert_eq!(result["usage"][output], 2, "{result}");
                if !anthropic {
                    assert_eq!(result["usage"]["total_tokens"], 13);
                }
            } else {
                assert_eq!(result["usage"], json!({}));
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e["type"] == "control_response"
                        && e["response"]["request_id"] == "stop")
                    .count(),
                1
            );
            let first_turn = result["turn"]["turnId"].clone();
            client.send(user("Continue normally")).await;
            let followup = loop {
                let event = next(&mut client, &mut events).await;
                if event["type"] == "result" {
                    break event;
                }
            };
            assert_eq!(followup["turn"]["status"], "completed", "{followup}");
            assert_eq!(followup["is_error"], false);
            assert_eq!(followup["result"], "next reply");
            let key = if anthropic {
                "input_tokens"
            } else {
                "prompt_tokens"
            };
            assert_eq!(followup["usage"][key], 7);
            let (status, extra) = client.finish().await;
            assert!(
                !status.success(),
                "an interrupted SDK session exits nonzero"
            );
            assert!(
                extra.is_empty(),
                "duplicate SDK terminal results: {extra:?}"
            );
            assert_eq!(server.requests.load(Ordering::SeqCst), 2);
            for result in [&result, &followup] {
                assert_eq!(
                    events
                        .iter()
                        .filter(|e| e["type"] == "turn_timing"
                            && e["turn"]["turnId"] == result["turn"]["turnId"]
                            && e["turn"]["status"] != "running")
                        .count(),
                    1
                );
            }
            let paths =
                deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
            let db = paths.open_database().unwrap();
            let rows = db.with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT t.id,t.status,count(m.message_id) FROM execution_turns t LEFT JOIN message_turns m ON m.turn_id=t.id GROUP BY t.id")?;
                let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u64>(2)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok::<_, anyhow::Error>(rows)
            });
            let rows = rows.unwrap().unwrap();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|r| r.2 > 0));
            assert_eq!(
                rows.iter()
                    .find(|r| r.0 == first_turn.as_str().unwrap())
                    .unwrap()
                    .1,
                "cancelled"
            );
            let log = std::fs::read_to_string(
                paths
                    .data_dir
                    .join("logs/request-attempts")
                    .join(format!("{}.jsonl", result["session_id"].as_str().unwrap())),
            )
            .unwrap();
            let terminal: Vec<Value> = log
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .filter(|e| e["type"] == "request_turn_end")
                .collect();
            assert_eq!(terminal.len(), 2);
            assert_eq!(
                terminal
                    .iter()
                    .filter(|e| e["turnId"] == first_turn && e["outcome"] == "cancelled")
                    .count(),
                1
            );
        }
    }
}
