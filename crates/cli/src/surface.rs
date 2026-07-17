use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SurfaceMode {
    Interactive,
    Headless,
    Batch,
    Daemon,
}

#[derive(Debug, Clone)]
pub struct CliSurface {
    pub mode: SurfaceMode,
    pub prompt: String,
    pub theme: String,
}

impl Default for CliSurface {
    fn default() -> Self {
        Self {
            mode: SurfaceMode::Interactive,
            prompt: "ardur> ".to_string(),
            theme: "default".to_string(),
        }
    }
}

impl CliSurface {
    pub fn new(mode: SurfaceMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    pub fn with_prompt(mut self, prompt: &str) -> Self {
        self.prompt = prompt.to_string();
        self
    }

    pub fn with_theme(mut self, theme: &str) -> Self {
        self.theme = theme.to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_surface_default() {
        let surface = CliSurface::default();
        assert_eq!(surface.mode, SurfaceMode::Interactive);
        assert_eq!(surface.prompt, "ardur> ");
    }

    #[test]
    fn test_surface_headless() {
        let surface = CliSurface::new(SurfaceMode::Headless);
        assert_eq!(surface.mode, SurfaceMode::Headless);
    }

    #[test]
    fn test_surface_custom_prompt() {
        let surface = CliSurface::default().with_prompt("$ ");
        assert_eq!(surface.prompt, "$ ");
    }
}
