//! Image requests use the selected gateway; OAuth and upstream routing stay in the gateway.
use crate::native_engine::provider_manager::ResolvedProvider;
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{io::Write, path::Path, time::Duration};

const MAX_IMAGE: usize = 32 * 1024 * 1024;
const MAX_RESPONSE: usize = MAX_IMAGE.div_ceil(3) * 4 + 1024 * 1024;

pub fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "ImageGenerate".into(),
        description: "Generate or edit an image through the selected provider's image API. The gateway handles image-model routing and authentication. Use a new .png file_path inside the workspace; existing files are never overwritten. For edits, supply up to five referenced_image_paths in order. The result contains the saved path and the actual generated image. This can take several minutes and consumes image-service usage. Relay endpoint errors; do not pretend that an image was created.".into(),
        input_schema: json!({"type":"object","properties":{
            "prompt":{"type":"string","minLength":1},
            "file_path":{"type":"string","description":"New PNG output file; relative to the workspace or an allowed absolute path."},
            "referenced_image_paths":{"type":"array","items":{"type":"string"},"maxItems":5},
            "transparent_background":{"type":"boolean"},
            "size":{"type":"string","description":"Optional size supported by the image service, for example 1024x1024."},
            "quality":{"type":"string","enum":["auto","low","medium","high"]}
        },"required":["prompt","file_path"],"additionalProperties":false}),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    prompt: String,
    file_path: String,
    #[serde(default)]
    referenced_image_paths: Vec<String>,
    #[serde(default)]
    transparent_background: bool,
    size: Option<String>,
    quality: Option<String>,
}

fn mime(bytes: &[u8]) -> Result<&'static str> {
    if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.len() >= 4 && bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok("image/jpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        anyhow::bail!("Image data must be PNG, JPEG or WebP")
    }
}

pub async fn generate(input: Value, cwd: &str, provider: &ResolvedProvider) -> Result<Value> {
    let input: Input = serde_json::from_value(input).context("Invalid ImageGenerate arguments")?;
    ensure!(
        !input.prompt.trim().is_empty() && input.prompt.len() <= 32_000,
        "Image prompt must contain 1 to 32000 UTF-8 bytes"
    );
    ensure!(
        input.referenced_image_paths.len() <= 5,
        "At most five reference images are supported"
    );
    let output = super::resolve_path(&input.file_path, cwd)?;
    let output = Path::new(&output);
    ensure!(
        output
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png")),
        "Image output must use a .png extension"
    );
    ensure!(
        !output.exists(),
        "Image output already exists; choose a new file path"
    );
    let mut base = reqwest::Url::parse(provider.provider.base_url.trim_end_matches('/'))
        .context("Image API needs a configured HTTP base URL")?;
    ensure!(
        matches!(base.scheme(), "http" | "https")
            && base.username().is_empty()
            && base.password().is_none()
            && base.query().is_none()
            && base.fragment().is_none(),
        "Invalid image API base URL"
    );
    let mut images = Vec::new();
    for source in &input.referenced_image_paths {
        let path = super::resolve_read_path(source, cwd)?;
        let metadata = std::fs::metadata(&path).context("Cannot read reference image")?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_IMAGE as u64,
            "Reference image exceeds 32 MiB or is not a file"
        );
        let bytes = std::fs::read(&path).context("Cannot read reference image")?;
        ensure!(bytes.len() <= MAX_IMAGE, "Reference image exceeds 32 MiB");
        images.push(
            json!({"image_url":format!("data:{};base64,{}",mime(&bytes)?,STANDARD.encode(&bytes))}),
        );
    }
    let endpoint = if images.is_empty() {
        "/images/generations"
    } else {
        "/images/edits"
    };
    let root = base.path().trim_end_matches('/');
    let root = root.strip_suffix("/responses").unwrap_or(root);
    base.set_path(&format!("{root}{endpoint}"));
    let mut body = json!({"model":"gpt-image-2","prompt":input.prompt,"n":1,
        "background":if input.transparent_background {"transparent"} else {"opaque"}});
    if !images.is_empty() {
        body["images"] = json!(images);
    }
    if let Some(size) = input.size {
        body["size"] = json!(size);
    }
    if let Some(quality) = input.quality {
        ensure!(
            ["auto", "low", "medium", "high"].contains(&quality.as_str()),
            "Invalid image quality"
        );
        body["quality"] = json!(quality);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut request = client
        .post(base)
        .header("accept", "application/json")
        .header("x-request-id", uuid::Uuid::new_v4().to_string())
        .json(&body);
    if !provider.provider.api_key.is_empty() {
        request = request.bearer_auth(&provider.provider.api_key);
    }
    // Do not retry paid generation automatically or follow redirects with credentials/images.
    let mut response = request
        .send()
        .await
        .context("Image gateway request failed")?;
    ensure!(
        response.status().is_success(),
        "Image gateway returned HTTP {}; verify its image model and routing",
        response.status().as_u16()
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_RESPONSE as u64),
        "Image response exceeds the size limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Image gateway response interrupted")?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_RESPONSE,
            "Image response exceeds the size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    let result: Value =
        serde_json::from_slice(&bytes).context("Image gateway returned invalid JSON")?;
    let data = result["data"]
        .as_array()
        .context("Image gateway returned no image data")?;
    ensure!(
        data.len() == 1,
        "Image gateway must return one image for this request"
    );
    let encoded = data[0]["b64_json"]
        .as_str()
        .context("Image gateway must return b64_json image data")?;
    let image = STANDARD
        .decode(encoded)
        .context("Image gateway returned invalid Base64")?;
    ensure!(
        image.len() <= MAX_IMAGE && mime(&image)? == "image/png",
        "Image gateway must return PNG data within 32 MiB"
    );
    let parent = output
        .parent()
        .context("Image output has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged
        .write_all(&image)
        .context("Cannot write generated image")?;
    staged
        .persist_noclobber(output)
        .map_err(|error| error.error)
        .context("Cannot save generated image; no existing file was overwritten")?;
    let text = format!("Image saved: {} ({} bytes)", output.display(), image.len());
    Ok(
        json!({"success":true,"content":text,"file_path":output,"bytes_written":image.len(),
        "usage":result["usage"],"generation_id":data[0]["generation_id"],
        "image":{"media_type":"image/png","data":encoded}}),
    )
}

#[cfg(test)]
mod tests;
