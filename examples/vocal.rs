// Vocal separation example using the jacoblincool/vocal-separation Gradio space.
//
// Build note: populate the cache first:
//   cargo build --features gradio_macro/update_cache
//
// Usage:
//   cargo run --example vocal -- -i audio.wav
//   cargo run --example vocal -- -i audio.wav --vocals out_vocals.wav --background out_bg.wav

use clap::Parser;
use gradio_macro::gradio_api;

#[gradio_api(url = "jacoblincool/vocal-separation", option = "async")]
pub struct VocalSeparation;

/// Separate vocals and background from an audio file.
#[derive(Parser, Debug)]
#[command(about = "Separate vocals and background with jacoblincool/vocal-separation")]
struct Args {
    /// Input audio file to process
    #[arg(short, long, value_name = "FILE")]
    input: String,

    /// Output file for the vocals track (default: vocals.wav)
    #[arg(long, default_value = "vocals.wav")]
    vocals: String,

    /// Output file for the background track (default: background.wav)
    #[arg(long, default_value = "background.wav")]
    background: String,
}

async fn download_file(file: gradio::GradioFileData, filename: String) {
    tokio::fs::write(&filename, file.download(None).await.unwrap()).await.unwrap();
    println!("Saved: {}", filename);
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    println!("Vocal Separation");
    let vocal = VocalSeparation::new().await.unwrap();

    // `separate` has optional parameters (model choice, etc.) – use the builder.
    // Call .call().await to execute with default optional params.
    // The returned typed struct has named fields: `vocals` and `background`.
    let result = vocal
        .separate(&args.input)
        .call()
        .await
        .expect("Failed to separate vocals");

    // Access outputs by name — no index juggling needed.
    let vocals_file = result.vocals.as_file().unwrap();
    let background_file = result.background.as_file().unwrap();

    let vocals_task = tokio::spawn({
        let path = args.vocals.clone();
        async move { download_file(vocals_file, path).await }
    });
    let background_task = tokio::spawn({
        let path = args.background.clone();
        async move { download_file(background_file, path).await }
    });
    let _ = tokio::join!(vocals_task, background_task);
}
