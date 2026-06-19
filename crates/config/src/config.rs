use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    List(Vec<ConfigValue>),
    Map(HashMap<String, ConfigValue>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConfigSource {
    Env,
    File,
    Cli,
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub values: HashMap<String, ConfigValue>,
    pub sources: HashMap<String, ConfigSource>,
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: &str, value: ConfigValue, source: ConfigSource) {
        self.values.insert(key.to_string(), value);
        self.sources.insert(key.to_string(), source);
    }

    pub fn get(&self, key: &str) -> crate::error::Result<&ConfigValue> {
        self.values
            .get(key)
            .ok_or_else(|| crate::error::ConfigError::KeyNotFound(key.to_string()))
    }

    pub fn get_string(&self, key: &str) -> crate::error::Result<String> {
        match self.get(key)? {
            ConfigValue::String(s) => Ok(s.clone()),
            _ => Err(crate::error::ConfigError::InvalidValue(format!(
                "expected string for key {}",
                key
            ))),
        }
    }

    pub fn merge(&mut self, other: Config) {
        for (k, v) in other.values {
            if !self.values.contains_key(&k) {
                self.values.insert(k.clone(), v);
                self.sources.insert(k, ConfigSource::Default);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_set_and_get() {
        let mut config = Config::new();
        config.set(
            "key1",
            ConfigValue::String("value1".to_string()),
            ConfigSource::Env,
        );
        assert_eq!(config.get_string("key1").unwrap(), "value1");
    }

    #[test]
    fn test_config_get_missing() {
        let config = Config::new();
        assert!(config.get("missing").is_err());
    }

    #[test]
    fn test_config_merge() {
        let mut config1 = Config::new();
        config1.set("a", ConfigValue::String("1".to_string()), ConfigSource::Env);
        let mut config2 = Config::new();
        config2.set(
            "b",
            ConfigValue::String("2".to_string()),
            ConfigSource::File,
        );
        config1.merge(config2);
        assert!(config1.get("a").is_ok());
        assert!(config1.get("b").is_ok());
    }

    #[test]
    fn test_config_merge_no_override() {
        let mut config1 = Config::new();
        config1.set("a", ConfigValue::String("1".to_string()), ConfigSource::Env);
        let mut config2 = Config::new();
        config2.set(
            "a",
            ConfigValue::String("2".to_string()),
            ConfigSource::File,
        );
        config1.merge(config2);
        assert_eq!(config1.get_string("a").unwrap(), "1");
    }
}
