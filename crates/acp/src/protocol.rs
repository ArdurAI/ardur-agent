use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpMessage {
    pub id: String,
    pub payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpRequest {
    pub id: String,
    pub action: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpResponse {
    pub id: String,
    pub status: String,
    pub result: serde_json::Value,
}

impl AcpRequest {
    pub fn new(action: &str) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            action: action.to_string(),
            params: serde_json::Value::Null,
        }
    }
}

impl AcpResponse {
    pub fn ok(id: &str, result: serde_json::Value) -> Self {
        Self {
            id: id.to_string(),
            status: "ok".to_string(),
            result,
        }
    }

    pub fn error(id: &str, message: &str) -> Self {
        Self {
            id: id.to_string(),
            status: "error".to_string(),
            result: serde_json::json!({ "message": message }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_creation() {
        let req = AcpRequest::new("test");
        assert_eq!(req.action, "test");
    }

    #[test]
    fn test_response_ok() {
        let resp = AcpResponse::ok("1", serde_json::json!({"data": "value"}));
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn test_response_error() {
        let resp = AcpResponse::error("1", "failed");
        assert_eq!(resp.status, "error");
    }
}
