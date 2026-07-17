//! Media tool stubs: voice (STT/TTS), image generation, and image analysis.
//!
//! These are placeholder implementations that return mock data. Full
//! integrations (Whisper API, ElevenLabs, DALL-E, vision models) will
//! replace these stubs in a future iteration.

use serde_json::json;

use crate::{Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema};

/// Speech-to-text: transcribe audio to text.
#[derive(Clone, Debug)]
pub struct SttTool {
    schema: ToolSchema,
}

impl Default for SttTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SttTool {
    /// Create a new STT tool stub.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Transcribe audio to text using speech-to-text".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "audio": { "type": "string", "description": "Base64-encoded audio data" }
                    },
                    "required": ["audio"]
                }),
                output_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "language": { "type": "string" },
                        "confidence": { "type": "number" }
                    }
                }),
                examples: Vec::new(),
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for SttTool {
    fn id(&self) -> ToolId {
        ToolId::new("whisper.transcribe")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!({
                "text": "This is a mock transcription. Replace with real STT integration.",
                "language": "en",
                "confidence": 0.95
            }),
            cost: ardur_runtime::CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::VoiceInput]
    }
}

/// Text-to-speech: convert text to audio bytes.
#[derive(Clone, Debug)]
pub struct TtsTool {
    schema: ToolSchema,
}

impl Default for TtsTool {
    fn default() -> Self {
        Self::new()
    }
}

impl TtsTool {
    /// Create a new TTS tool stub.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Convert text to speech audio".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Text to speak" },
                        "voice": { "type": "string", "description": "Voice ID (optional)" }
                    },
                    "required": ["text"]
                }),
                output_schema: json!({
                    "type": "object",
                    "properties": {
                        "audio_base64": { "type": "string" },
                        "format": { "type": "string" },
                        "duration_seconds": { "type": "number" }
                    }
                }),
                examples: Vec::new(),
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for TtsTool {
    fn id(&self) -> ToolId {
        ToolId::new("tts.speak")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!({
                "audio_base64": "bW9jay1hdWRpby1kYXRh",
                "format": "mp3",
                "duration_seconds": 3.5
            }),
            cost: ardur_runtime::CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::VoiceOutput]
    }
}

/// Voice note: record audio and transcribe.
#[derive(Clone, Debug)]
pub struct VoiceNoteTool {
    schema: ToolSchema,
}

impl Default for VoiceNoteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceNoteTool {
    /// Create a new voice note tool stub.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Record a voice note and transcribe it to text".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "duration_seconds": { "type": "integer", "description": "Max recording duration" }
                    }
                }),
                output_schema: json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" },
                        "language": { "type": "string" },
                        "duration_seconds": { "type": "number" }
                    }
                }),
                examples: Vec::new(),
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for VoiceNoteTool {
    fn id(&self) -> ToolId {
        ToolId::new("voice.record")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!({
                "text": "Mock voice note transcription. Replace with real recording + STT.",
                "language": "en",
                "duration_seconds": 5.0
            }),
            cost: ardur_runtime::CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::VoiceInput]
    }
}

/// Image generation: create an image from a prompt.
#[derive(Clone, Debug)]
pub struct ImageGenerateTool {
    schema: ToolSchema,
}

impl Default for ImageGenerateTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageGenerateTool {
    /// Create a new image generation tool stub.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Generate an image from a text prompt".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "Image description" },
                        "style": { "type": "string", "description": "Art style (optional)" },
                        "size": { "type": "string", "description": "Image size (e.g. 1024x1024)" }
                    },
                    "required": ["prompt"]
                }),
                output_schema: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "prompt": { "type": "string" },
                        "size": { "type": "string" }
                    }
                }),
                examples: Vec::new(),
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for ImageGenerateTool {
    fn id(&self) -> ToolId {
        ToolId::new("image.generate")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(ToolOutput {
            content: json!({
                "url": "https://example.com/mock-image.png",
                "prompt": prompt,
                "size": args.get("size").and_then(|v| v.as_str()).unwrap_or("1024x1024")
            }),
            cost: ardur_runtime::CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::ImageGenerate]
    }
}

/// Image analysis: describe an image.
#[derive(Clone, Debug)]
pub struct ImageAnalyzeTool {
    schema: ToolSchema,
}

impl Default for ImageAnalyzeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageAnalyzeTool {
    /// Create a new image analysis tool stub.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                description: "Analyze and describe an image".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "image_url": { "type": "string", "description": "URL of the image to analyze" },
                        "detail": { "type": "string", "description": "Detail level: low/medium/high" }
                    },
                    "required": ["image_url"]
                }),
                output_schema: json!({
                    "type": "object",
                    "properties": {
                        "description": { "type": "string" },
                        "image_url": { "type": "string" },
                        "objects": { "type": "array", "items": { "type": "string" } },
                        "confidence": { "type": "number" }
                    }
                }),
                examples: Vec::new(),
            },
        }
    }
}

#[async_trait::async_trait]
impl Tool for ImageAnalyzeTool {
    fn id(&self) -> ToolId {
        ToolId::new("image.describe")
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let image_url = args
            .get("image_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(ToolOutput {
            content: json!({
                "description": "A mock image description. Replace with real vision model integration.",
                "image_url": image_url,
                "objects": ["mock_object_1", "mock_object_2"],
                "confidence": 0.92
            }),
            cost: ardur_runtime::CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[Capability::ImageAnalyze]
    }
}
