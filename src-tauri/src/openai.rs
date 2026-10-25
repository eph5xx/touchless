//! OpenAI, the text model the app lends to plugins as [`Llm`]. Requests are
//! paid for with an API key or with the user's ChatGPT plan.

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    chatgpt::{self, ChatGpt},
    plugin_api::{BoxFuture, JsonRequest, Llm},
    settings::{OpenAiMethod, Provider, Settings},
};

const BASE_URL: &str = "https://api.openai.com/v1";
/// Fast and cheap; plugin prompts are short, focused tasks.
const MODEL: &str = "gpt-6-luna";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const USAGE_LIMIT_CODE: &str = "subscription_sharing_usage_limit_exceeded";

/// How a request proves who pays for it.
pub enum Auth {
    ApiKey(String),
    ChatGpt(Arc<ChatGpt>),
}

pub struct OpenAi {
    client: reqwest::Client,
    auth: Auth,
}

impl OpenAi {
    pub fn new(auth: Auth) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| format!("could not set up OpenAI: {error}"))?;
        Ok(Self { client, auth })
    }

    /// OpenAI through the way to connect selected in Settings.
    pub fn from_settings(settings: &Settings) -> Result<Self, String> {
        let auth = match settings.openai_method() {
            OpenAiMethod::ApiKey => Auth::ApiKey(
                settings
                    .api_key(Provider::OpenAi)
                    .ok_or("Add an OpenAI key in Settings → Services.")?,
            ),
            OpenAiMethod::ChatGpt => {
                let chatgpt = settings.chatgpt();
                if !chatgpt.signed_in() {
                    return Err(chatgpt::NOT_SIGNED_IN.into());
                }
                Auth::ChatGpt(chatgpt)
            }
        };
        Self::new(auth)
    }

    /// Checks that OpenAI accepts the credentials.
    pub async fn verify(&self) -> Result<String, String> {
        let response = self
            .client
            .get(format!("{BASE_URL}/models"))
            .bearer_auth(self.bearer().await?)
            .send()
            .await
            .map_err(|error| format!("could not reach OpenAI: {error}"))?;
        self.check_status(response).await?;
        Ok(match &self.auth {
            Auth::ApiKey(_) => "Connected to OpenAI".into(),
            Auth::ChatGpt(chatgpt) => match chatgpt.email() {
                Some(email) => format!("Connected to ChatGPT as {email}"),
                None => "Connected to ChatGPT".into(),
            },
        })
    }

    /// Rewrites dictated `text` following `instruction`.
    pub async fn rewrite(&self, instruction: &str, text: &str) -> Result<String, String> {
        parse_rewrite(&self.respond(rewrite_request(instruction, text)).await?)
    }

    async fn bearer(&self) -> Result<String, String> {
        match &self.auth {
            Auth::ApiKey(key) => Ok(key.clone()),
            Auth::ChatGpt(chatgpt) => chatgpt.access_token().await,
        }
    }

    async fn respond(&self, request: JsonRequest) -> Result<Value, String> {
        let response = self
            .client
            .post(format!("{BASE_URL}/responses"))
            .bearer_auth(self.bearer().await?)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&request_body(&request))
            .send()
            .await
            .map_err(|error| format!("could not reach OpenAI: {error}"))?;
        let response = self.check_status(response).await?;

        let mut reader = StreamReader::default();
        let mut body = response.bytes_stream();
        let outcome = loop {
            match body.next().await {
                Some(Ok(chunk)) => {
                    if let Some(outcome) = reader.push(&chunk) {
                        break outcome;
                    }
                }
                Some(Err(error)) => {
                    break Err(Failure::Message(format!(
                        "could not read the OpenAI response: {error}"
                    )));
                }
                None => break reader.finish(),
            }
        };
        outcome.map_err(|failure| match failure {
            Failure::Api(error) => self.fail(&error),
            Failure::Message(message) => message,
        })
    }

    async fn check_status(&self, response: reqwest::Response) -> Result<reqwest::Response, String> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.json::<Value>().await.unwrap_or(Value::Null);
        Err(self.fail(&ApiError::from_body(Some(status.as_u16()), &body)))
    }

    fn fail(&self, error: &ApiError) -> String {
        let chatgpt = match &self.auth {
            Auth::ApiKey(_) => None,
            Auth::ChatGpt(chatgpt) => Some(chatgpt),
        };
        if let Some(chatgpt) = chatgpt
            && error.code.as_deref() == Some(USAGE_LIMIT_CODE)
        {
            chatgpt.pause_for_usage_limit();
        }
        describe_error(chatgpt.is_some(), error)
    }
}

impl Llm for OpenAi {
    fn json(&self, request: JsonRequest) -> BoxFuture<'_, Result<Value, String>> {
        Box::pin(self.respond(request))
    }
}

/// An error from OpenAI, either an HTTP error body or a failed stream event.
#[derive(Debug, Default, PartialEq)]
struct ApiError {
    status: Option<u16>,
    code: Option<String>,
    param: Option<String>,
    message: Option<String>,
}

impl ApiError {
    /// Reads `{"error": {...}}` bodies and the `{"detail": "..."}` bodies the
    /// ChatGPT plan route returns before a response starts.
    fn from_body(status: Option<u16>, body: &Value) -> Self {
        if body["error"].is_object() {
            return Self {
                status,
                ..Self::from_error(&body["error"])
            };
        }
        Self {
            status,
            message: body["detail"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .map(str::to_owned),
            ..Default::default()
        }
    }

    fn from_error(error: &Value) -> Self {
        let text = |key: &str| error[key].as_str().map(str::to_owned);
        Self {
            status: None,
            code: text("code"),
            param: text("param"),
            message: text("message"),
        }
    }
}

fn describe_error(chatgpt: bool, error: &ApiError) -> String {
    match error.code.as_deref() {
        Some(USAGE_LIMIT_CODE) => return chatgpt::USAGE_LIMIT.into(),
        Some("subscription_sharing_user_not_eligible") => {
            return "Your ChatGPT plan can't be used in Touchless. Use an OpenAI API key instead."
                .into();
        }
        Some(
            "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable",
        ) => {
            return "ChatGPT plan usage is unavailable right now. Try again later.".into();
        }
        Some("subscription_sharing_invalid_user") => {
            return format!(
                "ChatGPT couldn't verify your account. {}",
                chatgpt::SIGN_IN_AGAIN
            );
        }
        Some("subscription_sharing_unsupported_capability") => {
            return match &error.param {
                Some(param) => format!("Your ChatGPT plan doesn't support this request ({param})"),
                None => "Your ChatGPT plan doesn't support this request".into(),
            };
        }
        _ => {}
    }
    if error.status == Some(401) {
        return if chatgpt {
            format!(
                "ChatGPT didn't accept the sign-in. {}",
                chatgpt::SIGN_IN_AGAIN
            )
        } else {
            "OpenAI rejected the key".into()
        };
    }
    let detail = error.message.as_deref().or(error.code.as_deref());
    match (error.status, detail) {
        (Some(status), Some(detail)) => format!("OpenAI responded with {status}: {detail}"),
        (Some(status), None) => format!("OpenAI responded with {status}"),
        (None, Some(detail)) => format!("OpenAI failed: {detail}"),
        (None, None) => "OpenAI failed without saying why".into(),
    }
}

fn request_body(request: &JsonRequest) -> Value {
    json!({
        "model": MODEL,
        "instructions": request.instructions,
        "input": [{ "role": "user", "content": request.input }],
        "reasoning": { "effort": "low" },
        "store": false,
        "stream": true,
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

#[derive(Debug, PartialEq)]
enum Failure {
    Api(ApiError),
    Message(String),
}

/// Reads server-sent events until the response finishes one way or another.
#[derive(Default)]
struct StreamReader {
    buffer: Vec<u8>,
}

impl StreamReader {
    /// Returns the outcome once a final event arrives.
    fn push(&mut self, chunk: &[u8]) -> Option<Result<Value, Failure>> {
        self.buffer
            .extend(chunk.iter().copied().filter(|byte| *byte != b'\r'));
        while let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..end + 2).collect();
            if let Some(outcome) = event_data(&block).and_then(|event| final_outcome(&event)) {
                return Some(outcome);
            }
        }
        None
    }

    /// The outcome when the stream ends.
    fn finish(&mut self) -> Result<Value, Failure> {
        let rest = std::mem::take(&mut self.buffer);
        event_data(&rest)
            .and_then(|event| final_outcome(&event))
            .unwrap_or_else(|| Err(Failure::Message("OpenAI's answer was cut off".into())))
    }
}

fn event_data(block: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(block);
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&data).ok()
}

fn final_outcome(event: &Value) -> Option<Result<Value, Failure>> {
    match event["type"].as_str()? {
        "response.completed" => Some(parse_output(&event["response"]).map_err(Failure::Message)),
        "response.failed" => Some(Err(Failure::Api(ApiError::from_error(
            &event["response"]["error"],
        )))),
        "response.incomplete" => {
            let reason = event["response"]["incomplete_details"]["reason"]
                .as_str()
                .unwrap_or("no reason given");
            Some(Err(Failure::Message(format!(
                "OpenAI stopped before finishing: {reason}"
            ))))
        }
        "error" => {
            let error = if event["error"].is_object() {
                &event["error"]
            } else {
                event
            };
            Some(Err(Failure::Api(ApiError::from_error(error))))
        }
        _ => None,
    }
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
    fn builds_a_strict_streaming_json_request() {
        let body = request_body(&JsonRequest {
            instructions: "Check grammar.".into(),
            input: "I goes".into(),
            schema_name: "check".into(),
            schema: json!({ "type": "object" }),
        });
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["instructions"], "Check grammar.");
        assert_eq!(
            body["input"],
            json!([{ "role": "user", "content": "I goes" }])
        );
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
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
        assert_eq!(body["input"][0]["content"], "i goes home");
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

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\r\ndata: {event}\r\n\r\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    /// Feeds the stream in small pieces, as the network would.
    fn read(stream: &str) -> Result<Value, Failure> {
        let mut reader = StreamReader::default();
        for chunk in stream.as_bytes().chunks(7) {
            if let Some(outcome) = reader.push(chunk) {
                return outcome;
            }
        }
        reader.finish()
    }

    #[test]
    fn reads_a_completed_stream() {
        let completed = json!({
            "type": "response.completed",
            "response": { "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "{\"ok\":true}" }]
            }]}
        });
        let stream = sse(&[
            json!({ "type": "response.created", "response": {} }),
            json!({ "type": "response.output_text.delta", "delta": "{\"ok\"" }),
            completed.clone(),
        ]);
        assert_eq!(read(&stream), Ok(json!({ "ok": true })));
        // The final event may arrive without a trailing blank line.
        let unterminated = format!("data: {completed}");
        assert_eq!(read(&unterminated), Ok(json!({ "ok": true })));
    }

    #[test]
    fn reads_a_failed_stream() {
        let stream = sse(&[
            json!({ "type": "response.output_text.delta", "delta": "{" }),
            json!({
                "type": "response.failed",
                "response": { "error": {
                    "code": "subscription_sharing_usage_limit_exceeded",
                    "message": "Usage limit reached",
                }}
            }),
        ]);
        let Err(Failure::Api(error)) = read(&stream) else {
            panic!("expected an API error");
        };
        assert_eq!(error.code.as_deref(), Some(USAGE_LIMIT_CODE));
        assert_eq!(describe_error(true, &error), chatgpt::USAGE_LIMIT);
    }

    #[test]
    fn reads_incomplete_and_cut_off_streams() {
        let incomplete = sse(&[json!({
            "type": "response.incomplete",
            "response": { "incomplete_details": { "reason": "max_output_tokens" } }
        })]);
        assert_eq!(
            read(&incomplete),
            Err(Failure::Message(
                "OpenAI stopped before finishing: max_output_tokens".into()
            ))
        );
        let cut_off = sse(&[json!({ "type": "response.output_text.delta", "delta": "{" })]);
        assert_eq!(
            read(&cut_off),
            Err(Failure::Message("OpenAI's answer was cut off".into()))
        );
        assert!(read("").is_err());

        let error = sse(&[json!({ "type": "error", "code": "server_error", "message": "oops" })]);
        assert_eq!(
            read(&error),
            Err(Failure::Api(ApiError {
                status: None,
                code: Some("server_error".into()),
                param: None,
                message: Some("oops".into()),
            }))
        );
    }

    #[test]
    fn describes_errors_for_each_way_to_connect() {
        let unauthorized = ApiError {
            status: Some(401),
            ..Default::default()
        };
        assert_eq!(
            describe_error(false, &unauthorized),
            "OpenAI rejected the key"
        );
        assert!(describe_error(true, &unauthorized).contains("Sign in with ChatGPT again"));

        let detail = ApiError::from_body(Some(403), &json!({ "detail": "Region not supported" }));
        assert_eq!(
            describe_error(true, &detail),
            "OpenAI responded with 403: Region not supported"
        );

        let standard = ApiError::from_body(
            Some(400),
            &json!({ "error": {
                "code": "subscription_sharing_unsupported_capability",
                "param": "reasoning",
                "message": "nope",
            }}),
        );
        assert_eq!(
            describe_error(true, &standard),
            "Your ChatGPT plan doesn't support this request (reasoning)"
        );

        let plain = ApiError::from_body(Some(500), &json!({ "error": { "message": "boom" } }));
        assert_eq!(
            describe_error(false, &plain),
            "OpenAI responded with 500: boom"
        );
        assert_eq!(
            describe_error(false, &ApiError::from_body(Some(502), &Value::Null)),
            "OpenAI responded with 502"
        );
    }
}
