use crate::responses::{
    ResponsesResponse, ResponsesSseBuffer, ResponsesStreamState, ResponsesToolSpec,
    build_responses_input, convert_tools, extract_responses_text, extract_responses_tool_calls,
    parse_responses_usage, process_sse_chunk,
};
use crate::stream_guard::AbortOnDrop;
use crate::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    ModelProvider, ProviderCapabilities, StreamChunk, StreamError, StreamEvent, StreamOptions,
    StreamResult,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream;
use reiclaw_api::tool::ToolSpec;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::Value;

/// Default endpoint for the OpenAI Responses API.
const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

pub(crate) fn validate_endpoint(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| anyhow::Error::msg("OpenAI Responses endpoint must be a valid URL"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
        "OpenAI Responses endpoint must use http or https"
    );
    anyhow::ensure!(
        parsed.host_str() != Some("api.openai.com") || parsed.scheme() == "https",
        "The official OpenAI Responses endpoint requires HTTPS"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none() && parsed.fragment().is_none(),
        "Set OpenAI Responses credentials in api_key and remove URL userinfo or fragments"
    );
    anyhow::ensure!(
        !parsed
            .path()
            .trim_end_matches('/')
            .ends_with("/chat/completions"),
        "Chat Completions endpoints are retired. {}",
        reiclaw_config::providers::RESPONSES_MIGRATION
    );
    Ok(())
}

pub(crate) fn is_official_endpoint(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| parsed.host_str() == Some("api.openai.com"))
}

/// Maximum silence between body reads for OpenAI Responses SSE streams.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Serialize)]
struct ResponsesApiRequest {
    model: String,
    input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesApiReasoning>,
}

#[derive(Debug, Serialize)]
struct ResponsesApiReasoning {
    effort: String,
}

fn has_responses_tools(tools: Option<&[ResponsesToolSpec]>) -> bool {
    tools.is_some_and(|tools| !tools.is_empty())
}

/// Drive a Responses API SSE connection to completion, emitting events on `tx`.
/// `request_builder` must already have URL, auth headers, `Accept: text/event-stream`,
/// and the JSON body attached. Sends `StreamEvent::Final` on clean stream end.
pub(crate) async fn run_responses_sse(
    request_builder: reqwest::RequestBuilder,
    tx: &tokio::sync::mpsc::Sender<StreamResult<StreamEvent>>,
    count_tokens: bool,
) {
    let http_response = match request_builder.send().await {
        Ok(r) => r,
        Err(err) => {
            let _ = tx.send(Err(StreamError::Http(err.to_string()))).await;
            return;
        }
    };

    if !http_response.status().is_success() {
        let status = http_response.status();
        let body = http_response.text().await.unwrap_or_default();
        let sanitized = super::sanitize_api_error(&body);
        let _ = tx
            .send(Err(StreamError::ModelProvider(format!(
                "OpenAI API error ({status}): {sanitized}"
            ))))
            .await;
        return;
    }

    let mut state = ResponsesStreamState::default();
    let mut byte_stream = http_response.bytes_stream();
    let mut buffer = ResponsesSseBuffer::default();

    while !state.saw_completion {
        let result = match byte_stream.next().await {
            Some(Ok(bytes)) => {
                let mut bytes = bytes.as_ref();
                loop {
                    match buffer.next_frame(&mut bytes) {
                        Ok(Some(frame)) => {
                            if !send_responses_frame(tx, &mut state, &frame, count_tokens).await {
                                return;
                            }
                            if state.saw_completion {
                                break Ok(None);
                            }
                        }
                        Ok(None) => break Ok(None),
                        Err(error) => break Err(error),
                    }
                }
            }
            Some(Err(error)) => Err(StreamError::Http(error.to_string())),
            None => buffer.take_frame().map(Some),
        };
        match result {
            Ok(Some(frame)) => {
                if !send_responses_frame(tx, &mut state, &frame, count_tokens).await {
                    return;
                }
                break;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = tx.send(Err(error)).await;
                return;
            }
        }
    }

    if !state.saw_text_delta
        && let Some(text) = state.fallback_text.filter(|t| !t.is_empty())
    {
        let chunk = if count_tokens {
            StreamChunk::delta(text).with_token_estimate()
        } else {
            StreamChunk::delta(text)
        };
        let _ = tx.send(Ok(StreamEvent::TextDelta(chunk))).await;
    }

    crate::stream_guard::finish_sse_stream(
        tx,
        state.saw_completion,
        "response.completed or [DONE]",
    )
    .await;
}

async fn send_responses_frame(
    tx: &tokio::sync::mpsc::Sender<StreamResult<StreamEvent>>,
    state: &mut ResponsesStreamState,
    frame: &str,
    count_tokens: bool,
) -> bool {
    match process_sse_chunk(frame, state) {
        Ok(events) => {
            for event in events {
                let event = match event {
                    StreamEvent::TextDelta(chunk) if count_tokens => {
                        StreamEvent::TextDelta(chunk.with_token_estimate())
                    }
                    other => other,
                };
                if tx.send(Ok(event)).await.is_err() {
                    return false;
                }
            }
            true
        }
        Err(error) => {
            let _ = tx
                .send(Err(StreamError::ModelProvider(error.to_string())))
                .await;
            false
        }
    }
}

pub struct OpenAiResponsesModelProvider {
    alias: String,
    responses_url: String,
    credential: Option<String>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    /// HTTP request timeout in seconds for non-streaming LLM API calls.
    /// Streaming SSE calls use `streaming_client` which sets only a
    /// connect timeout so long-running responses aren't killed mid-stream.
    /// Defaults to 120 seconds.
    timeout_secs: u64,
    tls_ca: Option<reqwest::Certificate>,
    extra_headers: std::collections::HashMap<String, String>,
}

/// Typed builder for [`OpenAiResponsesModelProvider`].
///
/// Only `alias` is required. `api_url` defaults to the OpenAI Responses
/// endpoint; if a custom URL is supplied, `/responses` is appended when
/// not already present so callers can pass either shape. Every runtime
/// override (`timeout_secs` / `max_tokens` / `reasoning_effort` /
/// `extra_headers`) is set via a chain method on this builder before
/// [`Self::build`] — the built provider itself has no post-construction
/// mutators.
#[must_use]
pub struct OpenAiResponsesBuilder {
    alias: String,
    api_url: Option<String>,
    credential: Option<String>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    timeout_secs: Option<u64>,
    tls_ca: Option<reqwest::Certificate>,
    extra_headers: std::collections::HashMap<String, String>,
}

impl OpenAiResponsesBuilder {
    /// Override the API endpoint. The `/responses` suffix is appended
    /// automatically if the input does not already end in it.
    pub fn api_url(mut self, api_url: &str) -> Self {
        self.api_url = Some(api_url.to_string());
        self
    }

    /// Explicit API credential. Whitespace-only inputs collapse to
    /// `None`.
    pub fn credential(mut self, credential: Option<&str>) -> Self {
        self.credential = credential
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(ToString::to_string);
        self
    }

    pub fn max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn reasoning_effort(mut self, reasoning_effort: Option<String>) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }

    /// Override the non-streaming HTTP request timeout. Values of 0 are
    /// ignored so a zero from config cannot disable the safety timeout.
    pub fn timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.timeout_secs = Some(secs);
        }
        self
    }

    /// Set extra HTTP headers to include on every request. Reserved
    /// keys (e.g. `Authorization`) are dropped at request-build time —
    /// see [`OpenAiResponsesModelProvider`] for details.
    pub fn extra_headers(mut self, headers: std::collections::HashMap<String, String>) -> Self {
        self.extra_headers = headers;
        self
    }

    pub fn tls_ca_cert_path(mut self, path: &str) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        anyhow::ensure!(
            std::path::Path::new(path).is_absolute(),
            "OpenAI Responses tls_ca_cert_path must be absolute"
        );
        let pem = std::fs::read(path).context("Read OpenAI Responses TLS CA certificate")?;
        self.tls_ca = Some(
            reqwest::Certificate::from_pem(&pem)
                .context("Parse OpenAI Responses TLS CA certificate")?,
        );
        Ok(self)
    }

    pub fn build(self) -> OpenAiResponsesModelProvider {
        let responses_url = self
            .api_url
            .as_deref()
            .map(|url| {
                let Ok(mut parsed) = reqwest::Url::parse(url) else {
                    return url.to_string();
                };
                let path = parsed.path().trim_end_matches('/').to_owned();
                if !path.ends_with("/responses") {
                    parsed.set_path(&format!("{path}/responses"));
                } else if path.len() != parsed.path().len() {
                    parsed.set_path(&path);
                }
                parsed.to_string()
            })
            .unwrap_or_else(|| RESPONSES_URL.to_string());
        OpenAiResponsesModelProvider {
            alias: self.alias,
            responses_url,
            credential: self.credential,
            max_tokens: self.max_tokens,
            reasoning_effort: self.reasoning_effort,
            timeout_secs: self.timeout_secs.unwrap_or(120),
            tls_ca: self.tls_ca,
            extra_headers: self.extra_headers,
        }
    }
}

impl OpenAiResponsesModelProvider {
    /// Entry point. Only `alias` is required; every other field is set
    /// via a labelled chain method on the returned [`OpenAiResponsesBuilder`].
    pub fn builder(alias: &str) -> OpenAiResponsesBuilder {
        OpenAiResponsesBuilder {
            alias: alias.to_string(),
            api_url: None,
            credential: None,
            max_tokens: None,
            reasoning_effort: None,
            timeout_secs: None,
            tls_ca: None,
            extra_headers: std::collections::HashMap::new(),
        }
    }

    /// Serialize the same tool definitions that go on the Responses wire.
    pub fn tools_wire(tools: &[ToolSpec]) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&convert_tools(Some(tools)))?)
    }

    fn models_url(&self) -> anyhow::Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.responses_url)?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| anyhow::Error::msg("OpenAI Responses endpoint cannot be a base URL"))?;
        segments.pop_if_empty().pop().push("models");
        drop(segments);
        Ok(url)
    }

    async fn model_entries(&self) -> anyhow::Result<Vec<Value>> {
        let credential = self.request_credential()?;
        let request = self.http_client()?.get(self.models_url()?);
        let response = authenticate(request, credential).send().await?;
        if !response.status().is_success() {
            return Err(super::api_error("OpenAI", response).await);
        }
        #[derive(serde::Deserialize)]
        struct ModelList {
            data: Vec<Value>,
        }
        Ok(response.json::<ModelList>().await?.data)
    }

    pub(crate) async fn context_window(&self, model: &str) -> Option<usize> {
        let entry = self
            .model_entries()
            .await
            .ok()?
            .into_iter()
            .find(|entry| entry["id"].as_str() == Some(model))?;
        let size = entry
            .get("context_length")
            .or_else(|| entry.get("context_window"))?
            .as_u64()?;
        usize::try_from(size).ok()
    }

    fn request_credential(&self) -> anyhow::Result<Option<&str>> {
        let key = self.credential.as_deref();
        if is_official_endpoint(&self.responses_url) && key.is_none() {
            return Err(missing_credential());
        }
        if key.is_some_and(reiclaw_config::secrets::SecretStore::is_encrypted) {
            anyhow::bail!("The OpenAI Responses api_key is an unresolved secret reference");
        }
        Ok(key)
    }

    fn build_request(
        &self,
        instructions: Option<String>,
        input: Vec<serde_json::Value>,
        tools: Option<Vec<ResponsesToolSpec>>,
        model: &str,
        temperature: Option<f64>,
        stream: bool,
    ) -> ResponsesApiRequest {
        let has_tools = has_responses_tools(tools.as_deref());
        let reasoning = self
            .reasoning_effort
            .as_deref()
            .map(|effort| ResponsesApiReasoning {
                effort: effort.to_string(),
            });
        ResponsesApiRequest {
            model: model.to_string(),
            input,
            instructions,
            stream,
            // ReiClaw supplies its own history on each call.
            store: false,
            tools,
            tool_choice: has_tools.then(|| "auto".to_string()),
            parallel_tool_calls: has_tools.then_some(true),
            temperature,
            max_output_tokens: self.max_tokens,
            reasoning,
        }
    }

    fn build_default_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (key, value) in &self.extra_headers {
            if key.eq_ignore_ascii_case("authorization") {
                ::reiclaw_log::record!(
                    WARN,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Reject)
                        .with_outcome(::reiclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "header": key,
                            "reason": "reserved_authorization_overridden_by_provider_credential",
                        })),
                    "Dropping reserved 'Authorization' entry from extra_headers; built-in provider credential is authoritative. Rotate the credential via the 'credential' constructor argument instead."
                );
                continue;
            }
            match (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(val)) => {
                    headers.insert(name, val);
                }
                _ => {
                    ::reiclaw_log::record!(
                        WARN,
                        ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Note)
                            .with_outcome(::reiclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"header": key})),
                        "Skipping invalid extra header name or value"
                    );
                }
            }
        }
        headers
    }

    fn http_client(&self) -> anyhow::Result<Client> {
        let default_headers = self.build_default_headers();
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(10));
        if !default_headers.is_empty() {
            builder = builder.default_headers(default_headers);
        }
        if let Some(ca) = &self.tls_ca {
            builder = builder.add_root_certificate(ca.clone());
        }
        Ok(
            reiclaw_http::apply_runtime_proxy_to_builder(builder, "model_provider.openai")
                .build()?,
        )
    }

    fn streaming_client(&self) -> anyhow::Result<Client> {
        let default_headers = self.build_default_headers();
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(STREAM_IDLE_TIMEOUT);
        if !default_headers.is_empty() {
            builder = builder.default_headers(default_headers);
        }
        if let Some(ca) = &self.tls_ca {
            builder = builder.add_root_certificate(ca.clone());
        }
        Ok(
            reiclaw_http::apply_runtime_proxy_to_builder(builder, "model_provider.openai")
                .build()?,
        )
    }
}

fn authenticate(request: reqwest::RequestBuilder, key: Option<&str>) -> reqwest::RequestBuilder {
    match key {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

fn missing_credential() -> anyhow::Error {
    anyhow::Error::msg(
        "OpenAI API key not set. Set api_key under [providers.models.openai.<alias>] or OPENAI_API_KEY for the default OpenAI endpoint.",
    )
}

#[async_trait]
impl ModelProvider for OpenAiResponsesModelProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            prompt_caching: false,
            extended_thinking: false,
        }
    }

    /// Reports the instance's resolved endpoint so callers can verify which
    /// host a responses provider will actually hit (e.g. a compat family's
    /// default base vs. OpenAI's).
    fn default_base_url(&self) -> Option<&str> {
        Some(&self.responses_url)
    }

    fn endpoint_host(&self) -> Option<String> {
        crate::extract_endpoint_host(&self.responses_url)
    }

    fn default_wire_api(&self) -> &str {
        "responses"
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_streaming_tool_events(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        let credential = self.request_credential()?;
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ChatMessage::system(sys));
        }
        messages.push(ChatMessage::user(message));
        let (instructions, input) = build_responses_input(&messages);
        let instructions = if instructions.is_empty() {
            None
        } else {
            Some(instructions)
        };
        let req = self.build_request(instructions, input, None, model, temperature, false);
        let request = self.http_client()?.post(&self.responses_url);
        let response = authenticate(request, credential).json(&req).send().await?;
        if !response.status().is_success() {
            return Err(super::api_error("OpenAI", response).await);
        }
        let body: ResponsesResponse = response.json().await?;
        extract_responses_text(&body).ok_or_else(|| anyhow::Error::msg("No response from OpenAI"))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ProviderChatResponse> {
        let credential = self.request_credential()?;
        let (instructions, input) = build_responses_input(request.messages);
        let instructions = if instructions.is_empty() {
            None
        } else {
            Some(instructions)
        };
        let tools = convert_tools(request.tools);
        let tools_count = tools.as_ref().map_or(0, Vec::len);
        let req = self.build_request(instructions, input, tools, model, temperature, false);
        if ::reiclaw_log::debug_enabled() {
            ::reiclaw_log::record!(
                DEBUG,
                ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Send).with_attrs(
                    ::serde_json::json!({
                        "provider": "openai",
                        "alias": &self.alias,
                        "request_api": "responses",
                        "model": model,
                        "stream": false,
                        "tools_count": tools_count,
                        "tool_choice": req.tool_choice.as_deref(),
                        "parallel_tool_calls": req.parallel_tool_calls,
                    })
                ),
                "openai responses provider request prepared"
            );
        }
        let request = self.http_client()?.post(&self.responses_url);
        let response = authenticate(request, credential).json(&req).send().await?;
        if !response.status().is_success() {
            return Err(super::api_error("OpenAI", response).await);
        }
        let body: ResponsesResponse = response.json().await?;
        Ok(ProviderChatResponse {
            text: extract_responses_text(&body),
            tool_calls: extract_responses_tool_calls(&body),
            usage: parse_responses_usage(body.usage.as_ref()),
            reasoning_content: None,
        })
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .model_entries()
            .await?
            .into_iter()
            .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
            .collect())
    }

    fn stream_chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamEvent>> {
        if !options.enabled {
            return stream::once(async { Ok(StreamEvent::Final) }).boxed();
        }

        let credential = match self.request_credential() {
            Ok(key) => key.map(str::to_owned),
            Err(error) => {
                let err = StreamError::ModelProvider(error.to_string());
                return stream::once(async move { Err(err) }).boxed();
            }
        };

        let (instructions, input) = build_responses_input(request.messages);
        let instructions = (!instructions.is_empty()).then_some(instructions);
        let tools = convert_tools(request.tools);
        let tools_count = tools.as_ref().map_or(0, Vec::len);
        let req = self.build_request(instructions, input, tools, model, temperature, true);
        let responses_url = self.responses_url.clone();
        let count_tokens = options.count_tokens;
        let client = match self.streaming_client() {
            Ok(client) => client,
            Err(error) => {
                return stream::once(async move { Err(StreamError::Http(error.to_string())) })
                    .boxed();
            }
        };
        let alias = ::reiclaw_log::debug_enabled().then(|| self.alias.clone());

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamEvent>>(100);
        let handle = ::reiclaw_log::spawn!(async move {
            if let Some(alias) = alias.as_deref() {
                ::reiclaw_log::record!(
                    DEBUG,
                    ::reiclaw_log::Event::new(module_path!(), ::reiclaw_log::Action::Send)
                        .with_attrs(::serde_json::json!({
                            "provider": "openai",
                            "alias": alias,
                            "request_api": "responses",
                            "model": &req.model,
                            "stream": true,
                            "tools_count": tools_count,
                            "tool_choice": req.tool_choice.as_deref(),
                            "parallel_tool_calls": req.parallel_tool_calls,
                        })),
                    "openai responses streaming provider request prepared"
                );
            }

            let request_builder = authenticate(client.post(&responses_url), credential.as_deref())
                .header("Accept", "text/event-stream")
                .json(&req);

            run_responses_sse(request_builder, &tx, count_tokens).await;
        });

        let guard = AbortOnDrop::new(handle.abort_handle());
        stream::unfold((rx, guard), |(mut rx, guard)| async move {
            rx.recv().await.map(|event| (event, (rx, guard)))
        })
        .boxed()
    }
}

impl ::reiclaw_api::attribution::Attributable for OpenAiResponsesModelProvider {
    fn role(&self) -> ::reiclaw_api::attribution::Role {
        ::reiclaw_api::attribution::Role::Provider(::reiclaw_api::attribution::ProviderKind::Model(
            ::reiclaw_api::attribution::ModelProviderKind::OpenAi,
        ))
    }
    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn responses_send_failure_preserves_transport_kind_for_auth_looking_urls() {
        for path in ["unauthorized", "forbidden"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let request = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(format!("http://{addr}/{path}"));
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(run_responses_sse(request, &tx, false), async {
                    // Drop the accepted connection before sending response headers.
                    let (socket, _) = listener.accept().await.unwrap();
                    drop(socket);
                });
            })
            .await
            .expect("request send failure must complete");
            drop(tx);
            let error = rx.recv().await.unwrap().unwrap_err();
            match error {
                StreamError::Http(message) => {
                    assert!(message.contains(path), "{message}");
                }
                other => panic!("transport failure was misclassified: {other:?}"),
            }
            assert!(rx.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn responses_completed_ignores_trailing_events_without_eof() {
        use axum::{Router, response::IntoResponse, routing::post};

        let app = Router::new().route(
            "/responses",
            post(|| async {
                let first = futures_util::stream::once(async {
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                        b"data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"output_text\":\"fallback\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"after-terminal\"}\n\n",
                    ))
                });
                let open = futures_util::stream::pending::<
                    Result<axum::body::Bytes, std::convert::Infallible>,
                >();
                axum::body::Body::from_stream(first.chain(open)).into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Responses SSE test server");
        let addr = listener.local_addr().expect("Responses SSE test address");
        let server = ::reiclaw_log::spawn!(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Responses SSE test");
        });
        let request = reqwest::Client::new().post(format!("http://{addr}/responses"));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamResult<StreamEvent>>(16);

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_responses_sse(request, &tx, false),
        )
        .await
        .expect("response.completed must finish without waiting for EOF");
        drop(tx);
        server.abort();

        let mut text = None;
        let mut saw_final = false;
        while let Some(event) = rx.recv().await {
            match event {
                Ok(StreamEvent::TextDelta(chunk)) => text = Some(chunk.delta),
                Ok(StreamEvent::Final) => saw_final = true,
                _ => {}
            }
        }
        assert_eq!(text.as_deref(), Some("fallback"));
        assert!(saw_final, "response.completed must emit Final");
    }

    #[tokio::test]
    async fn responses_chat_propagates_usage() {
        use axum::{Json, Router, routing::post};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/responses",
            post(|| async {
                Json(serde_json::json!({
                    "output_text": "ok",
                    "output": [],
                    "usage": {
                        "input_tokens": 120,
                        "input_tokens_details": {"cached_tokens": 45},
                        "output_tokens": 30
                    }
                }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = reiclaw_log::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = OpenAiResponsesModelProvider::builder("test")
            .api_url(&format!("http://{addr}"))
            .credential(Some("test-key"))
            .build();
        let messages = vec![ChatMessage::user("hello")];

        let response = provider
            .chat(
                ProviderChatRequest {
                    messages: &messages,
                    tools: None,
                    thinking: None,
                },
                "gpt-4o-mini",
                None,
            )
            .await
            .expect("chat should succeed");

        let usage = response
            .usage
            .expect("provider-reported usage should propagate");
        assert_eq!(usage.input_tokens, Some(120));
        assert_eq!(usage.cached_input_tokens, Some(45));
        assert_eq!(usage.output_tokens, Some(30));
        server_handle.abort();
    }

    #[test]
    fn creates_with_key() {
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(Some("openai-test-credential"))
            .build();
        assert_eq!(p.credential.as_deref(), Some("openai-test-credential"));
    }

    #[test]
    fn creates_without_key() {
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(None)
            .build();
        assert!(p.credential.is_none());
    }

    #[test]
    fn responses_url_appends_responses_to_custom_base() {
        let p = OpenAiResponsesModelProvider::builder("custom")
            .api_url("https://gateway.example.test/v1")
            .credential(None)
            .build();
        assert_eq!(p.responses_url, "https://gateway.example.test/v1/responses");
    }

    #[test]
    fn responses_url_defaults_to_openai_when_base_absent() {
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(None)
            .build();
        assert_eq!(p.responses_url, RESPONSES_URL);
    }

    #[test]
    fn responses_provider_defaults_timeout_to_120() {
        let p = OpenAiResponsesModelProvider::builder("test").build();
        assert_eq!(
            p.timeout_secs, 120,
            "fresh provider must default timeout_secs to 120"
        );
    }

    #[test]
    fn responses_provider_defaults_extra_headers_to_empty() {
        let p = OpenAiResponsesModelProvider::builder("test").build();
        assert!(
            p.extra_headers.is_empty(),
            "fresh provider must default extra_headers to an empty HashMap"
        );
    }

    #[test]
    fn timeout_secs_overrides_default() {
        let p = OpenAiResponsesModelProvider::builder("test")
            .timeout_secs(45)
            .build();
        assert_eq!(
            p.timeout_secs, 45,
            "builder .timeout_secs(...) must override the 120 default"
        );
    }

    #[test]
    fn extra_headers_propagates() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Title".to_string(), "zeroclaw".to_string());
        headers.insert(
            "HTTP-Referer".to_string(),
            "https://example.com".to_string(),
        );
        let p = OpenAiResponsesModelProvider::builder("test")
            .extra_headers(headers)
            .build();
        assert_eq!(
            p.extra_headers.len(),
            2,
            "builder .extra_headers(...) must store the configured headers"
        );
        assert_eq!(
            p.extra_headers.get("X-Title").map(String::as_str),
            Some("zeroclaw"),
            "configured extra_headers entry must be retrievable by name"
        );
        assert_eq!(
            p.extra_headers.get("HTTP-Referer").map(String::as_str),
            Some("https://example.com"),
            "configured extra_headers entry must be retrievable by name"
        );
    }

    #[test]
    fn build_default_headers_is_empty_when_no_extra_headers() {
        let p = OpenAiResponsesModelProvider::builder("test").build();
        let headers = p.build_default_headers();
        assert!(
            headers.is_empty(),
            "build_default_headers must return an empty HeaderMap when extra_headers is empty"
        );
    }

    #[test]
    fn build_default_headers_includes_every_configured_entry() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Title".to_string(), "zeroclaw".to_string());
        headers.insert(
            "HTTP-Referer".to_string(),
            "https://example.com".to_string(),
        );
        let p = OpenAiResponsesModelProvider::builder("test")
            .extra_headers(headers)
            .build();
        let default_headers = p.build_default_headers();
        assert_eq!(
            default_headers.len(),
            2,
            "every configured extra_headers entry must appear in build_default_headers output"
        );
        assert_eq!(
            default_headers.get("X-Title").and_then(|v| v.to_str().ok()),
            Some("zeroclaw"),
            "X-Title must round-trip into the HeaderMap"
        );
        assert_eq!(
            default_headers
                .get("HTTP-Referer")
                .and_then(|v| v.to_str().ok()),
            Some("https://example.com"),
            "HTTP-Referer must round-trip into the HeaderMap"
        );
    }

    #[test]
    fn build_default_headers_skips_invalid_header_name_without_panicking() {
        // A name with a space is invalid per RFC 7230; `HeaderName::from_bytes`
        // returns `Err`. The builder must log WARN and skip the entry rather
        // than panicking.
        let mut headers = std::collections::HashMap::new();
        headers.insert("X Valid".to_string(), "ok".to_string()); // space → invalid
        headers.insert("X-Also-Valid".to_string(), "ok".to_string());
        let p = OpenAiResponsesModelProvider::builder("test")
            .extra_headers(headers)
            .build();
        let default_headers = p.build_default_headers();
        assert_eq!(
            default_headers.len(),
            1,
            "only the valid header name should land in the HeaderMap"
        );
        assert!(
            default_headers.get("X-Also-Valid").is_some(),
            "X-Also-Valid must be present in the HeaderMap"
        );
    }

    #[test]
    fn build_default_headers_skips_invalid_header_value_without_panicking() {
        // A value containing a NUL byte is invalid per RFC 7230;
        // `HeaderValue::from_str` returns `Err`. The builder must skip
        // the entry rather than panicking.
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Bad-Value".to_string(), "has\0nul".to_string()); // NUL → invalid
        headers.insert("X-Good-Value".to_string(), "ok".to_string());
        let p = OpenAiResponsesModelProvider::builder("test")
            .extra_headers(headers)
            .build();
        let default_headers = p.build_default_headers();
        assert_eq!(
            default_headers.len(),
            1,
            "only the valid header value should land in the HeaderMap"
        );
        assert!(
            default_headers.get("X-Good-Value").is_some(),
            "X-Good-Value must be present in the HeaderMap"
        );
    }

    #[test]
    fn build_default_headers_drops_authorization_in_favor_of_builtin() {
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer operator-override".to_string(),
        );
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(Some("sk-builtin"))
            .extra_headers(headers)
            .build();
        let default_headers = p.build_default_headers();
        assert!(
            default_headers.get("Authorization").is_none(),
            "operator-set Authorization must be dropped from default_headers so the built-in provider credential stays authoritative on the wire"
        );
        assert_eq!(
            default_headers.len(),
            0,
            "the only configured extra_headers entry was the reserved Authorization and must not appear in the resulting HeaderMap"
        );
    }

    #[test]
    fn build_default_headers_drops_authorization_case_insensitively() {
        // `authorization`, `AUTHORIZATION`, `Authorization` must all be
        // dropped — reqwest stores header names lowercased internally,
        // so the public contract is "any case". Verify all three.
        for variant in [
            "Authorization",
            "authorization",
            "AUTHORIZATION",
            "AuThOrIzAtIoN",
        ] {
            let mut headers = std::collections::HashMap::new();
            headers.insert(variant.to_string(), "Bearer x".to_string());
            let p = OpenAiResponsesModelProvider::builder("test")
                .credential(Some("sk-builtin"))
                .extra_headers(headers)
                .build();
            let default_headers = p.build_default_headers();
            assert!(
                default_headers.get("Authorization").is_none(),
                "case variant {variant:?} must be dropped from default_headers (case-insensitive)"
            );
            assert_eq!(
                default_headers.len(),
                0,
                "case variant {variant:?} must produce an empty HeaderMap (only reserved Authorization configured)"
            );
        }
    }

    #[test]
    fn build_default_headers_preserves_non_authorization_extra_headers() {
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer operator-override".to_string(),
        );
        headers.insert("X-Title".to_string(), "zeroclaw".to_string());
        headers.insert(
            "HTTP-Referer".to_string(),
            "https://example.com".to_string(),
        );
        headers.insert("X-Trace-Id".to_string(), "trace-123".to_string());
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(Some("sk-builtin"))
            .extra_headers(headers)
            .build();
        let default_headers = p.build_default_headers();
        assert_eq!(
            default_headers.len(),
            3,
            "only the reserved Authorization is dropped; the three other custom headers must flow through"
        );
        assert!(
            default_headers.get("Authorization").is_none(),
            "Authorization must not appear in default_headers regardless of other entries"
        );
        assert_eq!(
            default_headers.get("X-Title").and_then(|v| v.to_str().ok()),
            Some("zeroclaw"),
            "X-Title must round-trip"
        );
        assert_eq!(
            default_headers
                .get("HTTP-Referer")
                .and_then(|v| v.to_str().ok()),
            Some("https://example.com"),
            "HTTP-Referer must round-trip"
        );
        assert_eq!(
            default_headers
                .get("X-Trace-Id")
                .and_then(|v| v.to_str().ok()),
            Some("trace-123"),
            "X-Trace-Id must round-trip"
        );
    }

    #[test]
    fn empty_key_is_treated_as_missing() {
        // Whitespace-only / empty credentials collapse to None so a stray
        // "" from config cannot produce a bogus `Bearer ` header. Matches
        // the trim-then-filter contract shared with anthropic/openrouter/
        // azure/compat.
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(Some(""))
            .build();
        assert!(p.credential.is_none());

        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(Some("  \t  "))
            .build();
        assert!(p.credential.is_none());
    }

    #[tokio::test]
    async fn chat_fails_without_key() {
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(None)
            .build();
        let result = p.chat_with_system(None, "hello", "gpt-4o", Some(0.7)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not set"));
    }

    #[tokio::test]
    async fn chat_with_system_fails_without_key() {
        let p = OpenAiResponsesModelProvider::builder("test")
            .credential(None)
            .build();
        let result = p
            .chat_with_system(Some("You are ZeroClaw"), "test", "gpt-4o", Some(0.5))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn responses_request_propagates_max_tokens_when_set() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .credential(None)
            .max_tokens(Some(2048))
            .build();
        let req = provider.build_request(
            None,
            vec![serde_json::json!({"role": "user", "content": "hi"})],
            None,
            "gpt-5",
            None,
            false,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert_eq!(
            json.get("max_output_tokens")
                .and_then(serde_json::Value::as_u64),
            Some(2048),
            "max_tokens configured on the provider must survive into max_output_tokens on the wire body"
        );
    }

    #[test]
    fn responses_request_omits_max_tokens_when_unset() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .credential(None)
            .build();
        assert!(
            provider.max_tokens.is_none(),
            "fresh provider must default max_tokens to None"
        );
        let req = provider.build_request(
            None,
            vec![serde_json::json!({"role": "user", "content": "hi"})],
            None,
            "gpt-5",
            None,
            false,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert!(
            json.get("max_output_tokens").is_none()
                || json
                    .get("max_output_tokens")
                    .and_then(serde_json::Value::as_null)
                    .is_some(),
            "unset max_tokens must not surface as a wire-bound integer (skipped via skip_serializing_if)"
        );
    }

    #[test]
    fn responses_request_propagates_reasoning_effort_when_set() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .credential(None)
            .reasoning_effort(Some("high".to_string()))
            .build();
        let req = provider.build_request(
            None,
            vec![serde_json::json!({"role": "user", "content": "hi"})],
            None,
            "o3",
            None,
            false,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert_eq!(json["store"], false);
        let reasoning = json
            .get("reasoning")
            .expect("reasoning_effort must populate the `reasoning` object");
        assert_eq!(
            reasoning.get("effort").and_then(serde_json::Value::as_str),
            Some("high"),
            ".reasoning_effort(Some(\"high\")) must surface as reasoning.effort = \"high\" on the wire body"
        );
    }

    #[test]
    fn responses_request_omits_reasoning_when_unset() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .credential(None)
            .build();
        assert!(
            provider.reasoning_effort.is_none(),
            "fresh provider must default reasoning_effort to None"
        );
        let req = provider.build_request(
            None,
            vec![serde_json::json!({"role": "user", "content": "hi"})],
            None,
            "gpt-5",
            None,
            false,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert!(
            json.get("reasoning").is_none()
                || json
                    .get("reasoning")
                    .and_then(serde_json::Value::as_null)
                    .is_some(),
            "unset reasoning_effort must not surface as a wire-bound object (skipped via skip_serializing_if)"
        );
    }

    #[test]
    fn responses_request_propagates_instructions_and_temperature_and_model() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .api_url("https://api.example.test/v1")
            .credential(None)
            .build();
        let req = provider.build_request(
            Some("You are a careful assistant.".to_string()),
            vec![serde_json::json!({"role": "user", "content": "summarize"})],
            None,
            "gpt-5-mini",
            Some(0.3),
            true,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert_eq!(
            json.get("model").and_then(serde_json::Value::as_str),
            Some("gpt-5-mini"),
            "model argument must reach the wire body verbatim"
        );
        assert_eq!(
            json.get("instructions").and_then(serde_json::Value::as_str),
            Some("You are a careful assistant."),
            "non-None instructions argument must reach the wire body"
        );
        assert_eq!(
            json.get("temperature").and_then(serde_json::Value::as_f64),
            Some(0.3),
            "temperature argument must reach the wire body as f64"
        );
        assert_eq!(
            json.get("stream").and_then(serde_json::Value::as_bool),
            Some(true),
            "stream argument must reach the wire body as bool"
        );
    }

    #[test]
    fn responses_request_propagates_tool_choice_and_parallel_when_tools_present() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .credential(None)
            .max_tokens(Some(1024))
            .build();
        let tools = Some(vec![ResponsesToolSpec {
            kind: "function".to_string(),
            name: "lookup_weather".to_string(),
            description: "Look up the weather for a city.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
            })
            .into(),
            strict: true,
        }]);
        let req = provider.build_request(
            None,
            vec![serde_json::json!({"role": "user", "content": "weather?"})],
            tools,
            "gpt-5",
            None,
            false,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert_eq!(
            json.get("tool_choice").and_then(serde_json::Value::as_str),
            Some("auto"),
            "with tools present, wire body must carry tool_choice = \"auto\""
        );
        assert_eq!(
            json.get("parallel_tool_calls")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "with tools present, wire body must carry parallel_tool_calls = true"
        );
        let wire_tools = json
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .expect("tools must be a JSON array on the wire body");
        assert_eq!(
            wire_tools.len(),
            1,
            "exactly one tool spec must reach the wire body"
        );
    }

    #[test]
    fn responses_request_omits_tool_choice_and_parallel_when_tools_absent() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .credential(None)
            .build();
        let req = provider.build_request(
            None,
            vec![serde_json::json!({"role": "user", "content": "hi"})],
            None,
            "gpt-5",
            None,
            false,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert!(
            json.get("tool_choice").is_none()
                || json
                    .get("tool_choice")
                    .and_then(serde_json::Value::as_null)
                    .is_some(),
            "no tools → tool_choice must be omitted (skipped via skip_serializing_if)"
        );
        assert!(
            json.get("parallel_tool_calls").is_none()
                || json
                    .get("parallel_tool_calls")
                    .and_then(serde_json::Value::as_null)
                    .is_some(),
            "no tools → parallel_tool_calls must be omitted (skipped via skip_serializing_if)"
        );
    }

    #[test]
    fn responses_request_omits_tool_choice_and_parallel_when_tools_empty() {
        let provider = OpenAiResponsesModelProvider::builder("openai")
            .credential(None)
            .build();
        let req = provider.build_request(
            None,
            vec![serde_json::json!({"role": "user", "content": "hi"})],
            Some(Vec::new()),
            "gpt-5",
            None,
            false,
        );
        let json = serde_json::to_value(&req).expect("ResponsesApiRequest must serialize");
        assert!(
            json.get("tool_choice").is_none()
                || json
                    .get("tool_choice")
                    .and_then(serde_json::Value::as_null)
                    .is_some(),
            "empty tools list → tool_choice must be omitted (vLLM rejects tool_choice without non-empty tools)"
        );
        assert!(
            json.get("parallel_tool_calls").is_none()
                || json
                    .get("parallel_tool_calls")
                    .and_then(serde_json::Value::as_null)
                    .is_some(),
            "empty tools list → parallel_tool_calls must be omitted with tool_choice"
        );
        let wire_tools = json
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .expect("tools must be a JSON array on the wire body");
        assert!(
            wire_tools.is_empty(),
            "empty tools should remain an empty tools array; only tool_choice and parallel_tool_calls are suppressed"
        );
    }
}
