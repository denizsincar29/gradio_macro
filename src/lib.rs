use gradio::ClientOptions;
use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::{Ident, Span};
use proc_macro::TokenStream;
use syn::{parse_macro_input, punctuated::Punctuated, Expr, ItemStruct, Meta};
use quote::quote;


enum Syncity {
    Sync,
    Async,
}

/// Parse `Literal['a', 'b', 'c']` or `Literal["x", "y"]` Python type annotations into a list of
/// string variants. Returns `None` when the type string is not a Literal type.
fn parse_literal_variants(python_type: &str) -> Option<Vec<String>> {
    // Quick path: must contain "Literal["
    if !python_type.contains("Literal[") {
        return None;
    }
    let start = python_type.find("Literal[")? + "Literal[".len();
    let inner = &python_type[start..];
    let end = inner.rfind(']')?;
    let inner = &inner[..end];

    // Collect all single-quoted or double-quoted strings
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
    // Percent-encode the URL so that `a/b` → `a%2Fb` and `a_b` → `a_b`
    // keeping only alphanumeric, `-`, `_`, and `.` as-is.
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
                    // Multi-byte: encode each UTF-8 byte
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

/// Load the API info from the local cache file, if present and valid.
fn load_api_from_cache(url: &str) -> Option<gradio::structs::ApiInfo> {
    let path = get_cache_path(url);
    if path.exists() {
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    } else {
        None
    }
}

/// Persist the API info to the local cache file.
fn save_api_to_cache(url: &str, api: &gradio::structs::ApiInfo) {
    let path = get_cache_path(url);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(api) {
        let _ = std::fs::write(&path, content);
    }
}

/// Fetch (or load from cache) the Gradio API info for `url`.
///
/// Cache is skipped when `refresh` is `true` or when the environment variable
/// `GRADIO_REFRESH_API_CACHE` is set to `"1"` or `"true"`.
fn get_api_info(
    url: &str,
    opts: ClientOptions,
    refresh: bool,
) -> Result<gradio::structs::ApiInfo, String> {
    let force_refresh = refresh
        || std::env::var("GRADIO_REFRESH_API_CACHE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

    if !force_refresh {
        if let Some(cached) = load_api_from_cache(url) {
            return Ok(cached);
        }
    }

    let api = gradio::Client::new_sync(url, opts)
        .map(|client| client.view_api())
        .map_err(|e| e.to_string())?;

    save_api_to_cache(url, &api);
    Ok(api)
}

/// Holds all code-generation fragments for a single API parameter.
struct ParamCodegen {
    /// The Rust type for the function parameter (e.g. `impl Into<String>`, `f64`).
    rust_type: proc_macro2::TokenStream,
    /// Optional let-binding to convert the caller's value to a concrete type
    /// (e.g. `let ident = ident.into();`).
    binding: Option<proc_macro2::TokenStream>,
    /// Optional runtime validation expression that returns `Err(...)`.
    validation: Option<proc_macro2::TokenStream>,
    /// Expression that constructs a `gradio::PredictionInput` from the (bound) ident.
    call_expr: proc_macro2::TokenStream,
    /// Rust field type used in the generated clap CLI struct.
    cli_type: proc_macro2::TokenStream,
    /// Extra clap `#[arg(...)]` attributes for the generated CLI field.
    cli_arg_attrs: Vec<proc_macro2::TokenStream>,
}

/// Map a Gradio API parameter to a [`ParamCodegen`] with all code fragments.
fn map_type(
    python_type: &str,
    is_file: bool,
    arg_ident: &Ident,
    has_default: bool,
    default_value: Option<&serde_json::Value>,
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
            binding: Some(quote! { let #arg_ident: std::path::PathBuf = #arg_ident.into(); }),
            validation: Some(file_exists_check),
            call_expr: quote! { gradio::PredictionInput::from_file(&#arg_ident) },
            cli_type: quote! { std::path::PathBuf },
            cli_arg_attrs: vec![],
        };
    }

    // ── Literal['a', 'b', ...] ────────────────────────────────────────────
    if let Some(variants) = parse_literal_variants(python_type) {
        let variant_strs: Vec<&str> = variants.iter().map(|s| s.as_str()).collect();
        let allowed_msg = variant_strs.join(", ");
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

        // Build clap attrs
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
        let cli_arg_combined = cli_arg_attrs;

        return ParamCodegen {
            rust_type: quote! { impl Into<String> },
            binding: Some(quote! { let #arg_ident: String = #arg_ident.into(); }),
            validation: Some(validation),
            call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident.clone()) },
            cli_type: quote! { String },
            cli_arg_attrs: cli_arg_combined,
        };
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
                binding: Some(quote! { let #arg_ident: String = #arg_ident.into(); }),
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident.clone()) },
                cli_type: quote! { String },
                cli_arg_attrs,
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
                binding: None,
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
                cli_type: quote! { f64 },
                cli_arg_attrs,
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
                binding: None,
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
                cli_type: quote! { i64 },
                cli_arg_attrs,
            }
        }
        "bool" => {
            let mut cli_arg_attrs = vec![];
            if has_default {
                if let Some(dv) = default_value {
                    let dv_str = dv.to_string(); // "true" / "false"
                    cli_arg_attrs.push(quote! { default_value = #dv_str });
                }
            }
            ParamCodegen {
                rust_type: quote! { bool },
                binding: None,
                validation: None,
                call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
                cli_type: quote! { bool },
                cli_arg_attrs,
            }
        }
        _ => ParamCodegen {
            rust_type: quote! { impl gradio::serde::Serialize },
            binding: None,
            validation: None,
            call_expr: quote! { gradio::PredictionInput::from_value(#arg_ident) },
            cli_type: quote! { String },
            cli_arg_attrs: vec![],
        },
    }
}

/// A comprehensive list of Rust keywords that cannot be used as plain identifiers.
/// These require raw-identifier (`r#...`) syntax when used as identifiers.
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
/// - Appends `_` (e.g. `type_`) when the result is a Rust keyword to avoid
///   parse errors without requiring raw-identifier syntax in generated code.
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
    // Disambiguate Rust keywords by appending `_`
    let final_name = if RUST_KEYWORDS.contains(&with_prefix.as_str()) {
        format!("{}_", with_prefix)
    } else {
        with_prefix
    };
    Ident::new(&final_name, Span::call_site())
}

/// Convert a name string into a valid Rust `UpperCamelCase` (PascalCase) identifier suitable
/// for use as an enum variant name.
/// - Prefixes with `Variant` when the result starts with a digit or is empty.
/// - Appends `_` when the result would be a Rust keyword.
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

/// A procedural macro for generating API client structs and methods for interacting with a Gradio-based API.
///
/// This macro generates a client struct for the specified Gradio API, along with methods to call the API endpoints
/// synchronously or asynchronously, depending on the provided option.
///
/// # Macro Parameters
///
/// - `url`: **Required**. The base URL of the Gradio API. This is the endpoint that the generated client will interact with.
/// - `option`: **Required**. Specifies whether the generated API methods should be synchronous or asynchronous.
///   - `"sync"`: Generates synchronous methods for interacting with the API.
///   - `"async"`: Generates asynchronous methods for interacting with the API.
/// - `hf_token` (optional): HuggingFace API token for private spaces.  If not specified the
///   generated `new()` method falls back to the `HF_TOKEN` environment variable at runtime.
/// - `auth_username` (optional): HuggingFace username (must be paired with `auth_password`).
/// - `auth_password` (optional): HuggingFace password (must be paired with `auth_username`).
/// - `cache` (optional): Set to `"refresh"` to bypass the local API cache and re-fetch from the server.
///
/// # Authentication
///
/// For private spaces you can provide a token either at compile time or at runtime:
///
/// ```rust
/// // Compile-time token (embedded in generated code – avoid committing real tokens!)
/// #[gradio_api(url = "my-org/private-space", option = "async", hf_token = "hf_...")]
/// pub struct Private;
///
/// // Runtime token via environment variable (recommended for production):
/// // export HF_TOKEN=hf_...
/// // Then just use the macro without hf_token – the generated new() picks it up.
/// ```
///
/// # Proxy Support
///
/// The HTTP client used at runtime is [`reqwest`](https://docs.rs/reqwest), which honours the
/// standard proxy environment variables: `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and
/// `NO_PROXY`. Set these before running your binary to route traffic through a proxy.
///
/// # API Caching
///
/// The macro caches the API spec as a JSON file under `.gradio_cache/` in your project root
/// (`CARGO_MANIFEST_DIR`). Subsequent compilations load the spec from the cache without a
/// network request. To force a refresh:
///
/// - Pass `cache = "refresh"` in the attribute: `#[gradio_api(url = "...", option = "async", cache = "refresh")]`
/// - Or set the environment variable `GRADIO_REFRESH_API_CACHE=1` before building.
///
/// You may commit the `.gradio_cache/` directory to version control for reproducible builds
/// without network access, or add it to `.gitignore` to always fetch fresh specs.
///
/// # Usage
///
/// The macro will generate the API struct and methods for you automatically, so you don't need to manually define the struct.
///
/// ```rust
/// use gradio_macro::gradio_api;
///
/// // Define the API client using the macro
/// #[gradio_api(url = "hf-audio/whisper-large-v3-turbo", option = "async")]
/// pub struct WhisperLarge;
///
/// #[tokio::main]
/// async fn main() {
///     println!("Whisper Large V3 turbo");
///
///     // Instantiate the API client
///     let whisper = WhisperLarge::new().await.unwrap();
///
///     // Call the API's predict method with input arguments
///     let mut result = whisper.predict("wavs/english.wav", "transcribe").await.unwrap();
///
///     // Handle the result
///     let result = result[0].clone().as_value().unwrap();
///     std::fs::write("result.txt", format!("{}", result)).expect("Can't write to file");
///     println!("result written to result.txt");
/// }
/// ```
///
/// This example shows how to define and use an asynchronous client with the `gradio_api` macro.
/// The API methods will be generated automatically, and you can call them using `.await` to handle asynchronous responses.
///
/// # Generated Methods
///
/// - For each API endpoint a blocking and a streaming method are generated.
/// - Parameter types are derived from the full Gradio API spec (e.g. `f64` for `float`, `i64` for `int`, `bool` for `bool`, concrete path types for files).
/// - Every generated method carries doc-comments with parameter names, types, descriptions and return-value information as reported by the Gradio server.
#[proc_macro_attribute]
pub fn gradio_api(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args with Punctuated::<Meta, syn::Token![,]>::parse_terminated);
    let input = parse_macro_input!(input as ItemStruct);
    let (mut url, mut option, mut grad_token, mut grad_login, mut grad_password, mut cache_refresh) =
        (None, None, None, None, None, false);

    // Parsing macro arguments
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
        } else if item.path().is_ident("cache") {
            cache_refresh = arg_value == "refresh";
        }
    }

    // Check if url is provided
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

    // Check if option is provided
    let Some(option) = option else {
        return make_compile_error("option is required");
    };

    // Fetch (or load from cache) the API spec
    let api = match get_api_info(&url, grad_opts, cache_refresh) {
        Ok(api) => api,
        Err(e) => return make_compile_error(&format!("Failed to fetch Gradio API for \"{}\": {}", url, e)),
    };
    let api = api.named_endpoints;

    // Generate the client options token-stream used at runtime.
    // If no `hf_token` was specified in the macro, fall back to the `HF_TOKEN`
    // environment variable at runtime so users don't need to hardcode tokens.
    let grad_auth_ts = if grad_auth.is_some() {
        quote! { Some((#grad_login.to_string(), #grad_password.to_string())) }
    } else {
        quote! { None }
    };
    let grad_token_ts = if let Some(ref val) = grad_token {
        // Compile-time token: embed directly.
        quote! { Some(#val.to_string()) }
    } else {
        // Runtime fallback: check HF_TOKEN env var.
        quote! { std::env::var("HF_TOKEN").ok() }
    };
    let grad_opts_ts = quote! {
        gradio::ClientOptions {
            auth: #grad_auth_ts,
            hf_token: #grad_token_ts,
        }
    };

    // Generate one method per named endpoint
    let mut functions: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, info) in api.iter() {
        let method_name = safe_ident(name, &format!("endpoint_{}", functions.len()));
        let background_name = Ident::new(
            &format!("{}_background", method_name),
            Span::call_site(),
        );

        // Build parameter list, bindings, validations, and call args
        let mut args_def: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut bindings: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut validations: Vec<proc_macro2::TokenStream> = Vec::new();
        let mut args_call: Vec<proc_macro2::TokenStream> = Vec::new();

        for (i, param) in info.parameters.iter().enumerate() {
            let ident = param
                .parameter_name
                .as_deref()
                .or(param.label.as_deref())
                .map(|n| safe_ident(n, &format!("arg{}", i)))
                .unwrap_or_else(|| Ident::new(&format!("arg{}", i), Span::call_site()));

            let is_file = param.python_type.r#type == "filepath";
            let codegen = map_type(
                &param.python_type.r#type,
                is_file,
                &ident,
                param.parameter_has_default.unwrap_or(false),
                param.parameter_default.as_ref(),
            );

            let rust_type = codegen.rust_type;
            args_def.push(quote! { #ident: #rust_type });
            if let Some(b) = codegen.binding { bindings.push(b); }
            if let Some(v) = codegen.validation { validations.push(v); }
            args_call.push(codegen.call_expr);
        }

        // Build doc-comment lines
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
        let bg_doc = format!("Submits the `{}` Gradio endpoint (`{}`) and returns a streaming handle.\nSee [`{}`] for parameter documentation.", name, method_name, method_name);

        // Create sync or async method pair
        let function = match option {
            Syncity::Sync => quote! {
                #(#doc_attrs)*
                pub fn #method_name(&self, #(#args_def),*) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    #(#bindings)*
                    #(#validations)*
                    self.client.predict_sync(#name, vec![#(#args_call),*])
                }

                #[doc = #bg_doc]
                pub fn #background_name(&self, #(#args_def),*) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    #(#bindings)*
                    #(#validations)*
                    self.client.submit_sync(#name, vec![#(#args_call),*])
                }
            },
            Syncity::Async => quote! {
                #(#doc_attrs)*
                pub async fn #method_name(&self, #(#args_def),*) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    #(#bindings)*
                    #(#validations)*
                    self.client.predict(#name, vec![#(#args_call),*]).await
                }

                #[doc = #bg_doc]
                pub async fn #background_name(&self, #(#args_def),*) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    #(#bindings)*
                    #(#validations)*
                    self.client.submit(#name, vec![#(#args_call),*]).await
                }
            },
        };

        functions.push(function);
    }

    // Build the final struct + impl block
    let vis = input.vis.clone();
    let struct_name = input.ident.clone();
    let api_struct = match option {
        Syncity::Sync => quote! {
            #vis struct #struct_name {
                client: gradio::Client,
            }

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
            #vis struct #struct_name {
                client: gradio::Client,
            }

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
/// - `hf_token`, `auth_username`, `auth_password`, `cache`: Same as [`gradio_api`].
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
    let (mut url, mut option, mut grad_token, mut grad_login, mut grad_password, mut cache_refresh) =
        (None, None, None, None, None, false);

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
        } else if item.path().is_ident("cache") {
            cache_refresh = arg_value == "refresh";
        }
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

    let api = match get_api_info(&url, grad_opts, cache_refresh) {
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
        // Runtime fallback: check HF_TOKEN env var.
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

    // Build the subcommand enum name: <StructName>Command
    let cmd_enum_name = Ident::new(
        &format!("{}Command", struct_name),
        Span::call_site(),
    );

    // Build one enum variant per named endpoint
    let mut variants: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut match_arms: Vec<proc_macro2::TokenStream> = Vec::new();

    for (ep_name, info) in api.named_endpoints.iter() {
        // e.g. "/predict" → "Predict", "/_design_fn" → "DesignFn"
        let variant_name = safe_variant_ident(
            &ep_name.trim_start_matches('/').replace('/', "_"),
            "Default",
        );

        // Build fields and match-arm locals for this endpoint
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
            let codegen = map_type(
                &param.python_type.r#type,
                is_file,
                &ident,
                param.parameter_has_default.unwrap_or(false),
                param.parameter_default.as_ref(),
            );

            let cli_type = codegen.cli_type;
            let cli_attrs = codegen.cli_arg_attrs;

            // Build doc for the field
            let label = param.label.as_deref().unwrap_or("");
            let py_type = &param.python_type.r#type;
            let field_doc = if label.is_empty() {
                format!("`{}` parameter ({})", ident, py_type)
            } else {
                format!("{} (`{}`)", label, py_type)
            };

            // For match arm, the field is accessed as a plain ident
            // We need to adapt cli_call_expr to use just `ident` rather than `self.ident`
            let match_call_expr = if is_file {
                quote! { gradio::PredictionInput::from_file(#ident) }
            } else {
                // Reuse call_expr pattern based on type
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

            // validation for match arm
            if let Some(v) = codegen.validation {
                match_locals.push(v);
            }
            call_inputs.push(match_call_expr);
            field_idents.push(ident);
        }

        // Build the doc comment for the variant
        let variant_doc = format!("Calls the `{}` Gradio endpoint.", ep_name);

        // Build the variant definition
        let variant_def = quote! {
            #[doc = #variant_doc]
            #variant_name {
                #(#field_defs)*
            },
        };
        variants.push(variant_def);

        // Build the match arm for the run() method
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
