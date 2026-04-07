//! # gradio_macro
//!
//! Procedural macros for generating type-safe Rust clients for [Gradio](https://www.gradio.app/)
//! spaces.
//!
//! ## Macros
//!
//! - [`gradio_api`] — generates a fully-typed struct with builder methods for every named
//!   endpoint, plus a [`check_cache`] helper for detecting upstream API spec changes during
//!   development.
//! - [`gradio_cli`] — generates a [`clap::Parser`]-based CLI struct from the same spec.
//!
//! ## Modules
//!
//! The implementation is split across four focused modules:
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`cache`] | Cache file I/O, URL encoding, and `get_api_info()` |
//! | [`codegen`] | Parameter/return type mapping, identifier helpers, doc builders |
//! | [`api_macro`] | `gradio_api` implementation and `check_cache()` generation |
//! | [`cli_macro`] | `gradio_cli` implementation |

use proc_macro::TokenStream;
use proc_macro2::Span;

mod cache;
mod codegen;
mod api_macro;
mod cli_macro;

/// Emit a `compile_error!` token stream with the given message.
pub(crate) fn make_compile_error(message: &str) -> TokenStream {
    syn::Error::new(Span::call_site(), message).to_compile_error().into()
}

/// A procedural macro for generating a type-safe API client struct for a Gradio space.
///
/// The macro introspects the API spec at compile time (using a local cache) and generates:
///
/// - A struct (the name you give to `#[gradio_api]`) with a `new()` constructor.
/// - A **builder** returned by each named endpoint method. The builder always has:
///   - `.call()` — executes the prediction and returns the typed output struct.
///   - `.call_background()` — submits the prediction and returns a `PredictionStream` for
///     streaming queue/progress messages.
///   - `.call_cli()` *(async only)* — submits, pretty-prints queue/progress to `stderr` on the
///     same terminal line, then returns the typed output. No boilerplate needed.
///   - `.with_<param>()` setters for any **optional** parameters (those with API-level defaults).
/// - Typed Rust enums for `Literal[...]` Python types, with `Display`, `Serialize`,
///   `Deserialize`, and `FromStr` implementations.
/// - A `custom_endpoint()` method for calling arbitrary endpoints.
/// - A `check_cache()` method (debug builds only) for detecting upstream spec changes.
///
/// # Macro Parameters
///
/// | Parameter | Required | Description |
/// |-----------|----------|-------------|
/// | `url` | ✅ | HuggingFace space identifier or full Gradio URL |
/// | `option` | ✅ | `"sync"` or `"async"` |
/// | `hf_token` | ❌ | HuggingFace API token (falls back to `HF_TOKEN` env var) |
/// | `auth_username` | ❌ | HuggingFace username (pair with `auth_password`) |
/// | `auth_password` | ❌ | HuggingFace password (pair with `auth_username`) |
///
/// # API Caching
///
/// The macro caches the API spec as a JSON file under `.gradio_cache/` in your project root
/// (`CARGO_MANIFEST_DIR`). Build with `--features gradio_macro/update_cache` to refresh:
///
/// ```sh
/// cargo build --features gradio_macro/update_cache
/// HF_TOKEN=hf_... cargo build --features gradio_macro/update_cache  # private spaces
/// ```
///
/// You may commit `.gradio_cache/` for reproducible offline builds, or add it to `.gitignore`
/// to always fetch fresh specs.
///
/// # Example
///
/// ```rust,ignore
/// use gradio_macro::gradio_api;
///
/// #[gradio_api(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
/// pub struct WhisperLarge;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let whisper = WhisperLarge::new().await?;
///
///     // .call_cli() pretty-prints progress to stderr, then returns the result.
///     let result = whisper
///         .predict("audio.wav")
///         .with_task(WhisperLargePredictTask::Transcribe)
///         .call_cli()
///         .await?;
///
///     println!("{}", result[0].clone().as_value()?);
///     Ok(())
/// }
/// ```
///
/// ## Manual streaming with `call_background()`
///
/// ```rust,ignore
/// use gradio::{structs::QueueDataMessage, PredictionStream};
///
/// let mut stream = whisper
///     .predict("audio.wav")
///     .call_background()
///     .await?;
///
/// while let Some(msg) = stream.next().await {
///     match msg? {
///         QueueDataMessage::Estimation { rank, queue_size, rank_eta, .. } => {
///             eprint!("\rQueue {}/{} (ETA: {:.1}s)  ", rank + 1, queue_size, rank_eta.unwrap_or(0.0));
///         }
///         QueueDataMessage::ProcessCompleted { output, success, .. } => {
///             eprintln!();
///             if success {
///                 let outputs: Vec<_> = output.try_into().unwrap();
///                 println!("{}", outputs[0].clone().as_value().unwrap());
///             }
///             break;
///         }
///         _ => {}
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn gradio_api(args: TokenStream, input: TokenStream) -> TokenStream {
    api_macro::gradio_api_impl(args, input)
}

/// A procedural macro that generates a [`clap::Parser`]-based CLI struct from a Gradio API spec.
///
/// Each named API endpoint becomes a subcommand with a field for every parameter.
/// The generated struct can be embedded in any binary with minimal boilerplate.
///
/// # Macro Parameters
///
/// - `url`: **Required**. The HuggingFace space identifier or full Gradio URL.
/// - `option`: **Required**. `"sync"` or `"async"` — controls whether `run()` is `async` or not.
/// - `hf_token`, `auth_username`, `auth_password`: Same as [`gradio_api`].
///
/// # Generated Output
///
/// ```rust,ignore
/// use gradio_macro::gradio_cli;
///
/// #[gradio_cli(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
/// pub struct WhisperCli;
///
/// #[tokio::main]
/// async fn main() -> Result<(), gradio::anyhow::Error> {
///     use clap::Parser;
///     let cli = WhisperCli::parse();
///     let result = cli.run().await?;
///     println!("{:?}", result);
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn gradio_cli(args: TokenStream, input: TokenStream) -> TokenStream {
    cli_macro::gradio_cli_impl(args, input)
}
