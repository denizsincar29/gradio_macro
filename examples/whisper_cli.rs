// whisper_cli.rs — a complete CLI for whisper-large-v3-turbo in ~10 lines
//
// Build:  cargo build --example whisper_cli
// Run:    cargo run --example whisper_cli -- predict --inputs wavs/english.wav
//         cargo run --example whisper_cli -- predict --inputs audio.wav --task translate

use clap::Parser;
use gradio_macro::gradio_cli;

/// A fully-featured CLI for the whisper-large-v3-turbo Gradio space.
#[gradio_cli(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
pub struct WhisperCli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = WhisperCli::parse();
    let result = cli.run().await?;

    for (i, output) in result.iter().enumerate() {
        match output.clone().as_value() {
            Ok(val) => println!("[{}] {}", i, val),
            Err(_) => println!("[{}] <file output>", i),
        }
    }
    Ok(())
}
