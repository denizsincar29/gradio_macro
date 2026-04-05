// OmniVoice TTS example using the k2-fsa/OmniVoice Gradio space.
//
// OmniVoice supports multilingual text-to-speech synthesis with a variety of
// voice, style, pitch, and speed controls via /_design_fn, and voice cloning
// from a reference audio file via /_clone_fn.
//
// Build note: the API spec is loaded from .gradio_cache/ so no network
// connection is needed at compile time after the first build.

use gradio_macro::gradio_api;

#[gradio_api(url = "k2-fsa/OmniVoice", option = "async")]
pub struct OmniVoice;

/// Download a file returned by the API to a local path.
async fn save_output(output: &gradio::PredictionOutput, path: &str) -> anyhow::Result<()> {
    let file_data = output.clone().as_file()?;
    let bytes = file_data.download(None).await?;
    tokio::fs::write(path, bytes).await?;
    println!("Saved: {}", path);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("OmniVoice – multilingual TTS & voice cloning");

    // Build a client for the OmniVoice space
    let omni = OmniVoice::new().await?;

    // ── Example 1: basic text-to-speech ─────────────────────────────────
    println!("\n[1] Synthesising English text …");
    let result = omni
        .design_fn(
            "Hello, this is a demonstration of OmniVoice.",
            // Language (defaults to "Auto" — let the model detect)
            "Auto",
            // Noise scale
            32.0_f64,
            // GS (speaking pace scale)
            2.0_f64,
            // Denoise
            true,
            // Speaking speed
            1.0_f64,
            // Duration (None / 0 means auto)
            0.0_f64,
            // Pause at punctuation
            true,
            // Post-processing
            true,
            // Gender
            "Auto",
            // Age
            "Auto",
            // Pitch
            "Auto",
            // Style
            "Auto",
            // Accent
            "Auto",
            // Dialect
            "Auto",
        )
        .await?;

    // Result[0] is the audio file, result[1] is a status string
    save_output(&result[0], "omnivoice_tts.wav").await?;

    if let Some(out) = result.get(1) {
        if let Ok(status) = out.clone().as_value() {
            println!("Status: {}", status);
        }
    }

    // ── Example 2: voice cloning (requires a reference .wav file) ────────
    // Uncomment and provide a reference audio file to try voice cloning.
    //
    // println!("\n[2] Cloning voice from reference audio …");
    // let ref_audio = std::path::PathBuf::from("wavs/your_reference.wav");
    // let clone_result = omni
    //     .clone_fn(
    //         "Cloning this voice in a new sentence.",
    //         "Auto",                   // language (auto-detect)
    //         ref_audio,                // reference audio
    //         "Your reference text.",   // transcript of the reference audio
    //         32.0_f64,                 // noise scale
    //         2.0_f64,                  // gs
    //         true,                     // denoise
    //         1.0_f64,                  // speaking speed
    //         0.0_f64,                  // duration (0 = auto)
    //         true,                     // pause at punctuation
    //         true,                     // post-process
    //     )
    //     .await?;
    // save_output(&clone_result[0], "omnivoice_clone.wav").await?;

    Ok(())
}
