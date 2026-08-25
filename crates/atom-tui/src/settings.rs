use serde::{Deserialize, Serialize};

const MAX_RECENT_MODELS: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PickerSettings {
    // Keep the Go client's field name so both clients share model pins.
    #[serde(default)]
    pub favorites: Vec<ModelRef>,
    #[serde(default)]
    pub recents: Vec<ModelRef>,
    #[serde(default)]
    pub pinned_sessions: Vec<String>,
}

fn path() -> std::path::PathBuf {
    atom_core::session::store::data_dir().join("model-picks.json")
}

pub fn load() -> PickerSettings {
    std::fs::read(path())
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub fn save(settings: &PickerSettings) -> std::io::Result<()> {
    let data = serde_json::to_vec_pretty(settings)?;
    std::fs::write(path(), data)
}

impl PickerSettings {
    pub fn model_ref(provider: &str, model: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    pub fn toggle_model(&mut self, model: ModelRef) -> bool {
        if let Some(i) = self.favorites.iter().position(|item| item == &model) {
            self.favorites.remove(i);
            false
        } else {
            self.favorites.push(model);
            true
        }
    }

    pub fn push_recent(&mut self, model: ModelRef) {
        self.recents.retain(|item| item != &model);
        self.recents.insert(0, model);
        self.recents.truncate(MAX_RECENT_MODELS);
    }

    pub fn toggle_session(&mut self, id: &str) -> bool {
        if let Some(i) = self.pinned_sessions.iter().position(|item| item == id) {
            self.pinned_sessions.remove(i);
            false
        } else {
            self.pinned_sessions.push(id.to_string());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_models_are_unique_and_capped() {
        let mut settings = PickerSettings::default();
        for i in 0..8 {
            settings.push_recent(PickerSettings::model_ref("p", &format!("m{i}")));
        }
        settings.push_recent(PickerSettings::model_ref("p", "m5"));
        assert_eq!(settings.recents.len(), MAX_RECENT_MODELS);
        assert_eq!(settings.recents[0].model, "m5");
        assert_eq!(
            settings
                .recents
                .iter()
                .filter(|item| item.model == "m5")
                .count(),
            1
        );
    }

    #[test]
    fn pins_toggle() {
        let mut settings = PickerSettings::default();
        let model = PickerSettings::model_ref("p", "m");
        assert!(settings.toggle_model(model.clone()));
        assert!(!settings.toggle_model(model));
        assert!(settings.toggle_session("s"));
        assert!(!settings.toggle_session("s"));
    }
}
