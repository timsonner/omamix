//! Metadata writes, and what a default-device observation means.
//!
//! Every metadata write goes through [`allow_metadata_write`]. That function
//! refuses the key `default` and every `default.*` key (`default.audio.source`,
//! `default.audio.sink`, `default.configured.*`). Observing a default is
//! separate: [`observe_default_source`] and [`observe_default_sink`] only
//! classify a name that was read.

use crate::route::MIX_MIC_NODE_NAME;

/// Why a metadata write was refused.
#[derive(Debug, PartialEq, Eq)]
pub struct RefusedDefaultKey {
    pub key: String,
}

/// `default` and `default.*` are session default-device keys.
pub fn is_default_metadata_key(key: &str) -> bool {
    key == "default" || key.starts_with("default.")
}

/// The only gate for a metadata write.
pub fn allow_metadata_write(key: &str) -> Result<(), RefusedDefaultKey> {
    if is_default_metadata_key(key) {
        Err(RefusedDefaultKey {
            key: key.to_string(),
        })
    } else {
        Ok(())
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
    fn metadata_write_refuses_default_keys() {
        for key in [
            "default",
            "default.audio.source",
            "default.audio.sink",
            "default.configured.audio.source",
            "default.configured.audio.sink",
        ] {
            assert!(is_default_metadata_key(key));
            assert_eq!(
                allow_metadata_write(key),
                Err(RefusedDefaultKey {
                    key: key.to_string(),
                })
            );
        }
        assert!(allow_metadata_write("node.name").is_ok());
        assert!(allow_metadata_write("priority.session").is_ok());
        assert!(allow_metadata_write("defaulted").is_ok());
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
        let default_dot = format!("{}{}", "default", ".");
        let quoted_default = format!("\"{}\"", "default");
        let mut hits = Vec::new();
        for path in source_files() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if text.contains(&dash) || text.contains(&underscore) {
                hits.push(path.display().to_string());
                continue;
            }
            if property_near_default_key(&text, &property, &default_dot, &quoted_default) {
                hits.push(path.display().to_string());
            }
        }
        assert!(
            hits.is_empty(),
            "default-device write in {}",
            hits.join(", ")
        );
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
