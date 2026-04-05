# gradio_macro

A macro that generates type-safe API client code for Gradio Rust crate endpoints at compile time.

## Usage

Add the crates to your project:

```toml
[dependencies]
gradio_macro = "0.4"
gradio = "0.3"
```

Then use the macro in your code:

```rust
use gradio_macro::gradio_api;
use std::fs;

/// Define the API client using the macro
#[gradio_api(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
pub struct WhisperLarge;

#[tokio::main]
async fn main() {
    println!("Whisper Large V3 turbo");

    // Instantiate the API client
    let whisper = WhisperLarge::new().await.unwrap();

    // Call the API's predict method with input arguments
    let result = whisper.predict("wavs/english.wav", "transcribe").await.unwrap();

    // Handle the result
    let result = result[0].clone().as_value().unwrap();

    // Save the result to a file
    std::fs::write("result.txt", format!("{}", result)).expect("Can't write to file");
    println!("result written to result.txt");
}
```

This example demonstrates how to define an asynchronous API client using the `gradio_api` macro to interact with the `hf-audio/whisper-large-v3-turbo` Gradio model.

### Macro parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | ✅ | HuggingFace space identifier or full Gradio URL |
| `option` | ✅ | `"sync"` or `"async"` |
| `hf_token` | ❌ | HuggingFace API token |
| `auth_username` | ❌ | HuggingFace username (pair with `auth_password`) |
| `auth_password` | ❌ | HuggingFace password (pair with `auth_username`) |
| `cache` | ❌ | Set to `"refresh"` to bypass the local cache and re-fetch the API spec |

### Explanation

The macro generates the `WhisperLarge` struct and all its methods automatically from the live Gradio API spec:

- Each named API endpoint becomes a method on the struct.
- A `_background` variant of every method returns a streaming `PredictionStream` handle instead of blocking.
- Parameter types are derived from the full Gradio API spec (`f64` for `float`, `i64` for `int`, `bool` for `bool`, `impl Into<std::path::PathBuf>` for file inputs, `impl Into<String>` for strings).
- Every generated method is documented with parameter names, types, descriptions and return-value information taken directly from the Gradio API spec – your IDE will show this information in hover tooltips.

## API caching

The first build fetches the API spec from the Gradio server and saves it to `.gradio_cache/<url>.json` in your project root. Subsequent builds load the spec from the cache without making any network request.

### Refreshing the cache with the CLI tool

Install the `gradio_cache_update` binary once (the `cli-tools` feature enables the binary deps):

```bash
cargo install gradio_macro --features cli-tools
```

Then, from your project root, update all caches automatically:

```bash
# Auto-scan the current project and refresh every cached spec
gradio_cache_update

# Cache specific spaces directly
gradio_cache_update hf-audio/whisper-large-v3-turbo jacoblincool/vocal-separation

# Scan a different directory
gradio_cache_update --scan path/to/other/project

# Write cache files to a custom directory
gradio_cache_update --output-dir my_cache

# Authenticate with HuggingFace for private spaces
gradio_cache_update --hf-token hf_...
# or via env var
HF_TOKEN=hf_... gradio_cache_update
```

### Other ways to refresh the cache

**Environment variable (refreshes all cached specs at build time):**
```bash
GRADIO_REFRESH_API_CACHE=1 cargo build
```

**Macro argument (refreshes only the annotated struct):**
```rust
#[gradio_api(url = "hf-audio/whisper-large-v3-turbo", option = "async", cache = "refresh")]
pub struct WhisperLarge;
```

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
built-in validation and shell completions for free.

## How it works

The `#[gradio_api(...)]` and `#[gradio_cli(...)]` attribute macros call the [gradio](https://crates.io/crates/gradio) Rust crate at compile time to introspect the target Gradio space and generate a bespoke client struct or CLI struct.

## Limitations

- Prediction outputs are `Vec<gradio::PredictionOutput>` (dynamically typed).  Extract values with `.as_value()` or `.as_file()`.
- Complex input types (lists, dicts, etc.) fall back to `impl gradio::serde::Serialize`.

## Credits

Big Thanks to [Jacob Lin](https://github.com/JacobLinCool) for the [gradio-rs](https://github.com/JacobLinCool/gradio-rs) crate and assistance.

