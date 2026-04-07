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
use gradio::{structs::QueueDataMessage, PredictionOutput, PredictionStream};
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
                eprint!("\rProcessing…                          ");
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
                    eprintln!("Failed.");
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
async fn main() {
    let args = Args::parse();

    println!("Whisper large-v3-turbo");
    let whisper = WhisperLarge::new().await.unwrap();

    // `predict` has an optional `task` parameter (Literal enum), so a builder is returned.
    // Parse the CLI string into the generated typed enum via its FromStr impl.
    let task: WhisperLargePredictTask = args.task
        .parse()
        .unwrap_or_else(|_| panic!("invalid task '{}'; expected \"transcribe\" or \"translate\"", args.task));

    // Use .with_task() to override the default.
    let mut stream = whisper
        .predict(&args.input)
        .with_task(task)
        .call_background()
        .await
        .unwrap();

    match show_progress(&mut stream).await {
        Some(result) => {
            let text = result[0].clone().as_value().unwrap();
            fs::write(&args.output, format!("{}", text)).expect("Can't write to file");
            println!("Result written to {}", args.output);
        }
        None => {
            eprintln!("Failed to transcribe");
        }
    }
}
