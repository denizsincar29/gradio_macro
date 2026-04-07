# gradio_macro

A macro that generates type-safe API client code for Gradio Rust crate endpoints at compile time.

## Installation

Add the crates to your project:

```toml
[dependencies]
gradio_macro = "0.6"
gradio = "0.3"
tokio = { version = "1", features = ["full"] }
```

## Usage

```rust
use gradio_macro::gradio_api;

/// Define the API client using the macro
#[gradio_api(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
pub struct WhisperLarge;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let whisper = WhisperLarge::new().await?;

    // Every endpoint returns a builder — call `.call().await` to execute.
    // `task` is optional (default: "transcribe"), so you can customise it:
    let result = whisper
        .predict("wavs/english.wav")
        .with_task(WhisperLargePredictTask::Transcribe)
        .call()
        .await?;

    let text = result[0].clone().as_value()?;
    std::fs::write("result.txt", format!("{}", text)).expect("Can't write to file");
    println!("Result written to result.txt");
    Ok(())
}
```

### Builder API

Every generated endpoint method returns a builder, regardless of whether it has optional parameters.
Each builder has three execute methods:

| Method | Description |
|--------|-------------|
| `.call().await?` | Waits for the full result; no progress output |
| `.call_background().await?` | Returns a `PredictionStream` — drive it yourself |
| `.call_cli().await?` | Streams queue/progress to `stderr` on the same line, returns the full result |

```rust
// Mandatory-only endpoint
client.encode("text").call().await?;

// Endpoint with optional parameters — chain .with_xxx() setters
whisper.predict("audio.wav")
    .with_task(WhisperLargePredictTask::Translate)
    .call_cli()   // prints progress, then returns result
    .await?;
```

### Streaming with `call_background()`

For full control over queue/progress messages, use `.call_background()` to receive a
`PredictionStream` handle and drive it yourself:

```rust
use gradio::{structs::QueueDataMessage, PredictionStream};

let mut stream = whisper
    .predict("audio.wav")
    .call_background()
    .await?;

while let Some(msg) = stream.next().await {
    match msg? {
        QueueDataMessage::Estimation { rank, queue_size, rank_eta, .. } => {
            eprint!("\rQueue {}/{} (ETA: {:.1}s)  ", rank + 1, queue_size, rank_eta.unwrap_or(0.0));
        }
        QueueDataMessage::ProcessStarts { .. } => eprint!("\rProcessing…    "),
        QueueDataMessage::ProcessCompleted { output, success, .. } => {
            eprintln!();
            if success {
                let outputs: Vec<_> = output.try_into().unwrap();
                println!("{}", outputs[0].clone().as_value().unwrap());
            }
            break;
        }
        _ => {}
    }
}
```

### Custom endpoints

Call any endpoint not covered by the generated methods using the builder-returning `custom_endpoint()`:

```rust
let result = client
    .custom_endpoint("/my_endpoint", vec![gradio::PredictionInput::from_value("hello")])
    .call()
    .await?;
```

### Macro parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | ✅ | HuggingFace space identifier or full Gradio URL |
| `option` | ✅ | `"sync"` or `"async"` |
| `hf_token` | ❌ | HuggingFace API token (falls back to `HF_TOKEN` env var) |
| `auth_username` | ❌ | HuggingFace username (pair with `auth_password`) |
| `auth_password` | ❌ | HuggingFace password (pair with `auth_username`) |

### What is generated

The macro generates the struct and all its methods automatically from the Gradio API spec:

- Each named API endpoint becomes a **factory method** on the struct that returns a **builder**.
- The builder always exposes `.call()`, `.call_background()`, and `.call_cli()` (async only).
  `.call_cli()` streams queue and progress messages to `stderr` on the same terminal line (`\r`
  updates) and returns the completed outputs — no boilerplate needed in your code.
- Endpoints with **optional parameters** (those with API-level defaults) expose `.with_xxx()` setter
  methods documented with the parameter description and default value.
- `Literal[...]` Python types become typed Rust **enums**
  (e.g. `WhisperLargePredictTask::Transcribe`), providing compile-time safety.
- Parameter types are derived from the full Gradio API spec (`f64` for `float`, `i64` for `int`,
  `bool` for `bool`, `impl Into<std::path::PathBuf>` for file inputs, `impl Into<String>` for strings).
- Every generated method and setter is **documented** with parameter names, types, descriptions,
  and return-value information taken from the Gradio API spec — your IDE shows this in hover tooltips.

## API caching

The macro spec from the Gradio server is cached in `.gradio_cache/<url>.json` in your project root.
Subsequent builds load the spec from the cache without making any network request, so **VS Code / rust-analyzer will not hang**.

### Populating and refreshing the cache

Enable the `update_cache` feature to fetch fresh specs from the network and write them to `.gradio_cache/`:

```bash
# First-time setup or cache refresh:
cargo build --features gradio_macro/update_cache

# For private spaces, pass a token via the environment:
HF_TOKEN=hf_... cargo build --features gradio_macro/update_cache
```

Without this feature, the macro **only** reads the local cache. If no cache is present the build fails with a clear error message pointing at this command. If the cache is older than 7 days a warning is printed.

### Committing the cache

You may commit the `.gradio_cache/` directory to version control for fully reproducible, offline-capable builds.  To always fetch a fresh spec instead, add `.gradio_cache/` to your `.gitignore`.

## Building CLI tools with `gradio_cli`

`gradio_cli` turns a Gradio API spec into a fully-featured `clap` CLI in a single attribute:

```rust
use clap::Parser;
use gradio_macro::gradio_cli;

#[gradio_cli(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
pub struct WhisperCli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = WhisperCli::parse();
    let result = cli.run().await?;
    for output in &result {
        println!("{}", output.clone().as_value()?);
    }
    Ok(())
}
```

Each named endpoint becomes a subcommand, each parameter a `--long` flag:

```text
$ cargo run -- --help
Gradio API client for hf-audio/whisper-large-v3-turbo

Usage: whisper_cli <COMMAND>

Commands:
  predict   Calls the `/predict` Gradio endpoint
  predict1  Calls the `/predict_1` Gradio endpoint
  predict2  Calls the `/predict_2` Gradio endpoint
  help      Print this message or the help of the given subcommand(s)

$ cargo run -- predict --help
Usage: whisper_cli predict [OPTIONS] --inputs <INPUTS>

Options:
  --inputs <INPUTS>  parameter_1 (filepath)
  --task   <TASK>    Task [default: transcribe] [possible values: transcribe, translate]
  -h, --help         Print help
```

`Literal[...]` Python types are automatically mapped to clap `possible_values`, giving
built-in validation and shell completions for free.  When used via `gradio_api` they
become typed Rust enums for compile-time safety.

## How it works

The `#[gradio_api(...)]` and `#[gradio_cli(...)]` attribute macros call the
[gradio](https://crates.io/crates/gradio) Rust crate at compile time to introspect the target
Gradio space and generate a bespoke client struct or CLI struct.

## Limitations

- Prediction outputs are `Vec<gradio::PredictionOutput>` (dynamically typed).  Extract values with `.as_value()` or `.as_file()`.
- Complex input types (lists, dicts, etc.) fall back to `impl gradio::serde::Serialize`.

## Credits

Big Thanks to [Jacob Lin](https://github.com/JacobLinCool) for the [gradio-rs](https://github.com/JacobLinCool/gradio-rs) crate and assistance.
