// Sound effect generator example using the fantaxy/Sound-AI-SFX Gradio space.
//
// Generates an audio clip from a text prompt, printing progress to the terminal.
//
// Build note: populate the cache first:
//   cargo build --features gradio_macro/update_cache
//
// Usage:
//   cargo run --example sound_generator -- -p "thunderstorm with heavy rain"
//   cargo run --example sound_generator -- --prompt-file prompt.txt -d 15.0 -o storm.wav

use clap::Parser;
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

    // .call_cli() streams progress to stderr on the same line, then returns the typed output.
    // Field names are derived from the Gradio API spec; call `generator.api()` to list them.
    let result = generator
        .gradio_generate(prompt)
        .with_duration(args.duration)
        .call_cli()
        .await?;

    // The generated audio file is in the first (and only) return field.
    // `result.generated_audio` is a `gradio::GradioFileData` directly — no `.as_file()` needed.
    // Use `generator.api()` to confirm the exact field name for this space.
    let bytes = result.generated_audio.download(None).await?;
    tokio::fs::write(&args.output, bytes).await?;
    println!("Saved: {}", args.output);
    Ok(())
}
