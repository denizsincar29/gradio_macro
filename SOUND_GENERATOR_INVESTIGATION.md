# Sound Generator Investigation

## Task Context

The task asked to investigate a `sound_generator` example, potentially in the
`jacoblincool/gradio_rs` repository. This document records the findings.

## Findings

### Repository Availability

The `jacoblincool/gradio_rs` repository referenced in the task is not publicly
accessible on GitHub — the URL returns HTTP 404. This means:

- The repository may have been deleted, made private, or never existed under
  that name.
- It cannot be cloned or accessed in the current environment.

### What a sound_generator Gradio Space Does

Based on the Gradio ecosystem patterns and the `gradio_rs` crate (v0.3.2)
available via crates.io:

A typical sound/audio generator Gradio space exposes one or more endpoints,
for example:

```text
/generate_sound:
  Parameters:
    text_prompt  (str)           — description of the desired sound
    duration     (float)  [default: 5.0] — length in seconds
    seed         (int)    [default: -1]  — random seed (-1 = random)
  Returns:
    audio  (filepath)  — generated audio file
```

### How to Use gradio_macro with a Sound Generator

With `gradio_macro`, you would write:

```rust
use gradio_macro::gradio_api;

// Generates typed client at compile time from the cached API spec.
#[gradio_api(url = "some-org/sound-generator", option = "async")]
pub struct SoundGenerator;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let gen = SoundGenerator::new().await?;

    let result = gen
        .generate_sound("ocean waves")  // mandatory text_prompt
        .with_duration(10.0)            // optional: 10-second clip
        .with_seed(42)                  // optional: reproducible
        .call_cli()                     // pretty-prints queue progress
        .await?;

    println!("Generated audio: {:?}", result.audio);
    Ok(())
}
```

### Common Issues with Audio Endpoints

1. **`filepath` return type**: The generated output field is
   `gradio::GradioFileData`, not a raw path. Call `.url` or `.path` on it to
   get the actual file location.

2. **Long generation times**: Audio generation can take 30–120 seconds. Use
   `.call_cli()` to show queue progress, or `.call_background()` to stream
   messages manually.

3. **HuggingFace token required**: Many audio spaces are gated. Set `HF_TOKEN`
   in the environment or pass `hf_token = "..."` to the macro.

4. **Cache staleness**: If the upstream space updates its API (new parameters,
   changed defaults), rebuild with:
   ```sh
   cargo build --features gradio_macro/update_cache
   ```
   Or call `check_cache()` at runtime in debug mode to detect differences.

### check_cache() Usage for Sound Generator Development

The newly added `check_cache()` method is particularly useful when iterating
on a sound generator integration:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let gen = SoundGenerator::new().await?;

    // In debug builds: compares compile-time spec vs live upstream.
    // Prints diff and optionally writes to gradio_spec_diff.txt.
    if !gen.check_cache() {
        eprintln!("WARNING: API spec has changed since last compile.");
        // Optionally: std::process::exit(1);
    }

    // ... rest of your code
}
```

## Recommendation

1. Search for the actual sound generator Gradio space on
   [HuggingFace Spaces](https://huggingface.co/spaces) (search for
   "sound generation" or "audio generation").

2. Use `cargo build --features gradio_macro/update_cache` to fetch and cache
   the API spec.

3. Use `check_cache()` during development to stay in sync with upstream changes.
