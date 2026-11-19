//! Mixer window, or a headless PipeWire client.
//!
//! `--headless` never opens a window. Without `--features pipewire` it exits
//! before touching the audio graph. `--headless` without `--mix-mic` also
//! leaves the graph alone. The GUI path still uses [`NullEngine`] and does
//! not publish Mix Mic.

#[cfg(feature = "gui")]
mod ui;

use std::process::ExitCode;
use std::time::Duration;

#[cfg(feature = "gui")]
use eframe::egui;
use omamix_core::graph::Scene;
use omamix_core::route::{RouteKey, SourceKind, StripRequest};
#[cfg(feature = "gui")]
use omamix_pw::{AudioEngine, EngineCommand, NullEngine};

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
            || !cli.strips.is_empty()
        {
            eprintln!(
                "omamix: --list does not take --mix-mic, --strips, --monitor-mix, --tone, or --duration"
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
        || !cli.strips.is_empty()
    {
        eprintln!(
            "omamix: --mix-mic, --strips, --monitor-mix, --tone, and --duration need --headless"
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
A missing default or a missing device name exits 2."
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

#[cfg(feature = "pipewire")]
fn run_headless(cli: Cli) -> ExitCode {
    let options = omamix_pw::HeadlessOptions {
        scene: scene_from_cli(&cli),
        duration: cli.duration,
        publish_mix_mic: cli.mix_mic,
        tone: cli.tone,
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
fn run_headless(cli: Cli) -> ExitCode {
    // Parsed so a bad --strips value still fails above. Nothing here connects.
    let _ = scene_from_cli(&cli);
    eprintln!(
        "omamix: --headless requires a build with --features pipewire; no audio client was opened"
    );
    ExitCode::FAILURE
}

#[cfg(feature = "gui")]
fn run_gui() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("omamix")
            .with_inner_size([980.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "omamix",
        native_options,
        Box::new(|_cc| Ok(Box::new(OmamixApp::new()))),
    )
}

#[cfg(feature = "gui")]
struct OmamixApp {
    scene: Scene,
    /// The window does not open a PipeWire client.
    engine: Box<dyn AudioEngine>,
}

#[cfg(feature = "gui")]
impl OmamixApp {
    fn new() -> Self {
        Self {
            scene: Scene::placeholder(),
            engine: Box::new(NullEngine::new()),
        }
    }
}

#[cfg(feature = "gui")]
impl eframe::App for OmamixApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("omamix");
                ui.label("pre-alpha");
                ui.separator();
                ui.label(format!("engine: {}", self.engine.name()));
                ui.label("virtual mic not published");
            });
            ui.separator();
            ui.label("Click a fader, then scroll while the pointer is still on it.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if let Some(strip) = self.scene.strips.first_mut() {
                    ui::strip_panel(ui, strip);
                }
            });
        });
    }

    fn on_exit(&mut self) {
        // Enqueue only. The audio thread applies Shutdown; this call does not.
        self.engine.command_sender().submit(EngineCommand::Shutdown);
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
}
