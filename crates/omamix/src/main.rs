//! Mixer window, or a headless PipeWire client.
//!
//! `--headless` never opens a window. Without `--features pipewire` it exits
//! before touching the audio graph. `--headless` without `--mix-mic` also
//! leaves the graph alone. A window built with `--features pipewire` opens
//! the PipeWire engine and publishes Mix Mic until the window closes. A
//! window built without that feature keeps the null engine.

#[cfg(feature = "gui")]
mod picker;
#[cfg(feature = "gui")]
mod ui;
#[cfg(feature = "gui")]
mod window;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[cfg(feature = "gui")]
use eframe::egui;
use omamix_core::graph::Scene;
use omamix_core::route::{RouteKey, SourceKind, StripRequest};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("omamix: {err}");
            print_help();
            return ExitCode::from(2);
        }
    };
    if cli.list {
        if cli.mix_mic
            || cli.monitor_mix
            || cli.tone
            || cli.duration.is_some()
            || cli.save_scene
            || cli.scene.is_some()
            || !cli.strips.is_empty()
        {
            eprintln!(
                "omamix: --list does not take --mix-mic, --strips, --monitor-mix, --tone, --duration, --save-scene, or --scene"
            );
            return ExitCode::from(2);
        }
        return run_list();
    }
    if cli.headless {
        return run_headless(cli);
    }
    if cli.mix_mic
        || cli.monitor_mix
        || cli.tone
        || cli.duration.is_some()
        || cli.save_scene
        || cli.scene.is_some()
        || !cli.strips.is_empty()
    {
        eprintln!(
            "omamix: --mix-mic, --strips, --monitor-mix, --tone, --duration, --save-scene, and --scene need --headless"
        );
        return ExitCode::from(2);
    }
    #[cfg(feature = "gui")]
    {
        match run_gui() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("omamix: {err}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(feature = "gui"))]
    {
        eprintln!("omamix: the window requires a build with --features gui; no window was opened");
        ExitCode::FAILURE
    }
}

fn print_help() {
    println!(
        "\
omamix [--list]
       [--headless] [--mix-mic] [--duration SECS] [--monitor-mix] [--tone]
       [--strips other] [--strips default]
       [--strips device:NODE] [--strips app:NAME:BINARY]
       [--save-scene] [--scene PATH]

--list prints capture devices and application playback streams.
It does not open a window. Each line is tab-separated. The last field
is a --strips value: device:NODE or app:NAME:BINARY. Omamix nodes are omitted.
--headless does not open a window.
A build without the gui feature has no window. Pass --headless, or
rebuild with --features gui.
--mix-mic publishes the Mix Mic Audio/Source/Virtual. Without it, --headless
connects to nothing.
--duration stops the process after that many seconds (headless only).
--monitor-mix also publishes the monitor playback stream.
--tone writes a 440 Hz sine on Mix Mic so a recorder aimed at that node
can measure it. It does not retarget the default source or another device.
--strips may be repeated. default uses default.audio.source, or
default.configured.audio.source when that name is absent.
device: is a capture node.name. app: is application.name and
application.process.binary.
A start attaches the current default capture device to a strip when
the scene has none. That strip follows a later change of the default
source. A device: strip is linked as well. One capture port is copied
to both sides. FL and FR are used when both exist. Otherwise the first
two ports in port id order are used, and further channels are not summed.
Mix Mic is never linked into the engine.
OMAMIX_DISABLE_AUTO_ATTACH=1 skips the automatic strip.
A missing default or a missing device name on the command line exits 2.
--save-scene writes the scene that this command will run.
The file is $XDG_CONFIG_HOME/omamix/scene.toml, or
$HOME/.config/omamix/scene.toml when XDG_CONFIG_HOME is unset.
--scene PATH uses that file instead.
A headless start with no --strips loads the file when it exists.
An unreadable or invalid file prints a warning and starts without it.
A saved device name that is not on this machine uses the default source
and prints a warning."
    );
}

struct Cli {
    list: bool,
    headless: bool,
    mix_mic: bool,
    monitor_mix: bool,
    tone: bool,
    duration: Option<Duration>,
    strips: Vec<StripRequest>,
    save_scene: bool,
    scene: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli {
        list: false,
        headless: false,
        mix_mic: false,
        monitor_mix: false,
        tone: false,
        duration: None,
        strips: Vec::new(),
        save_scene: false,
        scene: None,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--list" => cli.list = true,
            "--headless" => cli.headless = true,
            "--mix-mic" => cli.mix_mic = true,
            "--monitor-mix" => cli.monitor_mix = true,
            "--tone" => cli.tone = true,
            "--duration" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or("--duration needs a number of seconds")?;
                let secs: f64 = value
                    .parse()
                    .map_err(|_| format!("--duration {value} is not a number"))?;
                if !secs.is_finite() || secs < 0.0 {
                    return Err(format!("--duration {value} is not a positive length"));
                }
                cli.duration = Some(Duration::from_secs_f64(secs));
            }
            "--strips" => {
                index += 1;
                let value = args.get(index).ok_or("--strips needs a value")?;
                cli.strips.push(parse_strip(value)?);
            }
            "--save-scene" => cli.save_scene = true,
            "--scene" => {
                index += 1;
                let value = args.get(index).ok_or("--scene needs a path")?;
                if value.is_empty() {
                    return Err("--scene needs a path".into());
                }
                cli.scene = Some(PathBuf::from(value));
            }
            other => return Err(format!("unknown argument {other}")),
        }
        index += 1;
    }
    Ok(cli)
}

fn parse_strip(value: &str) -> Result<StripRequest, String> {
    omamix_core::route::parse_strip_request(value)
}

fn scene_from_cli(cli: &Cli) -> Scene {
    let mut scene = Scene::empty("headless");
    scene.master.monitor_mix = cli.monitor_mix;
    for (index, strip) in cli.strips.iter().enumerate() {
        let source = match strip {
            StripRequest::Other => SourceKind::OtherDesktopAudio,
            StripRequest::Default => SourceKind::DefaultSource,
            StripRequest::Device(node_name) => SourceKind::CaptureDevice {
                node_name: node_name.clone(),
            },
            StripRequest::App { name, binary } => {
                SourceKind::Application(RouteKey::new(name.clone(), binary.clone()))
            }
        };
        let label = match strip {
            StripRequest::Other => "other".to_string(),
            StripRequest::Default => "default".to_string(),
            StripRequest::Device(name) => name.clone(),
            StripRequest::App { name, .. } => name.clone(),
        };
        scene
            .strips
            .push(omamix_core::Strip::new((index + 1) as u64, label, source));
    }
    scene
}

fn scene_path_for(cli: &Cli) -> Result<PathBuf, omamix_core::SceneFileError> {
    if let Some(path) = &cli.scene {
        Ok(path.clone())
    } else {
        omamix_core::default_scene_path()
    }
}

fn auto_attach_enabled() -> bool {
    std::env::var("OMAMIX_DISABLE_AUTO_ATTACH").ok().as_deref() != Some("1")
}

fn attach_default_if_enabled(scene: &mut Scene) {
    if auto_attach_enabled() {
        scene.ensure_default_capture_strip();
    }
}

fn resolve_scene(cli: &Cli) -> Result<(Scene, bool), String> {
    let (mut scene, fallback) = resolve_stored_scene(cli)?;
    attach_default_if_enabled(&mut scene);
    Ok((scene, fallback))
}

fn resolve_stored_scene(cli: &Cli) -> Result<(Scene, bool), String> {
    if !cli.strips.is_empty() {
        return Ok((scene_from_cli(cli), false));
    }
    let path = match scene_path_for(cli) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("omamix: warning: {err}");
            return Ok((scene_from_cli(cli), false));
        }
    };
    match omamix_core::load_scene_file(&path) {
        Ok(mut scene) => {
            if cli.monitor_mix {
                scene.master.monitor_mix = true;
            }
            Ok((scene, true))
        }
        Err(err) if err.is_missing() => Ok((scene_from_cli(cli), false)),
        Err(err) => {
            eprintln!("omamix: warning: {err}");
            Ok((scene_from_cli(cli), false))
        }
    }
}

fn save_requested_scene(cli: &Cli, scene: &Scene) -> Result<(), String> {
    let path = scene_path_for(cli).map_err(|err| err.to_string())?;
    omamix_core::save_scene_file(&path, scene)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    eprintln!("omamix: saved scene {}", path.display());
    Ok(())
}

fn run_headless(cli: Cli) -> ExitCode {
    let (scene, fallback) = match resolve_scene(&cli) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("omamix: {err}");
            return ExitCode::FAILURE;
        }
    };
    if cli.save_scene
        && let Err(err) = save_requested_scene(&cli, &scene)
    {
        eprintln!("omamix: {err}");
        return ExitCode::FAILURE;
    }
    start_headless(cli, scene, fallback)
}

#[cfg(feature = "pipewire")]
fn start_headless(cli: Cli, scene: Scene, fallback_missing_devices: bool) -> ExitCode {
    let options = omamix_pw::HeadlessOptions {
        scene,
        duration: cli.duration,
        publish_mix_mic: cli.mix_mic,
        tone: cli.tone,
        fallback_missing_devices,
    };
    match omamix_pw::run_headless(options) {
        Ok(_report) => ExitCode::SUCCESS,
        Err(omamix_pw::EngineError::Usage(err)) => {
            eprintln!("omamix: {err}");
            ExitCode::from(2)
        }
        Err(err) => {
            eprintln!("omamix: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "pipewire")]
fn run_list() -> ExitCode {
    match omamix_pw::run_list() {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("omamix: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "pipewire"))]
fn run_list() -> ExitCode {
    eprintln!(
        "omamix: --list requires a build with --features pipewire; no audio client was opened"
    );
    ExitCode::FAILURE
}

#[cfg(not(feature = "pipewire"))]
fn start_headless(cli: Cli, scene: Scene, fallback_missing_devices: bool) -> ExitCode {
    let _ = (cli, scene, fallback_missing_devices);
    eprintln!(
        "omamix: --headless requires a build with --features pipewire; no audio client was opened"
    );
    ExitCode::FAILURE
}

#[cfg(feature = "gui")]
fn window_inner_size() -> [f32; 2] {
    let Some(raw) = std::env::var("OMAMIX_WINDOW").ok() else {
        return [1440.0, 980.0];
    };
    let Some((width, height)) = raw.split_once('x') else {
        return [1440.0, 980.0];
    };
    let Ok(width) = width.parse::<f32>() else {
        return [1440.0, 980.0];
    };
    let Ok(height) = height.parse::<f32>() else {
        return [1440.0, 980.0];
    };
    if !(640.0..=7680.0).contains(&width) || !(480.0..=4320.0).contains(&height) {
        return [1440.0, 980.0];
    }
    [width, height]
}

fn run_gui() -> eframe::Result {
    let (scene, scene_path) = load_window_scene();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("omamix")
            .with_position([0.0, 0.0])
            .with_inner_size(window_inner_size())
            .with_min_inner_size([crate::ui::WINDOW_MIN_WIDTH, 480.0]),
        ..Default::default()
    };
    let host = window::WindowHost::new(scene, scene_path);
    eframe::run_native("omamix", native_options, Box::new(|_cc| Ok(Box::new(host))))
}

#[cfg(feature = "gui")]
fn load_window_scene() -> (Scene, Option<PathBuf>) {
    let (mut scene, path) = load_window_scene_file();
    attach_default_if_enabled(&mut scene);
    (scene, path)
}

fn load_window_scene_file() -> (Scene, Option<PathBuf>) {
    let path = match omamix_core::default_scene_path() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("omamix: warning: {err}");
            return (Scene::empty("window"), None);
        }
    };
    match omamix_core::load_scene_file(&path) {
        Ok(scene) => (scene, Some(path)),
        Err(err) if err.is_missing() => (Scene::empty("window"), Some(path)),
        Err(err) => {
            eprintln!("omamix: warning: {err}");
            (Scene::empty("window"), Some(path))
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn headless_without_mix_mic_does_not_publish() {
        let cli = parse_args(&args(&["--headless"])).unwrap();
        assert!(cli.headless);
        assert!(!cli.mix_mic);
        assert!(cli.strips.is_empty());
    }

    #[test]
    fn strips_duration_and_monitor_parse() {
        let cli = parse_args(&args(&[
            "--headless",
            "--mix-mic",
            "--duration",
            "2",
            "--strips",
            "other",
            "--strips",
            "device:alsa_input.x",
            "--strips",
            "app:mpv:mpv",
            "--monitor-mix",
            "--tone",
        ]))
        .unwrap();
        assert!(cli.mix_mic);
        assert!(cli.tone);
        assert_eq!(cli.duration, Some(Duration::from_secs(2)));
        assert!(cli.monitor_mix);
        let scene = scene_from_cli(&cli);
        assert_eq!(scene.strips.len(), 3);
        assert!(scene.master.monitor_mix);
        assert!(matches!(
            scene.strips[0].source,
            SourceKind::OtherDesktopAudio
        ));
        assert!(matches!(
            scene.strips[1].source,
            SourceKind::CaptureDevice { .. }
        ));
        assert!(matches!(scene.strips[2].source, SourceKind::Application(_)));
    }

    #[test]
    fn strips_default_is_a_source_kind() {
        let cli = parse_args(&args(&[
            "--headless",
            "--mix-mic",
            "--strips",
            "default",
            "--list",
        ]))
        .unwrap();
        assert!(cli.list);
        let scene = scene_from_cli(&cli);
        assert!(matches!(scene.strips[0].source, SourceKind::DefaultSource));
        assert_eq!(scene.strips[0].name, "default");
    }

    #[test]
    fn save_scene_and_scene_path_parse() {
        let cli = parse_args(&args(&[
            "--headless",
            "--save-scene",
            "--scene",
            "scene-dir/scene.toml",
            "--strips",
            "device:node-a",
            "--strips",
            "default",
        ]))
        .unwrap();
        assert!(cli.save_scene);
        assert_eq!(
            cli.scene.as_deref(),
            Some(std::path::Path::new("scene-dir/scene.toml"))
        );
        let (scene, fallback) = resolve_scene(&cli).unwrap();
        assert!(!fallback);
        assert_eq!(scene.strips.len(), 2);
        assert!(matches!(
            scene.strips[0].source,
            SourceKind::CaptureDevice { .. }
        ));
        assert!(matches!(scene.strips[1].source, SourceKind::DefaultSource));
    }

    #[test]
    fn a_headless_start_without_strips_loads_the_scene_file() {
        let dir = std::env::temp_dir().join(format!("omamix-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("scene.toml");
        let mut stored = Scene::empty("stored");
        stored.strips.push(omamix_core::Strip::new(
            3,
            "mic",
            SourceKind::CaptureDevice {
                node_name: "node-a".into(),
            },
        ));
        stored.strips[0].fader_db = -4.0;
        stored.strips[0].mute = true;
        omamix_core::save_scene_file(&path, &stored).unwrap();
        let cli = parse_args(&args(&[
            "--headless",
            "--scene",
            path.to_str().unwrap(),
            "--monitor-mix",
        ]))
        .unwrap();
        let (scene, fallback) = resolve_scene(&cli).unwrap();
        assert!(fallback);
        assert!(scene.master.monitor_mix);
        assert_eq!(scene.strips.len(), 2);
        assert!(matches!(scene.strips[1].source, SourceKind::DefaultSource));
        assert_eq!(scene.strips[0].fader_db, -4.0);
        assert!(scene.strips[0].mute);
        assert!(matches!(
            scene.strips[0].source,
            SourceKind::CaptureDevice { ref node_name } if node_name == "node-a"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_scene_file_does_not_fail_the_start() {
        let dir = std::env::temp_dir().join(format!("omamix-cli-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scene.toml");
        std::fs::write(&path, "{{{").unwrap();
        let cli = parse_args(&args(&["--headless", "--scene", path.to_str().unwrap()])).unwrap();
        let (scene, fallback) = resolve_scene(&cli).unwrap();
        assert!(!fallback);
        assert_eq!(scene.strips.len(), 1);
        assert!(matches!(scene.strips[0].source, SourceKind::DefaultSource));
        let missing = dir.join("absent.toml");
        let cli = parse_args(&args(&["--headless", "--scene", missing.to_str().unwrap()])).unwrap();
        let (scene, fallback) = resolve_scene(&cli).unwrap();
        assert!(!fallback);
        assert_eq!(scene.strips.len(), 1);
        assert!(matches!(scene.strips[0].source, SourceKind::DefaultSource));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
