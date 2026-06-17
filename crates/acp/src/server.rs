use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8080,
        }
    }
}

pub struct AcpServer {
    config: ServerConfig,
    handlers: HashMap<String, Box<dyn Fn(serde_json::Value) -> serde_json::Value + Send + Sync>>,
}

impl std::fmt::Debug for AcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpServer")
            .field("config", &self.config)
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Clone for AcpServer {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            handlers: HashMap::new(), // handlers can't be cloned, start fresh
        }
    }
}

impl AcpServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            handlers: HashMap::new(),
        }
    }

    pub fn register_handler<F>(&mut self, action: &str, handler: F)
    where
        F: Fn(serde_json::Value) -> serde_json::Value + Send + Sync + 'static,
    {
        self.handlers.insert(action.to_string(), Box::new(handler));
    }

    pub fn handle(&self, request: &crate::protocol::AcpRequest) -> crate::protocol::AcpResponse {
        match self.handlers.get(&request.action) {
            Some(handler) => {
                let result = handler(request.params.clone());
                crate::protocol::AcpResponse::ok(&request.id, result)
            }
            None => crate::protocol::AcpResponse::error(
                &request.id,
                &format!("unknown action: {}", request.action),
            ),
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_handle() {
        let mut server = AcpServer::new(ServerConfig::default());
        server.register_handler("echo", |v| v);
        let req = crate::protocol::AcpRequest::new("echo");
        let resp = server.handle(&req);
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn test_server_unknown_action() {
        let server = AcpServer::new(ServerConfig::default());
        let req = crate::protocol::AcpRequest::new("unknown");
        let resp = server.handle(&req);
        assert_eq!(resp.status, "error");
    }
}
