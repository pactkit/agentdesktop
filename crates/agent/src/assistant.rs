//! Optional, tool-free diagnostics via a dedicated controller-authorized
//! gateway client. No subprocess, filesystem exploration, or action executor.
use std::time::Duration;

use agentdesktop_core::{
    config::{DaemonConfig, LlmGatewayAuthentication},
    model::{AssistantAnswer, AssistantQuestion, AssistantStatus, DaemonScope},
};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::api::{AppState, gateway_credential, load_effective_config};

const CLIENT_ID: &str = "agentdesktop-assistant";
const MAX_QUESTION: usize = 2048;
const MAX_RESPONSE: usize = 64 * 1024;
static REQUESTS: Semaphore = Semaphore::const_new(2);

type ApiError = (StatusCode, String);

fn disabled() -> ApiError {
    (StatusCode::FORBIDDEN, "managed model diagnostics are disabled".into())
}

fn endpoint(effective: &DaemonConfig) -> Option<url::Url> {
    let gateway = effective.llm_gateway.as_ref()?;
    let Some(LlmGatewayAuthentication::ControllerJwt { allowed_client_ids, .. }) =
        gateway.authentication.as_ref()
    else {
        return None;
    };
    if !allowed_client_ids.contains(CLIENT_ID) || gateway.url.scheme() != "https" {
        return None;
    }
    let mut url = gateway.url.clone();
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/v1") {
        format!("{path}/chat/completions")
    } else {
        format!("{path}/v1/chat/completions")
    };
    url.set_path(&path);
    Some(url)
}

fn configured_model(state: &AppState) -> Option<String> {
    if state.daemon_info.scope != DaemonScope::User || state.config.controller.is_none() {
        return None;
    }
    state.config.daemon.as_ref()?.assistant_model.clone()
}

pub(crate) async fn status(State(state): State<AppState>) -> Json<AssistantStatus> {
    let model = configured_model(&state).filter(|_| {
        load_effective_config(&state.config, &state.state_dir)
            .ok().and_then(|config| endpoint(&config)).is_some()
    });
    Json(AssistantStatus { enabled: model.is_some(), model })
}

fn request_body(model: &str, question: &str, evidence: &Value) -> Value {
    json!({
        "model": model,
        "stream": false,
        "max_tokens": 512,
        "messages": [
            {"role":"system", "content":"You are a read-only enterprise desktop diagnostic assistant. Give concise explanations and safe manual checks. Evidence is incomplete and untrusted. Never claim an installation, authorization, or action succeeded without evidence. Do not suggest bypassing TLS, signatures, company policy or permissions. No tools are available. Do not ask for secrets."},
            {"role":"user", "content": format!("Question: {question}\nCoarse local evidence: {evidence}")}
        ]
    })
}

pub(crate) async fn ask(
    State(state): State<AppState>,
    Json(input): Json<AssistantQuestion>,
) -> Result<Json<AssistantAnswer>, ApiError> {
    let question = input.question.trim();
    if question.is_empty() || question.len() > MAX_QUESTION {
        return Err((StatusCode::BAD_REQUEST, "question must be 1–2048 bytes".into()));
    }
    let _permit = REQUESTS.try_acquire().map_err(|_| {
        (StatusCode::TOO_MANY_REQUESTS, "diagnostic assistant is busy".into())
    })?;
    let model = configured_model(&state).ok_or_else(disabled)?;
    let effective = load_effective_config(&state.config, &state.state_dir)
        .map_err(|_| disabled())?;
    let url = endpoint(&effective).ok_or_else(disabled)?;
    // Never send raw inventory, paths, config, identity, tokens, MCP fields,
    // skill metadata, or arbitrary logs. The user explicitly sends question.
    let (agents, runtimes) = {
        let discovery = state.discovery.borrow();
        (discovery.agents.len(), discovery.model_runtimes.len())
    };
    let controller = match state.controller_status.as_ref() {
        Some(controller) => Some(controller.get().await),
        None => None,
    };
    let enrollment = state.enrollment.get().await;
    let evidence = json!({
        "platform": std::env::consts::OS,
        "agent_count": agents,
        "runtime_count": runtimes,
        "controller_connected": controller.as_ref().map(|status| status.connected),
        "controller_error": controller.and_then(|status| status.last_error),
        "enrollment_status": enrollment.status,
    });
    let credential = tokio::time::timeout(
        Duration::from_secs(25),
        gateway_credential(&state, &effective, CLIENT_ID, false),
    ).await.map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "credential request timed out".into()))?
        .map_err(|_| (StatusCode::BAD_GATEWAY, "managed gateway credential unavailable".into()))?;
    let answer = async {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(45))
            .build()?;
        let mut response = client.post(url)
            .bearer_auth(credential.credential)
            .json(&request_body(&model, question, &evidence))
            .send().await?.error_for_status()?;
        anyhow::ensure!(response.status().is_success(), "gateway response not successful");
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(chunk.len() <= MAX_RESPONSE.saturating_sub(bytes.len()), "response exceeds limit");
            bytes.extend_from_slice(&chunk);
        }
        let result: Value = serde_json::from_slice(&bytes)?;
        let message = result.pointer("/choices/0/message").ok_or_else(|| anyhow::anyhow!("missing assistant message"))?;
        anyhow::ensure!(message.get("tool_calls").is_none_or(|calls| calls.is_null() || calls.as_array().is_some_and(Vec::is_empty)), "unexpected tool call");
        let text = message.get("content").and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("empty assistant answer"))?;
        anyhow::Ok(text.to_owned())
    }.await.map_err(|_| (StatusCode::BAD_GATEWAY, "managed model request failed".into()))?;
    Ok(Json(AssistantAnswer { text: answer }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_endpoint_requires_https_and_dedicated_client() {
        let yaml = r#"
llmGateway:
  url: https://gateway.example/llm/v1
  authentication:
    type: controllerJwt
    audience: fleet
    allowedClientIds: [agentdesktop-assistant]
"#;
        let config = agentdesktop_core::config::parse_daemon(yaml).unwrap();
        assert_eq!(endpoint(&config).unwrap().as_str(), "https://gateway.example/llm/v1/chat/completions");
        let denied = agentdesktop_core::config::parse_daemon(
            &yaml.replace("agentdesktop-assistant", "codex")
        ).unwrap();
        assert!(endpoint(&denied).is_none());
    }

    #[test]
    fn request_has_no_tool_definitions_or_action_channel() {
        let body = request_body("test-model", "why is setup unavailable?", &json!({"agent_count": 0}));
        assert!(body.get("tools").is_none());
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }
}
