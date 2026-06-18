//! OpenAPI 3.0 spec and generated client helpers for the HTTP server.

use serde_json::{Value, json};

/// Generate the OpenAPI 3.0 specification for the currently mounted server
/// endpoints.
#[must_use]
pub fn openapi_spec() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Ardur Agent Server API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "HTTP API for health, chat submission, Slack events, MCP mounting, and OpenAPI discovery."
        },
        "paths": {
            "/healthz": {
                "get": {
                    "summary": "Liveness and build metadata",
                    "operationId": "healthz",
                    "responses": {
                        "200": {"description": "Server is alive", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HealthResponse"}}}}
                    }
                }
            },
            "/chat": {
                "post": {
                    "summary": "Submit one synchronous chat turn",
                    "operationId": "chat",
                    "security": [{"BearerAuth": []}],
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ChatRequest"}}}},
                    "responses": {
                        "200": {"description": "Turn completed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ChatResponse"}}}},
                        "400": {"description": "Malformed request"},
                        "401": {"description": "Missing or invalid bearer token"},
                        "502": {"description": "Runtime/provider rejected the turn"}
                    }
                }
            },
            "/slack/events": {
                "post": {
                    "summary": "Slack Events API webhook",
                    "operationId": "slackEvents",
                    "parameters": [
                        {"name": "X-Slack-Signature", "in": "header", "required": true, "schema": {"type": "string"}},
                        {"name": "X-Slack-Request-Timestamp", "in": "header", "required": true, "schema": {"type": "string"}}
                    ],
                    "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object"}}}},
                    "responses": {
                        "200": {"description": "Event accepted or ignored"},
                        "400": {"description": "Malformed event"},
                        "401": {"description": "Signature verification failed"}
                    }
                }
            },
            "/openapi.json": {
                "get": {
                    "summary": "OpenAPI document",
                    "operationId": "openapiJson",
                    "responses": {"200": {"description": "OpenAPI 3.0 document", "content": {"application/json": {"schema": {"type": "object"}}}}}
                }
            },
            "/openapi/clients/rust": {
                "get": {
                    "summary": "Generated Rust client source",
                    "operationId": "rustClient",
                    "responses": {"200": {"description": "Rust client source", "content": {"text/plain": {"schema": {"type": "string"}}}}}
                }
            },
            "/openapi/clients/python": {
                "get": {
                    "summary": "Generated Python client source",
                    "operationId": "pythonClient",
                    "responses": {"200": {"description": "Python client source", "content": {"text/plain": {"schema": {"type": "string"}}}}}
                }
            }
        },
        "components": {
            "securitySchemes": {"BearerAuth": {"type": "http", "scheme": "bearer"}},
            "schemas": {
                "HealthResponse": {
                    "type": "object",
                    "required": ["status", "build", "tests"],
                    "properties": {
                        "status": {"type": "string"},
                        "build": {"type": "string"},
                        "tests": {"type": "string"}
                    }
                },
                "ChatRequest": {
                    "type": "object",
                    "required": ["message"],
                    "properties": {
                        "message": {"type": "string"},
                        "session_id": {"type": "string", "format": "uuid"},
                        "stream": {"type": "boolean", "default": false}
                    }
                },
                "ChatResponse": {
                    "type": "object",
                    "required": ["session_id", "reply", "tokens", "cost_usd", "tools_called", "receipt_id"],
                    "properties": {
                        "session_id": {"type": "string", "format": "uuid"},
                        "reply": {"type": "string"},
                        "tokens": {"type": "object", "properties": {"input": {"type": "integer"}, "output": {"type": "integer"}}},
                        "cost_usd": {"type": "number"},
                        "tools_called": {"type": "array", "items": {"type": "string"}},
                        "receipt_id": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// Generate a small Rust client source file from the server's OpenAPI surface.
#[must_use]
pub fn generate_rust_client() -> String {
    r##"use serde_json::Value;

pub struct ArdurClient {
    base_url: String,
    bearer_token: Option<String>,
    client: reqwest::Client,
}

impl ArdurClient {
    pub fn new(base_url: impl Into<String>, bearer_token: Option<String>) -> Self {
        Self { base_url: base_url.into().trim_end_matches('/').to_string(), bearer_token, client: reqwest::Client::new() }
    }

    pub async fn healthz(&self) -> Result<Value, reqwest::Error> {
        self.client.get(format!("{}/healthz", self.base_url)).send().await?.error_for_status()?.json().await
    }

    pub async fn chat(&self, message: &str) -> Result<Value, reqwest::Error> {
        let mut request = self.client.post(format!("{}/chat", self.base_url)).json(&serde_json::json!({"message": message}));
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token);
        }
        request.send().await?.error_for_status()?.json().await
    }
}
"##
    .to_string()
}

/// Generate a small Python client source file from the server's OpenAPI surface.
#[must_use]
pub fn generate_python_client() -> String {
    r##"import json
import urllib.request


class ArdurClient:
    def __init__(self, base_url, bearer_token=None):
        self.base_url = base_url.rstrip('/')
        self.bearer_token = bearer_token

    def _request(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode('utf-8')
        headers = {'Accept': 'application/json'}
        if body is not None:
            headers['Content-Type'] = 'application/json'
        if self.bearer_token:
            headers['Authorization'] = 'Bearer ' + self.bearer_token
        req = urllib.request.Request(self.base_url + path, data=data, headers=headers, method=method)
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode('utf-8'))

    def healthz(self):
        return self._request('GET', '/healthz')

    def chat(self, message):
        return self._request('POST', '/chat', {'message': message})
"##
    .to_string()
}

/// Minimal generated Rust client made available to Rust callers/tests.
pub struct GeneratedRustClient {
    base_url: String,
    bearer_token: Option<String>,
    client: reqwest::Client,
}

impl GeneratedRustClient {
    /// Create a client for `base_url` with an optional bearer token.
    #[must_use]
    pub fn new(base_url: impl Into<String>, bearer_token: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bearer_token,
            client: reqwest::Client::new(),
        }
    }

    /// Call `GET /healthz`.
    pub async fn healthz(&self) -> anyhow::Result<Value> {
        let value = self
            .client
            .get(format!("{}/healthz", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(value)
    }

    /// Call `POST /chat` with the configured bearer token.
    pub async fn chat(&self, message: &str) -> anyhow::Result<Value> {
        let mut request = self
            .client
            .post(format!("{}/chat", self.base_url))
            .json(&json!({"message": message}));
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token);
        }
        let value = request.send().await?.error_for_status()?.json().await?;
        Ok(value)
    }
}
