//! OpenAI, the text model the app lends to plugins as [`Llm`].

use std::time::Duration;

use serde_json::{Value, json};

use crate::plugin_api::{BoxFuture, JsonRequest, Llm};

const BASE_URL: &str = "https://api.openai.com/v1";
/// Fast and cheap; plugin prompts are short, focused tasks.
const MODEL: &str = "gpt-6-luna";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

pub struct OpenAi {
    client: reqwest::Client,
    api_key: String,
}

impl OpenAi {
    pub fn new(api_key: String) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| format!("could not set up OpenAI: {error}"))?;
        Ok(Self { client, api_key })
    }

    /// Checks that OpenAI accepts the key.
    pub async fn verify(&self) -> Result<String, String> {
        let response = self
            .client
            .get(format!("{BASE_URL}/models"))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| format!("could not reach OpenAI: {error}"))?;
        check_status(response).await?;
        Ok("Connected to OpenAI".into())
    }

    /// Rewrites dictated `text` following `instruction`.
    pub async fn rewrite(&self, instruction: &str, text: &str) -> Result<String, String> {
        parse_rewrite(&self.respond(rewrite_request(instruction, text)).await?)
    }

    async fn respond(&self, request: JsonRequest) -> Result<Value, String> {
        let response = self
            .client
            .post(format!("{BASE_URL}/responses"))
            .bearer_auth(&self.api_key)
            .json(&request_body(&request))
            .send()
            .await
            .map_err(|error| format!("could not reach OpenAI: {error}"))?;
        let response = check_status(response).await?;
        let body: Value = response
            .json()
            .await
            .map_err(|error| format!("could not read the OpenAI response: {error}"))?;
        parse_output(&body)
    }
}

impl Llm for OpenAi {
    fn json(&self, request: JsonRequest) -> BoxFuture<'_, Result<Value, String>> {
        Box::pin(self.respond(request))
    }
}

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status.as_u16() == 401 {
        return Err("OpenAI rejected the key".into());
    }
    let detail = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| body["error"]["message"].as_str().map(str::to_owned));
    Err(match detail {
        Some(detail) => format!("OpenAI responded with {status}: {detail}"),
        None => format!("OpenAI responded with {status}"),
    })
}

fn request_body(request: &JsonRequest) -> Value {
    json!({
        "model": MODEL,
        "instructions": request.instructions,
        "input": request.input,
        "reasoning": { "effort": "low" },
        "store": false,
        "text": {
            "format": {
                "type": "json_schema",
                "name": request.schema_name,
                "schema": request.schema,
                "strict": true,
            }
        }
    })
}

fn rewrite_request(instruction: &str, text: &str) -> JsonRequest {
    JsonRequest {
        instructions: instruction.to_owned(),
        input: text.to_owned(),
        schema_name: "rewrite".into(),
        schema: json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
            "additionalProperties": false,
        }),
    }
}

fn parse_rewrite(answer: &Value) -> Result<String, String> {
    answer["text"]
        .as_str()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "OpenAI returned no rewritten text".into())
}

/// Pulls the JSON answer out of a Responses API body.
fn parse_output(body: &Value) -> Result<Value, String> {
    let contents = body["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten());
    for content in contents {
        match content["type"].as_str() {
            Some("output_text") => {
                let text = content["text"].as_str().unwrap_or_default();
                return serde_json::from_str(text)
                    .map_err(|error| format!("OpenAI did not return valid JSON: {error}"));
            }
            Some("refusal") => {
                let reason = content["refusal"].as_str().unwrap_or("no reason given");
                return Err(format!("OpenAI declined: {reason}"));
            }
            _ => {}
        }
    }
    Err("OpenAI returned no answer".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_strict_json_request() {
        let body = request_body(&JsonRequest {
            instructions: "Check grammar.".into(),
            input: "I goes".into(),
            schema_name: "check".into(),
            schema: json!({ "type": "object" }),
        });
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["instructions"], "Check grammar.");
        assert_eq!(body["input"], "I goes");
        assert_eq!(body["store"], false);
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["name"], "check");
        assert_eq!(body["text"]["format"]["strict"], true);
        assert_eq!(
            body["text"]["format"]["schema"],
            json!({ "type": "object" })
        );
    }

    #[test]
    fn builds_and_reads_a_rewrite() {
        let body = request_body(&rewrite_request("Fix grammar.", "i goes home"));
        assert_eq!(body["instructions"], "Fix grammar.");
        assert_eq!(body["input"], "i goes home");
        assert_eq!(body["text"]["format"]["name"], "rewrite");
        assert_eq!(body["text"]["format"]["strict"], true);
        assert_eq!(
            body["text"]["format"]["schema"]["required"],
            json!(["text"])
        );

        let response = json!({
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "{\"text\":\" I go home. \"}" }]
            }]
        });
        let answer = parse_output(&response).unwrap();
        assert_eq!(parse_rewrite(&answer), Ok("I go home.".into()));
        assert!(parse_rewrite(&json!({ "text": " " })).is_err());
        assert!(parse_rewrite(&json!({})).is_err());
    }

    #[test]
    fn reads_the_answer_after_reasoning_items() {
        let body = json!({
            "output": [
                { "type": "reasoning", "summary": [] },
                {
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "{\"ok\":true}" }]
                }
            ]
        });
        assert_eq!(parse_output(&body), Ok(json!({ "ok": true })));
    }

    #[test]
    fn reports_refusals_and_missing_answers() {
        let refusal = json!({
            "output": [{
                "type": "message",
                "content": [{ "type": "refusal", "refusal": "can't help" }]
            }]
        });
        assert_eq!(
            parse_output(&refusal),
            Err("OpenAI declined: can't help".into())
        );
        assert!(parse_output(&json!({ "output": [] })).is_err());

        let not_json = json!({
            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "hi" }] }]
        });
        assert!(parse_output(&not_json).is_err());
    }
}
