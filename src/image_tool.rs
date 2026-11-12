//! `image_gen`: генерация изображений через `OpenRouter` (google image-модели).
//! Результат — файл в `./.makedo/assets/`. Дефолт — самая дешёвая модель;
//! переопределение: env `MAKEDO_IMAGE_MODEL`.
// ponytail: конфиг-секция [provider.image] — когда появится второй image-бэкенд

use std::path::PathBuf;

use base64::Engine as _;
use makedo_tools::{Tool, ToolCtx, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

const DEFAULT_MODEL: &str = "google/gemini-3.1-flash-lite-image";
const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImageArgs {
    /// What to draw
    pub prompt: String,
    /// Output file name without extension (default: derived from prompt)
    pub filename: Option<String>,
}

#[derive(Debug)]
pub struct ImageGenTool {
    pub api_key: String,
    pub model: String,
}

impl ImageGenTool {
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY").ok()?;
        let model =
            std::env::var("MAKEDO_IMAGE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
        Some(Self { api_key, model })
    }
}

fn safe_name(s: &str) -> String {
    let name: String = s
        .chars()
        .take(40)
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    name.trim_matches('-').to_lowercase()
}

impl Tool for ImageGenTool {
    type Args = ImageArgs;

    fn name(&self) -> &'static str {
        "image_gen"
    }

    fn description(&self) -> &'static str {
        "Generate an image from a text prompt. Saves it under .makedo/assets/ \
         and returns its path."
    }

    async fn run(&self, ctx: &ToolCtx, args: ImageArgs) -> Result<String, ToolError> {
        let request = async {
            let body = serde_json::json!({
                "model": self.model,
                "messages": [{"role": "user", "content": args.prompt}],
                "modalities": ["image", "text"],
            });
            let resp = reqwest::Client::new()
                .post(ENDPOINT)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| ToolError::Failed(format!("image request failed: {e}")))?;
            let status = resp.status();
            let data: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ToolError::Failed(format!("bad image response: {e}")))?;
            if !status.is_success() {
                return Err(ToolError::Failed(format!("image api {status}: {data}")));
            }
            let url = data["choices"][0]["message"]["images"][0]["image_url"]["url"]
                .as_str()
                .ok_or_else(|| {
                    ToolError::Failed(format!(
                        "no image in response (model {}): {}",
                        self.model,
                        data["choices"][0]["message"]["content"]
                            .as_str()
                            .unwrap_or("?")
                    ))
                })?;
            // data:image/<ext>;base64,<...> — расширение берём из mime
            let ext = url
                .strip_prefix("data:image/")
                .and_then(|r| r.split(';').next())
                .filter(|e| e.chars().all(char::is_alphanumeric))
                .unwrap_or("png")
                .to_owned();
            let b64 = url
                .split_once("base64,")
                .map(|(_, b)| b)
                .ok_or_else(|| ToolError::Failed("image url is not a data URL".to_owned()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| ToolError::Failed(format!("base64 decode: {e}")))?;
            let dir = ctx.cwd.join(".makedo/assets");
            std::fs::create_dir_all(&dir)
                .map_err(|e| ToolError::Failed(format!("cannot create {}: {e}", dir.display())))?;
            let name = args
                .filename
                .map_or_else(|| safe_name(&args.prompt), |f| safe_name(&f));
            let path = dir.join(format!("{name}.{ext}"));
            std::fs::write(&path, &bytes)
                .map_err(|e| ToolError::Failed(format!("cannot write image: {e}")))?;
            Ok(format!(
                "Image saved: {} ({} bytes, model {})",
                path.display(),
                bytes.len(),
                self.model
            ))
        };
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
            result = request => result,
        }
    }

    fn access_paths(&self, args: &ImageArgs) -> Vec<PathBuf> {
        vec![PathBuf::from(format!(
            ".makedo/assets/{}",
            args.filename
                .as_deref()
                .map_or_else(|| safe_name(&args.prompt), safe_name)
        ))]
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn safe_name_sanitizes() {
        assert_eq!(super::safe_name("A cat/on the moon!"), "a-cat-on-the-moon");
    }
}
