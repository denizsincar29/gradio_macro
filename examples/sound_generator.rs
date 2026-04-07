// Sound effect generator example using the fantaxy/Sound-AI-SFX Gradio space.
//
// Generates an audio clip from a text prompt, streaming progress to the terminal.
//
// Build note: populate the cache first:
//   cargo build --features gradio_macro/update_cache
//
// Usage:
//   cargo run --example sound_generator -- -p "thunderstorm with heavy rain"
//   cargo run --example sound_generator -- --prompt-file prompt.txt -d 15.0 -o storm.wav

use clap::Parser;
use gradio::{structs::QueueDataMessage, PredictionOutput, PredictionStream};
use gradio_macro::gradio_api;

#[gradio_api(url="fantaxy/Sound-AI-SFX", option="async")]
pub struct SoundGenerator;

/// Generate sound effects from text prompts.
#[derive(clap::Parser, Debug)]
#[command(about = "Generate sound effects with fantaxy/Sound-AI-SFX")]
struct Args {
    /// Text prompt describing the desired sound effect
    #[arg(short, long, value_name = "PROMPT", conflicts_with = "prompt_file")]
    prompt: Option<String>,
    /// Prompt text file to generate sound effect from
    #[arg(long, value_name = "FILE")]
    prompt_file: Option<String>,
    /// Duration of the generated sound effect in seconds
    #[arg(short, long, default_value = "10.0")]
    duration: f64,
    /// Output file for the generated sound effect
    #[arg(short, long, default_value = "output.wav")]
    output: String,
}

/// Stream queue and progress messages, updating the same terminal line.
/// Returns the final outputs when the prediction completes.
pub async fn show_progress(stream: &mut PredictionStream) -> Option<Vec<PredictionOutput>> {
    while let Some(message) = stream.next().await {
        if let Err(val) = message {
            eprintln!("\rError: {:?}                    ", val);
            continue;
        }
        match message.unwrap() {
            QueueDataMessage::Open => eprint!("\rConnected, waiting in queue…    "),
            QueueDataMessage::Estimation { rank, queue_size, rank_eta, .. } => {
                eprint!(
                    "\rQueue position {}/{} (ETA: {:.1}s)  ",
                    rank + 1,
                    queue_size,
                    rank_eta.unwrap_or(0.0)
                );
            }
            QueueDataMessage::ProcessStarts { .. } => {
                eprint!("\rGenerating…                          ");
            }
            QueueDataMessage::Progress { progress_data, .. } => {
                if let Some(pd) = progress_data {
                    let p = &pd[0];
                    eprint!(
                        "\rProgress: {}/{} {:?}    ",
                        p.index + 1,
                        p.length.unwrap_or(0),
                        p.unit
                    );
                }
            }
            QueueDataMessage::ProcessCompleted { output, success, .. } => {
                eprintln!(); // finish the inline progress line
                if !success {
                    eprintln!("Generation failed.");
                    return None;
                }
                eprintln!("Completed!");
                return Some(output.try_into().unwrap());
            }
            QueueDataMessage::Heartbeat => {}
            QueueDataMessage::Log { event_id } => {
                eprint!("\rLog: {}              ", event_id.unwrap_or_default());
            }
            QueueDataMessage::UnexpectedError { message } => {
                eprintln!("\rUnexpected error: {}", message.unwrap_or_default());
            }
            QueueDataMessage::Unknown(_) => {}
        }
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let prompt = if let Some(prompt_file) = args.prompt_file {
        tokio::fs::read_to_string(prompt_file).await?
    } else if let Some(prompt) = args.prompt {
        prompt
    } else {
        anyhow::bail!("Either --prompt or --prompt-file must be provided.");
    };

    let generator = SoundGenerator::new().await?;

    eprintln!("Submitting generation request…");
    let mut stream = generator
        .gradio_generate(prompt)
        .with_duration(args.duration)
        .call_background()
        .await?;

    let result = show_progress(&mut stream).await
        .ok_or_else(|| anyhow::anyhow!("No result received from the API"))?;
    let file = result[0].clone().as_file()?;
    let bytes = file.download(None).await?;
    tokio::fs::write(&args.output, bytes).await?;
    println!("Saved: {}", args.output);
    Ok(())
}
