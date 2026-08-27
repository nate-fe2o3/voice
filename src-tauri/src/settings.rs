use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Hotkey {
    RightOption,
    F8,
    CommandShiftSpace,
}

impl Hotkey {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::RightOption => "Right Option",
            Self::F8 => "F8",
            Self::CommandShiftSpace => "Command + Shift + Space",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub hotkey: Hotkey,
    pub launch_at_login: bool,
    pub tones_enabled: bool,
    pub overlay_preview: bool,
    pub microphone: Option<String>,
    pub language: String,
    pub preferred_terms: Vec<String>,
    pub excluded_apps: Vec<String>,
    pub onboarding_complete: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: Hotkey::RightOption,
            launch_at_login: true,
            tones_enabled: true,
            overlay_preview: true,
            microphone: None,
            language: "auto".into(),
            preferred_terms: Vec::new(),
            excluded_apps: Vec::new(),
            onboarding_complete: false,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path).context("read settings")?;
        let mut settings: Self = serde_json::from_slice(&bytes).context("parse settings")?;
        settings.normalize()?;
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut normalized = self.clone();
        normalized.normalize()?;
        let parent = path.parent().context("settings path has no parent")?;
        fs::create_dir_all(parent).context("create settings directory")?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(&normalized)?)
            .context("write temporary settings")?;
        fs::rename(&temporary, path).context("replace settings")?;
        Ok(())
    }

    pub fn normalize(&mut self) -> Result<()> {
        if self.language != "auto" && self.language != "english" {
            anyhow::bail!("language must be auto or english");
        }
        self.preferred_terms = normalize_list(&self.preferred_terms, 100, 80)?;
        self.excluded_apps = normalize_list(&self.excluded_apps, 100, 255)?;
        if let Some(microphone) = self.microphone.as_mut() {
            *microphone = microphone.trim().to_string();
            if microphone.is_empty() {
                self.microphone = None;
            }
        }
        Ok(())
    }

    pub fn initial_context(&self) -> Option<String> {
        (!self.preferred_terms.is_empty())
            .then(|| format!("Preferred terms: {}", self.preferred_terms.join(", ")))
    }
}

fn normalize_list(values: &[String], max_items: usize, max_length: usize) -> Result<Vec<String>> {
    if values.len() > max_items {
        anyhow::bail!("too many list entries");
    }
    let mut result = Vec::new();
    for value in values {
        let value = value.trim();
        if value.chars().count() > max_length {
            anyhow::bail!("list entry is too long");
        }
        if !value.is_empty() && !result.iter().any(|existing| existing == value) {
            result.push(value.to_string());
        }
    }
    Ok(result)
}

pub fn settings_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_lists_without_storing_empty_values() {
        let mut settings = Settings {
            preferred_terms: vec![" Qwen ".into(), "".into(), "Qwen".into()],
            ..Settings::default()
        };
        settings.normalize().unwrap();
        assert_eq!(settings.preferred_terms, vec!["Qwen"]);
    }

    #[test]
    fn rejects_unknown_language() {
        let mut settings = Settings {
            language: "klingon".into(),
            ..Settings::default()
        };
        assert!(settings.normalize().is_err());
    }
}
