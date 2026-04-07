# gradio_macro

A macro that generates type-safe API client code for Gradio Rust crate endpoints at compile time.

## Installation

Add the crates to your project:

```toml
[dependencies]
gradio_macro = "0.6"
gradio = "0.3"
tokio = { version = "1", features = ["full"] }
anyhow = "1"
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

    // `call()` returns a typed struct — access outputs by name with concrete types:
    let text: serde_json::Value = result.output;  // str → serde_json::Value
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
| `.call_cli().await?` | Streams queue/progress to `stderr` on the same line, returns the typed result |

```rust
// Mandatory-only endpoint
whisper.predict("audio.wav").call().await?;

// Endpoint with optional parameters — chain .with_xxx() setters
let result = whisper.predict("audio.wav")
    .with_task(WhisperLargePredictTask::Translate)
    .call_cli()   // prints progress, then returns typed result
    .await?;

// Access the transcription text directly — no conversion call needed:
let text: serde_json::Value = result.output;  // str → serde_json::Value
println!("{}", text);
```

### Typed Output Structs

`call()` and `call_cli()` return a **typed struct** for each endpoint instead of a raw
`Vec<PredictionOutput>`.  Every return value from the Gradio API spec becomes a named field
holding a **concrete Rust type** resolved at compile time:

| Gradio API type | Rust field type |
|-----------------|-----------------|
| `filepath` | `gradio::GradioFileData` |
| `str`, `int`, `float`, `bool`, etc. | `serde_json::Value` |

No `.as_file()` or `.as_value()` call is needed at the call site:

```rust
// Whisper: output is str → field is serde_json::Value
let result = whisper.predict("audio.wav").call().await?;
let text: serde_json::Value = result.output;
println!("{}", text);

// Vocal separation: both outputs are filepath → fields are GradioFileData
let result = vocal.separate("audio.wav").call().await?;
let vocals_bytes    = result.vocals.download(None).await?;
let background_bytes = result.background.download(None).await?;
```

Use the `.api()` method to discover field names at runtime (useful while exploring a new
space):

```rust
println!("{}", whisper.api());
// /predict:
//   Parameters:
//     inputs (filepath)  — Audio file
//     task (Literal['transcribe', 'translate']) [default: "transcribe"]  — Task
//   Returns:
//     output (str)  — Transcription
```

Use `.endpoints()` to get the raw JSON spec as a `serde_json::Value`:

```rust
let spec = whisper.endpoints();
println!("{}", serde_json::to_string_pretty(&spec).unwrap());
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
            eprint!("\rQueue {}/{} (ETA: {:.1}s)  ", rank + 1, queue_size, rank_eta);
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
let result = whisper
    .custom_endpoint("/my_endpoint", vec![gradio::PredictionInput::from_value("hello")])
    .call()
    .await?;
```

`custom_endpoint()` returns `Vec<gradio::PredictionOutput>` since the output structure is
not known at compile time.

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
  updates) and returns the completed typed output — no boilerplate needed in your code.
- `call()` and `call_cli()` return a **typed output struct** (e.g. `WhisperLargePredictOutput`)
  with named fields for each return value.  Field types are resolved from the Gradio API spec at
  compile time: `filepath` returns become `gradio::GradioFileData`, all other types become
  `serde_json::Value`.  No `.as_file()` / `.as_value()` call is needed at the call site.
- Endpoints with **optional parameters** (those with API-level defaults) expose `.with_xxx()` setter
  methods documented with the parameter description and default value.
- `Literal[...]` Python types become typed Rust **enums**
  (e.g. `WhisperLargePredictTask::Transcribe`), providing compile-time safety.
- Parameter types are derived from the full Gradio API spec (`f64` for `float`, `i64` for `int`,
  `bool` for `bool`, `impl Into<std::path::PathBuf>` for file inputs, `impl Into<String>` for strings).
- Every generated method, setter, and output type is **documented** with parameter names, types,
  descriptions, and return-value information taken from the Gradio API spec — your IDE shows this
  in hover tooltips.
- `.endpoints()` returns the named-endpoints spec as a `serde_json::Value`.
- `.api()` returns a human-readable `&str` listing every endpoint, its parameters, and its returns.

## API caching

The macro spec from the Gradio server is cached in `.gradio_cache/<url>.json` in your project root.
Subsequent builds load the spec from the cache without making any network request, so **VS Code / rust-analyzer will not hang**.

When no cache exists and `update_cache` is **not** enabled, the macro attempts a short network
request (10 s timeout) to fetch the spec automatically.  If the endpoint is unreachable the build
fails with a clear error message.

### Populating and refreshing the cache

Enable the `update_cache` feature to fetch fresh specs from the network and write them to `.gradio_cache/`:

```bash
# First-time setup or cache refresh:
cargo build --features gradio_macro/update_cache

# For private spaces, pass a token via the environment:
HF_TOKEN=hf_... cargo build --features gradio_macro/update_cache
```

Without this feature, the macro first checks the local cache.  If no cache is present it falls
back to a 10 s network request.  If the cache is older than 7 days a warning is printed.

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

- Output struct fields hold concrete types resolved from the Gradio API spec at compile time:
  `filepath` → `gradio::GradioFileData`, everything else → `serde_json::Value` — no `.as_file()`
  or `.as_value()` needed.  For `custom_endpoint()` and `gradio_cli`, outputs remain
  `Vec<gradio::PredictionOutput>` (type not known at compile time).
- Complex input types (lists, dicts, etc.) fall back to `impl gradio::serde::Serialize`.

## Credits

Big Thanks to [Jacob Lin](https://github.com/JacobLinCool) for the [gradio-rs](https://github.com/JacobLinCool/gradio-rs) crate and assistance.
