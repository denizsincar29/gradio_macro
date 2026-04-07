// OmniVoice voice-cloning example using the k2-fsa/OmniVoice Gradio space.
//
// Clones a voice from a reference audio file and synthesises the supplied text.
//
// Usage:
//   cargo run --example omnivoice -- -i ref.wav -t "Hello world"
//   cargo run --example omnivoice -- -i ref.wav --text-file input.txt -o out.wav
//   echo "Hello world" | cargo run --example omnivoice -- -i ref.wav
//
// Build note: the API spec is loaded from .gradio_cache/ so no network
// connection is needed at compile time.  Populate the cache with:
//   cargo build --features gradio_macro/update_cache

use std::path::PathBuf;
use clap::Parser;
use gradio_macro::gradio_api;

#[gradio_api(url = "k2-fsa/OmniVoice", option = "async")]
pub struct OmniVoice;

/// Voice cloning with OmniVoice (k2-fsa/OmniVoice)
#[derive(Parser, Debug)]
#[command(about = "Clone a voice and synthesise speech with OmniVoice")]
struct Args {
    /// Reference audio file to clone the voice from (WAV / MP3)
    #[arg(short, long, value_name = "FILE")]
    input: PathBuf,

    /// Output audio file path
    #[arg(short, long, value_name = "FILE", default_value = "output.wav")]
    output: String,

    /// Text to synthesise (mutually exclusive with --text-file; reads stdin if neither is set)
    #[arg(short, long, conflicts_with = "text_file")]
    text: Option<String>,

    /// Path to a plain-text file whose contents are synthesised
    #[arg(long, value_name = "FILE", conflicts_with = "text")]
    text_file: Option<PathBuf>,

    /// Language for synthesis (default: Auto — model auto-detects)
    #[arg(long, default_value = "Auto")]
    language: String,

    /// Transcript of the reference audio (helps the model clone the voice)
    #[arg(long, default_value = "")]
    ref_text: String,
}

/// Resolve the synthesis text from CLI args or stdin.
async fn resolve_text(args: &Args) -> anyhow::Result<String> {
    if let Some(ref t) = args.text {
        return Ok(t.clone());
    }
    if let Some(ref path) = args.text_file {
        return Ok(tokio::fs::read_to_string(path).await?);
    }
    // Fall back to stdin
    use tokio::io::AsyncReadExt as _;
    let mut buf = String::new();
    tokio::io::stdin().read_to_string(&mut buf).await?;
    Ok(buf.trim_end_matches('\n').to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let text = resolve_text(&args).await?;
    if text.is_empty() {
        anyhow::bail!("No text provided. Use --text, --text-file, or pipe text via stdin.");
    }

    println!("Connecting to OmniVoice …");
    let omni = OmniVoice::new().await?;

    println!("Cloning voice from '{}' …", args.input.display());

    // The builder API for the /_clone_fn endpoint:
    //   clone_fn(text, ref_aud, ref_text, du)  <- mandatory params
    //     .with_lang(...)                       <- optional (default: Auto)
    //     .call().await?
    //
    // `lang` is a typed enum generated from the Gradio API spec.
    // Parse the CLI string via its FromStr impl.
    let language_enum: OmniVoiceCloneFnLang = args.language
        .parse()
        .unwrap_or(OmniVoiceCloneFnLang::Auto);

    let result = omni
        .clone_fn(
            text,
            &args.input,            // ref_aud: reference audio file
            args.ref_text.clone(),  // ref_text: transcript of the reference audio
            0.0_f64,                // du: duration (0.0 = auto-detect)
        )
        .with_lang(language_enum)
        .call()
        .await?;

    // `result.output` is a `gradio::GradioFileData` directly — no `.as_file()` needed.
    // Field names come from the Gradio API spec; call `omni.api()` to list them.
    let bytes = result.output.download(None).await?;
    tokio::fs::write(&args.output, bytes).await?;
    println!("Saved: {}", args.output);

    Ok(())
}
