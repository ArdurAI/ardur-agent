use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
    List,
    Map,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaField {
    pub name: String,
    pub field_type: FieldType,
    pub required: bool,
    pub default: Option<crate::config::ConfigValue>,
    pub description: String,
}

impl SchemaField {
    pub fn new(name: &str, field_type: FieldType) -> Self {
        Self {
            name: name.to_string(),
            field_type,
            required: true,
            default: None,
            description: String::new(),
        }
    }

    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    pub fn with_default(mut self, default: crate::config::ConfigValue) -> Self {
        self.default = Some(default);
        self
    }

    pub fn with_description(mut self, desc: &str) -> Self {
        self.description = desc.to_string();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSchema {
    pub fields: Vec<SchemaField>,
}

impl ConfigSchema {
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    pub fn add_field(mut self, field: SchemaField) -> Self {
        self.fields.push(field);
        self
    }

    pub fn validate(&self, config: &crate::config::Config) -> crate::error::Result<()> {
        for field in &self.fields {
            if field.required && !config.values.contains_key(&field.name) {
                return Err(crate::error::ConfigError::SchemaValidationFailed(format!(
                    "required field '{}' is missing",
                    field.name
                )));
            }
        }
        Ok(())
    }
}

impl Default for ConfigSchema {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_field_creation() {
        let field = SchemaField::new("test", FieldType::String)
            .optional()
            .with_description("A test field");
        assert!(!field.required);
        assert_eq!(field.description, "A test field");
    }

    #[test]
    fn test_schema_validation_pass() {
        let schema = ConfigSchema::new().add_field(SchemaField::new("key", FieldType::String));
        let mut config = crate::config::Config::new();
        config.set(
            "key",
            crate::config::ConfigValue::String("value".to_string()),
            crate::config::ConfigSource::Env,
        );
        assert!(schema.validate(&config).is_ok());
    }

    #[test]
    fn test_schema_validation_fail_missing() {
        let schema = ConfigSchema::new().add_field(SchemaField::new("key", FieldType::String));
        let config = crate::config::Config::new();
        assert!(schema.validate(&config).is_err());
    }

    #[test]
    fn test_schema_validation_optional() {
        let schema =
            ConfigSchema::new().add_field(SchemaField::new("key", FieldType::String).optional());
        let config = crate::config::Config::new();
        assert!(schema.validate(&config).is_ok());
    }
}
