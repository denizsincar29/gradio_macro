//! # gradio_cache_update
//!
//! A utility that scans your Rust project for `#[gradio_api(url = "...")]`
//! attribute usages and pre-fetches the Gradio API spec for each discovered
//! space, writing the result to `.gradio_cache/<url>.json` in the output
//! directory.
//!
//! ## Usage
//!
//! Install once (requires the `cli-tools` feature):
//! ```text
//! cargo install gradio_macro --features cli-tools
//! ```
//!
//! Then, from your project root:
//! ```text
//! # Auto-scan the current directory and update all caches
//! gradio_cache_update
//!
//! # Provide URLs directly (no scan needed)
//! gradio_cache_update hf-audio/whisper-large-v3-turbo jacoblincool/vocal-separation
//!
//! # Scan a different directory
//! gradio_cache_update --scan path/to/project
//!
//! # Write cache to a custom directory
//! gradio_cache_update --output-dir my_cache
//!
//! # Authenticate with HuggingFace
//! gradio_cache_update --hf-token hf_...
//! ```

use anyhow::{bail, Context, Result};
use clap::Parser;
use gradio::{Client, ClientOptions};
use std::path::{Path, PathBuf};

/// Update the Gradio API spec cache for all spaces used in your Rust project.
///
/// The tool scans `.rs` files for `#[gradio_api(url = "...")]` attributes,
/// fetches the API spec for each discovered URL and writes it to
/// `.gradio_cache/<url>.json` so that subsequent compilations can work
/// without a network connection.
#[derive(Parser, Debug)]
#[command(
    name = "gradio_cache_update",
    version,
    author,
    about = "Pre-fetch and cache Gradio API specs for your project"
)]
struct Args {
    /// Gradio space identifiers or full URLs to cache directly.
    ///
    /// When provided, these are cached in addition to (or instead of) any
    /// URLs found by `--scan`.
    #[arg(value_name = "URL")]
    urls: Vec<String>,

    /// Directory to scan for `#[gradio_api(url = "...")]` usages.
    ///
    /// Defaults to the current working directory when no explicit URLs are
    /// given.
    #[arg(long, value_name = "DIR")]
    scan: Option<PathBuf>,

    /// Directory where `.gradio_cache/*.json` files are written.
    ///
    /// Defaults to `.gradio_cache` in the current working directory, which
    /// matches what the `gradio_macro` proc-macro expects when
    /// `CARGO_MANIFEST_DIR` equals the project root.
    #[arg(long, value_name = "DIR", default_value = ".gradio_cache")]
    output_dir: PathBuf,

    /// HuggingFace API token (for private spaces).
    #[arg(long, value_name = "TOKEN", env = "HF_TOKEN")]
    hf_token: Option<String>,

    /// HuggingFace username (must be paired with `--password`).
    #[arg(long, value_name = "NAME", requires = "password")]
    username: Option<String>,

    /// HuggingFace password (must be paired with `--username`).
    #[arg(long, value_name = "PASS", requires = "username")]
    password: Option<String>,
}

// ── cache helpers ──────────────────────────────────────────────────────────

fn cache_file_path(url: &str, output_dir: &Path) -> PathBuf {
    let safe_name: String = url
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    output_dir.join(format!("{}.json", safe_name))
}

fn write_cache(url: &str, api: &gradio::structs::ApiInfo, output_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create cache directory {}", output_dir.display()))?;
    let path = cache_file_path(url, output_dir);
    let content = serde_json::to_string_pretty(api).context("Failed to serialise API info")?;
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write cache file {}", path.display()))?;
    println!("  ✓  {}", path.display());
    Ok(())
}

// ── source scanning ────────────────────────────────────────────────────────

/// Walk `dir` recursively and collect all `.rs` file paths.
/// Hidden directories and the `target/` directory are skipped.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if !name.starts_with('.') && name != "target" {
                collect_rs_files(&path, out);
            }
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Scan all `.rs` files under `dir` for `#[gradio_api(url = "...")]`
/// attribute usages and return the deduplicated list of URL strings found.
fn scan_for_urls(dir: &Path) -> Result<Vec<String>> {
    // Simple text-based regex – no need to parse the full AST.
    let re = regex::Regex::new(r#"#\s*\[\s*gradio_api\s*\([^)]*url\s*=\s*"([^"]+)""#)
        .expect("invalid regex");

    let mut files = Vec::new();
    collect_rs_files(dir, &mut files);

    let mut urls: Vec<String> = Vec::new();
    for path in &files {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        for cap in re.captures_iter(&content) {
            let url = cap[1].to_string();
            if !urls.contains(&url) {
                println!("  Found  {}  ({})", url, path.display());
                urls.push(url);
            }
        }
    }
    Ok(urls)
}

// ── main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Validate auth pair
    if args.username.is_some() != args.password.is_some() {
        bail!("--username and --password must both be provided together");
    }

    // Collect URLs: explicit args + optional scan
    let mut urls: Vec<String> = args.urls.clone();

    let should_scan = args.scan.is_some() || urls.is_empty();
    if should_scan {
        let scan_dir = args
            .scan
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        println!(
            "Scanning {} for #[gradio_api] usages …",
            scan_dir.display()
        );
        let found = scan_for_urls(&scan_dir)?;
        for url in found {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }

    if urls.is_empty() {
        println!("No Gradio URLs found. Pass URLs directly or use --scan <DIR>.");
        return Ok(());
    }

    println!("\nFetching API specs …");

    // Build client options
    let options = ClientOptions {
        hf_token: args.hf_token.clone(),
        auth: args
            .username
            .clone()
            .zip(args.password.clone()),
    };

    let mut errors: Vec<String> = Vec::new();
    for url in &urls {
        print!("  {url} … ");
        // Flush stdout so the URL appears before the potential network delay
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let client_opts = ClientOptions {
            hf_token: options.hf_token.clone(),
            auth: options.auth.clone(),
        };
        match Client::new(url, client_opts).await {
            Ok(client) => {
                let api = client.view_api();
                match write_cache(url, &api, &args.output_dir) {
                    Ok(()) => {}
                    Err(e) => errors.push(format!("{url}: {e}")),
                }
            }
            Err(e) => {
                println!("✗  error: {e}");
                errors.push(format!("{url}: {e}"));
            }
        }
    }

    if errors.is_empty() {
        println!("\nAll caches updated successfully.");
        Ok(())
    } else {
        eprintln!("\nFinished with {} error(s):", errors.len());
        for e in &errors {
            eprintln!("  - {e}");
        }
        std::process::exit(1);
    }
}
