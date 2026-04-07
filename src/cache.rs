use gradio::ClientOptions;

/// Returns the path to the cache file for the given URL.
/// The cache is stored in `.gradio_cache/` relative to `CARGO_MANIFEST_DIR`.
///
/// The filename encodes the URL by percent-encoding non-alphanumeric
/// characters except `-`, `_`, and `.`, which avoids collisions between URLs
/// that only differ in separator characters (e.g. `a/b` vs `a_b`).
pub(crate) fn get_cache_path(url: &str) -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let encoded: String = encode_url_for_cache(url);
    std::path::PathBuf::from(manifest_dir)
        .join(".gradio_cache")
        .join(format!("{}.json", encoded))
}

/// Percent-encode a URL for use as a cache filename.
///
/// Non-alphanumeric characters (except `-`, `_`, `.`) are encoded so that
/// URLs which differ only in separator characters produce distinct filenames.
pub(crate) fn encode_url_for_cache(url: &str) -> String {
    url.chars()
        .flat_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                vec![c]
            } else {
                let byte = c as u32;
                if byte <= 0xFF {
                    format!("%{:02X}", byte).chars().collect()
                } else {
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf);
                    s.bytes()
                        .flat_map(|b| format!("%{:02X}", b).chars().collect::<Vec<_>>())
                        .collect()
                }
            }
        })
        .collect()
}

/// Cache file envelope that stores the API spec together with a fetch timestamp.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CacheEntry {
    pub timestamp_secs: u64,
    pub api: serde_json::Value,
}

/// Load the API info from the local cache file, if present and valid.
#[cfg(not(feature = "update_cache"))]
pub(crate) fn load_api_from_cache(url: &str) -> Option<gradio::structs::ApiInfo> {
    let path = get_cache_path(url);
    if path.exists() {
        let content = std::fs::read_to_string(&path).ok()?;
        // Try new envelope format first
        if let Ok(entry) = serde_json::from_str::<CacheEntry>(&content) {
            return serde_json::from_value(entry.api).ok();
        }
        // Fall back to old flat format
        serde_json::from_str(&content).ok()
    } else {
        None
    }
}

/// Persist the API info to the local cache file.
///
/// This function is intentionally compiled in all configurations (not feature-gated)
/// because it is called from two code paths:
/// * When `update_cache` is enabled — after fetching a fresh spec from the network.
/// * When `update_cache` is disabled and no cache exists — after the short-timeout
///   fallback fetch succeeds, so the result is saved for future offline builds.
pub(crate) fn save_api_to_cache(url: &str, api: &gradio::structs::ApiInfo) {
    let path = get_cache_path(url);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let timestamp_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let api_value = match serde_json::to_value(api) {
        Ok(v) => v,
        Err(_) => return,
    };
    let entry = CacheEntry { timestamp_secs, api: api_value };
    if let Ok(content) = serde_json::to_string_pretty(&entry) {
        let _ = std::fs::write(&path, content);
    }
}

/// Returns the age of the cache for `url` in seconds, or `None` if
/// there is no cache entry or its timestamp cannot be read.
#[cfg(not(feature = "update_cache"))]
pub(crate) fn get_cache_age_secs(url: &str) -> Option<u64> {
    let path = get_cache_path(url);
    let content = std::fs::read_to_string(path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&content).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(entry.timestamp_secs))
}

/// Fetch (or load from cache) the Gradio API info for `url`.
///
/// When the `update_cache` feature is active the spec is fetched from the
/// network and written to the local cache. Otherwise the local cache is
/// checked first. If no cache exists, a short-timeout network request is
/// attempted (10 s). On success the result is saved to the cache for future
/// builds. On timeout or connection failure a descriptive compile-time error
/// is returned.
pub(crate) fn get_api_info(url: &str, opts: ClientOptions) -> Result<gradio::structs::ApiInfo, String> {
    #[cfg(feature = "update_cache")]
    {
        let api = gradio::Client::new_sync(url, opts)
            .map(|client| client.view_api())
            .map_err(|e| e.to_string())?;
        save_api_to_cache(url, &api);
        return Ok(api);
    }
    #[cfg(not(feature = "update_cache"))]
    {
        // ── cache hit ────────────────────────────────────────────────────
        if let Some(api) = load_api_from_cache(url) {
            if let Some(age) = get_cache_age_secs(url) {
                const SECS_PER_DAY: u64 = 24 * 3600;
                const SEVEN_DAYS_SECS: u64 = 7 * SECS_PER_DAY;
                if age > SEVEN_DAYS_SECS {
                    let days = age / SECS_PER_DAY;
                    eprintln!(
                        "gradio_macro: cache for '{}' is {} day(s) old – \
                         run `cargo build --features gradio_macro/update_cache` to refresh",
                        url, days
                    );
                }
            }
            return Ok(api);
        }

        // ── no cache: short-timeout fetch ────────────────────────────────
        // This lets rust-analyzer / VS Code expand the macro without hanging
        // indefinitely when no cache exists. If the endpoint is unreachable
        // the build fails with a clear error after at most 10 seconds.
        //
        // The timeout is enforced *inside* the async runtime so the spawned
        // thread exits as soon as the deadline fires rather than lingering
        // until the OS tears down the proc-macro process.
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        let url_owned = url.to_string();
        let hf_token = opts.hf_token;
        let auth = opts.auth;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let fetch_opts = gradio::ClientOptions { hf_token, auth };
            let result = rt.block_on(async move {
                match tokio::time::timeout(
                    FETCH_TIMEOUT,
                    gradio::Client::new(&url_owned, fetch_opts),
                )
                .await
                {
                    Ok(Ok(client)) => Ok(client.view_api()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err(format!(
                        "timed out after {} s",
                        FETCH_TIMEOUT.as_secs()
                    )),
                }
            });
            let _ = tx.send(result);
        });

        // Give a small extra buffer beyond the async timeout so we don't race
        // the channel send; the real bounding is done inside the runtime above.
        match rx.recv_timeout(FETCH_TIMEOUT + std::time::Duration::from_secs(2)) {
            Ok(Ok(api)) => {
                // Persist the freshly-fetched spec so the next build is instant.
                save_api_to_cache(url, &api);
                Ok(api)
            }
            Ok(Err(e)) => Err(format!(
                "No cache found for the endpoint and failed to fetch the spec from the \
                 endpoint: {}. Please make sure you are online and the endpoint is \
                 correct, or enable update_cache feature to fetch the spec with normal \
                 timeout.",
                e
            )),
            Err(_) => Err(
                "No cache found for the endpoint and failed to fetch the spec from the \
                 endpoint. Please make sure you are online and the endpoint is correct, \
                 or enable update_cache feature to fetch the spec with normal timeout."
                    .to_string(),
            ),
        }
    }
}
