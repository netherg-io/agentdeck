//! `agentdeck imagegen`: one raster image per call through Vercel AI Gateway
//! or OpenRouter (plugin `imagegen`). Image-only gateway models use the
//! OpenAI Images API (`/images/generations`, `/images/edits`); multimodal
//! LLMs (Gemini/GPT image) use chat completions with `modalities`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentdeck_core::imagegen::{self as ig, SIZES};
use agentdeck_core::settings::PROVIDER_OPENROUTER;
use agentdeck_core::{Paths, Registry, plugins};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clap::Args;
use serde_json::{Value, json};

const VERCEL: &str = "https://ai-gateway.vercel.sh/v1";
const OPENROUTER: &str = "https://openrouter.ai/api/v1";

#[derive(Args)]
pub struct ImagegenArgs {
    /// Full image spec; `-` reads it from stdin.
    prompt: String,
    /// Output file; never overwritten without --force. The extension follows
    /// the returned format.
    #[arg(short, long)]
    out: PathBuf,
    /// Image to edit (Image 1); its shape is kept unless --aspect is given.
    #[arg(short, long)]
    edit: Option<PathBuf>,
    /// Reference image for style/composition/subject (repeatable).
    #[arg(short, long = "ref")]
    refs: Vec<PathBuf>,
    /// Aspect ratio (default 1:1).
    #[arg(short, long, value_parser = SIZES.map(|s| s.0))]
    aspect: Option<String>,
    /// Model override, e.g. google/gemini-3.1-flash-image.
    #[arg(long)]
    model: Option<String>,
    /// Gateway override: vercel or openrouter.
    #[arg(long, value_parser = ["vercel", "openrouter"])]
    provider: Option<String>,
    /// Transparent PNG (Vercel transparent_model, default gpt-image-1-mini).
    /// With --edit it cuts the subject out of an existing image.
    #[arg(short, long)]
    transparent: bool,
    /// Quality for models that take one (gpt-image: low, medium, high).
    #[arg(short, long, value_parser = ["low", "medium", "high"])]
    quality: Option<String>,
    #[arg(long)]
    force: bool,
}

/// Prints one JSON line (result or `{"error"}`) and returns the exit code:
/// 0 ok, 1 gateway/model failure, 2 bad input.
pub async fn run(paths: &Paths, args: ImagegenArgs) -> i32 {
    match generate(paths, args).await {
        Ok(v) => {
            println!("{v}");
            0
        }
        Err((code, e)) => {
            println!("{}", json!({ "error": format!("{e:#}") }));
            code
        }
    }
}

type Failure = (i32, anyhow::Error);

fn input(e: impl Into<anyhow::Error>) -> Failure {
    (2, e.into())
}

fn remote(e: impl Into<anyhow::Error>) -> Failure {
    (1, e.into())
}

async fn generate(paths: &Paths, mut a: ImagegenArgs) -> Result<Value, Failure> {
    let reg = Registry::load(paths).map_err(input)?;
    if !plugins::plugin_enabled(&reg, plugins::PLUGIN_IMAGEGEN) {
        return Err(input(anyhow::anyhow!(
            "imagegen plugin is disabled (agentdeck plugin enable imagegen)"
        )));
    }
    if a.prompt == "-" {
        a.prompt = std::io::read_to_string(std::io::stdin()).map_err(input)?;
    }
    let prompt = a.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(input(anyhow::anyhow!("empty prompt")));
    }
    if a.out.exists() && !a.force {
        return Err(input(anyhow::anyhow!(
            "{} exists; pick a versioned name or pass --force",
            a.out.display()
        )));
    }
    let mut images = Vec::new();
    for p in a.edit.iter().chain(&a.refs) {
        let bytes = std::fs::read(p)
            .map_err(|e| input(anyhow::anyhow!("cannot read {}: {e}", p.display())))?;
        let (_, mime) = ig::sniff(&bytes)
            .ok_or_else(|| input(anyhow::anyhow!("{}: not a PNG, JPEG or WebP", p.display())))?;
        images.push((bytes, mime));
    }
    let ratio = a
        .aspect
        .clone()
        .or_else(|| {
            let (bytes, _) = images.first().filter(|_| a.edit.is_some())?;
            ig::dimensions(bytes).map(|(w, h)| ig::nearest_ratio(w, h).to_string())
        })
        .unwrap_or_else(|| "1:1".into());
    let settings = ig::read(&reg);
    if a.transparent && a.provider.as_deref() == Some(PROVIDER_OPENROUTER) {
        return Err(input(anyhow::anyhow!(
            "--transparent needs Vercel: no OpenRouter image model returns alpha"
        )));
    }
    let provider = if a.transparent {
        agentdeck_core::settings::PROVIDER_VERCEL.to_string()
    } else {
        a.provider.clone().unwrap_or(settings.provider.clone())
    };
    let model = a.model.clone().unwrap_or_else(|| {
        if a.transparent {
            settings.transparent_model.clone()
        } else if provider == settings.provider {
            settings.model.clone()
        } else {
            ig::default_model(&provider).to_string()
        }
    });
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let key = ig::gateway_key(&reg, &home, &provider).ok_or_else(|| {
        input(anyhow::anyhow!(
            "no {provider} key: set {} or write ~/.config/model-route/{provider}.key",
            if provider == PROVIDER_OPENROUTER {
                "OPENROUTER_API_KEY"
            } else {
                "AI_GATEWAY_API_KEY"
            }
        ))
    })?;
    let full_prompt = labeled_prompt(&prompt, a.edit.is_some(), a.refs.len());
    let data_urls: Vec<String> = images
        .iter()
        .map(|(b, mime)| format!("data:{mime};base64,{}", B64.encode(b)))
        .collect();

    let started = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(remote)?;
    let image_api = provider != PROVIDER_OPENROUTER && is_image_model(&client, &model).await?;
    if a.transparent && !image_api {
        return Err(input(anyhow::anyhow!(
            "--transparent needs an image-only model with alpha output; {model} is not one"
        )));
    }
    let (url, body) = if image_api {
        let (w, h) = ig::size_for(&model, &ratio).unwrap_or((1024, 1024));
        let mut body = json!({
            "model": model, "prompt": full_prompt, "n": 1,
            "size": format!("{w}x{h}"), "response_format": "b64_json",
        });
        if a.transparent {
            body["background"] = json!("transparent");
            body["output_format"] = json!("png");
        }
        if let Some(q) = &a.quality {
            body["quality"] = json!(q);
        }
        if model.starts_with("bytedance/") {
            // Seedream stamps "AI generated" on edits unless told not to.
            body["providerOptions"] = json!({ "bytedance": { "watermark": false } });
        }
        if data_urls.is_empty() {
            (format!("{VERCEL}/images/generations"), body)
        } else {
            body["images"] = data_urls
                .iter()
                .map(|u| json!({ "image_url": u }))
                .collect();
            (format!("{VERCEL}/images/edits"), body)
        }
    } else {
        let mut content = vec![json!({ "type": "text", "text": full_prompt })];
        content.extend(
            data_urls
                .iter()
                .map(|u| json!({ "type": "image_url", "image_url": { "url": u } })),
        );
        let base = if provider == PROVIDER_OPENROUTER {
            OPENROUTER
        } else {
            VERCEL
        };
        (
            format!("{base}/chat/completions"),
            json!({
                "model": model,
                "modalities": ["image", "text"],
                "messages": [{ "role": "user", "content": content }],
                "image_config": { "aspect_ratio": ratio },
            }),
        )
    };
    let resp = post(&client, &url, &key, &body).await?;
    let bytes = extract_image(&client, &resp).await?;
    let (ext, _) = ig::sniff(&bytes)
        .ok_or_else(|| remote(anyhow::anyhow!("gateway returned an unknown image format")))?;
    // shortcut: checks for an alpha channel, not that any pixel is transparent; decode if a model starts sending opaque RGBA.
    if a.transparent && !ig::has_alpha(&bytes) {
        return Err(remote(anyhow::anyhow!(
            "{model} returned an opaque {ext}: it ignores background=transparent; use the default transparent_model"
        )));
    }
    let out = with_format(&a.out, ext);
    if out != a.out && out.exists() && !a.force {
        return Err(input(anyhow::anyhow!(
            "{} exists; pick a versioned name or pass --force",
            out.display()
        )));
    }
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(input)?;
    }
    std::fs::write(&out, &bytes).map_err(input)?;
    let (w, h) = ig::dimensions(&bytes).unzip();
    let path = std::fs::canonicalize(&out).unwrap_or(out);
    Ok(json!({
        "path": path, "width": w, "height": h, "provider": provider, "model": model,
        "seconds": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        "cost": cost(&resp),
    }))
}

fn labeled_prompt(prompt: &str, edit: bool, refs: usize) -> String {
    let mut lines = Vec::new();
    if edit {
        lines.push(
            "Image 1 is the edit target: change only what the request says and keep everything else unchanged."
                .to_string(),
        );
    }
    let first = if edit { 2 } else { 1 };
    for i in first..first + refs {
        lines.push(format!(
            "Image {i} is a reference for style, composition or subject only."
        ));
    }
    lines.push(prompt.to_string());
    lines.join("\n")
}

/// Vercel lists image-only models with `type: "image"`; they reject chat
/// completions, multimodal LLMs reject the Images API.
async fn is_image_model(client: &reqwest::Client, model: &str) -> Result<bool, Failure> {
    let v: Value = client
        .get(format!("{VERCEL}/models"))
        .send()
        .await
        .map_err(remote)?
        .json()
        .await
        .map_err(remote)?;
    let entry = v["data"]
        .as_array()
        .and_then(|d| d.iter().find(|m| m["id"] == model))
        .ok_or_else(|| input(anyhow::anyhow!("model {model} is not on Vercel AI Gateway")))?;
    Ok(entry["type"] == "image")
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    body: &Value,
) -> Result<Value, Failure> {
    for attempt in 0..2 {
        let res = client
            .post(url)
            .bearer_auth(key)
            .json(body)
            .send()
            .await
            .map_err(|e| remote(anyhow::anyhow!("request failed: {e}")))?;
        let status = res.status();
        let retry = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok());
        let text = res.text().await.map_err(remote)?;
        if status.is_success() {
            return serde_json::from_str(&text)
                .map_err(|e| remote(anyhow::anyhow!("invalid JSON from gateway: {e}")));
        }
        if status.as_u16() == 429 && attempt == 0 && retry.unwrap_or(20) <= 60 {
            tokio::time::sleep(Duration::from_secs(retry.unwrap_or(20))).await;
            continue;
        }
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.pointer("/error/message")?.as_str().map(str::to_string))
            .unwrap_or_else(|| text.chars().take(300).collect());
        return Err(remote(anyhow::anyhow!(
            "HTTP {}: {message}",
            status.as_u16()
        )));
    }
    unreachable!()
}

async fn extract_image(client: &reqwest::Client, v: &Value) -> Result<Vec<u8>, Failure> {
    let src = v
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .map(|b| format!("data:;base64,{b}"))
        .or_else(|| v.pointer("/data/0/url")?.as_str().map(str::to_string))
        .or_else(|| {
            v.pointer("/choices/0/message/images/0/image_url/url")?
                .as_str()
                .map(str::to_string)
        });
    let Some(src) = src else {
        let said = v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(remote(anyhow::anyhow!(
            "no image in the response{}",
            if said.is_empty() {
                String::new()
            } else {
                format!(
                    "; model said: {}",
                    said.chars().take(300).collect::<String>()
                )
            }
        )));
    };
    if let Some(b64) = src
        .strip_prefix("data:")
        .and_then(|s| s.split_once(','))
        .map(|s| s.1)
    {
        return B64
            .decode(b64.trim())
            .map_err(|e| remote(anyhow::anyhow!("bad base64 image: {e}")));
    }
    let res = client.get(&src).send().await.map_err(remote)?;
    Ok(res.bytes().await.map_err(remote)?.to_vec())
}

fn cost(v: &Value) -> Option<f64> {
    ["/providerMetadata/gateway/cost", "/usage/cost"]
        .iter()
        .find_map(|p| {
            let c = v.pointer(p)?;
            c.as_f64().or_else(|| c.as_str()?.parse().ok())
        })
}

/// `out` with the extension of the bytes actually returned.
fn with_format(out: &Path, ext: &str) -> PathBuf {
    let cur = out
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match cur.as_deref() {
        Some(e) if e == ext || (ext == "jpg" && e == "jpeg") => out.to_path_buf(),
        _ => out.with_extension(ext),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_labels_roles_in_order() {
        let p = labeled_prompt("snow", true, 2);
        let lines: Vec<&str> = p.lines().collect();
        assert!(lines[0].starts_with("Image 1 is the edit target"));
        assert!(lines[1].starts_with("Image 2 is a reference"));
        assert!(lines[2].starts_with("Image 3 is a reference"));
        assert_eq!(lines[3], "snow");
        assert_eq!(labeled_prompt("x", false, 0), "x");
    }

    #[test]
    fn output_extension_follows_format() {
        assert_eq!(
            with_format(Path::new("a/b.png"), "jpg"),
            PathBuf::from("a/b.jpg")
        );
        assert_eq!(
            with_format(Path::new("b.jpeg"), "jpg"),
            PathBuf::from("b.jpeg")
        );
        assert_eq!(
            with_format(Path::new("b.PNG"), "png"),
            PathBuf::from("b.PNG")
        );
        assert_eq!(with_format(Path::new("b"), "webp"), PathBuf::from("b.webp"));
    }

    #[test]
    fn cost_reads_both_gateways() {
        assert_eq!(
            cost(&json!({"providerMetadata":{"gateway":{"cost":"0.035"}}})),
            Some(0.035)
        );
        assert_eq!(cost(&json!({"usage":{"cost":0.04}})), Some(0.04));
        assert_eq!(cost(&json!({})), None);
    }
}
