use crate::error::AppError;

pub async fn call_openrouter_json<T>(
    http: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    schema_hint: &str,
) -> Result<T, AppError>
where
    T: serde::de::DeserializeOwned,
{
    let mut current_prompt = prompt.to_owned();
    let mut last_error = String::new();

    for attempt in 1..=3 {
        eprintln!("agent_json: attempt={attempt}");
        let model_response = call_openrouter(http, api_key, &current_prompt).await?;
        eprintln!(
            "agent_json: response attempt={attempt} bytes={} preview={}",
            model_response.len(),
            preview(&model_response)
        );
        match parse_json_content::<T>(&model_response) {
            Ok(output) => return Ok(output),
            Err(error) => {
                last_error = error.0;
                eprintln!("agent_json: parse_failed attempt={attempt} error={last_error}");
                current_prompt = format!(
                    "{prompt}\n\nYour previous response was invalid. {schema_hint}\nParse error: {last_error}\nPrevious response: {model_response}\nReturn corrected JSON only."
                );
            }
        }
    }

    Err(AppError(format!(
        "agent returned invalid JSON after retries: {last_error}"
    )))
}

pub async fn call_openrouter(
    http: &reqwest::Client,
    api_key: &str,
    prompt: &str,
) -> Result<String, AppError> {
    let models = [
        "inclusionai/ling-3.0-flash:free",
        "google/gemma-4-31b-it:free",
    ];
    let mut last_error = String::new();

    for model in models {
        eprintln!(
            "openrouter: request model={model} prompt_bytes={}",
            prompt.len()
        );
        let response = http
            .post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(api_key)
            .json(&serde_json::json!({
                "model": model,
                "messages": [{ "role": "user", "content": prompt }]
            }))
            .send()
            .await?;
        let status = response.status();
        eprintln!("openrouter: response model={model} status={status}");
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
            last_error = format!(
                "OpenRouter request failed model={model} status={status} body={}",
                preview(&body)
            );
            eprintln!("openrouter: {last_error}");
            continue;
        }

        let value = response.json::<serde_json::Value>().await?;
        eprintln!("openrouter: response JSON parsed model={model}");
        if let Some(content) = value["choices"][0]["message"]["content"].as_str() {
            return Ok(content.to_owned());
        }

        last_error = format!(
            "OpenRouter response missing content model={model} body={}",
            preview(&value.to_string())
        );
        eprintln!("openrouter: {last_error}");
    }

    Err(AppError(last_error))
}

pub fn preview(text: &str) -> String {
    const MAX_CHARS: usize = 300;
    let mut output: String = text.chars().take(MAX_CHARS).collect();
    if text.chars().count() > MAX_CHARS {
        output.push_str("...");
    }
    output.replace('\n', " ")
}

pub fn parse_json_content<T: serde::de::DeserializeOwned>(content: &str) -> Result<T, AppError> {
    let trimmed = content.trim();
    let json_text = trimmed
        .strip_prefix("```json")
        .and_then(|text| text.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    serde_json::from_str(json_text)
        .map_err(|error| AppError(format!("agent returned invalid JSON: {error}: {content}")))
}
