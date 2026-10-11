//! Metadata writes, and what a default-device observation means.
//!
//! Every metadata write goes through [`allow_metadata_write`]. The allow list
//! is empty, so every key is refused, including `default` and `default.*`.
//! Observing a default is separate: [`observe_default_source`] and
//! [`observe_default_sink`] only classify a name that was read.

use crate::route::MIX_MIC_NODE_NAME;

/// Why a metadata write was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct RefusedDefaultKey {
    pub key: String,
}

/// Keys a metadata write may set. Empty: this process does not write metadata.
const ALLOWED_METADATA_KEYS: &[&str] = &[];

/// `default` and `default.*` are session default-device keys.
pub fn is_default_metadata_key(key: &str) -> bool {
    let key = key.trim();
    key.eq_ignore_ascii_case("default") || key_has_default_prefix(key)
}

fn key_has_default_prefix(key: &str) -> bool {
    let prefix = format!("{}.", "default");
    key.len() >= prefix.len() && key[..prefix.len()].eq_ignore_ascii_case(&prefix)
}

/// The only gate for a metadata write. Matching is trimmed and case-insensitive.
pub fn allow_metadata_write(key: &str) -> Result<(), RefusedDefaultKey> {
    if metadata_key_allowed(key, ALLOWED_METADATA_KEYS) {
        Ok(())
    } else {
        Err(RefusedDefaultKey {
            key: key.to_string(),
        })
    }
}

fn metadata_key_allowed(key: &str, allowed: &[&str]) -> bool {
    let key = key.trim();
    allowed.iter().any(|item| key.eq_ignore_ascii_case(item))
}

/// Node name inside a PipeWire metadata value.
///
/// WirePlumber stores `{"name":"alsa_input..."}`. A bare name is accepted too.
/// This parses a value that was read. It does not write.
pub fn metadata_node_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if !value.starts_with('{') {
        return Some(value.to_string());
    }
    let marker = format!("\"{}\"", "name");
    let at = value.find(&marker)?;
    let rest = value[at + marker.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let name = &rest[..end];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// What to do after reading a default device name.
///
/// [`DefaultDeviceObservation::MixMicIsDefault`] means destroy the Mix Mic
/// node. None of these variants is a write.
#[derive(Debug, PartialEq, Eq)]
pub enum DefaultDeviceObservation {
    Steady,
    Changed { now: String },
    MixMicIsDefault,
}

/// Classify the default source. Mix Mic as that source is
/// [`DefaultDeviceObservation::MixMicIsDefault`] even when the name did not
/// change, so a node that is already the default is torn down.
pub fn observe_default_source(
    previous: Option<&str>,
    now: Option<&str>,
) -> DefaultDeviceObservation {
    observe_name(previous, now, true)
}

/// Classify the default sink. Mix Mic is a source, so a sink name is only
/// [`DefaultDeviceObservation::Steady`] or [`DefaultDeviceObservation::Changed`].
pub fn observe_default_sink(previous: Option<&str>, now: Option<&str>) -> DefaultDeviceObservation {
    observe_name(previous, now, false)
}

fn observe_name(
    previous: Option<&str>,
    now: Option<&str>,
    source: bool,
) -> DefaultDeviceObservation {
    if source && now == Some(MIX_MIC_NODE_NAME) {
        return DefaultDeviceObservation::MixMicIsDefault;
    }
    match now {
        Some(name) if previous != Some(name) => DefaultDeviceObservation::Changed {
            now: name.to_string(),
        },
        _ => DefaultDeviceObservation::Steady,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn metadata_write_refuses_every_key_including_defaults() {
        for key in [
            "default",
            "default.audio.source",
            "default.audio.sink",
            "default.configured.audio.source",
            "default.configured.audio.sink",
            " Default.Audio.Source ",
            "node.name",
            "priority.session",
            "defaulted",
        ] {
            assert_eq!(
                allow_metadata_write(key),
                Err(RefusedDefaultKey {
                    key: key.to_string(),
                })
            );
        }
        assert!(is_default_metadata_key("default"));
        assert!(is_default_metadata_key(" Default.Audio.Source "));
        assert!(!is_default_metadata_key("defaulted"));
        assert!(metadata_key_allowed(
            " Default.Audio.Source ",
            &["default.audio.source"]
        ));
        assert!(!metadata_key_allowed("node.name", &[]));
    }

    #[test]
    fn only_source_selects_mix_mic_and_the_write_is_refused() {
        assert_eq!(
            observe_default_source(None, Some(MIX_MIC_NODE_NAME)),
            DefaultDeviceObservation::MixMicIsDefault
        );
        assert_eq!(
            observe_default_source(Some(MIX_MIC_NODE_NAME), Some(MIX_MIC_NODE_NAME)),
            DefaultDeviceObservation::MixMicIsDefault
        );
        assert!(allow_metadata_write("default.audio.source").is_err());
        assert_eq!(
            metadata_node_name("{\"name\":\"alsa_input.usb-Shure\"}"),
            Some("alsa_input.usb-Shure".into())
        );
        assert_eq!(
            metadata_node_name("alsa_output.pci"),
            Some("alsa_output.pci".into())
        );
    }

    #[test]
    fn observe_default_source_logs_a_change_and_flags_mix_mic() {
        assert_eq!(
            observe_default_source(Some("alsa"), Some("alsa")),
            DefaultDeviceObservation::Steady
        );
        assert_eq!(
            observe_default_source(Some("alsa"), Some("built-in")),
            DefaultDeviceObservation::Changed {
                now: "built-in".into(),
            }
        );
        assert_eq!(
            observe_default_source(Some("alsa"), Some(MIX_MIC_NODE_NAME)),
            DefaultDeviceObservation::MixMicIsDefault
        );
        assert_eq!(
            observe_default_source(Some(MIX_MIC_NODE_NAME), Some(MIX_MIC_NODE_NAME)),
            DefaultDeviceObservation::MixMicIsDefault
        );
        assert_eq!(
            observe_default_source(Some("alsa"), None),
            DefaultDeviceObservation::Steady
        );
        assert_eq!(
            observe_default_sink(Some("speakers"), Some(MIX_MIC_NODE_NAME)),
            DefaultDeviceObservation::Changed {
                now: MIX_MIC_NODE_NAME.into(),
            }
        );
        assert_eq!(
            observe_default_sink(Some("speakers"), Some("speakers")),
            DefaultDeviceObservation::Steady
        );
    }

    #[test]
    fn source_tree_has_no_default_device_writes() {
        let dash = format!("{}-default", "set");
        let underscore = format!("{}_default", "set");
        let property = format!("set_{}", "property");
        let set_volume = format!("{}-volume", "set");
        let set_mute = format!("{}-mute", "set");
        let pw_metadata = format!("{}-{}", "pw", "metadata");
        let mixer_cmd = format!("{}ixer", "am");
        let alsa_cmd = format!("{}ctl", "alsa");
        let command_new = format!("Command::{}", "new");
        let param_call = format!("set_{}", "param");
        let set_dash = format!("{}-", "set");
        let config_wp = format!(".config/{}", "wireplumber");
        let config_pw = format!(".config/{}", "pipewire");
        let config_hypr = format!(".config/{}", "hypr");
        let default_dot = format!("{}{}", "default", ".");
        let quoted_default = format!("\"{}\"", "default");
        let default_word = "default";
        let mut hits = Vec::new();
        for path in source_files() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let mut reasons = Vec::new();
            if text.contains(&dash) {
                reasons.push("dash");
            }
            if text.contains(&underscore) {
                reasons.push("underscore");
            }
            if text.contains(&set_volume) {
                reasons.push("volume");
            }
            if text.contains(&set_mute) {
                reasons.push("mute");
            }
            if text.contains(&pw_metadata) {
                reasons.push("metadata-tool");
            }
            if text.contains(&mixer_cmd) {
                reasons.push("mixer");
            }
            if text.contains(&alsa_cmd) {
                reasons.push("alsa-ctl");
            }
            if text.contains(&config_wp) || text.contains(&config_pw) || text.contains(&config_hypr)
            {
                reasons.push("config-dir");
            }
            if name != "build.rs" && text.contains(&command_new) {
                reasons.push("spawn");
            }
            if name != "filter.rs" && text.contains(&param_call) {
                reasons.push("param");
            }
            if property_near_default_key(&text, &property, &default_dot, &quoted_default) {
                reasons.push("property");
            }
            if set_dash_near(&text, &set_dash, default_word) {
                reasons.push("split");
            }
            if !reasons.is_empty() {
                hits.push(format!("{} ({})", path.display(), reasons.join(", ")));
            }
        }
        assert!(hits.is_empty(), "device write in {}", hits.join(", "));
    }

    fn set_dash_near(text: &str, set_dash: &str, default_word: &str) -> bool {
        let mut start = 0;
        while let Some(found) = text[start..].find(set_dash) {
            let at = start + found;
            let from = at.saturating_sub(48);
            let to = (at + set_dash.len() + 48).min(text.len());
            if text[from..to].contains(default_word) {
                return true;
            }
            start = at + set_dash.len();
        }
        false
    }

    fn property_near_default_key(
        text: &str,
        property: &str,
        default_dot: &str,
        quoted_default: &str,
    ) -> bool {
        let mut start = 0;
        while let Some(found) = text[start..].find(property) {
            let at = start + found;
            let from = at.saturating_sub(160);
            let to = (at + property.len() + 160).min(text.len());
            let window = &text[from..to];
            if window.contains(default_dot) || window.contains(quoted_default) {
                return true;
            }
            start = at + property.len();
        }
        false
    }

    fn source_files() -> Vec<PathBuf> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut out = Vec::new();
        collect(&root.join("crates"), &mut out);
        collect(&root.join("scripts"), &mut out);
        let pkgbuild = root.join("PKGBUILD");
        if pkgbuild.is_file() {
            out.push(pkgbuild);
        }
        out
    }

    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                collect(&path, out);
            } else if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("rs") | Some("sh") | Some("toml")
            ) {
                out.push(path);
            }
        }
    }
}
