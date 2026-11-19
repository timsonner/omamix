use std::time::{Duration, Instant};

use omamix_core::graph::{Scene, Strip};
use omamix_core::route::{CatalogNode, ListedKind, ListedNode, SourceKind};

pub const SCENE_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerChoice {
    Default,
    Listed(usize),
}

#[derive(Clone, Debug)]
pub struct Picker {
    scene: Scene,
    catalog: Vec<ListedNode>,
    default_source: Option<String>,
    selected: PickerChoice,
    dirty_at: Option<Instant>,
    retargeted: bool,
}

impl Picker {
    pub fn from_scene(scene: Scene) -> Self {
        Self {
            scene,
            catalog: Vec::new(),
            default_source: None,
            selected: PickerChoice::Default,
            dirty_at: None,
            retargeted: false,
        }
    }

    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    pub fn catalog(&self) -> &[ListedNode] {
        &self.catalog
    }

    pub fn default_source(&self) -> Option<&str> {
        self.default_source.as_deref()
    }

    pub fn selected(&self) -> PickerChoice {
        self.selected
    }

    pub fn select(&mut self, choice: PickerChoice) {
        match choice {
            PickerChoice::Default => self.selected = PickerChoice::Default,
            PickerChoice::Listed(index) if index < self.catalog.len() => {
                self.selected = PickerChoice::Listed(index);
            }
            PickerChoice::Listed(_) => self.selected = PickerChoice::Default,
        }
    }

    pub fn refresh(&mut self, nodes: Vec<ListedNode>, default_source: Option<String>) {
        self.catalog = nodes;
        self.default_source = default_source.filter(|name| !name.is_empty());
        if let PickerChoice::Listed(index) = self.selected
            && index >= self.catalog.len()
        {
            self.selected = PickerChoice::Default;
        }
    }

    pub fn add(&mut self, choice: PickerChoice, now: Instant) -> bool {
        let Some(source) = self.source_for(choice) else {
            return false;
        };
        if self
            .scene
            .strips
            .iter()
            .any(|strip| conflicts(&strip.source, &source, self.default_source.as_deref()))
        {
            return false;
        }
        let name = match &source {
            SourceKind::DefaultSource => "default".to_string(),
            SourceKind::CaptureDevice { node_name } => node_name.clone(),
            SourceKind::Application(key) => key.application_name.clone(),
            SourceKind::OtherDesktopAudio => "other".to_string(),
        };
        let id = self
            .scene
            .strips
            .iter()
            .map(|strip| strip.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.scene.strips.push(Strip::new(id, name, source));
        self.touch(now);
        true
    }

    pub fn remove(&mut self, id: u64, now: Instant) -> bool {
        let before = self.scene.strips.len();
        self.scene.strips.retain(|strip| strip.id.0 != id);
        if self.scene.strips.len() == before {
            return false;
        }
        self.touch(now);
        true
    }

    pub fn set_fader(&mut self, id: u64, db: f32, now: Instant) -> bool {
        self.edit_strip(id, now, |strip| {
            if strip.fader_db == db {
                false
            } else {
                strip.fader_db = db;
                true
            }
        })
    }

    pub fn set_trim(&mut self, id: u64, db: f32, now: Instant) -> bool {
        self.edit_strip(id, now, |strip| {
            let previous = strip.input_gain_db;
            strip.set_input_gain_db(db);
            strip.input_gain_db != previous
        })
    }

    pub fn set_mute(&mut self, id: u64, mute: bool, now: Instant) -> bool {
        self.edit_strip(id, now, |strip| {
            if strip.mute == mute {
                false
            } else {
                strip.mute = mute;
                true
            }
        })
    }

    pub fn set_solo(&mut self, id: u64, solo: bool, now: Instant) -> bool {
        self.edit_strip(id, now, |strip| {
            if strip.solo == solo {
                false
            } else {
                strip.solo = solo;
                true
            }
        })
    }

    pub fn set_master_fader(&mut self, db: f32, now: Instant) -> bool {
        let previous = self.scene.master.fader_db();
        self.scene.master.set_fader_db(db);
        if self.scene.master.fader_db() == previous {
            return false;
        }
        self.touch(now);
        true
    }

    pub fn retarget_missing_once(&mut self) -> bool {
        if self.retargeted {
            return false;
        }
        self.retargeted = true;
        let nodes = catalog_from_listed(&self.catalog);
        let missing = omamix_core::route::retarget_absent_devices(&mut self.scene.strips, &nodes);
        !missing.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_at.is_some()
    }

    pub fn save_due(&self, now: Instant, debounce: Duration) -> bool {
        self.dirty_at
            .is_some_and(|at| now.saturating_duration_since(at) >= debounce)
    }

    pub fn mark_saved(&mut self) {
        self.dirty_at = None;
    }

    fn source_for(&self, choice: PickerChoice) -> Option<SourceKind> {
        match choice {
            PickerChoice::Default => Some(SourceKind::DefaultSource),
            PickerChoice::Listed(index) => self.catalog.get(index).map(source_from_listed),
        }
    }

    fn edit_strip(&mut self, id: u64, now: Instant, edit: impl FnOnce(&mut Strip) -> bool) -> bool {
        let Some(strip) = self.scene.strips.iter_mut().find(|strip| strip.id.0 == id) else {
            return false;
        };
        if !edit(strip) {
            return false;
        }
        self.touch(now);
        true
    }

    fn touch(&mut self, now: Instant) {
        self.dirty_at = Some(now);
    }
}

pub fn source_from_listed(node: &ListedNode) -> SourceKind {
    match node.kind {
        ListedKind::Device => SourceKind::CaptureDevice {
            node_name: node.node_name.clone(),
        },
        ListedKind::App => SourceKind::Application(omamix_core::route::RouteKey::new(
            node.application_name.clone(),
            node.binary.clone(),
        )),
    }
}

pub fn choice_label(node: &ListedNode) -> String {
    match node.kind {
        ListedKind::Device if node.description.is_empty() => {
            format!("device {}", node.node_name)
        }
        ListedKind::Device => format!("device {} ({})", node.description, node.node_name),
        ListedKind::App => format!("app {} ({})", node.application_name, node.binary),
    }
}

fn conflicts(existing: &SourceKind, incoming: &SourceKind, default_source: Option<&str>) -> bool {
    if same_source(existing, incoming) {
        return true;
    }
    let Some(default_source) = default_source else {
        return false;
    };
    match (existing, incoming) {
        (SourceKind::DefaultSource, SourceKind::CaptureDevice { node_name })
        | (SourceKind::CaptureDevice { node_name }, SourceKind::DefaultSource) => {
            node_name == default_source
        }
        _ => false,
    }
}

fn same_source(left: &SourceKind, right: &SourceKind) -> bool {
    match (left, right) {
        (SourceKind::DefaultSource, SourceKind::DefaultSource)
        | (SourceKind::OtherDesktopAudio, SourceKind::OtherDesktopAudio) => true,
        (
            SourceKind::CaptureDevice { node_name: left },
            SourceKind::CaptureDevice { node_name: right },
        ) => left == right,
        (SourceKind::Application(left), SourceKind::Application(right)) => {
            left.application_name == right.application_name
                && left.binary == right.binary
                && left.media_name == right.media_name
        }
        _ => false,
    }
}

fn catalog_from_listed(nodes: &[ListedNode]) -> Vec<CatalogNode> {
    nodes
        .iter()
        .map(|node| {
            let (media_class, application_name, binary) = match node.kind {
                ListedKind::Device => ("Audio/Source", String::new(), String::new()),
                ListedKind::App => (
                    "Stream/Output/Audio",
                    node.application_name.clone(),
                    node.binary.clone(),
                ),
            };
            CatalogNode {
                node_name: node.node_name.clone(),
                description: node.description.clone(),
                media_class: media_class.to_string(),
                application_name,
                binary,
                client_id: None,
                pid: None,
                omamix_owned: false,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use omamix_core::route::ListedKind;

    fn device(name: &str) -> ListedNode {
        ListedNode {
            kind: ListedKind::Device,
            node_name: name.to_string(),
            description: String::new(),
            application_name: String::new(),
            binary: String::new(),
        }
    }

    fn app(name: &str, binary: &str) -> ListedNode {
        ListedNode {
            kind: ListedKind::App,
            node_name: format!("stream-{name}"),
            description: String::new(),
            application_name: name.to_string(),
            binary: binary.to_string(),
        }
    }

    #[test]
    fn refresh_replaces_the_picker_list() {
        let mut picker = Picker::from_scene(Scene::empty("window"));
        picker.refresh(vec![device("node-a"), app("player", "player")], None);
        picker.select(PickerChoice::Listed(1));
        assert_eq!(picker.catalog().len(), 2);
        assert_eq!(picker.catalog()[0].node_name, "node-a");
        assert_eq!(picker.catalog()[1].application_name, "player");
        picker.refresh(vec![device("node-b")], Some("node-b".into()));
        assert_eq!(picker.catalog().len(), 1);
        assert_eq!(picker.catalog()[0].node_name, "node-b");
        assert_eq!(picker.default_source(), Some("node-b"));
        assert_eq!(picker.selected(), PickerChoice::Default);
    }

    #[test]
    fn add_and_remove_default_device_and_app() {
        let mut picker = Picker::from_scene(Scene::empty("window"));
        let now = Instant::now();
        picker.refresh(
            vec![device("node-a"), app("player", "player")],
            Some("node-a".into()),
        );
        assert!(picker.add(PickerChoice::Default, now));
        assert!(picker.add(PickerChoice::Listed(1), now));
        assert!(!picker.add(PickerChoice::Default, now));
        assert!(!picker.add(PickerChoice::Listed(0), now));
        assert_eq!(picker.scene().strips.len(), 2);
        assert!(matches!(
            picker.scene().strips[0].source,
            SourceKind::DefaultSource
        ));
        assert!(matches!(
            picker.scene().strips[1].source,
            SourceKind::Application(_)
        ));
        let app_id = picker.scene().strips[1].id.0;
        assert!(picker.remove(app_id, now));
        assert!(!picker.remove(app_id, now));
        assert_eq!(picker.scene().strips.len(), 1);
        picker.refresh(
            vec![device("node-b"), device("node-a")],
            Some("other".into()),
        );
        assert!(picker.add(PickerChoice::Listed(0), now));
        assert_eq!(picker.scene().strips.len(), 2);
    }

    #[test]
    fn changes_save_only_after_the_debounce_and_the_file_matches() {
        let dir = std::env::temp_dir().join(format!("omamix-picker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("scene.toml");
        let mut picker = Picker::from_scene(Scene::empty("window"));
        let start = Instant::now();
        assert!(picker.add(PickerChoice::Default, start));
        let id = picker.scene().strips[0].id.0;
        assert!(picker.set_fader(id, -6.0, start));
        assert!(picker.set_trim(id, 3.0, start));
        assert!(picker.set_mute(id, true, start));
        assert!(picker.set_master_fader(-3.0, start));
        assert!(picker.is_dirty());
        assert!(!picker.save_due(start + Duration::from_millis(399), SCENE_SAVE_DEBOUNCE));
        let later = start + Duration::from_millis(200);
        assert!(picker.set_fader(id, -8.0, later));
        assert!(!picker.save_due(later + Duration::from_millis(399), SCENE_SAVE_DEBOUNCE));
        let due = later + SCENE_SAVE_DEBOUNCE;
        assert!(picker.save_due(due, SCENE_SAVE_DEBOUNCE));
        omamix_core::save_scene_file(&path, picker.scene()).unwrap();
        picker.mark_saved();
        assert!(!picker.is_dirty());
        assert!(!picker.save_due(due + SCENE_SAVE_DEBOUNCE, SCENE_SAVE_DEBOUNCE));
        let loaded = omamix_core::load_scene_file(&path).unwrap();
        assert_eq!(loaded.strips.len(), 1);
        assert_eq!(loaded.strips[0].fader_db, -8.0);
        assert_eq!(loaded.strips[0].input_gain_db, 3.0);
        assert!(loaded.strips[0].mute);
        assert_eq!(loaded.master.fader_db(), -3.0);
        assert!(matches!(loaded.strips[0].source, SourceKind::DefaultSource));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
