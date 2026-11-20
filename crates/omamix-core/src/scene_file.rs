use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::comp::{
    COMP_ATTACK_DEFAULT_MS, COMP_KNEE_DEFAULT_DB, COMP_MAKEUP_DEFAULT_DB, COMP_RATIO_DEFAULT,
    COMP_RELEASE_DEFAULT_MS, COMP_THRESHOLD_DEFAULT_DB, CompSettings,
};
use crate::effect::{EffectBlock, EffectKind, EffectParams, MasterChain};
use crate::eq::{EQ_BANDS, EqSettings, HPF_DEFAULT_HZ};
use crate::fader;
use crate::graph::{MasterBus, Scene, Strip};
use crate::limit::{LIMIT_CEILING_DEFAULT_DB, LIMIT_LOOKAHEAD_DEFAULT_MS, LimitSettings};
use crate::route::{RouteKey, SourceKind, parse_strip_request};

const SCENE_NAME: &str = "scene.toml";

#[derive(Debug)]
pub enum SceneFileError {
    Missing,
    NoHome,
    Io(std::io::Error),
    Format(String),
}

impl SceneFileError {
    pub fn is_missing(&self) -> bool {
        matches!(self, SceneFileError::Missing)
    }
}

impl std::fmt::Display for SceneFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SceneFileError::Missing => write!(f, "scene file is missing"),
            SceneFileError::NoHome => {
                write!(f, "XDG_CONFIG_HOME and HOME are unset")
            }
            SceneFileError::Io(err) => write!(f, "{err}"),
            SceneFileError::Format(err) => write!(f, "scene file is invalid: {err}"),
        }
    }
}

impl std::error::Error for SceneFileError {}

pub fn default_scene_path() -> Result<PathBuf, SceneFileError> {
    scene_config_path(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

pub fn scene_config_path(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, SceneFileError> {
    if let Some(xdg) = xdg_config_home.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("omamix").join(SCENE_NAME));
    }
    let Some(home) = home.filter(|value| !value.is_empty()) else {
        return Err(SceneFileError::NoHome);
    };
    Ok(PathBuf::from(home)
        .join(".config")
        .join("omamix")
        .join(SCENE_NAME))
}

pub fn save_scene_file(path: &Path, scene: &Scene) -> Result<(), SceneFileError> {
    let body = scene_to_toml(scene)?;
    write_atomic(path, &body)
}

pub fn load_scene_file(path: &Path) -> Result<Scene, SceneFileError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(SceneFileError::Missing);
        }
        Err(err) => return Err(SceneFileError::Io(err)),
    };
    scene_from_toml(&text)
}

pub fn scene_to_toml(scene: &Scene) -> Result<String, SceneFileError> {
    let document = SceneDocument::from_scene(scene);
    toml::to_string_pretty(&document).map_err(|err| SceneFileError::Format(err.to_string()))
}

pub fn scene_from_toml(text: &str) -> Result<Scene, SceneFileError> {
    let document: SceneDocument =
        toml::from_str(text).map_err(|err| SceneFileError::Format(err.to_string()))?;
    document.into_scene()
}

fn write_atomic(path: &Path, body: &str) -> Result<(), SceneFileError> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&parent).map_err(SceneFileError::Io)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(SCENE_NAME);
    let tmp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let write_result = (|| -> Result<(), SceneFileError> {
        let mut file = File::create(&tmp).map_err(SceneFileError::Io)?;
        file.write_all(body.as_bytes())
            .map_err(SceneFileError::Io)?;
        file.sync_all().map_err(SceneFileError::Io)?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(SceneFileError::Io(err));
    }
    Ok(())
}

fn default_version() -> u32 {
    1
}

fn default_rate() -> u32 {
    48_000
}

fn default_hpf() -> f32 {
    HPF_DEFAULT_HZ
}

fn default_threshold() -> f32 {
    COMP_THRESHOLD_DEFAULT_DB
}

fn default_ratio() -> f32 {
    COMP_RATIO_DEFAULT
}

fn default_attack() -> f32 {
    COMP_ATTACK_DEFAULT_MS
}

fn default_release() -> f32 {
    COMP_RELEASE_DEFAULT_MS
}

fn default_knee() -> f32 {
    COMP_KNEE_DEFAULT_DB
}

fn default_makeup() -> f32 {
    COMP_MAKEUP_DEFAULT_DB
}

fn default_ceiling() -> f32 {
    LIMIT_CEILING_DEFAULT_DB
}

fn default_lookahead() -> f32 {
    LIMIT_LOOKAHEAD_DEFAULT_MS
}

fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SceneDocument {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    master: MasterDocument,
    #[serde(default)]
    strips: Vec<StripDocument>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct MasterDocument {
    #[serde(default)]
    fader_db: f32,
    #[serde(default)]
    eq_enabled: bool,
    #[serde(default)]
    compressor_enabled: bool,
    #[serde(default = "default_enabled")]
    limiter_enabled: bool,
    #[serde(default = "default_ceiling")]
    limiter_ceiling_dbtp: f32,
    #[serde(default)]
    monitor_mix: bool,
    #[serde(default)]
    eq: EqDocument,
    #[serde(default)]
    compressor: CompDocument,
    #[serde(default)]
    limiter: LimitDocument,
    /// Absent on a scene written before the chain. Load builds the default.
    #[serde(default)]
    chain: Option<Vec<ChainEntryDocument>>,
}

impl Default for MasterDocument {
    fn default() -> Self {
        Self {
            fader_db: 0.0,
            eq_enabled: false,
            compressor_enabled: false,
            limiter_enabled: true,
            limiter_ceiling_dbtp: LIMIT_CEILING_DEFAULT_DB,
            monitor_mix: false,
            eq: EqDocument::default(),
            compressor: CompDocument::default(),
            limiter: LimitDocument::default(),
            chain: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct StripDocument {
    role: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    fader_db: f32,
    #[serde(default)]
    trim_db: f32,
    #[serde(default)]
    mute: bool,
    #[serde(default)]
    solo: bool,
    #[serde(default)]
    pan: f32,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    monitor_headphones: bool,
    #[serde(default)]
    eq_enabled: bool,
    #[serde(default)]
    eq: EqDocument,
    #[serde(default)]
    compressor: CompDocument,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct EqDocument {
    #[serde(default = "default_rate")]
    sample_rate: u32,
    #[serde(default)]
    hpf_enabled: bool,
    #[serde(default = "default_hpf")]
    hpf_hz: f32,
    #[serde(default)]
    bands: [f32; EQ_BANDS],
}

impl Default for EqDocument {
    fn default() -> Self {
        Self::from_settings(EqSettings::flat(48_000))
    }
}

impl EqDocument {
    fn from_settings(settings: EqSettings) -> Self {
        Self {
            sample_rate: settings.sample_rate(),
            hpf_enabled: settings.hpf_enabled(),
            hpf_hz: settings.hpf_hz(),
            bands: settings.band_db(),
        }
    }

    fn into_settings(self) -> EqSettings {
        let mut settings = EqSettings::flat(self.sample_rate);
        settings.set_hpf_enabled(self.hpf_enabled);
        settings.set_hpf_hz(self.hpf_hz);
        for (index, gain) in self.bands.into_iter().enumerate() {
            let _ = settings.set_band(index, gain);
        }
        settings
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct CompDocument {
    #[serde(default = "default_rate")]
    sample_rate: u32,
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_threshold")]
    threshold_db: f32,
    #[serde(default = "default_ratio")]
    ratio: f32,
    #[serde(default = "default_attack")]
    attack_ms: f32,
    #[serde(default = "default_release")]
    release_ms: f32,
    #[serde(default = "default_knee")]
    knee_db: f32,
    #[serde(default = "default_makeup")]
    makeup_db: f32,
}

impl Default for CompDocument {
    fn default() -> Self {
        Self::from_settings(CompSettings::bypass(48_000))
    }
}

impl CompDocument {
    fn from_settings(settings: CompSettings) -> Self {
        Self {
            sample_rate: settings.sample_rate(),
            enabled: settings.enabled(),
            threshold_db: settings.threshold_db(),
            ratio: settings.ratio(),
            attack_ms: settings.attack_ms(),
            release_ms: settings.release_ms(),
            knee_db: settings.knee_db(),
            makeup_db: settings.makeup_db(),
        }
    }

    fn into_settings(self) -> CompSettings {
        let mut settings = CompSettings::bypass(self.sample_rate);
        settings.set_enabled(self.enabled);
        settings.set_threshold_db(self.threshold_db);
        settings.set_ratio(self.ratio);
        settings.set_attack_ms(self.attack_ms);
        settings.set_release_ms(self.release_ms);
        settings.set_knee_db(self.knee_db);
        settings.set_makeup_db(self.makeup_db);
        settings
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct LimitDocument {
    #[serde(default = "default_rate")]
    sample_rate: u32,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_ceiling")]
    ceiling_db: f32,
    #[serde(default = "default_lookahead")]
    lookahead_ms: f32,
}

impl Default for LimitDocument {
    fn default() -> Self {
        Self::from_settings(LimitSettings::master(48_000))
    }
}

impl LimitDocument {
    fn from_settings(settings: LimitSettings) -> Self {
        Self {
            sample_rate: settings.sample_rate(),
            enabled: settings.enabled(),
            ceiling_db: settings.ceiling_db(),
            lookahead_ms: settings.lookahead_ms(),
        }
    }

    fn into_settings(self) -> LimitSettings {
        let mut settings = LimitSettings::bypass(self.sample_rate);
        settings.set_enabled(self.enabled);
        settings.set_ceiling_db(self.ceiling_db);
        settings.set_lookahead_ms(self.lookahead_ms);
        settings
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ChainEntryDocument {
    kind: String,
    #[serde(default)]
    bypass: bool,
    #[serde(default)]
    params: Option<toml::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct GainParamDocument {
    #[serde(default)]
    gain_db: f32,
}

impl Default for GainParamDocument {
    fn default() -> Self {
        Self { gain_db: 0.0 }
    }
}

impl SceneDocument {
    fn from_scene(scene: &Scene) -> Self {
        Self {
            version: default_version(),
            name: scene.name.clone(),
            master: MasterDocument::from_master(&scene.master),
            strips: scene.strips.iter().map(StripDocument::from_strip).collect(),
        }
    }

    fn into_scene(self) -> Result<Scene, SceneFileError> {
        let mut scene = Scene::empty(self.name);
        scene.master = self.master.into_master()?;
        for (index, strip) in self.strips.into_iter().enumerate() {
            scene.strips.push(strip.into_strip(index)?);
        }
        Ok(scene)
    }
}

impl MasterDocument {
    fn from_master(master: &MasterBus) -> Self {
        Self {
            fader_db: finite_fader(master.fader_db()),
            eq_enabled: master.eq_enabled,
            compressor_enabled: master.compressor_enabled,
            limiter_enabled: master.limiter_enabled,
            limiter_ceiling_dbtp: master.limiter_ceiling_dbtp,
            monitor_mix: master.monitor_mix,
            eq: EqDocument::from_settings(master.eq),
            compressor: CompDocument::from_settings(master.comp),
            limiter: LimitDocument::from_settings(master.limit),
            chain: Some(chain_entries(&master.chain)),
        }
    }

    fn into_master(self) -> Result<MasterBus, SceneFileError> {
        let mut master = MasterBus::default();
        master.set_fader_db(self.fader_db);
        master.eq_enabled = self.eq_enabled;
        master.compressor_enabled = self.compressor_enabled;
        master.limiter_enabled = self.limiter_enabled;
        master.limiter_ceiling_dbtp = self.limiter_ceiling_dbtp;
        master.monitor_mix = self.monitor_mix;
        master.eq = self.eq.into_settings();
        master.comp = self.compressor.into_settings();
        master.limit = self.limiter.into_settings();
        match self.chain {
            None => master.chain = MasterChain::from_settings(master.comp, master.limit),
            Some(entries) => {
                let (chain, warnings) = chain_from_entries(&entries)?;
                for warning in &warnings {
                    eprintln!("omamix: warning: master chain {warning}");
                }
                if let Some(settings) = chain.compressor_settings() {
                    master.comp = settings;
                    master.compressor_enabled = settings.enabled();
                }
                if let Some(settings) = chain.limiter_settings() {
                    master.limit = settings;
                    master.limiter_enabled = settings.enabled();
                    master.limiter_ceiling_dbtp = settings.ceiling_db();
                }
                master.chain = chain;
            }
        }
        Ok(master)
    }
}

fn document_value<T: Serialize>(value: &T) -> toml::Value {
    toml::to_string(value)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()))
}

fn document_from_value<T>(value: Option<&toml::Value>) -> Result<T, SceneFileError>
where
    T: serde::de::DeserializeOwned + Default,
{
    let Some(value) = value else {
        return Ok(T::default());
    };
    T::deserialize(value.clone()).map_err(|err| SceneFileError::Format(err.to_string()))
}

fn chain_entries(chain: &MasterChain) -> Vec<ChainEntryDocument> {
    chain
        .blocks()
        .iter()
        .map(|block| {
            let params = match block.params() {
                EffectParams::Compressor { .. } => block
                    .comp_settings()
                    .map(|settings| document_value(&CompDocument::from_settings(settings))),
                EffectParams::Limiter { .. } => block
                    .limit_settings()
                    .map(|settings| document_value(&LimitDocument::from_settings(settings))),
                EffectParams::Gain { gain_db } => {
                    Some(document_value(&GainParamDocument { gain_db }))
                }
            };
            ChainEntryDocument {
                kind: block.kind_id().to_string(),
                bypass: block.bypassed(),
                params,
            }
        })
        .collect()
}

fn chain_from_entries(
    entries: &[ChainEntryDocument],
) -> Result<(MasterChain, Vec<String>), SceneFileError> {
    let mut warnings = Vec::new();
    let mut blocks = Vec::new();
    for entry in entries {
        let Some(kind) = EffectKind::parse(&entry.kind) else {
            warnings.push(format!("skipped unknown effect '{}'", entry.kind));
            continue;
        };
        let mut block = match kind {
            EffectKind::Compressor => {
                let doc = document_from_value::<CompDocument>(entry.params.as_ref())?;
                EffectBlock::compressor(doc.into_settings())
            }
            EffectKind::Limiter => {
                let doc = document_from_value::<LimitDocument>(entry.params.as_ref())?;
                EffectBlock::limiter(doc.into_settings())
            }
            EffectKind::Gain => {
                let doc = document_from_value::<GainParamDocument>(entry.params.as_ref())?;
                EffectBlock::gain(doc.gain_db)
            }
        };
        block.set_bypass(entry.bypass);
        blocks.push(block);
    }
    let (chain, more) = MasterChain::from_blocks(&blocks);
    warnings.extend(more);
    Ok((chain, warnings))
}

impl StripDocument {
    fn from_strip(strip: &Strip) -> Self {
        Self {
            role: role_token(&strip.source),
            name: strip.name.clone(),
            id: strip.id.0,
            fader_db: finite_fader(strip.fader_db),
            trim_db: strip.input_gain_db,
            mute: strip.mute,
            solo: strip.solo,
            pan: finite_pan(strip.pan),
            enabled: strip.enabled,
            monitor_headphones: strip.monitor_headphones,
            eq_enabled: strip.eq_enabled,
            eq: EqDocument::from_settings(strip.eq),
            compressor: CompDocument::from_settings(strip.comp),
        }
    }

    fn into_strip(self, index: usize) -> Result<Strip, SceneFileError> {
        let source = source_from_role(&self.role)?;
        let id = if self.id == 0 {
            (index + 1) as u64
        } else {
            self.id
        };
        let mut strip = Strip::new(id, self.name, source);
        strip.fader_db = finite_fader(self.fader_db);
        strip.set_input_gain_db(self.trim_db);
        strip.mute = self.mute;
        strip.solo = self.solo;
        strip.pan = finite_pan(self.pan);
        strip.enabled = self.enabled;
        strip.monitor_headphones = self.monitor_headphones;
        strip.eq_enabled = self.eq_enabled;
        strip.eq = self.eq.into_settings();
        strip.comp = self.compressor.into_settings();
        Ok(strip)
    }
}

fn role_token(source: &SourceKind) -> String {
    match source {
        SourceKind::DefaultSource => "default".to_string(),
        SourceKind::OtherDesktopAudio => "other".to_string(),
        SourceKind::CaptureDevice { node_name } => format!("device:{node_name}"),
        SourceKind::Application(key) => {
            format!("app:{}:{}", key.application_name, key.binary)
        }
    }
}

fn source_from_role(role: &str) -> Result<SourceKind, SceneFileError> {
    match parse_strip_request(role).map_err(SceneFileError::Format)? {
        crate::route::StripRequest::Other => Ok(SourceKind::OtherDesktopAudio),
        crate::route::StripRequest::Default => Ok(SourceKind::DefaultSource),
        crate::route::StripRequest::Device(node_name) => {
            Ok(SourceKind::CaptureDevice { node_name })
        }
        crate::route::StripRequest::App { name, binary } => {
            Ok(SourceKind::Application(RouteKey::new(name, binary)))
        }
    }
}

fn finite_fader(db: f32) -> f32 {
    fader::sanitize_fader_db(db)
}

fn finite_pan(pan: f32) -> f32 {
    if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::StripId;

    fn sample_scene() -> Scene {
        let mut scene = Scene::empty("desk");
        scene.master.set_fader_db(-6.0);
        scene.master.eq_enabled = true;
        scene.master.compressor_enabled = true;
        scene.master.limiter_enabled = true;
        scene.master.limiter_ceiling_dbtp = -2.0;
        scene.master.monitor_mix = true;
        scene.master.eq.set_hpf_enabled(true);
        scene.master.eq.set_hpf_hz(100.0);
        assert!(scene.master.eq.set_band(4, 3.0));
        scene.master.comp.set_enabled(true);
        scene.master.comp.set_threshold_db(-24.0);
        scene.master.comp.set_ratio(2.0);
        scene.master.comp.set_attack_ms(20.0);
        scene.master.comp.set_release_ms(200.0);
        scene.master.comp.set_knee_db(6.0);
        scene.master.comp.set_makeup_db(1.0);
        scene.master.limit.set_enabled(true);
        scene.master.limit.set_ceiling_db(-2.0);
        scene.master.limit.set_lookahead_ms(10.0);
        scene.master.sync_chain_params();

        let mut device = Strip::new(
            4,
            "mic",
            SourceKind::CaptureDevice {
                node_name: "node-a".into(),
            },
        );
        device.fader_db = -3.0;
        device.set_input_gain_db(2.0);
        device.mute = true;
        device.solo = true;
        device.pan = -0.5;
        device.enabled = false;
        device.monitor_headphones = true;
        device.eq_enabled = true;
        device.eq.set_hpf_enabled(true);
        assert!(device.eq.set_band(1, -6.0));
        device.comp.set_enabled(true);
        device.comp.set_threshold_db(-12.0);
        scene.strips.push(device);
        scene
            .strips
            .push(Strip::new(5, "default", SourceKind::DefaultSource));
        scene.strips.push(Strip::new(
            6,
            "player",
            SourceKind::Application(RouteKey::new("player", "player-bin")),
        ));
        scene
            .strips
            .push(Strip::new(7, "other", SourceKind::OtherDesktopAudio));
        scene
    }

    #[test]
    fn scene_toml_round_trips() {
        let scene = sample_scene();
        let text = scene_to_toml(&scene).unwrap();
        assert!(text.contains("role = \"device:node-a\""));
        assert!(text.contains("role = \"default\""));
        assert!(text.contains("role = \"app:player:player-bin\""));
        assert!(text.contains("role = \"other\""));
        assert!(text.contains("eq_enabled = true"));
        assert!(text.contains("hpf_hz"));
        assert!(text.contains("threshold_db"));
        assert!(!text.contains("node_id"));
        let loaded = scene_from_toml(&text).unwrap();
        assert_eq!(loaded, scene);
        let again = scene_from_toml(&scene_to_toml(&loaded).unwrap()).unwrap();
        assert_eq!(again, scene);
    }

    #[test]
    fn a_route_key_pid_is_not_stored() {
        let mut scene = Scene::empty("desk");
        let mut key = RouteKey::new("player", "player-bin");
        key.pid = Some(424_242);
        key.object_serial = Some(99);
        key.media_name = "tab".into();
        scene
            .strips
            .push(Strip::new(1, "player", SourceKind::Application(key)));
        let text = scene_to_toml(&scene).unwrap();
        assert!(!text.contains("424242"));
        assert!(!text.contains("424_242"));
        assert!(text.contains("app:player:player-bin"));
        let loaded = scene_from_toml(&text).unwrap();
        match &loaded.strips[0].source {
            SourceKind::Application(key) => {
                assert!(key.pid.is_none());
                assert!(key.object_serial.is_none());
                assert!(key.media_name.is_empty());
                assert_eq!(key.application_name, "player");
                assert_eq!(key.binary, "player-bin");
            }
            other => panic!("role became {other:?}"),
        }
    }

    #[test]
    fn an_invalid_scene_file_is_an_error() {
        let err = scene_from_toml("{{{ not a scene").unwrap_err();
        assert!(matches!(err, SceneFileError::Format(_)));
        let err = scene_from_toml("version = \"nope\"\n").unwrap_err();
        assert!(matches!(err, SceneFileError::Format(_)));
        let err = scene_from_toml("version = 1\n[[strips]]\nrole = \"nope\"\n").unwrap_err();
        assert!(matches!(err, SceneFileError::Format(_)));
    }

    #[test]
    fn a_newer_version_and_unknown_fields_still_load() {
        let text = r#"
version = 9
name = "later"
extra = true

[master]
fader_db = -6.0
spare = 1

[[strips]]
role = "device:node-a"
fader_db = -3.0
trim_db = 2.0
mute = true
future = "ok"

[[strips]]
role = "default"

[[strips]]
role = "app:player:player-bin"

[[strips]]
role = "other"
"#;
        let scene = scene_from_toml(text).unwrap();
        assert_eq!(scene.name, "later");
        assert_eq!(scene.master.fader_db(), -6.0);
        assert_eq!(scene.strips.len(), 4);
        assert_eq!(scene.strips[0].id, StripId(1));
        assert_eq!(scene.strips[0].fader_db, -3.0);
        assert_eq!(scene.strips[0].input_gain_db, 2.0);
        assert!(scene.strips[0].mute);
        assert!(matches!(
            scene.strips[0].source,
            SourceKind::CaptureDevice { ref node_name } if node_name == "node-a"
        ));
        assert!(matches!(scene.strips[1].source, SourceKind::DefaultSource));
        assert!(matches!(scene.strips[2].source, SourceKind::Application(_)));
        assert!(matches!(
            scene.strips[3].source,
            SourceKind::OtherDesktopAudio
        ));
        assert_eq!(scene.strips[1].eq, EqSettings::flat(48_000));
        assert_eq!(scene.strips[1].comp, CompSettings::bypass(48_000));
        assert!(!scene.strips[0].eq_enabled);
        assert!(!scene.strips[0].eq.hpf_enabled());
        assert_eq!(scene.strips[0].eq.hpf_hz(), HPF_DEFAULT_HZ);
        assert_eq!(scene.strips[0].eq.band_db(), [0.0; EQ_BANDS]);
        assert_eq!(scene.strips[0].comp, CompSettings::bypass(48_000));
        assert!(!scene.master.eq_enabled);
        assert_eq!(scene.master.eq, EqSettings::flat(48_000));
        assert!(!scene.master.eq.hpf_enabled());
        assert_eq!(scene.master.eq.hpf_hz(), HPF_DEFAULT_HZ);
        assert_eq!(scene.master.comp, CompSettings::bypass(48_000));
        assert_eq!(scene.master.chain, MasterChain::standard());
        assert_eq!(scene.master.limit, LimitSettings::master(48_000));
    }

    #[test]
    fn config_path_uses_xdg_then_home() {
        let xdg = scene_config_path(
            Some(std::ffi::OsStr::new("xdg-root")),
            Some(std::ffi::OsStr::new("home-root")),
        )
        .unwrap();
        assert_eq!(xdg, PathBuf::from("xdg-root/omamix/scene.toml"));
        let home = scene_config_path(
            Some(std::ffi::OsStr::new("")),
            Some(std::ffi::OsStr::new("home-root")),
        )
        .unwrap();
        assert_eq!(home, PathBuf::from("home-root/.config/omamix/scene.toml"));
        let unset = scene_config_path(None, Some(std::ffi::OsStr::new("home-root"))).unwrap();
        assert_eq!(unset, home);
        assert!(scene_config_path(None, None).is_err());
        assert!(scene_config_path(Some(std::ffi::OsStr::new("")), None).is_err());
    }

    #[test]
    fn save_replaces_the_file_without_leaving_a_temp_file() {
        let dir = std::env::temp_dir().join(format!("omamix-scene-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("scene.toml");
        let scene = sample_scene();
        save_scene_file(&path, &scene).unwrap();
        let loaded = load_scene_file(&path).unwrap();
        assert_eq!(loaded, scene);
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("scene.toml")]);
        fs::write(&path, "{{{").unwrap();
        assert!(load_scene_file(&path).is_err());
        let missing = dir.join("absent.toml");
        assert!(load_scene_file(&missing).unwrap_err().is_missing());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scene_without_a_chain_loads_the_default_order() {
        let scene = scene_from_toml("version = 1\nname = \"old\"\n").unwrap();
        assert_eq!(scene.master.chain.kind_ids(), vec!["compressor", "limiter"]);
        assert_eq!(scene.master.chain, MasterChain::standard());
        let text = scene_to_toml(&scene).unwrap();
        assert!(text.contains("kind = \"compressor\""));
        assert!(text.contains("kind = \"limiter\""));
        let loaded = scene_from_toml(&text).unwrap();
        assert_eq!(loaded.master.chain, scene.master.chain);
    }

    #[test]
    fn a_chain_round_trips_and_an_unknown_kind_is_skipped() {
        let mut scene = Scene::empty("desk");
        let mut comp = CompSettings::bypass(48_000);
        comp.set_enabled(true);
        comp.set_threshold_db(-12.0);
        let gain = EffectBlock::gain(-6.0);
        let (chain, warnings) = MasterChain::from_blocks(&[
            gain,
            EffectBlock::compressor(comp),
            EffectBlock::limiter(LimitSettings::master(48_000)),
        ]);
        assert!(warnings.is_empty());
        scene.master.comp = comp;
        scene.master.compressor_enabled = true;
        scene.master.chain = chain;
        scene.master.sync_chain_params();
        let text = scene_to_toml(&scene).unwrap();
        assert!(text.contains("kind = \"gain\""));
        assert!(text.contains("gain_db = -6.0") || text.contains("gain_db = -6"));
        let loaded = scene_from_toml(&text).unwrap();
        assert_eq!(loaded, scene);

        let odd = r#"
version = 1
name = "odd"

[[master.chain]]
kind = "gain"

[master.chain.params]
gain_db = 3.0

[[master.chain]]
kind = "plate"

[[master.chain]]
kind = "limiter"
bypass = true
"#;
        let scene = scene_from_toml(odd).unwrap();
        assert_eq!(scene.master.chain.kind_ids(), vec!["gain", "limiter"]);
        assert!(scene.master.chain.blocks()[1].bypassed());
        assert!(scene.master.limiter_enabled);

        #[derive(Deserialize)]
        struct Wrap {
            chain: Vec<ChainEntryDocument>,
        }
        let wrap: Wrap = toml::from_str("[[chain]]\nkind = \"plate\"\n").unwrap();
        let (chain, warnings) = chain_from_entries(&wrap.chain).unwrap();
        assert!(chain.is_empty());
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("plate") && line.contains("unknown effect"))
        );
    }
}
