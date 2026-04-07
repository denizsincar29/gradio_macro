/// This example needs to be rewritten to use thiserror and librarie's error types instead of anyhow.

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
    /// prompt text file to generate sound effect from
    #[arg(long, value_name = "FILE")]
    prompt_file: Option<String>,
    /// duration of the generated sound effect in seconds (default: 10.0)
    #[arg(short, long, default_value = "10.0")]
    duration: f64,
    // Output file for the generated sound effect (default: output.wav)
    #[arg(short, long, default_value = "output.wav")]
    output: String,

}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let prompt = if let Some(prompt_file) = args.prompt_file {
        tokio::fs::read_to_string(prompt_file).await.expect("Failed to read prompt file")
    } else if let Some(prompt) = args.prompt {
        prompt
    } else {
        eprintln!("Error: Either --prompt or --prompt-file must be provided.");
        std::process::exit(1);
    };
    let generator = SoundGenerator::new().await.unwrap();
    let result = generator.gradio_generate(prompt).with_duration(args.duration).call().await.unwrap()[0].clone();
    let file = result.as_file().unwrap();
    tokio::fs::write(&args.output, file.download(None).await.unwrap()).await.unwrap();
}