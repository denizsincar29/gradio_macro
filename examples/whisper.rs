// Whisper large-v3-turbo transcription/translation example.
//
// Streams progress to the terminal (same-line updates) and writes the result to a file.
//
// Build note: populate the cache first:
//   cargo build --features gradio_macro/update_cache
//
// Usage:
//   cargo run --example whisper -- -i audio.wav
//   cargo run --example whisper -- -i audio.wav --task translate -o result.txt

use std::fs;

use clap::Parser;
use gradio_macro::gradio_api;

#[gradio_api(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
pub struct WhisperLarge;

/// Whisper large-v3-turbo: transcribe or translate an audio file.
#[derive(Parser, Debug)]
#[command(about = "Transcribe or translate audio with Whisper large-v3-turbo")]
struct Args {
    /// Audio file to process
    #[arg(short, long, value_name = "FILE")]
    input: String,

    /// Task: "transcribe" (default) or "translate"
    #[arg(long, default_value = "transcribe")]
    task: String,

    /// Output file for the result (default: result.txt)
    #[arg(short, long, default_value = "result.txt")]
    output: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    println!("Whisper large-v3-turbo");
    let whisper = WhisperLarge::new().await?;

    // Print the human-readable API description (useful for discovering field names).
    // eprintln!("{}", whisper.api());

    // `predict` has an optional `task` parameter (Literal enum), so a builder is returned.
    // Parse the CLI string into the generated typed enum via its FromStr impl.
    let task: WhisperLargePredictTask = args.task
        .parse()
        .unwrap_or_else(|_| panic!("invalid task '{}'; expected \"transcribe\" or \"translate\"", args.task));

    // .call_cli() streams queue/progress to stderr on the same terminal line,
    // then returns the typed output struct for this endpoint.
    let result = whisper
        .predict(&args.input)
        .with_task(task)
        .call_cli()
        .await?;

    // `result.output` is the transcription text.  Field names come from the
    // Gradio API spec; run `println!("{}", whisper.api())` to see all fields.
    let text = result.output.as_value()?;
    fs::write(&args.output, format!("{}", text)).expect("Can't write to file");
    println!("Result written to {}", args.output);
    Ok(())
}
