use gradio::ClientOptions;
use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::{Ident, Span};
use proc_macro::TokenStream;
use syn::{parse_macro_input, punctuated::Punctuated, Expr, ItemStruct, Meta};
use quote::quote;

#[derive(Clone, Copy)]
enum Syncity {
    Sync,
    Async,
}

/// Parse `Literal['a', 'b', 'c']` or `Literal["x", "y"]` Python type annotations into a list of
/// string variants. Returns `None` when the type string is not a Literal type.
fn parse_literal_variants(python_type: &str) -> Option<Vec<String>> {
    if !python_type.contains("Literal[") {
        return None;
    }
    let start = python_type.find("Literal[")? + "Literal[".len();
    let inner = &python_type[start..];
    let end = inner.rfind(']')?;
    let inner = &inner[..end];

    let mut variants = Vec::new();
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\'' || chars[i] == '"' {
            let quote_char = chars[i];
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != quote_char {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    s.push(chars[i]);
                } else {
                    s.push(chars[i]);
                }
                i += 1;
            }
            variants.push(s);
        }
        i += 1;
    }

    if variants.is_empty() { None } else { Some(variants) }
}

fn make_compile_error(message: &str) -> TokenStream {
    syn::Error::new(Span::call_site(), message).to_compile_error().into()
}

/// Returns the path to the cache file for the given URL.
/// The cache is stored in `.gradio_cache/` relative to `CARGO_MANIFEST_DIR`.
///
/// The filename encodes the URL by percent-encoding non-alphanumeric/dash/dot
/// characters, which avoids collisions between URLs that only differ in
/// separator characters (e.g. `a/b` vs `a_b`).
fn get_cache_path(url: &str) -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let encoded: String = url
        .chars()
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
                    s.bytes().flat_map(|b| format!("%{:02X}", b).chars().collect::<Vec<_>>()).collect()
                }
            }
        })
        .collect();
    std::path::PathBuf::from(manifest_dir)
        .join(".gradio_cache")
        .join(format!("{}.json", encoded))
}

/// Cache file envelope that stores the API spec together with a fetch timestamp.
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    timestamp_secs: u64,
    api: serde_json::Value,
}

/// Load the API info from the local cache file, if present and valid.
#[cfg(not(feature = "update_cache"))]
fn load_api_from_cache(url: &str) -> Option<gradio::structs::ApiInfo> {
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
fn save_api_to_cache(url: &str, api: &gradio::structs::ApiInfo) {
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
fn get_cache_age_secs(url: &str) -> Option<u64> {
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
fn get_api_info(url: &str, opts: ClientOptions) -> Result<gradio::structs::ApiInfo, String> {
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

/// Holds all code-generation fragments for a single API parameter.
struct ParamCodegen {
    /// The Rust type for the function parameter (may use `impl Trait`).
    rust_type: proc_macro2::TokenStream,
    /// Concrete Rust type for struct/builder fields.
    field_type: proc_macro2::TokenStream,
    /// Optional binding to convert from `rust_type` to `field_type`.
    binding: Option<proc_macro2::TokenStream>,
    /// Optional runtime validation expression (e.g. file-existence check).
    validation: Option<proc_macro2::TokenStream>,
    /// Expression that constructs a `gradio::PredictionInput` from the (bound) ident.
    call_expr: proc_macro2::TokenStream,
    /// Rust field type used in the generated clap CLI struct.
    cli_type: proc_macro2::TokenStream,
    /// Extra clap `#[arg(...)]` attributes for the generated CLI field.
    cli_arg_attrs: Vec<proc_macro2::TokenStream>,
    /// Optional enum type definition to emit (for Literal types in `gradio_api`).
    enum_def: Option<proc_macro2::TokenStream>,
}

/// Map a Gradio API parameter to a [`ParamCodegen`] with all code fragments.
fn map_type(
    python_type: &str,
    is_file: bool,
    arg_ident: &Ident,
    has_default: bool,
    default_value: Option<&serde_json::Value>,
    enum_type_name: &str,
) -> ParamCodegen {
    // ── filepath ──────────────────────────────────────────────────────────
    if is_file {
        let file_exists_check = quote! {
            {
                let __path: &std::path::Path = #arg_ident.as_ref();
                if !__path.is_file() {
                    return Err(gradio::anyhow::anyhow!(
                        "Path for parameter `{}` is not a file: {}",
                        stringify!(#arg_ident),
                        __path.display()
                    ));
                }
            }
        };
        return ParamCodegen {
            rust_type: quote! { impl Into<std::path::PathBuf> + AsRef<std::path::Path> },
            field_type: quote! { std::path::PathBuf },
            binding: Some(quote! { let #arg_ident: std::path::PathBuf = #arg_ident.into(); }),
            validation: Some(file_exists_check),
            call_expr: quote! { gradio::PredictionInput::from_file(&#arg_ident) },
            cli_type: quote! { std::path::PathBuf },
            cli_arg_attrs: vec![],
            enum_def: None,
        };
    }

    // ── Literal['a', 'b', ...] ────────────────────────────────────────────
    if let Some(variants) = parse_literal_variants(python_type) {
        let variant_strs: Vec<&str> = variants.iter().map(|s| s.as_str()).collect();
        let allowed_msg = variant_strs.join(", ");

        // Build clap attrs (kept for CLI use regardless of enum_type_name)
        let mut cli_arg_attrs = vec![
            quote! { value_parser = [#(#variant_strs),*] },
        ];
        if has_default {
            if let Some(dv) = default_value {
                let dv_str = match dv {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                cli_arg_attrs.push(quote! { default_value = #dv_str });
            }
        }

        if !enum_type_name.is_empty() {
            // Generate a typed enum instead of runtime validation
            let enum_ident = Ident::new(enum_type_name, Span::call_site());

            // Deduplicate variant names: if safe_variant_ident produces the same
            // identifier for different strings (e.g. non-ASCII chars all → "Variant"),
            // append a numeric suffix (Variant2, Variant3, …).
            let mut used_names: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            let variant_idents: Vec<Ident> = variants.iter()
                .map(|s| {
                    let base = safe_variant_ident(s, "Variant").to_string();
                    let count = used_names.entry(base.clone()).or_insert(0);
                    *count += 1;
                    if *count == 1 {
                        Ident::new(&base, Span::call_site())
                    } else {
                        Ident::new(&format!("{}{}", base, count), Span::call_site())
                    }
                })
                .collect();

            let enum_def = quote! {
                #[derive(Debug, Clone, Copy, PartialEq, Eq)]
                pub enum #enum_ident {
                    #(#variant_idents,)*
                }

                impl std::fmt::Display for #enum_ident {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        match self {
                            #(Self::#variant_idents => write!(f, #variant_strs),)*
                        }
                    }
                }

                impl gradio::serde::Serialize for #enum_ident {
                    fn serialize<S: gradio::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                        serializer.serialize_str(&self.to_string())
                    }
                }

                impl<'de> gradio::serde::Deserialize<'de> for #enum_ident {
                    fn deserialize<D: gradio::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                        let s = <String as gradio::serde::Deserialize<'de>>::deserialize(deserializer)?;
                        match s.as_str() {
                            #(#variant_strs => Ok(Self::#variant_idents),)*
                            _ => Err(gradio::serde::de::Error::custom(format!("unknown variant: {}", s))),
                        }
                    }
                }

                impl std::str::FromStr for #enum_ident {
                    type Err = String;
                    fn from_str(s: &str) -> Result<Self, Self::Err> {
                        match s {
                            #(#variant_strs => Ok(Self::#variant_idents),)*
                            _ => Err(format!("unknown variant for {}: {}", stringify!(#enum_ident), s)),
                        }
                    }
                }
            };

            return ParamCodegen {
                rust_type: quote! { #enum_ident },
                field_type: quote! { #enum_ident },
                binding: None,
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
                cli_type: quote! { String },
                cli_arg_attrs,
                enum_def: Some(enum_def),
            };
        } else {
            // Runtime validation (used by gradio_cli with enum_type_name = "")
            let validation = quote! {
                {
                    const __ALLOWED: &[&str] = &[#(#variant_strs),*];
                    if !__ALLOWED.contains(&#arg_ident.as_str()) {
                        return Err(gradio::anyhow::anyhow!(
                            "Invalid value `{}` for parameter `{}`. Must be one of: {}",
                            #arg_ident,
                            stringify!(#arg_ident),
                            #allowed_msg,
                        ));
                    }
                }
            };

            return ParamCodegen {
                rust_type: quote! { impl Into<String> },
                field_type: quote! { String },
                binding: Some(quote! { let #arg_ident: String = #arg_ident.into(); }),
                validation: Some(validation),
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident.clone()) },
                cli_type: quote! { String },
                cli_arg_attrs,
                enum_def: None,
            };
        }
    }

    // ── Primitive types ───────────────────────────────────────────────────
    match python_type {
        "str" => {
            let mut cli_arg_attrs = vec![];
            if has_default {
                if let Some(dv) = default_value {
                    let dv_str = match dv {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    cli_arg_attrs.push(quote! { default_value = #dv_str });
                }
            }
            ParamCodegen {
                rust_type: quote! { impl Into<String> },
                field_type: quote! { String },
                binding: Some(quote! { let #arg_ident: String = #arg_ident.into(); }),
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident.clone()) },
                cli_type: quote! { String },
                cli_arg_attrs,
                enum_def: None,
            }
        }
        "float" => {
            let mut cli_arg_attrs = vec![];
            if has_default {
                if let Some(dv) = default_value {
                    let dv_str = dv.to_string();
                    cli_arg_attrs.push(quote! { default_value = #dv_str });
                }
            }
            ParamCodegen {
                rust_type: quote! { f64 },
                field_type: quote! { f64 },
                binding: None,
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
                cli_type: quote! { f64 },
                cli_arg_attrs,
                enum_def: None,
            }
        }
        "int" => {
            let mut cli_arg_attrs = vec![];
            if has_default {
                if let Some(dv) = default_value {
                    let dv_str = dv.to_string();
                    cli_arg_attrs.push(quote! { default_value = #dv_str });
                }
            }
            ParamCodegen {
                rust_type: quote! { i64 },
                field_type: quote! { i64 },
                binding: None,
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
                cli_type: quote! { i64 },
                cli_arg_attrs,
                enum_def: None,
            }
        }
        "bool" => {
            let mut cli_arg_attrs = vec![];
            if has_default {
                if let Some(dv) = default_value {
                    let dv_str = dv.to_string();
                    cli_arg_attrs.push(quote! { default_value = #dv_str });
                }
            }
            ParamCodegen {
                rust_type: quote! { bool },
                field_type: quote! { bool },
                binding: None,
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
                cli_type: quote! { bool },
                cli_arg_attrs,
                enum_def: None,
            }
        }
        _ => ParamCodegen {
            rust_type: quote! { impl gradio::serde::Serialize },
            field_type: quote! { serde_json::Value },
            binding: Some(quote! { let #arg_ident = serde_json::to_value(#arg_ident).unwrap_or(serde_json::Value::Null); }),
            validation: None,
            call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
            cli_type: quote! { String },
            cli_arg_attrs: vec![],
            enum_def: None,
        },
    }
}

/// A comprehensive list of Rust keywords that cannot be used as plain identifiers.
const RUST_KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const",
    "continue", "crate", "do", "dyn", "else", "enum", "extern", "false",
    "final", "fn", "for", "if", "impl", "in", "let", "loop", "macro",
    "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "try", "type", "typeof", "union", "unsafe", "unsized", "use", "virtual",
    "where", "while", "yield",
];

/// Convert a name string into a valid Rust snake_case identifier.
/// - Prefixes with `arg_` when the result starts with a digit or is empty.
/// - Appends `_` when the result is a Rust keyword.
fn safe_ident(name: &str, fallback: &str) -> Ident {
    let snake_cased = name.to_snake_case();
    let with_fallback = if snake_cased.is_empty() {
        fallback.to_snake_case()
    } else {
        snake_cased
    };
    let with_prefix = if with_fallback
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        format!("arg_{}", with_fallback)
    } else {
        with_fallback
    };
    let final_name = if RUST_KEYWORDS.contains(&with_prefix.as_str()) {
        format!("{}_", with_prefix)
    } else {
        with_prefix
    };
    Ident::new(&final_name, Span::call_site())
}

/// Convert a name string into a valid Rust `UpperCamelCase` (PascalCase) identifier suitable
/// for use as an enum variant name.
fn safe_variant_ident(name: &str, fallback: &str) -> Ident {
    let camel = name.to_upper_camel_case();
    let with_fallback = if camel.is_empty() {
        fallback.to_upper_camel_case()
    } else {
        camel
    };
    let with_prefix = if with_fallback
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        format!("Variant{}", with_fallback)
    } else {
        with_fallback
    };
    let final_name = if RUST_KEYWORDS.contains(&with_prefix.as_str()) {
        format!("{}_", with_prefix)
    } else {
        with_prefix
    };
    Ident::new(&final_name, Span::call_site())
}

/// Compute the Rust expression used to initialise an optional builder field.
fn make_default_expr(
    python_type: &str,
    is_file: bool,
    default_value: Option<&serde_json::Value>,
    enum_ident: Option<&Ident>,
    variant_strs: Option<&[String]>,
) -> proc_macro2::TokenStream {
    if is_file {
        return match default_value {
            Some(serde_json::Value::String(s)) => {
                let s = s.clone();
                quote! { std::path::PathBuf::from(#s) }
            }
            _ => quote! { std::path::PathBuf::new() },
        };
    }
    // Literal / enum
    if let (Some(eid), Some(vstrs)) = (enum_ident, variant_strs) {
        if let Some(serde_json::Value::String(s)) = default_value {
            for v in vstrs {
                if v == s {
                    let vi = safe_variant_ident(v, "Variant");
                    return quote! { #eid::#vi };
                }
            }
        }
        if let Some(first) = vstrs.first() {
            let vi = safe_variant_ident(first, "Variant");
            return quote! { #eid::#vi };
        }
        return quote! { Default::default() };
    }
    match default_value {
        Some(serde_json::Value::String(s)) => {
            let s = s.clone();
            quote! { #s.to_string() }
        }
        Some(serde_json::Value::Number(n)) => match python_type {
            "int" => {
                let v = proc_macro2::Literal::i64_suffixed(n.as_i64().unwrap_or(0));
                quote! { #v }
            }
            _ => {
                let v = proc_macro2::Literal::f64_suffixed(n.as_f64().unwrap_or(0.0));
                quote! { #v }
            }
        },
        Some(serde_json::Value::Bool(b)) => quote! { #b },
        Some(serde_json::Value::Null) => quote! { serde_json::Value::Null },
        None => match python_type {
            "str" => quote! { String::new() },
            "float" => quote! { 0.0_f64 },
            "int" => quote! { 0_i64 },
            "bool" => quote! { false },
            _ => quote! { serde_json::Value::Null },
        },
        _ => quote! { serde_json::Value::Null },
    }
}

/// Build the doc-comment token streams for an endpoint's factory method.
///
/// Only mandatory (non-optional) parameters are listed in the factory-method doc.
/// Optional parameters are documented individually in their `.with_xxx()` setter.
///
/// Returns:
/// - `factory_doc_attrs`: `#[doc = ...]` attrs for the factory method.
/// - `bg_doc`: Short doc string for `call_background()`.
fn build_doc_attrs(
    name: &str,
    method_name: &Ident,
    info: &gradio::structs::EndpointInfo,
    optional_flags: &[bool],
) -> (Vec<proc_macro2::TokenStream>, String) {
    let mut doc_lines: Vec<String> = Vec::new();
    doc_lines.push(format!("Calls the `{}` Gradio endpoint.", name));
    doc_lines.push(String::new());

    let mandatory_params: Vec<_> = info
        .parameters
        .iter()
        .enumerate()
        .filter(|(i, _)| !optional_flags.get(*i).copied().unwrap_or(false))
        .collect();

    if !mandatory_params.is_empty() {
        doc_lines.push("# Parameters".to_string());
        doc_lines.push(String::new());
        for (i, param) in &mandatory_params {
            let ident_name = param
                .parameter_name
                .as_deref()
                .or(param.label.as_deref())
                .unwrap_or(&format!("arg{}", i))
                .to_snake_case();
            let py_type = &param.python_type.r#type;
            let description = param.python_type.description.trim();
            let label = param.label.as_deref().unwrap_or("").trim();
            let detail = if !description.is_empty() {
                description.to_string()
            } else if !label.is_empty() {
                label.to_string()
            } else {
                String::new()
            };
            if detail.is_empty() {
                doc_lines.push(format!("- `{}` (`{}`)", ident_name, py_type));
            } else {
                doc_lines.push(format!("- `{}` (`{}`): {}", ident_name, py_type, detail));
            }
        }
    }

    if !info.returns.is_empty() {
        doc_lines.push(String::new());
        doc_lines.push("# Returns".to_string());
        doc_lines.push(String::new());
        for ret in &info.returns {
            let ret_label = ret.label.as_deref().unwrap_or("output");
            let py_type = &ret.python_type.r#type;
            let description = ret.python_type.description.trim();
            if description.is_empty() {
                doc_lines.push(format!("- `{}` (`{}`)", ret_label, py_type));
            } else {
                doc_lines.push(format!("- `{}` (`{}`): {}", ret_label, py_type, description));
            }
        }
    }

    let factory_doc_attrs: Vec<proc_macro2::TokenStream> = doc_lines
        .iter()
        .map(|line| quote! { #[doc = #line] })
        .collect();
    let bg_doc = format!(
        "Submits the `{}` Gradio endpoint (`{}`) and returns a streaming handle.\nSee [`{}`] for parameter documentation.",
        name, method_name, method_name
    );
    (factory_doc_attrs, bg_doc)
}

/// Build the doc string for a single optional-parameter setter (`.with_xxx()`).
fn build_setter_doc(param: &gradio::structs::ApiData, index: usize) -> String {
    let raw_name = param
        .parameter_name
        .as_deref()
        .or(param.label.as_deref())
        .map(|name| name.to_owned())
        .unwrap_or_else(|| format!("arg{}", index));
    let ident_name = safe_ident(&raw_name.to_snake_case(), &format!("arg{}", index)).to_string();
    let py_type = &param.python_type.r#type;
    let description = param.python_type.description.trim();
    let label = param.label.as_deref().unwrap_or("").trim();
    let detail = if !description.is_empty() {
        format!(" — {}", description)
    } else if !label.is_empty() {
        format!(" — {}", label)
    } else {
        String::new()
    };
    let default_part = if param.parameter_has_default.unwrap_or(false) {
        if let Some(dv) = &param.parameter_default {
            format!(" (default: `{}`)", dv)
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    format!(
        "Sets the `{}` optional parameter (`{}`){}{}.",
        ident_name, py_type, detail, default_part
    )
}

/// Build a human-readable summary of all named endpoints from an API spec.
///
/// The output mirrors the format that `gradio-rs` prints when you call
/// `client.view_api()` on the command line.
fn build_api_string(
    api: &std::collections::HashMap<String, gradio::structs::EndpointInfo>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut names: Vec<&String> = api.keys().collect();
    names.sort();
    for name in names {
        let info = &api[name];
        lines.push(format!("{}:", name));
        if !info.parameters.is_empty() {
            lines.push("  Parameters:".to_string());
            for (i, param) in info.parameters.iter().enumerate() {
                let fallback = format!("arg{}", i);
                let label = param
                    .parameter_name
                    .as_deref()
                    .or(param.label.as_deref())
                    .unwrap_or(&fallback);
                let py_type = &param.python_type.r#type;
                let desc = param.python_type.description.trim();
                let has_default = param.parameter_has_default.unwrap_or(false);
                let default_str = if has_default {
                    if let Some(dv) = &param.parameter_default {
                        format!(" [default: {}]", dv)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                if desc.is_empty() {
                    lines.push(format!("    {} ({}){}", label, py_type, default_str));
                } else {
                    lines.push(format!("    {} ({}){}  — {}", label, py_type, default_str, desc));
                }
            }
        } else {
            lines.push("  Parameters:  (none)".to_string());
        }
        if !info.returns.is_empty() {
            lines.push("  Returns:".to_string());
            for ret in &info.returns {
                let label = ret.label.as_deref().unwrap_or("output");
                let py_type = &ret.python_type.r#type;
                let desc = ret.python_type.description.trim();
                if desc.is_empty() {
                    lines.push(format!("    {} ({})", label, py_type));
                } else {
                    lines.push(format!("    {} ({})  — {}", label, py_type, desc));
                }
            }
        } else {
            lines.push("  Returns:  (none)".to_string());
        }
        lines.push(String::new());
    }
    lines.join("\n")
}


/// A procedural macro for generating a type-safe API client struct for a Gradio space.
///
/// The macro introspects the API spec at compile time (using a local cache) and generates:
///
/// - A struct (the name you give to `#[gradio_api]`) with a `new()` constructor.
/// - A **builder** returned by each named endpoint method. The builder always has:
///   - `.call()` — executes the prediction and returns `Vec<PredictionOutput>`.
///   - `.call_background()` — submits the prediction and returns a `PredictionStream` for
///     streaming queue/progress messages.
///   - `.call_cli()` *(async only)* — submits, pretty-prints queue/progress to `stderr` on the
///     same terminal line, then returns `Vec<PredictionOutput>`. No boilerplate needed.
///   - `.with_<param>()` setters for any **optional** parameters (those with API-level defaults).
/// - Typed Rust enums for `Literal[...]` Python types, with `Display`, `Serialize`,
///   `Deserialize`, and `FromStr` implementations.
/// - A `custom_endpoint()` method that returns a builder for calling arbitrary endpoints.
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
    let args = parse_macro_input!(args with Punctuated::<Meta, syn::Token![,]>::parse_terminated);
    let input = parse_macro_input!(input as ItemStruct);
    let (mut url, mut option, mut grad_token, mut grad_login, mut grad_password) =
        (None, None, None, None, None);

    for item in args.iter() {
        let Ok(meta_value) = item.require_name_value() else { continue; };
        let Expr::Lit(ref lit_val) = meta_value.value else { continue; };
        let syn::Lit::Str(ref lit_val) = lit_val.lit else { continue; };
        let arg_value = lit_val.value();
        if item.path().is_ident("url") {
            url = Some(arg_value);
        } else if item.path().is_ident("option") {
            option = Some(match arg_value.as_str() {
                "sync" => Syncity::Sync,
                "async" => Syncity::Async,
                _ => return make_compile_error(
                    "invalid value for `option`: expected \"sync\" or \"async\"",
                ),
            });
        } else if item.path().is_ident("hf_token") {
            grad_token = Some(arg_value);
        } else if item.path().is_ident("auth_username") {
            grad_login = Some(arg_value);
        } else if item.path().is_ident("auth_password") {
            grad_password = Some(arg_value);
        }
        // `cache` option silently ignored for backward compatibility
    }

    let Some(url) = url else {
        return make_compile_error("url is required");
    };

    let mut grad_opts = ClientOptions::default();
    let mut grad_auth = None;
    if grad_token.is_some() {
        grad_opts.hf_token = grad_token.clone();
    }
    if grad_login.is_some() ^ grad_password.is_some() {
        return make_compile_error("Both login and password must be present!");
    } else if grad_login.is_some() && grad_password.is_some() {
        grad_auth = Some((grad_login.clone().unwrap(), grad_password.clone().unwrap()));
        grad_opts.auth = grad_auth.clone();
    }

    let Some(option) = option else {
        return make_compile_error("option is required");
    };

    let api_info = match get_api_info(&url, grad_opts) {
        Ok(api) => api,
        Err(e) => return make_compile_error(&format!("Failed to fetch Gradio API for \"{}\": {}", url, e)),
    };
    let api = api_info.named_endpoints;

    // Build the two static strings that `.endpoints()` and `.api()` will return.
    let endpoints_json = match serde_json::to_string(&api) {
        Ok(json) => json,
        Err(e) => return make_compile_error(&format!(
            "Failed to serialize generated Gradio endpoint specification for \"{}\": {}",
            url, e
        )),
    };
    let api_human_str = build_api_string(&api);

    let grad_auth_ts = if grad_auth.is_some() {
        quote! { Some((#grad_login.to_string(), #grad_password.to_string())) }
    } else {
        quote! { None }
    };
    let grad_token_ts = if let Some(ref val) = grad_token {
        quote! { Some(#val.to_string()) }
    } else {
        quote! { std::env::var("HF_TOKEN").ok() }
    };
    let grad_opts_ts = quote! {
        gradio::ClientOptions {
            auth: #grad_auth_ts,
            hf_token: #grad_token_ts,
        }
    };

    let vis = input.vis.clone();
    let struct_name = input.ident.clone();
    let struct_name_str = struct_name.to_string();

    let mut enum_defs: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut output_types: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut builder_structs: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut builder_impls: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut functions: Vec<proc_macro2::TokenStream> = Vec::new();

    for (name, info) in api.iter() {
        let ep_camel = name.trim_start_matches('/').to_upper_camel_case();
        let method_name = safe_ident(name, &format!("endpoint_{}", functions.len()));

        // ── Per-param data ────────────────────────────────────────────────
        let mut p_idents: Vec<Ident> = Vec::new();
        let mut p_rust_types: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut p_field_types: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut p_bindings: Vec<Option<proc_macro2::TokenStream>> = Vec::new();
        let mut p_validations: Vec<Option<proc_macro2::TokenStream>> = Vec::new();
        let mut p_call_exprs: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut p_is_optional: Vec<bool> = Vec::new();
        let mut p_python_types: Vec<String> = Vec::new();
        let mut p_is_file: Vec<bool> = Vec::new();
        let mut p_defaults: Vec<Option<serde_json::Value>> = Vec::new();
        let mut p_variants: Vec<Option<Vec<String>>> = Vec::new();
        let mut p_enum_type_names: Vec<String> = Vec::new();

        for (i, param) in info.parameters.iter().enumerate() {
            let ident = param
                .parameter_name
                .as_deref()
                .or(param.label.as_deref())
                .map(|n| safe_ident(n, &format!("arg{}", i)))
                .unwrap_or_else(|| Ident::new(&format!("arg{}", i), Span::call_site()));

            let is_file = param.python_type.r#type == "filepath";
            let is_optional = param.parameter_has_default.unwrap_or(false);

            let param_camel = param
                .parameter_name
                .as_deref()
                .or(param.label.as_deref())
                .map(|n| n.to_upper_camel_case())
                .unwrap_or_else(|| format!("Arg{}", i));

            let enum_type_name = format!("{}{}{}", struct_name_str, ep_camel, param_camel);

            let codegen = map_type(
                &param.python_type.r#type,
                is_file,
                &ident,
                is_optional,
                param.parameter_default.as_ref(),
                &enum_type_name,
            );

            if let Some(ed) = codegen.enum_def {
                enum_defs.push(ed);
            }

            let variants = parse_literal_variants(&param.python_type.r#type);

            p_idents.push(ident);
            p_rust_types.push(codegen.rust_type);
            p_field_types.push(codegen.field_type);
            p_bindings.push(codegen.binding);
            p_validations.push(codegen.validation);
            p_call_exprs.push(codegen.call_expr);
            p_is_optional.push(is_optional);
            p_python_types.push(param.python_type.r#type.clone());
            p_is_file.push(is_file);
            p_defaults.push(param.parameter_default.clone());
            p_variants.push(variants);
            p_enum_type_names.push(enum_type_name);
        }

        // ── Output types ──────────────────────────────────────────────────
        // Generate a typed output struct for each endpoint so that `call()`
        // returns a concrete struct with named fields instead of a raw
        // `Vec<gradio::PredictionOutput>`.
        //
        // The concrete field type is resolved from the Gradio API spec at
        // compile time:
        //   `filepath`  →  `gradio::GradioFileData`
        //   anything else  →  `serde_json::Value`
        //
        // No intermediate wrapper structs are generated — the field holds the
        // final value directly, so no `.as_file()` / `.as_value()` call is
        // needed at the call site.
        let output_struct_ident = Ident::new(
            &format!("{}{}Output", struct_name_str, ep_camel),
            Span::call_site(),
        );

        let mut output_struct_field_defs: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut output_try_from_fields: Vec<proc_macro2::TokenStream> = Vec::new();

        for (ret_idx, ret) in info.returns.iter().enumerate() {
            let ret_label = ret.label.as_deref().unwrap_or("output");
            let field_ident = safe_ident(ret_label, &format!("output_{}", ret_idx));

            let py_type = &ret.python_type.r#type;
            let desc = ret.python_type.description.trim();
            let is_ret_file = py_type == "filepath";

            // Concrete Rust type based on the API-spec return type.
            let field_rust_type: proc_macro2::TokenStream = if is_ret_file {
                quote! { gradio::GradioFileData }
            } else {
                quote! { serde_json::Value }
            };

            // Expression that converts a `PredictionOutput` into the concrete type.
            let extract_expr: proc_macro2::TokenStream = if is_ret_file {
                quote! { __item.as_file()? }
            } else {
                quote! { __item.as_value()? }
            };

            let field_doc = if desc.is_empty() {
                format!("`{}` (`{}`) output.", ret_label, py_type)
            } else {
                format!("`{}` (`{}`) output: {}", ret_label, py_type, desc)
            };
            output_struct_field_defs.push(quote! {
                #[doc = #field_doc]
                pub #field_ident: #field_rust_type,
            });

            output_try_from_fields.push(quote! {
                #field_ident: {
                    // Safety: exact count validated above.
                    let __item = __iter.next().unwrap();
                    #extract_expr
                },
            });
        }

        let output_struct_doc = format!(
            "Typed output of the `{}` endpoint of [`{}`].",
            name, struct_name_str
        );
        let expected_count_lit = proc_macro2::Literal::usize_suffixed(info.returns.len());
        let output_struct_def = quote! {
            #[doc = #output_struct_doc]
            #[derive(Clone, Debug)]
            pub struct #output_struct_ident {
                #(#output_struct_field_defs)*
            }

            impl std::convert::TryFrom<Vec<gradio::PredictionOutput>> for #output_struct_ident {
                type Error = gradio::anyhow::Error;

                fn try_from(outputs: Vec<gradio::PredictionOutput>) -> Result<Self, Self::Error> {
                    let __expected = #expected_count_lit;
                    let __actual = outputs.len();
                    if __actual != __expected {
                        return Err(gradio::anyhow::anyhow!(
                            "endpoint returned {} output(s) but the API spec expects {}",
                            __actual,
                            __expected,
                        ));
                    }
                    let mut __iter = outputs.into_iter();
                    Ok(Self {
                        #(#output_try_from_fields)*
                    })
                }
            }
        };

        output_types.push(output_struct_def);

        // ── Always use builder pattern ────────────────────────────────────
        // Even endpoints with only mandatory parameters return a builder with
        // `call()` and `call_background()` methods, giving a consistent API.
        let builder_ident = Ident::new(
            &format!("{}{}Builder", struct_name_str, ep_camel),
            Span::call_site(),
        );

        let (doc_attrs, bg_doc) = build_doc_attrs(name, &method_name, info, &p_is_optional);

        // Builder struct fields (client ref + all params)
        let mut builder_field_defs: Vec<proc_macro2::TokenStream> = Vec::new();
        builder_field_defs.push(quote! { client: &'a gradio::Client, });
        for j in 0..p_idents.len() {
            let id = &p_idents[j];
            let ft = &p_field_types[j];
            builder_field_defs.push(quote! { #id: #ft, });
        }

        let builder_doc = format!("Builder for the `{}` endpoint of [`{}`].", name, struct_name_str);
        let builder_struct = quote! {
            #[doc = #builder_doc]
            pub struct #builder_ident<'a> {
                #(#builder_field_defs)*
            }
        };
        builder_structs.push(builder_struct);

        // Factory method: mandatory params as direct args, optional params get defaults
        let mandatory_args: Vec<proc_macro2::TokenStream> = p_idents.iter()
            .zip(p_rust_types.iter())
            .zip(p_is_optional.iter())
            .filter(|(_, &opt)| !opt)
            .map(|((id, rt), _)| quote! { #id: #rt })
            .collect();

        let mandatory_bindings: Vec<proc_macro2::TokenStream> = p_bindings.iter()
            .zip(p_is_optional.iter())
            .filter(|(_, &opt)| !opt)
            .filter_map(|(b, _)| b.clone())
            .collect();

        let init_fields: Vec<proc_macro2::TokenStream> = (0..p_idents.len())
            .map(|j| {
                let id = &p_idents[j];
                if p_is_optional[j] {
                    let enum_ident_opt = if !p_enum_type_names[j].is_empty()
                        && p_variants[j].is_some()
                    {
                        Some(Ident::new(&p_enum_type_names[j], Span::call_site()))
                    } else {
                        None
                    };
                    let de = make_default_expr(
                        &p_python_types[j],
                        p_is_file[j],
                        p_defaults[j].as_ref(),
                        enum_ident_opt.as_ref(),
                        p_variants[j].as_deref(),
                    );
                    quote! { #id: #de }
                } else {
                    quote! { #id }
                }
            })
            .collect();

        let factory_method = quote! {
            #(#doc_attrs)*
            pub fn #method_name(&self, #(#mandatory_args),*) -> #builder_ident<'_> {
                #(#mandatory_bindings)*
                #builder_ident {
                    client: &self.client,
                    #(#init_fields),*
                }
            }
        };
        functions.push(factory_method);

        // Setter methods for optional params — each gets its own doc comment
        let setters: Vec<proc_macro2::TokenStream> = (0..p_idents.len())
            .filter(|&j| p_is_optional[j])
            .map(|j| {
                let id = &p_idents[j];
                let rt = &p_rust_types[j];
                let setter_name = Ident::new(&format!("with_{}", id), Span::call_site());
                let binding_ts = match &p_bindings[j] {
                    Some(b) => quote! { #b },
                    None => quote! {},
                };
                let setter_doc = if j < info.parameters.len() {
                    build_setter_doc(&info.parameters[j], j)
                } else {
                    format!("Sets the `{}` parameter.", id)
                };
                quote! {
                    #[doc = #setter_doc]
                    pub fn #setter_name(mut self, #id: #rt) -> Self {
                        #binding_ts
                        self.#id = #id;
                        self
                    }
                }
            })
            .collect();

        // Extract all fields in call()/call_background()
        let extract_fields: Vec<proc_macro2::TokenStream> = p_idents.iter()
            .map(|id| quote! { let #id = self.#id; })
            .collect();

        let validations: Vec<proc_macro2::TokenStream> = p_validations.iter()
            .filter_map(|v| v.clone())
            .collect();

        let call_exprs: Vec<&proc_macro2::TokenStream> = p_call_exprs.iter().collect();

        let call_methods = match option {
            Syncity::Async => quote! {
                /// Execute this request and return the typed output.
                pub async fn call(self) -> Result<#output_struct_ident, gradio::anyhow::Error> {
                    let __builder_client = self.client;
                    #(#extract_fields)*
                    #(#validations)*
                    let __raw = __builder_client.predict(#name, vec![#(#call_exprs),*]).await?;
                    std::convert::TryFrom::try_from(__raw)
                }

                #[doc = #bg_doc]
                pub async fn call_background(self) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    let __builder_client = self.client;
                    #(#extract_fields)*
                    #(#validations)*
                    __builder_client.submit(#name, vec![#(#call_exprs),*]).await
                }

                /// Submit this request and pretty-print queue / progress messages to `stderr`,
                /// then return the typed output.
                ///
                /// Uses `\r` to update the same terminal line while the task is queued or
                /// running, so the console stays clean. Equivalent to calling
                /// `.call_background().await?` and driving the stream yourself.
                pub async fn call_cli(self) -> Result<#output_struct_ident, gradio::anyhow::Error> {
                    use gradio::structs::QueueDataMessage;
                    let mut stream = self.call_background().await?;
                    loop {
                        match stream.next().await {
                            None => {
                                eprintln!();
                                return Err(gradio::anyhow::anyhow!("stream ended without a result"));
                            }
                            Some(Err(e)) => {
                                eprintln!("\r[error] {:?}                    ", e);
                                return Err(e);
                            }
                            Some(Ok(msg)) => match msg {
                                QueueDataMessage::Open => {
                                    eprint!("\rConnected, waiting in queue…    ");
                                }
                                QueueDataMessage::Estimation { rank, queue_size, rank_eta, .. } => {
                                    eprint!(
                                        "\rQueue position {}/{} (ETA: {:.1}s)  ",
                                        rank + 1,
                                        queue_size,
                                        rank_eta
                                    );
                                }
                                QueueDataMessage::ProcessStarts { .. } => {
                                    eprint!("\rProcessing…                          ");
                                }
                                QueueDataMessage::Progress { progress_data, .. } => {
                                    if let Some(pd) = progress_data {
                                        if let Some(p) = pd.first() {
                                            eprint!(
                                                "\rProgress: {}/{} {:?}    ",
                                                p.index + 1,
                                                p.length.unwrap_or(0),
                                                p.unit
                                            );
                                        }
                                    }
                                }
                                QueueDataMessage::ProcessCompleted { output, success, .. } => {
                                    eprintln!();
                                    if !success {
                                        return Err(gradio::anyhow::anyhow!("prediction failed"));
                                    }
                                    let __raw: Vec<gradio::PredictionOutput> =
                                        output.try_into().map_err(|e: gradio::anyhow::Error| e)?;
                                    return std::convert::TryFrom::try_from(__raw);
                                }
                                QueueDataMessage::Log { event_id } => {
                                    eprint!("\rLog: {}              ", event_id.unwrap_or_default());
                                }
                                QueueDataMessage::UnexpectedError { message } => {
                                    eprintln!("\r[unexpected error] {}              ", message.unwrap_or_default());
                                }
                                QueueDataMessage::Heartbeat | QueueDataMessage::Unknown(_) => {}
                            },
                        }
                    }
                }
            },
            Syncity::Sync => quote! {
                /// Execute this request and return the typed output.
                pub fn call(self) -> Result<#output_struct_ident, gradio::anyhow::Error> {
                    let __builder_client = self.client;
                    #(#extract_fields)*
                    #(#validations)*
                    let __raw = __builder_client.predict_sync(#name, vec![#(#call_exprs),*])?;
                    std::convert::TryFrom::try_from(__raw)
                }

                #[doc = #bg_doc]
                pub fn call_background(self) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    let __builder_client = self.client;
                    #(#extract_fields)*
                    #(#validations)*
                    __builder_client.submit_sync(#name, vec![#(#call_exprs),*])
                }
            },
        };

        let builder_impl_doc = format!("Builder methods for the `{}` endpoint.", name);
        let builder_impl_ts = quote! {
            #[doc = #builder_impl_doc]
            impl<'a> #builder_ident<'a> {
                #(#setters)*
                #call_methods
            }
        };
        builder_impls.push(builder_impl_ts);
    }

    // ── Custom-endpoint builder ───────────────────────────────────────────
    let custom_builder_ident = Ident::new(
        &format!("{}CustomEndpointBuilder", struct_name_str),
        Span::call_site(),
    );
    let custom_builder_struct = quote! {
        /// Builder returned by [`custom_endpoint`] for calling an arbitrary Gradio endpoint.
        ///
        /// Use `.call()` to wait for the full output or `.call_background()` to receive a
        /// streaming [`gradio::PredictionStream`] handle.
        pub struct #custom_builder_ident<'a> {
            client: &'a gradio::Client,
            endpoint: String,
            arguments: Vec<gradio::PredictionInput>,
        }
    };

    let custom_builder_impl = match option {
        Syncity::Async => quote! {
            impl<'a> #custom_builder_ident<'a> {
                /// Execute the custom endpoint and return the raw outputs.
                pub async fn call(self) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    let Self { client, endpoint, arguments } = self;
                    client.predict(&endpoint, arguments).await
                }
                /// Submit the custom endpoint and return a streaming handle.
                pub async fn call_background(self) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    let Self { client, endpoint, arguments } = self;
                    client.submit(&endpoint, arguments).await
                }
                /// Submit and pretty-print queue / progress messages to `stderr`, then return the raw outputs.
                pub async fn call_cli(self) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    use gradio::structs::QueueDataMessage;
                    let mut stream = self.call_background().await?;
                    loop {
                        match stream.next().await {
                            None => {
                                eprintln!();
                                return Err(gradio::anyhow::anyhow!("stream ended without a result"));
                            }
                            Some(Err(e)) => {
                                eprintln!("\r[error] {:?}                    ", e);
                                return Err(e);
                            }
                            Some(Ok(msg)) => match msg {
                                QueueDataMessage::Open => {
                                    eprint!("\rConnected, waiting in queue…    ");
                                }
                                QueueDataMessage::Estimation { rank, queue_size, rank_eta, .. } => {
                                    eprint!(
                                        "\rQueue position {}/{} (ETA: {:.1}s)  ",
                                        rank + 1,
                                        queue_size,
                                        rank_eta
                                    );
                                }
                                QueueDataMessage::ProcessStarts { .. } => {
                                    eprint!("\rProcessing…                          ");
                                }
                                QueueDataMessage::Progress { progress_data, .. } => {
                                    if let Some(pd) = progress_data {
                                        if let Some(p) = pd.first() {
                                            eprint!(
                                                "\rProgress: {}/{} {:?}    ",
                                                p.index + 1,
                                                p.length.unwrap_or(0),
                                                p.unit
                                            );
                                        }
                                    }
                                }
                                QueueDataMessage::ProcessCompleted { output, success, .. } => {
                                    eprintln!();
                                    if !success {
                                        return Err(gradio::anyhow::anyhow!("prediction failed"));
                                    }
                                    return output.try_into().map_err(|e: gradio::anyhow::Error| e);
                                }
                                QueueDataMessage::Log { event_id } => {
                                    eprint!("\rLog: {}              ", event_id.unwrap_or_default());
                                }
                                QueueDataMessage::UnexpectedError { message } => {
                                    eprintln!("\r[unexpected error] {}              ", message.unwrap_or_default());
                                }
                                QueueDataMessage::Heartbeat | QueueDataMessage::Unknown(_) => {}
                            },
                        }
                    }
                }
            }
        },
        Syncity::Sync => quote! {
            impl<'a> #custom_builder_ident<'a> {
                /// Execute the custom endpoint and return the raw outputs.
                pub fn call(self) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    let Self { client, endpoint, arguments } = self;
                    client.predict_sync(&endpoint, arguments)
                }
                /// Submit the custom endpoint and return a streaming handle.
                pub fn call_background(self) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    let Self { client, endpoint, arguments } = self;
                    client.submit_sync(&endpoint, arguments)
                }
            }
        },
    };

    // Methods shared by both sync and async variants that expose the embedded API spec.
    let endpoints_doc = format!(
        "Returns the raw JSON spec for the named endpoints of the `{}` Gradio space.",
        url
    );
    let api_doc = format!(
        "Returns a human-readable description of all endpoints of the `{}` Gradio space.",
        url
    );
    let spec_methods = quote! {
        #[doc = #endpoints_doc]
        ///
        /// The value is the `named_endpoints` map from the Gradio `/info` response,
        /// serialised to JSON at compile time and embedded in the binary.
        /// The JSON is parsed at most once per process (cached in an `OnceLock`).
        pub fn endpoints(&self) -> serde_json::Value {
            static ENDPOINTS: std::sync::OnceLock<serde_json::Value> =
                std::sync::OnceLock::new();
            ENDPOINTS
                .get_or_init(|| {
                    serde_json::from_str(#endpoints_json)
                        .expect("embedded endpoint spec is valid JSON")
                })
                .clone()
        }

        #[doc = #api_doc]
        pub fn api(&self) -> &'static str {
            #api_human_str
        }
    };

    // Build the final output
    let api_struct = match option {
        Syncity::Sync => quote! {
            #(#enum_defs)*

            #(#output_types)*

            #(#builder_structs)*

            #custom_builder_struct

            #vis struct #struct_name {
                client: gradio::Client,
            }

            #(#builder_impls)*

            #custom_builder_impl

            #[allow(clippy::too_many_arguments)]
            impl #struct_name {
                /// Create a new client connecting to the configured Gradio space.
                ///
                /// Reads `HF_TOKEN` from the environment when no `hf_token` was given to the macro.
                pub fn new() -> Result<Self, gradio::anyhow::Error> {
                    let client = gradio::Client::new_sync(#url, #grad_opts_ts)?;
                    Ok(Self { client })
                }

                /// Build a request for an arbitrary endpoint not covered by the generated methods.
                ///
                /// Returns a builder with `.call()` and `.call_background()` methods.
                pub fn custom_endpoint(
                    &self,
                    endpoint: impl Into<String>,
                    arguments: Vec<gradio::PredictionInput>,
                ) -> #custom_builder_ident<'_> {
                    #custom_builder_ident {
                        client: &self.client,
                        endpoint: endpoint.into(),
                        arguments,
                    }
                }

                #spec_methods

                #(#functions)*
            }
        },
        Syncity::Async => quote! {
            #(#enum_defs)*

            #(#output_types)*

            #(#builder_structs)*

            #custom_builder_struct

            #vis struct #struct_name {
                client: gradio::Client,
            }

            #(#builder_impls)*

            #custom_builder_impl

            #[allow(clippy::too_many_arguments)]
            impl #struct_name {
                /// Create a new client connecting to the configured Gradio space.
                ///
                /// Reads `HF_TOKEN` from the environment when no `hf_token` was given to the macro.
                pub async fn new() -> Result<Self, gradio::anyhow::Error> {
                    let client = gradio::Client::new(#url, #grad_opts_ts).await?;
                    Ok(Self { client })
                }

                /// Build a request for an arbitrary endpoint not covered by the generated methods.
                ///
                /// Returns a builder with `.call()` and `.call_background()` methods.
                pub fn custom_endpoint(
                    &self,
                    endpoint: impl Into<String>,
                    arguments: Vec<gradio::PredictionInput>,
                ) -> #custom_builder_ident<'_> {
                    #custom_builder_ident {
                        client: &self.client,
                        endpoint: endpoint.into(),
                        arguments,
                    }
                }

                #spec_methods

                #(#functions)*
            }
        },
    };

    api_struct.into()
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
/// ```rust
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
    let args = parse_macro_input!(args with Punctuated::<Meta, syn::Token![,]>::parse_terminated);
    let input = parse_macro_input!(input as ItemStruct);
    let (mut url, mut option, mut grad_token, mut grad_login, mut grad_password) =
        (None, None, None, None, None);

    for item in args.iter() {
        let Ok(meta_value) = item.require_name_value() else { continue; };
        let Expr::Lit(ref lit_val) = meta_value.value else { continue; };
        let syn::Lit::Str(ref lit_val) = lit_val.lit else { continue; };
        let arg_value = lit_val.value();
        if item.path().is_ident("url") {
            url = Some(arg_value);
        } else if item.path().is_ident("option") {
            option = Some(match arg_value.as_str() {
                "sync" => Syncity::Sync,
                "async" => Syncity::Async,
                _ => return make_compile_error(
                    "invalid value for `option`: expected \"sync\" or \"async\"",
                ),
            });
        } else if item.path().is_ident("hf_token") {
            grad_token = Some(arg_value);
        } else if item.path().is_ident("auth_username") {
            grad_login = Some(arg_value);
        } else if item.path().is_ident("auth_password") {
            grad_password = Some(arg_value);
        }
        // `cache` option silently ignored for backward compatibility
    }

    let Some(url) = url else {
        return make_compile_error("url is required");
    };

    let mut grad_opts = ClientOptions::default();
    let mut grad_auth: Option<(String, String)> = None;
    if grad_token.is_some() {
        grad_opts.hf_token = grad_token.clone();
    }
    if grad_login.is_some() ^ grad_password.is_some() {
        return make_compile_error("Both login and password must be present!");
    } else if grad_login.is_some() && grad_password.is_some() {
        grad_auth = Some((grad_login.clone().unwrap(), grad_password.clone().unwrap()));
        grad_opts.auth = grad_auth.clone();
    }

    let Some(option) = option else {
        return make_compile_error("option is required");
    };

    let api = match get_api_info(&url, grad_opts) {
        Ok(api) => api,
        Err(e) => return make_compile_error(&format!("Failed to fetch Gradio API for \"{}\": {}", url, e)),
    };

    let grad_auth_ts = if grad_auth.is_some() {
        quote! { Some((#grad_login.to_string(), #grad_password.to_string())) }
    } else {
        quote! { None }
    };
    let grad_token_ts = if let Some(ref val) = grad_token {
        quote! { Some(#val.to_string()) }
    } else {
        quote! { std::env::var("HF_TOKEN").ok() }
    };
    let grad_opts_ts = quote! {
        gradio::ClientOptions {
            auth: #grad_auth_ts,
            hf_token: #grad_token_ts,
        }
    };

    let vis = input.vis.clone();
    let struct_name = input.ident.clone();

    let cmd_enum_name = Ident::new(
        &format!("{}Command", struct_name),
        Span::call_site(),
    );

    let mut variants: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut match_arms: Vec<proc_macro2::TokenStream> = Vec::new();

    for (ep_name, info) in api.named_endpoints.iter() {
        let variant_name = safe_variant_ident(
            &ep_name.trim_start_matches('/').replace('/', "_"),
            "Default",
        );

        let mut field_defs: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut match_locals: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut call_inputs: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut field_idents: Vec<Ident> = Vec::new();

        for (i, param) in info.parameters.iter().enumerate() {
            let ident = param
                .parameter_name
                .as_deref()
                .or(param.label.as_deref())
                .map(|n| safe_ident(n, &format!("arg{}", i)))
                .unwrap_or_else(|| Ident::new(&format!("arg{}", i), Span::call_site()));

            let is_file = param.python_type.r#type == "filepath";
            // CLI always passes "" for enum_type_name to keep String + value_parser behavior
            let codegen = map_type(
                &param.python_type.r#type,
                is_file,
                &ident,
                param.parameter_has_default.unwrap_or(false),
                param.parameter_default.as_ref(),
                "",
            );

            let cli_type = codegen.cli_type;
            let cli_attrs = codegen.cli_arg_attrs;

            let label = param.label.as_deref().unwrap_or("");
            let py_type = &param.python_type.r#type;
            let field_doc = if label.is_empty() {
                format!("`{}` parameter ({})", ident, py_type)
            } else {
                format!("{} (`{}`)", label, py_type)
            };

            let match_call_expr = if is_file {
                quote! { gradio::PredictionInput::from_file(#ident) }
            } else {
                quote! { gradio::PredictionInput::from_value(#ident.clone()) }
            };

            if cli_attrs.is_empty() {
                field_defs.push(quote! {
                    #[doc = #field_doc]
                    #[arg(long)]
                    #ident: #cli_type,
                });
            } else {
                field_defs.push(quote! {
                    #[doc = #field_doc]
                    #[arg(long, #(#cli_attrs),*)]
                    #ident: #cli_type,
                });
            }

            if let Some(v) = codegen.validation {
                match_locals.push(v);
            }
            call_inputs.push(match_call_expr);
            field_idents.push(ident);
        }

        let variant_doc = format!("Calls the `{}` Gradio endpoint.", ep_name);
        let variant_def = quote! {
            #[doc = #variant_doc]
            #variant_name {
                #(#field_defs)*
            },
        };
        variants.push(variant_def);

        let match_arm_async = quote! {
            #cmd_enum_name::#variant_name { #(ref #field_idents),* } => {
                #(#match_locals)*
                client.predict(#ep_name, vec![#(#call_inputs),*]).await
            }
        };
        let match_arm_sync = quote! {
            #cmd_enum_name::#variant_name { #(ref #field_idents),* } => {
                #(#match_locals)*
                client.predict_sync(#ep_name, vec![#(#call_inputs),*])
            }
        };

        match option {
            Syncity::Async => match_arms.push(match_arm_async),
            Syncity::Sync => match_arms.push(match_arm_sync),
        }
    }

    let about_str = format!("Gradio API client for {}", url);
    let run_fn = match option {
        Syncity::Async => quote! {
            /// Run the selected subcommand against the Gradio API.
            pub async fn run(&self) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                let client = gradio::Client::new(#url, #grad_opts_ts).await?;
                match &self.command {
                    #(#match_arms)*
                }
            }
        },
        Syncity::Sync => quote! {
            /// Run the selected subcommand against the Gradio API.
            pub fn run(&self) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                let client = gradio::Client::new_sync(#url, #grad_opts_ts)?;
                match &self.command {
                    #(#match_arms)*
                }
            }
        },
    };

    let output = quote! {
        #[derive(Debug, clap::Parser)]
        #[command(about = #about_str)]
        #vis struct #struct_name {
            #[command(subcommand)]
            pub command: #cmd_enum_name,
        }

        #[derive(Debug, clap::Subcommand)]
        #vis enum #cmd_enum_name {
            #(#variants)*
        }

        impl #struct_name {
            #run_fn
        }
    };

    output.into()
}
