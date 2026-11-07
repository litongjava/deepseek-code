//! Persist safe retry events independently of the tracing subscriber.
use anyhow::Result;
use serde_json::Value;
use std::{io::Write, path::Path};

pub fn record(root: &Path, session: &str, event: &Value) -> Result<()> {
    let session = uuid::Uuid::parse_str(session)?;
    let directory = root.join("logs").join("request-attempts");
    std::fs::create_dir_all(&directory)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(format!("{session}.jsonl")))?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}
