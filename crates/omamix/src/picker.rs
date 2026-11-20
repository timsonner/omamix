use std::time::{Duration, Instant};

use omamix_core::comp::CompUpdate;
use omamix_core::effect::{EffectBlock, EffectKind, MasterChain};
use omamix_core::eq::{EqSettings, EqUpdate};
use omamix_core::graph::{Scene, Strip};
use omamix_core::route::{CatalogNode, ListedKind, ListedNode, SourceKind};

/// Result of one picker edit.
#[derive(Clone, Debug, PartialEq)]
pub enum EditResult {
    /// Stored value already matched.
    Unchanged,
    /// Scene changed. The full scene submit publishes it.
    Scene,
    /// Scene changed, and this command is the matching engine update.
    Command(EffectCommand),
}

/// One effect edit, ready for [`omamix_pw::EngineCommand`].
#[derive(Clone, Debug, PartialEq)]
pub enum EffectCommand {
    /// Strip EQ field. The window fills the tap slot.
    StripEq { id: u64, update: EqUpdate },
    /// Master EQ field.
    MasterEq(EqUpdate),
    /// Strip compressor field. The window fills the tap slot.
    StripComp { id: u64, update: CompUpdate },
    /// Master compressor field.
    MasterComp(CompUpdate),
    /// Master chain order, bypass, and block settings.
    Chain(Box<MasterChain>),
}

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

    /// Chain delay in milliseconds at `sample_rate` and `quantum`.
    ///
    /// A zero rate uses 48 kHz. A zero quantum uses 256 frames. That pair is
    /// the nominal graph: the default limiter is 5.0 ms.
    pub fn latency_ms(&self, sample_rate: u32, quantum: u32) -> f32 {
        let rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        let frames = if quantum == 0 { 256 } else { quantum };
        let samples = self.scene.master.chain.latency_samples(rate, frames);
        let ms = samples as f32 * 1000.0 / rate as f32;
        if ms.is_finite() && ms >= 0.0 { ms } else { 0.0 }
    }

    pub fn set_strip_eq_enabled(&mut self, id: u64, enabled: bool, now: Instant) -> EditResult {
        let mut update = None;
        let changed = self.edit_strip(id, now, |strip| {
            if strip.eq_enabled == enabled {
                return false;
            }
            strip.eq_enabled = enabled;
            if !enabled && strip.eq != EqSettings::flat(strip.eq.sample_rate()) {
                update = Some(EqUpdate::Reset);
            }
            true
        });
        finish(
            changed,
            update.map(|update| EffectCommand::StripEq { id, update }),
        )
    }

    pub fn set_strip_band(
        &mut self,
        id: u64,
        index: usize,
        gain_db: f32,
        now: Instant,
    ) -> EditResult {
        let mut update = None;
        let changed = self.edit_strip(id, now, |strip| {
            let previous = strip.eq.band_db();
            if !strip.eq.set_band(index, gain_db) {
                return false;
            }
            if strip.eq.band_db() == previous {
                return false;
            }
            update = Some(EqUpdate::Band {
                index: index as u8,
                gain_db: strip.eq.band_db()[index],
            });
            true
        });
        finish(
            changed,
            update.map(|update| EffectCommand::StripEq { id, update }),
        )
    }

    pub fn set_strip_hpf_enabled(&mut self, id: u64, enabled: bool, now: Instant) -> EditResult {
        let mut update = None;
        let changed = self.edit_strip(id, now, |strip| {
            if strip.eq.hpf_enabled() == enabled {
                return false;
            }
            strip.eq.set_hpf_enabled(enabled);
            update = Some(EqUpdate::HighPassEnabled(enabled));
            true
        });
        finish(
            changed,
            update.map(|update| EffectCommand::StripEq { id, update }),
        )
    }

    pub fn set_strip_hpf_hz(&mut self, id: u64, hz: f32, now: Instant) -> EditResult {
        let mut update = None;
        let changed = self.edit_strip(id, now, |strip| {
            let previous = strip.eq.hpf_hz();
            strip.eq.set_hpf_hz(hz);
            if strip.eq.hpf_hz() == previous {
                return false;
            }
            update = Some(EqUpdate::HighPassHz(strip.eq.hpf_hz()));
            true
        });
        finish(
            changed,
            update.map(|update| EffectCommand::StripEq { id, update }),
        )
    }

    pub fn reset_strip_eq(&mut self, id: u64, now: Instant) -> EditResult {
        let mut update = None;
        let changed = self.edit_strip(id, now, |strip| {
            let flat = EqSettings::flat(strip.eq.sample_rate());
            if strip.eq == flat {
                return false;
            }
            let publish = strip.eq_enabled;
            strip.eq = flat;
            if publish {
                update = Some(EqUpdate::Reset);
            }
            true
        });
        finish(
            changed,
            update.map(|update| EffectCommand::StripEq { id, update }),
        )
    }

    pub fn set_strip_comp_enabled(&mut self, id: u64, enabled: bool, now: Instant) -> EditResult {
        self.edit_strip_comp(id, now, |comp| {
            if comp.enabled() == enabled {
                return None;
            }
            comp.set_enabled(enabled);
            Some(CompUpdate::Enabled(enabled))
        })
    }

    pub fn set_strip_comp_threshold(&mut self, id: u64, db: f32, now: Instant) -> EditResult {
        self.edit_strip_comp(id, now, |comp| {
            let previous = comp.threshold_db();
            comp.set_threshold_db(db);
            (comp.threshold_db() != previous)
                .then_some(CompUpdate::ThresholdDb(comp.threshold_db()))
        })
    }

    pub fn set_strip_comp_ratio(&mut self, id: u64, ratio: f32, now: Instant) -> EditResult {
        self.edit_strip_comp(id, now, |comp| {
            let previous = comp.ratio();
            comp.set_ratio(ratio);
            (comp.ratio() != previous).then_some(CompUpdate::Ratio(comp.ratio()))
        })
    }

    pub fn set_strip_comp_attack(&mut self, id: u64, ms: f32, now: Instant) -> EditResult {
        self.edit_strip_comp(id, now, |comp| {
            let previous = comp.attack_ms();
            comp.set_attack_ms(ms);
            (comp.attack_ms() != previous).then_some(CompUpdate::AttackMs(comp.attack_ms()))
        })
    }

    pub fn set_strip_comp_release(&mut self, id: u64, ms: f32, now: Instant) -> EditResult {
        self.edit_strip_comp(id, now, |comp| {
            let previous = comp.release_ms();
            comp.set_release_ms(ms);
            (comp.release_ms() != previous).then_some(CompUpdate::ReleaseMs(comp.release_ms()))
        })
    }

    pub fn set_strip_comp_makeup(&mut self, id: u64, db: f32, now: Instant) -> EditResult {
        self.edit_strip_comp(id, now, |comp| {
            let previous = comp.makeup_db();
            comp.set_makeup_db(db);
            (comp.makeup_db() != previous).then_some(CompUpdate::MakeupDb(comp.makeup_db()))
        })
    }

    pub fn set_master_eq_enabled(&mut self, enabled: bool, now: Instant) -> EditResult {
        if self.scene.master.eq_enabled == enabled {
            return EditResult::Unchanged;
        }
        self.scene.master.eq_enabled = enabled;
        let update = if !enabled
            && self.scene.master.eq != EqSettings::flat(self.scene.master.eq.sample_rate())
        {
            Some(EffectCommand::MasterEq(EqUpdate::Reset))
        } else {
            None
        };
        self.touch(now);
        finish(true, update)
    }

    pub fn set_master_band(&mut self, index: usize, gain_db: f32, now: Instant) -> EditResult {
        let previous = self.scene.master.eq.band_db();
        if !self.scene.master.eq.set_band(index, gain_db) {
            return EditResult::Unchanged;
        }
        if self.scene.master.eq.band_db() == previous {
            return EditResult::Unchanged;
        }
        self.touch(now);
        let update = EqUpdate::Band {
            index: index as u8,
            gain_db: self.scene.master.eq.band_db()[index],
        };
        EditResult::Command(EffectCommand::MasterEq(update))
    }

    pub fn set_master_hpf_enabled(&mut self, enabled: bool, now: Instant) -> EditResult {
        if self.scene.master.eq.hpf_enabled() == enabled {
            return EditResult::Unchanged;
        }
        self.scene.master.eq.set_hpf_enabled(enabled);
        self.touch(now);
        EditResult::Command(EffectCommand::MasterEq(EqUpdate::HighPassEnabled(enabled)))
    }

    pub fn set_master_hpf_hz(&mut self, hz: f32, now: Instant) -> EditResult {
        let previous = self.scene.master.eq.hpf_hz();
        self.scene.master.eq.set_hpf_hz(hz);
        if self.scene.master.eq.hpf_hz() == previous {
            return EditResult::Unchanged;
        }
        self.touch(now);
        let hz = self.scene.master.eq.hpf_hz();
        EditResult::Command(EffectCommand::MasterEq(EqUpdate::HighPassHz(hz)))
    }

    pub fn reset_master_eq(&mut self, now: Instant) -> EditResult {
        let flat = EqSettings::flat(self.scene.master.eq.sample_rate());
        if self.scene.master.eq == flat {
            return EditResult::Unchanged;
        }
        let publish = self.scene.master.eq_enabled;
        self.scene.master.eq = flat;
        self.touch(now);
        if publish {
            EditResult::Command(EffectCommand::MasterEq(EqUpdate::Reset))
        } else {
            EditResult::Scene
        }
    }

    pub fn set_master_comp_enabled(&mut self, enabled: bool, now: Instant) -> EditResult {
        self.edit_master_comp(now, |comp| {
            if comp.enabled() == enabled {
                return None;
            }
            comp.set_enabled(enabled);
            Some(CompUpdate::Enabled(enabled))
        })
    }

    pub fn set_master_comp_threshold(&mut self, db: f32, now: Instant) -> EditResult {
        self.edit_master_comp(now, |comp| {
            let previous = comp.threshold_db();
            comp.set_threshold_db(db);
            (comp.threshold_db() != previous)
                .then_some(CompUpdate::ThresholdDb(comp.threshold_db()))
        })
    }

    pub fn set_master_comp_ratio(&mut self, ratio: f32, now: Instant) -> EditResult {
        self.edit_master_comp(now, |comp| {
            let previous = comp.ratio();
            comp.set_ratio(ratio);
            (comp.ratio() != previous).then_some(CompUpdate::Ratio(comp.ratio()))
        })
    }

    pub fn set_master_comp_attack(&mut self, ms: f32, now: Instant) -> EditResult {
        self.edit_master_comp(now, |comp| {
            let previous = comp.attack_ms();
            comp.set_attack_ms(ms);
            (comp.attack_ms() != previous).then_some(CompUpdate::AttackMs(comp.attack_ms()))
        })
    }

    pub fn set_master_comp_release(&mut self, ms: f32, now: Instant) -> EditResult {
        self.edit_master_comp(now, |comp| {
            let previous = comp.release_ms();
            comp.set_release_ms(ms);
            (comp.release_ms() != previous).then_some(CompUpdate::ReleaseMs(comp.release_ms()))
        })
    }

    pub fn set_master_comp_makeup(&mut self, db: f32, now: Instant) -> EditResult {
        self.edit_master_comp(now, |comp| {
            let previous = comp.makeup_db();
            comp.set_makeup_db(db);
            (comp.makeup_db() != previous).then_some(CompUpdate::MakeupDb(comp.makeup_db()))
        })
    }

    #[cfg(test)]
    pub fn set_master_chain(&mut self, chain: MasterChain, now: Instant) -> EditResult {
        if self.scene.master.chain == chain {
            return EditResult::Unchanged;
        }
        self.scene.master.chain = chain;
        self.touch(now);
        EditResult::Command(EffectCommand::Chain(Box::new(chain)))
    }

    pub fn set_chain_bypass(&mut self, index: usize, bypass: bool, now: Instant) -> EditResult {
        let before = self.scene.master.chain;
        if !self.scene.master.chain.set_bypass(index, bypass) || self.scene.master.chain == before {
            return EditResult::Unchanged;
        }
        let chain = self.scene.master.chain;
        self.touch(now);
        EditResult::Command(EffectCommand::Chain(Box::new(chain)))
    }

    /// Move one block by `direction` slots. `-1` is up, `+1` is down.
    ///
    /// The limiter stays last. A move that would pass it, or move it, is refused.
    pub fn move_chain_block(&mut self, index: usize, direction: i32, now: Instant) -> EditResult {
        if direction == 0 {
            return EditResult::Unchanged;
        }
        let blocks: Vec<EffectBlock> = self.scene.master.chain.blocks().to_vec();
        let len = blocks.len();
        if index >= len {
            return EditResult::Unchanged;
        }
        if blocks[index].kind() == EffectKind::Limiter {
            return EditResult::Unchanged;
        }
        let target = index as i32 + direction;
        if target < 0 || target as usize >= len {
            return EditResult::Unchanged;
        }
        let target = target as usize;
        if blocks[target].kind() == EffectKind::Limiter {
            return EditResult::Unchanged;
        }
        let mut next = blocks;
        next.swap(index, target);
        let (chain, _) = MasterChain::from_blocks(&next);
        if chain == self.scene.master.chain {
            return EditResult::Unchanged;
        }
        self.scene.master.chain = chain;
        self.touch(now);
        EditResult::Command(EffectCommand::Chain(Box::new(chain)))
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

    fn edit_strip_comp(
        &mut self,
        id: u64,
        now: Instant,
        edit: impl FnOnce(&mut omamix_core::comp::CompSettings) -> Option<CompUpdate>,
    ) -> EditResult {
        let mut update = None;
        let changed = self.edit_strip(id, now, |strip| {
            update = edit(&mut strip.comp);
            update.is_some()
        });
        finish(
            changed,
            update.map(|update| EffectCommand::StripComp { id, update }),
        )
    }

    fn edit_master_comp(
        &mut self,
        now: Instant,
        edit: impl FnOnce(&mut omamix_core::comp::CompSettings) -> Option<CompUpdate>,
    ) -> EditResult {
        let update = edit(&mut self.scene.master.comp);
        let Some(update) = update else {
            return EditResult::Unchanged;
        };
        self.scene.master.compressor_enabled = self.scene.master.comp.enabled();
        self.scene.master.sync_chain_params();
        self.touch(now);
        EditResult::Command(EffectCommand::MasterComp(update))
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

fn finish(changed: bool, command: Option<EffectCommand>) -> EditResult {
    if !changed {
        EditResult::Unchanged
    } else if let Some(command) = command {
        EditResult::Command(command)
    } else {
        EditResult::Scene
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
    use omamix_core::eq::EQ_BANDS;
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

    fn one_strip() -> (Picker, u64, Instant) {
        let mut picker = Picker::from_scene(Scene::empty("window"));
        let now = Instant::now();
        assert!(picker.add(PickerChoice::Default, now));
        let id = picker.scene().strips[0].id.0;
        picker.mark_saved();
        (picker, id, now)
    }

    #[test]
    fn eq_band_clamps_resets_flat_and_saves() {
        let (mut picker, id, now) = one_strip();
        assert!(matches!(
            picker.set_strip_band(id, EQ_BANDS, 3.0, now),
            EditResult::Unchanged
        ));
        assert!(matches!(
            picker.set_strip_band(id, 2, 30.0, now),
            EditResult::Command(EffectCommand::StripEq {
                update: EqUpdate::Band {
                    index: 2,
                    gain_db,
                },
                ..
            }) if (gain_db - 12.0).abs() < 1.0e-5
        ));
        assert!(!picker.scene().strips[0].eq_enabled);
        assert!(matches!(
            picker.set_strip_eq_enabled(id, true, now),
            EditResult::Scene
        ));
        assert!((picker.scene().strips[0].eq.band_db()[2] - 12.0).abs() < 1.0e-5);
        assert!(matches!(
            picker.set_strip_band(id, 4, f32::NAN, now),
            EditResult::Unchanged
        ));
        assert!(matches!(
            picker.set_strip_band(id, 0, -40.0, now),
            EditResult::Command(_)
        ));
        assert!((picker.scene().strips[0].eq.band_db()[0] - (-12.0)).abs() < 1.0e-5);
        picker.set_strip_hpf_enabled(id, true, now);
        picker.set_strip_hpf_hz(id, 10_000.0, now);
        assert!(picker.scene().strips[0].eq.hpf_enabled());
        assert!((picker.scene().strips[0].eq.hpf_hz() - 400.0).abs() < 1.0e-4);
        assert!(matches!(
            picker.reset_strip_eq(id, now),
            EditResult::Command(EffectCommand::StripEq {
                update: EqUpdate::Reset,
                ..
            })
        ));
        assert_eq!(
            picker.scene().strips[0].eq,
            EqSettings::flat(picker.scene().strips[0].eq.sample_rate())
        );
        assert!(matches!(
            picker.reset_strip_eq(id, now),
            EditResult::Unchanged
        ));
        assert!(matches!(
            picker.set_master_band(1, 9.0, now),
            EditResult::Command(_)
        ));
        assert!(!picker.scene().master.eq_enabled);
        assert!(matches!(
            picker.set_master_eq_enabled(true, now),
            EditResult::Scene
        ));
        assert!(matches!(
            picker.set_master_band(1, 80.0, now),
            EditResult::Command(EffectCommand::MasterEq(EqUpdate::Band {
                index: 1,
                gain_db,
            })) if (gain_db - 12.0).abs() < 1.0e-5
        ));
        assert!(matches!(
            picker.reset_master_eq(now),
            EditResult::Command(EffectCommand::MasterEq(EqUpdate::Reset))
        ));
        assert!(!picker.scene().master.eq.hpf_enabled());
        assert_eq!(picker.scene().master.eq.band_db(), [0.0; EQ_BANDS]);
        assert!(picker.is_dirty());
        assert!(!picker.save_due(now, SCENE_SAVE_DEBOUNCE));
        let due = now + SCENE_SAVE_DEBOUNCE;
        assert!(picker.save_due(due, SCENE_SAVE_DEBOUNCE));
        picker.set_strip_band(id, 3, 6.0, due);
        picker.set_strip_comp_enabled(id, true, due);
        picker.set_master_comp_enabled(true, due);
        let dir = std::env::temp_dir().join(format!("omamix-eq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("scene.toml");
        omamix_core::save_scene_file(&path, picker.scene()).unwrap();
        picker.mark_saved();
        assert!(!picker.is_dirty());
        let loaded = omamix_core::load_scene_file(&path).unwrap();
        assert!(loaded.strips[0].eq_enabled);
        assert!((loaded.strips[0].eq.band_db()[3] - 6.0).abs() < 1.0e-5);
        assert!(loaded.strips[0].comp.enabled());
        assert!(loaded.master.eq_enabled);
        assert!(loaded.master.comp.enabled());
        assert!(loaded.master.compressor_enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressor_values_clamp() {
        let (mut picker, id, now) = one_strip();
        assert!(matches!(
            picker.set_strip_comp_ratio(id, 80.0, now),
            EditResult::Command(EffectCommand::StripComp {
                update: CompUpdate::Ratio(ratio),
                ..
            }) if (ratio - 20.0).abs() < 1.0e-5
        ));
        assert!(matches!(
            picker.set_strip_comp_threshold(id, 6.0, now),
            EditResult::Command(_)
        ));
        assert!((picker.scene().strips[0].comp.threshold_db() - 0.0).abs() < 1.0e-5);
        assert!(matches!(
            picker.set_strip_comp_threshold(id, -80.0, now),
            EditResult::Command(_)
        ));
        assert!((picker.scene().strips[0].comp.threshold_db() - (-60.0)).abs() < 1.0e-4);
        assert!(matches!(
            picker.set_strip_comp_attack(id, f32::NAN, now),
            EditResult::Unchanged
        ));
        assert!(matches!(
            picker.set_strip_comp_release(id, 9_000.0, now),
            EditResult::Command(_)
        ));
        assert!((picker.scene().strips[0].comp.release_ms() - 2_000.0).abs() < 1.0e-3);
        assert!(matches!(
            picker.set_master_comp_makeup(40.0, now),
            EditResult::Command(EffectCommand::MasterComp(CompUpdate::MakeupDb(db)))
                if (db - 24.0).abs() < 1.0e-5
        ));
        assert!((picker.scene().master.comp.makeup_db() - 24.0).abs() < 1.0e-5);
        assert!(
            (picker
                .scene()
                .master
                .chain
                .compressor_settings()
                .unwrap()
                .makeup_db()
                - 24.0)
                .abs()
                < 1.0e-5
        );
        assert!(picker.is_dirty());
    }

    #[test]
    fn chain_reorder_keeps_the_limiter_last_and_bypass_clears_latency() {
        let (mut picker, _id, now) = one_strip();
        assert!((picker.latency_ms(48_000, 256) - 5.0).abs() < 1.0e-4);
        assert!((picker.latency_ms(0, 0) - 5.0).abs() < 1.0e-4);
        assert!(matches!(
            picker.move_chain_block(0, 1, now),
            EditResult::Unchanged
        ));
        assert!(matches!(
            picker.move_chain_block(1, -1, now),
            EditResult::Unchanged
        ));
        assert_eq!(
            picker.scene().master.chain.kind_ids(),
            vec!["compressor", "limiter"]
        );
        let gain = EffectBlock::gain(-3.0);
        let (chain, warnings) = MasterChain::from_blocks(&[
            gain,
            EffectBlock::compressor(omamix_core::comp::CompSettings::bypass(48_000)),
            EffectBlock::limiter(omamix_core::limit::LimitSettings::master(48_000)),
        ]);
        assert!(warnings.is_empty());
        assert!(matches!(
            picker.set_master_chain(chain, now),
            EditResult::Command(_)
        ));
        assert!(matches!(
            picker.move_chain_block(0, 1, now),
            EditResult::Command(_)
        ));
        assert_eq!(
            picker.scene().master.chain.kind_ids(),
            vec!["compressor", "gain", "limiter"]
        );
        assert!(matches!(
            picker.move_chain_block(1, 1, now),
            EditResult::Unchanged
        ));
        assert_eq!(
            picker.scene().master.chain.blocks().last().unwrap().kind(),
            EffectKind::Limiter
        );
        let limiter = picker.scene().master.chain.len() - 1;
        assert!(matches!(
            picker.set_chain_bypass(limiter, true, now),
            EditResult::Command(_)
        ));
        assert!(picker.scene().master.chain.blocks()[limiter].bypassed());
        assert!((picker.latency_ms(48_000, 256) - 0.0).abs() < 1.0e-6);
        assert!(matches!(
            picker.set_chain_bypass(limiter, true, now),
            EditResult::Unchanged
        ));
        assert!(picker.is_dirty());
        let dir = std::env::temp_dir().join(format!("omamix-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("scene.toml");
        omamix_core::save_scene_file(&path, picker.scene()).unwrap();
        let loaded = omamix_core::load_scene_file(&path).unwrap();
        assert_eq!(
            loaded.master.chain.kind_ids(),
            picker.scene().master.chain.kind_ids()
        );
        assert!(loaded.master.chain.blocks()[limiter].bypassed());
        picker.mark_saved();
        assert!(!picker.is_dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_the_eq_off_keeps_the_bands_and_requests_a_flat_publish() {
        let (mut picker, id, now) = one_strip();
        picker.set_strip_band(id, 5, 3.0, now);
        picker.set_strip_eq_enabled(id, true, now);
        assert!(matches!(
            picker.set_strip_eq_enabled(id, false, now),
            EditResult::Command(EffectCommand::StripEq {
                update: EqUpdate::Reset,
                ..
            })
        ));
        assert!(!picker.scene().strips[0].eq_enabled);
        assert!((picker.scene().strips[0].eq.band_db()[5] - 3.0).abs() < 1.0e-5);
        picker.set_master_band(6, -2.0, now);
        picker.set_master_eq_enabled(true, now);
        assert!(matches!(
            picker.set_master_eq_enabled(false, now),
            EditResult::Command(EffectCommand::MasterEq(EqUpdate::Reset))
        ));
        assert!(!picker.scene().master.eq_enabled);
        assert!((picker.scene().master.eq.band_db()[6] - (-2.0)).abs() < 1.0e-5);
    }
}
