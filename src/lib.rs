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
#[cfg(feature = "update_cache")]
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
/// network and written to the local cache. Otherwise only the local cache is
/// used: a warning is printed when the cache is older than 7 days, and an
/// error is returned when no cache exists at all.
#[allow(unused_variables)]
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
        match load_api_from_cache(url) {
            Some(api) => {
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
                Ok(api)
            }
            None => Err(format!(
                "no cache for '{}' – run: cargo build --features gradio_macro/update_cache",
                url
            )),
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
            let variant_idents: Vec<Ident> = variants.iter()
                .map(|s| safe_variant_ident(s, "Variant"))
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
                        let s = String::deserialize(deserializer)?;
                        match s.as_str() {
                            #(#variant_strs => Ok(Self::#variant_idents),)*
                            _ => Err(gradio::serde::de::Error::custom(format!("unknown variant: {}", s))),
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
        return quote! { std::path::PathBuf::new() };
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
        _ => match python_type {
            "str" => quote! { String::new() },
            "float" => quote! { 0.0_f64 },
            "int" => quote! { 0_i64 },
            "bool" => quote! { false },
            _ => quote! { serde_json::Value::Null },
        },
    }
}

/// Build the doc-comment token streams for an endpoint.
fn build_doc_attrs(
    name: &str,
    method_name: &Ident,
    info: &gradio::structs::EndpointInfo,
) -> (Vec<proc_macro2::TokenStream>, String) {
    let mut doc_lines: Vec<String> = Vec::new();
    doc_lines.push(format!("Calls the `{}` Gradio endpoint.", name));
    doc_lines.push(String::new());

    if !info.parameters.is_empty() {
        doc_lines.push("# Parameters".to_string());
        doc_lines.push(String::new());
        for (i, param) in info.parameters.iter().enumerate() {
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
            if let Some(default) = &param.parameter_default {
                if param.parameter_has_default.unwrap_or(false) {
                    doc_lines.push(format!("  - Default: `{}`", default));
                }
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

    let doc_attrs: Vec<proc_macro2::TokenStream> = doc_lines
        .iter()
        .map(|line| quote! { #[doc = #line] })
        .collect();
    let bg_doc = format!(
        "Submits the `{}` Gradio endpoint (`{}`) and returns a streaming handle.\nSee [`{}`] for parameter documentation.",
        name, method_name, method_name
    );
    (doc_attrs, bg_doc)
}

/// A procedural macro for generating API client structs and methods for interacting with a Gradio-based API.
///
/// This macro generates a client struct for the specified Gradio API, along with methods to call the API endpoints
/// synchronously or asynchronously, depending on the provided option.
///
/// # Macro Parameters
///
/// - `url`: **Required**. The base URL of the Gradio API.
/// - `option`: **Required**. `"sync"` or `"async"`.
/// - `hf_token` (optional): HuggingFace API token for private spaces.
/// - `auth_username` (optional): HuggingFace username (must be paired with `auth_password`).
/// - `auth_password` (optional): HuggingFace password (must be paired with `auth_username`).
///
/// # API Caching
///
/// The macro caches the API spec as a JSON file under `.gradio_cache/` in your project root
/// (`CARGO_MANIFEST_DIR`). To refresh the cache:
///
/// ```sh
/// cargo build --features gradio_macro/update_cache
/// ```
///
/// You may commit the `.gradio_cache/` directory to version control for reproducible builds
/// without network access, or add it to `.gitignore` to always fetch fresh specs.
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

    let api = match get_api_info(&url, grad_opts) {
        Ok(api) => api,
        Err(e) => return make_compile_error(&format!("Failed to fetch Gradio API for \"{}\": {}", url, e)),
    };
    let api = api.named_endpoints;

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
    let mut builder_structs: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut builder_impls: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut functions: Vec<proc_macro2::TokenStream> = Vec::new();

    for (name, info) in api.iter() {
        let ep_camel = name.trim_start_matches('/').to_upper_camel_case();
        let method_name = safe_ident(name, &format!("endpoint_{}", functions.len()));

        let (doc_attrs, bg_doc) = build_doc_attrs(name, &method_name, info);

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

        let has_optional = p_is_optional.iter().any(|&v| v);

        if has_optional {
            // ── Builder pattern ───────────────────────────────────────────
            let builder_ident = Ident::new(
                &format!("{}{}Builder", struct_name_str, ep_camel),
                Span::call_site(),
            );

            // Builder struct fields
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

            // Setter methods for optional params
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
                    quote! {
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
                    /// Run this request and return the full output.
                    pub async fn call(self) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                        let __builder_client = self.client;
                        #(#extract_fields)*
                        #(#validations)*
                        __builder_client.predict(#name, vec![#(#call_exprs),*]).await
                    }

                    #[doc = #bg_doc]
                    pub async fn call_background(self) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                        let __builder_client = self.client;
                        #(#extract_fields)*
                        #(#validations)*
                        __builder_client.submit(#name, vec![#(#call_exprs),*]).await
                    }
                },
                Syncity::Sync => quote! {
                    /// Run this request and return the full output.
                    pub fn call(self) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                        let __builder_client = self.client;
                        #(#extract_fields)*
                        #(#validations)*
                        __builder_client.predict_sync(#name, vec![#(#call_exprs),*])
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
        } else {
            // ── Direct function (all params mandatory) ────────────────────
            let background_name = Ident::new(
                &format!("{}_background", method_name),
                Span::call_site(),
            );

            let args_def: Vec<proc_macro2::TokenStream> = p_idents.iter()
                .zip(p_rust_types.iter())
                .map(|(id, rt)| quote! { #id: #rt })
                .collect();

            let all_bindings: Vec<proc_macro2::TokenStream> = p_bindings.iter()
                .filter_map(|b| b.clone())
                .collect();

            let validations: Vec<proc_macro2::TokenStream> = p_validations.iter()
                .filter_map(|v| v.clone())
                .collect();

            let call_exprs: Vec<&proc_macro2::TokenStream> = p_call_exprs.iter().collect();

            let function = match option {
                Syncity::Sync => quote! {
                    #(#doc_attrs)*
                    pub fn #method_name(&self, #(#args_def),*) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                        #(#all_bindings)*
                        #(#validations)*
                        self.client.predict_sync(#name, vec![#(#call_exprs),*])
                    }

                    #[doc = #bg_doc]
                    pub fn #background_name(&self, #(#args_def),*) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                        #(#all_bindings)*
                        #(#validations)*
                        self.client.submit_sync(#name, vec![#(#call_exprs),*])
                    }
                },
                Syncity::Async => quote! {
                    #(#doc_attrs)*
                    pub async fn #method_name(&self, #(#args_def),*) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                        #(#all_bindings)*
                        #(#validations)*
                        self.client.predict(#name, vec![#(#call_exprs),*]).await
                    }

                    #[doc = #bg_doc]
                    pub async fn #background_name(&self, #(#args_def),*) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                        #(#all_bindings)*
                        #(#validations)*
                        self.client.submit(#name, vec![#(#call_exprs),*]).await
                    }
                },
            };

            functions.push(function);
        }
    }

    // Build the final output
    let api_struct = match option {
        Syncity::Sync => quote! {
            #(#enum_defs)*

            #(#builder_structs)*

            #vis struct #struct_name {
                client: gradio::Client,
            }

            #(#builder_impls)*

            #[allow(clippy::too_many_arguments)]
            impl #struct_name {
                /// Create a new client connecting to the configured Gradio space.
                pub fn new() -> Result<Self, gradio::anyhow::Error> {
                    let client = gradio::Client::new_sync(#url, #grad_opts_ts)?;
                    Ok(Self { client })
                }

                /// Call an arbitrary endpoint not covered by the generated methods.
                pub fn custom_endpoint(
                    &self,
                    endpoint: &str,
                    arguments: Vec<gradio::PredictionInput>,
                ) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    self.client.predict_sync(endpoint, arguments)
                }

                /// Submit an arbitrary endpoint and return a streaming handle.
                pub fn custom_endpoint_background(
                    &self,
                    endpoint: &str,
                    arguments: Vec<gradio::PredictionInput>,
                ) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    self.client.submit_sync(endpoint, arguments)
                }

                #(#functions)*
            }
        },
        Syncity::Async => quote! {
            #(#enum_defs)*

            #(#builder_structs)*

            #vis struct #struct_name {
                client: gradio::Client,
            }

            #(#builder_impls)*

            #[allow(clippy::too_many_arguments)]
            impl #struct_name {
                /// Create a new client connecting to the configured Gradio space.
                pub async fn new() -> Result<Self, gradio::anyhow::Error> {
                    let client = gradio::Client::new(#url, #grad_opts_ts).await?;
                    Ok(Self { client })
                }

                /// Call an arbitrary endpoint not covered by the generated methods.
                pub async fn custom_endpoint(
                    &self,
                    endpoint: &str,
                    arguments: Vec<gradio::PredictionInput>,
                ) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    self.client.predict(endpoint, arguments).await
                }

                /// Submit an arbitrary endpoint and return a streaming handle.
                pub async fn custom_endpoint_background(
                    &self,
                    endpoint: &str,
                    arguments: Vec<gradio::PredictionInput>,
                ) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    self.client.submit(endpoint, arguments).await
                }

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
/// async fn main() -> anyhow::Result<()> {
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
