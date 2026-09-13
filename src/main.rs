use clap::{Parser, Subcommand};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::io::Read;
use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    rc::Rc,
};
use voice_runtime::{
    audit,
    clock::{Clock, TokioClock},
    event,
    fake::FakeProviders,
    playback::CountingSink,
    scenario,
    session::{Session, SessionConfig, SessionReport},
    wav,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(
    version,
    about = "Interruptible voice runtime: deterministic providers, actual PCM consumption, auditable replay"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run deterministic A–E fixtures (virtual time by default).
    Run {
        #[arg(long, default_value = "all", value_parser = ["A", "B", "C", "D", "E", "all"])]
        scenario: String,
        #[arg(long, default_value = "output")]
        output: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        seed: Option<u64>,
        #[arg(long)]
        jitter_ms: Option<u64>,
        #[arg(long)]
        real_time: bool,
    },
    /// Real WAV + WebRTC VAD; transcript/reply data must be supplied explicitly.
    Wav {
        input: PathBuf,
        #[arg(long)]
        script: PathBuf,
        #[arg(long, default_value = "output/wav")]
        output: PathBuf,
        #[arg(long)]
        real_time: bool,
    },
    /// Independently validate a trace and reconstruct metrics, text truth and played WAV.
    Replay {
        trace: PathBuf,
        #[arg(long, default_value = "output/replay")]
        output: PathBuf,
    },
}

fn write_json(path: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer_pretty(File::create(path)?, value)?;
    Ok(())
}
fn read_config<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    File::open(path)?.take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        return Err("configuration exceeds 1 MiB".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn truth(replies: &[voice_runtime::playback::ReplyRecord]) -> serde_json::Value {
    json!(replies.iter().map(|r| json!({"identity": r.identity, "generated_text": r.generated_text,
        "enqueued_text": r.enqueued_text(), "heard_text": r.heard_text(), "heard_ranges": r.heard_ranges(),
        "played_samples": r.played_samples(), "played_duration_ms": r.played_samples() as f64 / 16.0,
        "partial_words": r.partial_words(), "interrupted": r.interrupted, "chunks": r.chunks})).collect::<Vec<_>>())
}
fn write_report(output: &Path, report: &SessionReport) -> Result<audit::Metrics> {
    fs::create_dir_all(output)?;
    event::write_jsonl(&report.events, File::create(output.join("trace.jsonl"))?)?;
    let audit = audit::analyze(&report.events);
    write_json(output.join("metrics.json"), &audit.metrics)?;
    write_json(output.join("playback-truth.json"), &truth(&report.replies))?;
    write_json(
        output.join("lifecycle.json"),
        &json!({"close_reason": report.close_reason, "active_tasks": report.active_tasks,
        "trace_complete": report.trace_complete, "violations": audit.violations}),
    )?;
    fs::write(output.join("sequence.mmd"), audit::mermaid(&report.events))?;
    if !report.trace_complete || !audit.violations.is_empty() || audit.replies != report.replies {
        return Err(format!("trace/ledger validation failed: {:?}", audit.violations).into());
    }
    wav::export_played(&report.events, &output.join("played.wav"))?;
    Ok(audit.metrics)
}

async fn execute(command: Command) -> Result<()> {
    match command {
        Command::Run {
            scenario: selected,
            output,
            config,
            seed,
            jitter_ms,
            ..
        } => {
            let mut config: SessionConfig = match config {
                Some(path) => read_config(&path)?,
                None => SessionConfig::default(),
            };
            for timing in [
                &mut config.vad,
                &mut config.asr,
                &mut config.llm,
                &mut config.tts,
            ] {
                if let Some(seed) = seed {
                    timing.seed = seed;
                }
                if let Some(jitter) = jitter_ms {
                    timing.jitter_ms = jitter;
                }
            }
            config.validate()?;
            let names: Vec<_> = if selected == "all" {
                vec!["A", "B", "C", "D", "E"]
            } else {
                vec![selected.as_str()]
            };
            let mut metrics = std::collections::BTreeMap::new();
            let mut failed = false;
            fs::create_dir_all(&output)?;
            write_json(
                output.join("manifest.json"),
                &json!({"config": config, "providers": FakeProviders::default(),
                "fixture_version": 1, "audio": "mono PCM16 16000 Hz; 20 ms", "tts": "deterministic tone, not speech"}),
            )?;
            for name in names {
                let report =
                    scenario::run(name, config.clone(), Rc::new(TokioClock::default())).await;
                failed |= report.close_reason != "active_close"
                    || report.events.iter().any(|e| {
                        e.event_type == "provider_failed" || e.event_type == "turn_failed"
                    });
                metrics.insert(name, write_report(&output.join(name), &report)?);
            }
            write_json(output.join("metrics.json"), &metrics)?;
            println!("{}", serde_json::to_string_pretty(&metrics)?);
            if failed {
                return Err("scenario failed; inspect lifecycle.json and trace.jsonl".into());
            }
        }
        Command::Wav {
            input,
            script,
            output,
            ..
        } => {
            if !cfg!(feature = "real-vad") {
                return Err("rebuild with --features real-vad".into());
            }
            let mut providers: FakeProviders = read_config(&script)?;
            providers.validate()?;
            providers.real_vad = true;
            let source = wav::WavSource::new(BufReader::new(File::open(&input)?))?;
            let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
            let (session, mut handle) = Session::new(
                SessionConfig::default(),
                Rc::new(providers.clone()),
                clock.clone(),
                Box::<CountingSink>::default(),
            )?;
            let owner =
                tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(session.run()));
            let mut sequence = 0;
            for frame in source {
                let frame = frame?;
                clock.sleep_until(frame.timestamp + 20).await;
                sequence = frame.sequence + 1;
                handle.send_audio(frame).await?;
            }
            scenario::feed(&handle, clock.as_ref(), &mut sequence, 1500, 0, None).await?;
            scenario::wait_until(&mut handle, |s| s.idle).await?;
            handle.close();
            let report = owner.await?;
            let metrics = write_report(&output, &report)?;
            write_json(
                output.join("manifest.json"),
                &json!({"input": input, "script": script, "providers": providers, "asr": "scripted", "llm": "scripted", "tts": "tone"}),
            )?;
            println!("{}", serde_json::to_string_pretty(&metrics)?);
        }
        Command::Replay { trace, output } => {
            let events = audit::read_jsonl(BufReader::new(File::open(trace)?))?;
            let audit = audit::analyze(&events);
            fs::create_dir_all(&output)?;
            write_json(output.join("audit.json"), &audit)?;
            write_json(output.join("playback-truth.json"), &truth(&audit.replies))?;
            fs::write(output.join("sequence.mmd"), audit::mermaid(&events))?;
            if !audit.violations.is_empty() {
                return Err(format!("invalid trace: {:?}", audit.violations).into());
            }
            wav::export_played(&events, &output.join("played.wav"))?;
            println!(
                "Trace verified; stale audio played: {}",
                audit.metrics.stale_chunk_played_count
            );
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let virtual_time = match &cli.command {
        Command::Run { real_time, .. } | Command::Wav { real_time, .. } => !real_time,
        Command::Replay { .. } => true,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(virtual_time)
        .build()?;
    let started = std::time::Instant::now();
    let result = rt.block_on(tokio::task::LocalSet::new().run_until(execute(cli.command)));
    eprintln!(
        "Wall time: {:.3}s; virtual clock: {virtual_time}",
        started.elapsed().as_secs_f64()
    );
    result
}
