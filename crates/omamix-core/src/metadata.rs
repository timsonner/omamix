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
        let mut hits = Vec::new();
        for path in source_files() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let reasons = scan_text(name, &text);
            if !reasons.is_empty() {
                hits.push(format!("{} ({})", path.display(), reasons.join(", ")));
            }
        }
        assert!(hits.is_empty(), "device write in {}", hits.join(", "));
    }

    #[test]
    fn scanner_catches_alias_link_join_and_split_format() {
        let alias = word(&[
            117, 115, 101, 32, 115, 116, 100, 58, 58, 112, 114, 111, 99, 101, 115, 115, 58, 58, 67,
            111, 109, 109, 97, 110, 100, 32, 97, 115, 32, 67, 109, 100, 59, 10, 67, 109, 100, 58,
            58, 110, 101, 119, 40, 34, 112, 119, 45, 108, 105, 110, 107, 34, 41, 59, 10,
        ]);
        assert!(scan_text("evil.rs", &alias).contains(&"spawn"));
        assert!(scan_text("evil.rs", &alias).contains(&"link-tool"));

        let join = word(&[
            104, 111, 109, 101, 46, 106, 111, 105, 110, 40, 34, 46, 99, 111, 110, 102, 105, 103,
            34, 41, 46, 106, 111, 105, 110, 40, 34, 119, 105, 114, 101, 112, 108, 117, 109, 98,
            101, 114, 34, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &join).contains(&"config-dir"));

        let split = word(&[
            102, 111, 114, 109, 97, 116, 33, 40, 34, 123, 125, 45, 123, 125, 34, 44, 32, 34, 115,
            101, 116, 34, 44, 32, 34, 118, 111, 108, 117, 109, 101, 34, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &split).contains(&"volume"));

        let ctl = word(&[115, 121, 115, 116, 101, 109, 99, 116, 108, 10]);
        assert!(scan_text("evil.rs", &ctl).contains(&"service"));

        let param = word(&[
            110, 111, 100, 101, 46, 115, 101, 116, 95, 112, 97, 114, 97, 109, 40, 105, 100, 41, 10,
        ]);
        assert!(scan_text("filter.rs", &param).contains(&"param"));

        let mut far = "x".repeat(200);
        far.push_str(&word(&[
            115, 101, 116, 95, 112, 114, 111, 112, 101, 114, 116, 121,
        ]));
        far.push_str(&"y".repeat(200));
        far.push_str(&word(&[
            100, 101, 102, 97, 117, 108, 116, 46, 99, 111, 110, 102, 105, 103, 117, 114, 101, 100,
        ]));
        assert!(scan_text("evil.rs", &far).contains(&"configured"));

        let build_bad = word(&[
            67, 111, 109, 109, 97, 110, 100, 58, 58, 110, 101, 119, 40, 34, 115, 121, 115, 116,
            101, 109, 99, 116, 108, 34, 41, 10,
        ]);
        assert!(scan_text("build.rs", &build_bad).contains(&"spawn"));
        let build_ok = word(&[
            67, 111, 109, 109, 97, 110, 100, 58, 58, 110, 101, 119, 40, 38, 112, 107, 103, 95, 99,
            111, 110, 102, 105, 103, 41, 10,
        ]);
        assert!(scan_text("build.rs", &build_ok).is_empty());

        let angled = word(&[
            60, 67, 111, 109, 109, 97, 110, 100, 62, 58, 58, 110, 101, 119, 40, 34, 112, 119, 45,
            108, 105, 110, 107, 34, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &angled).contains(&"spawn"));

        let glued = word(&[
            99, 111, 110, 99, 97, 116, 33, 40, 34, 112, 119, 45, 34, 44, 32, 34, 108, 105, 110,
            107, 34, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &glued).contains(&"link-tool"));

        let slice = word(&[
            91, 34, 115, 101, 116, 34, 44, 32, 34, 112, 97, 114, 97, 109, 34, 93, 46, 106, 111,
            105, 110, 40, 34, 95, 34, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &slice).contains(&"param"));

        let exec_call = word(&[
            108, 105, 98, 99, 58, 58, 101, 120, 101, 99, 118, 112, 40, 112, 97, 116, 104, 44, 32,
            97, 114, 103, 118, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &exec_call).contains(&"exec"));

        let short_exec = word(&[
            108, 105, 98, 99, 58, 58, 101, 120, 101, 99, 118, 40, 112, 97, 116, 104, 44, 32, 97,
            114, 103, 118, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &short_exec).contains(&"exec"));

        let posix = word(&[
            112, 111, 115, 105, 120, 95, 115, 112, 97, 119, 110, 40, 112, 97, 116, 104, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &posix).contains(&"posix"));

        let system_call = word(&[
            108, 105, 98, 99, 58, 58, 115, 121, 115, 116, 101, 109, 40, 99, 109, 100, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &system_call).contains(&"system"));

        let cli = word(&[
            112, 119, 45, 99, 108, 105, 32, 100, 101, 115, 116, 114, 111, 121, 10,
        ]);
        assert!(scan_text("evil.rs", &cli).contains(&"cli"));

        let bare = word(&[
            46, 106, 111, 105, 110, 40, 34, 119, 105, 114, 101, 112, 108, 117, 109, 98, 101, 114,
            34, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &bare).contains(&"config-dir"));

        let property_only = word(&[
            110, 111, 100, 101, 46, 115, 101, 116, 95, 112, 114, 111, 112, 101, 114, 116, 121, 40,
            105, 100, 41, 10,
        ]);
        assert!(scan_text("evil.rs", &property_only).contains(&"property"));
    }

    fn scan_text<'a>(name: &str, text: &str) -> Vec<&'a str> {
        let dash = word(&[115, 101, 116, 45, 100, 101, 102, 97, 117, 108, 116]);
        let underscore = word(&[115, 101, 116, 95, 100, 101, 102, 97, 117, 108, 116]);
        let property = word(&[115, 101, 116, 95, 112, 114, 111, 112, 101, 114, 116, 121]);
        let set_volume = word(&[115, 101, 116, 45, 118, 111, 108, 117, 109, 101]);
        let set_mute = word(&[115, 101, 116, 45, 109, 117, 116, 101]);
        let pw_metadata = word(&[112, 119, 45, 109, 101, 116, 97, 100, 97, 116, 97]);
        let link_tool = word(&[112, 119, 45, 108, 105, 110, 107]);
        let cli_tool = word(&[112, 119, 45, 99, 108, 105]);
        let service = word(&[115, 121, 115, 116, 101, 109, 99, 116, 108]);
        let mixer_cmd = word(&[97, 109, 105, 120, 101, 114]);
        let alsa_cmd = word(&[97, 108, 115, 97, 99, 116, 108]);
        let param_call = word(&[115, 101, 116, 95, 112, 97, 114, 97, 109]);
        let set_dash = word(&[115, 101, 116, 45]);
        let config_wp = word(&[
            46, 99, 111, 110, 102, 105, 103, 47, 119, 105, 114, 101, 112, 108, 117, 109, 98, 101,
            114,
        ]);
        let config_pw = word(&[
            46, 99, 111, 110, 102, 105, 103, 47, 112, 105, 112, 101, 119, 105, 114, 101,
        ]);
        let config_hypr = word(&[46, 99, 111, 110, 102, 105, 103, 47, 104, 121, 112, 114]);
        let configured = word(&[
            100, 101, 102, 97, 117, 108, 116, 46, 99, 111, 110, 102, 105, 103, 117, 114, 101, 100,
        ]);
        let default_word = word(&[100, 101, 102, 97, 117, 108, 116]);
        let exec = word(&[101, 120, 101, 99, 118]);
        let posix = word(&[112, 111, 115, 105, 120, 95, 115, 112, 97, 119, 110]);
        let libc_system = word(&[108, 105, 98, 99, 58, 58, 115, 121, 115, 116, 101, 109]);
        let mut reasons = Vec::new();
        if mentions(text, &dash) {
            reasons.push("dash");
        }
        if mentions(text, &underscore) {
            reasons.push("underscore");
        }
        if mentions(text, &set_volume) {
            reasons.push("volume");
        }
        if mentions(text, &set_mute) {
            reasons.push("mute");
        }
        if mentions(text, &pw_metadata) {
            reasons.push("metadata-tool");
        }
        if mentions(text, &link_tool) {
            reasons.push("link-tool");
        }
        if mentions(text, &cli_tool) {
            reasons.push("cli");
        }
        if mentions(text, &service) {
            reasons.push("service");
        }
        if mentions(text, &mixer_cmd) {
            reasons.push("mixer");
        }
        if mentions(text, &alsa_cmd) {
            reasons.push("alsa-ctl");
        }
        if mentions(text, &config_wp)
            || mentions(text, &config_pw)
            || mentions(text, &config_hypr)
            || joined_config(text, &config_wp, &config_pw, &config_hypr)
            || bare_join(text, &config_wp, &config_pw, &config_hypr)
        {
            reasons.push("config-dir");
        }
        if spawn_forbidden(name, text) {
            reasons.push("spawn");
        }
        if mentions(text, &exec) {
            reasons.push("exec");
        }
        if mentions(text, &posix) {
            reasons.push("posix");
        }
        if mentions(text, &libc_system) {
            reasons.push("system");
        }
        if mentions(text, &param_call) {
            reasons.push("param");
        }
        if mentions(text, &property) {
            reasons.push("property");
        }
        if text.contains(&property) && text.contains(&configured) {
            reasons.push("configured");
        }
        if set_dash_near(text, &set_dash, &default_word) {
            reasons.push("split");
        }
        reasons
    }

    fn word(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    fn spawn_forbidden(name: &str, text: &str) -> bool {
        let command = word(&[67, 111, 109, 109, 97, 110, 100]);
        let mut callers = vec![command.clone()];
        callers.extend(command_aliases(text, &command));
        let mut sites = Vec::new();
        for caller in &callers {
            for token in [format!("{caller}::new"), format!("{caller}>::new")] {
                let mut rest = text;
                while let Some(at) = rest.find(&token) {
                    let after = rest[at + token.len()..].trim_start();
                    sites.push(after.chars().take(48).collect::<String>());
                    rest = &rest[at + token.len()..];
                }
            }
        }
        if sites.is_empty() {
            return false;
        }
        if name == "build.rs" {
            return !sites.iter().all(|site| {
                site.starts_with("(&pkg_config)") || site.starts_with("(\"pkg-config\")")
            });
        }
        true
    }

    fn command_aliases(text: &str, command: &str) -> Vec<String> {
        let mut names = Vec::new();
        let marker = format!("{command} as ");
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("type ")
                && let Some((name, rhs)) = rest.split_once('=')
                && rhs.contains(command)
            {
                let name = name.trim().trim_end_matches(';');
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            }
            if let Some(at) = line.find(&marker) {
                let after = &line[at + marker.len()..];
                let ident: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !ident.is_empty() {
                    names.push(ident);
                }
            }
        }
        names
    }

    fn joined_config(text: &str, config_wp: &str, config_pw: &str, config_hypr: &str) -> bool {
        let join = word(&[46, 106, 111, 105, 110, 40]);
        let config = word(&[46, 99, 111, 110, 102, 105, 103]);
        let prefix_len = word(&[46, 99, 111, 110, 102, 105, 103, 47]).len();
        let dirs = [
            &config_wp[prefix_len..],
            &config_pw[prefix_len..],
            &config_hypr[prefix_len..],
        ];
        let mut start = 0;
        while let Some(found) = text[start..].find(&join) {
            let at = start + found;
            let from = at.saturating_sub(80);
            let to = (at + 160).min(text.len());
            let window = &text[from..to];
            if window.contains(&config) && dirs.iter().any(|dir| window.contains(dir)) {
                return true;
            }
            start = at + join.len();
        }
        false
    }

    fn bare_join(text: &str, config_wp: &str, config_pw: &str, config_hypr: &str) -> bool {
        let join_quote = word(&[46, 106, 111, 105, 110, 40, 34]);
        let prefix_len = word(&[46, 99, 111, 110, 102, 105, 103, 47]).len();
        let dirs = [
            &config_wp[prefix_len..],
            &config_pw[prefix_len..],
            &config_hypr[prefix_len..],
        ];
        dirs.iter()
            .any(|dir| text.contains(&format!("{join_quote}{dir}")))
    }

    fn mentions(text: &str, needle: &str) -> bool {
        text.contains(needle)
            || format_assembles(text, &[needle])
            || concat_assembles(text, &[needle])
            || slice_join_assembles(text, &[needle])
    }

    fn concat_assembles(text: &str, forbidden: &[&str]) -> bool {
        let marker = word(&[99, 111, 110, 99, 97, 116, 33]);
        let mut rest = text;
        while let Some(at) = rest.find(&marker) {
            let tail = rest[at + marker.len()..].trim_start();
            if let Some(body) = paren_body(tail) {
                let built: String = string_literals(body).concat();
                if forbidden.iter().any(|item| built.contains(*item)) {
                    return true;
                }
            }
            rest = &rest[at + marker.len()..];
        }
        false
    }

    fn slice_join_assembles(text: &str, forbidden: &[&str]) -> bool {
        let marker = word(&[46, 106, 111, 105, 110, 40]);
        let mut rest = text;
        while let Some(at) = rest.find(&marker) {
            let before = rest[..at].trim_end();
            if let Some(open) = before.rfind('[')
                && before.ends_with(']')
            {
                let inside = &before[open + 1..before.len() - 1];
                let parts = string_literals(inside);
                let tail = &rest[at + marker.len()..];
                let sep = string_literals(tail.split(')').next().unwrap_or(""));
                let built = parts.join(sep.first().map(String::as_str).unwrap_or(""));
                if forbidden.iter().any(|item| built.contains(*item)) {
                    return true;
                }
            }
            rest = &rest[at + marker.len()..];
        }
        false
    }

    fn format_assembles(text: &str, forbidden: &[&str]) -> bool {
        let mut rest = text;
        while let Some(at) = rest.find("format!") {
            let tail = rest[at + "format!".len()..].trim_start();
            if let Some(body) = paren_body(tail)
                && let Some(built) = assemble_format(body)
                && forbidden.iter().any(|item| built.contains(*item))
            {
                return true;
            }
            rest = &rest[at + "format!".len()..];
        }
        false
    }

    fn paren_body(text: &str) -> Option<&str> {
        let mut chars = text.char_indices();
        if chars.next()?.1 != '(' {
            return None;
        }
        let mut depth = 1;
        let mut in_str = false;
        let mut escape = false;
        for (index, ch) in chars {
            if in_str {
                if escape {
                    escape = false;
                    continue;
                }
                if ch == '\\' {
                    escape = true;
                    continue;
                }
                if ch == '"' {
                    in_str = false;
                }
                continue;
            }
            match ch {
                '"' => in_str = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&text[1..index]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn assemble_format(body: &str) -> Option<String> {
        let literals = string_literals(body);
        let template = literals.first()?;
        let mut args = literals.iter().skip(1);
        let mut out = String::new();
        let mut chars = template.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '{' && chars.peek() == Some(&'}') {
                chars.next();
                out.push_str(args.next()?);
            } else {
                out.push(ch);
            }
        }
        Some(out)
    }

    fn string_literals(body: &str) -> Vec<String> {
        let mut out = Vec::new();
        let bytes = body.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b'"' {
                index += 1;
                continue;
            }
            index += 1;
            let mut literal = String::new();
            while index < bytes.len() {
                let ch = bytes[index] as char;
                index += 1;
                if ch == '\\' && index < bytes.len() {
                    literal.push(bytes[index] as char);
                    index += 1;
                    continue;
                }
                if ch == '"' {
                    break;
                }
                literal.push(ch);
            }
            out.push(literal);
        }
        out
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
