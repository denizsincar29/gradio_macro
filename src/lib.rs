use gradio::ClientOptions;
use heck::ToSnakeCase;
use proc_macro2::{Ident, Span};
use proc_macro::TokenStream;
use syn::{parse_macro_input, punctuated::Punctuated, Expr, ItemStruct, Meta};
use quote::quote;


enum Syncity {
    Sync,
    Async,
}

fn make_compile_error(message: &str) -> TokenStream {
    syn::Error::new(Span::call_site(), message).to_compile_error().into()
}

/// Returns the path to the cache file for the given URL.
/// The cache is stored in `.gradio_cache/` relative to `CARGO_MANIFEST_DIR`.
fn get_cache_path(url: &str) -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let safe_name: String = url
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    std::path::PathBuf::from(manifest_dir)
        .join(".gradio_cache")
        .join(format!("{}.json", safe_name))
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

/// Map a Gradio python type string to a concrete Rust parameter type and a
/// corresponding `PredictionInput` construction expression.
fn map_type(
    python_type: &str,
    is_file: bool,
    arg_ident: &Ident,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    if is_file {
        return (
            quote! { impl Into<std::path::PathBuf> },
            quote! { gradio::PredictionInput::from_file(#arg_ident) },
        );
    }
    match python_type {
        "str" => (
            quote! { impl Into<String> },
            quote! { gradio::PredictionInput::from_value(#arg_ident.into()) },
        ),
        "float" => (
            quote! { f64 },
            quote! { gradio::PredictionInput::from_value(#arg_ident) },
        ),
        "int" => (
            quote! { i64 },
            quote! { gradio::PredictionInput::from_value(#arg_ident) },
        ),
        "bool" => (
            quote! { bool },
            quote! { gradio::PredictionInput::from_value(#arg_ident) },
        ),
        _ => (
            quote! { impl gradio::serde::Serialize },
            quote! { gradio::PredictionInput::from_value(#arg_ident) },
        ),
    }
}

/// Convert a name string into a valid Rust snake_case identifier.
/// Prefixes with `arg_` when the result starts with a digit or is empty.
fn safe_ident(name: &str, fallback: &str) -> Ident {
    let snake_cased = name.to_snake_case();
    let with_fallback = if snake_cased.is_empty() {
        fallback.to_snake_case()
    } else {
        snake_cased
    };
    let final_name = if with_fallback
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        format!("arg_{}", with_fallback)
    } else {
        with_fallback
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
/// - `hf_token` (optional): HuggingFace space token.
/// - `auth_username` (optional): HuggingFace username.
/// - `auth_password` (optional): HuggingFace password.
/// - `cache` (optional): Set to `"refresh"` to bypass the local API cache and re-fetch from the server.
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
            option = Some(if arg_value == "sync" { Syncity::Sync } else { Syncity::Async });
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

    // Generate the client options token-stream used at runtime
    let grad_auth_ts = if grad_auth.is_some() {
        quote! { Some((#grad_login.to_string(), #grad_password.to_string())) }
    } else {
        quote! { None }
    };
    let grad_token_ts = if let Some(ref val) = grad_token {
        quote! { Some(#val.to_string()) }
    } else {
        quote! { None }
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

        // Build parameter list and call args
        let (args_def, args_call): (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>) =
            info.parameters
                .iter()
                .enumerate()
                .map(|(i, param)| {
                    let ident = param
                        .parameter_name
                        .as_deref()
                        .or(param.label.as_deref())
                        .map(|n| safe_ident(n, &format!("arg{}", i)))
                        .unwrap_or_else(|| Ident::new(&format!("arg{}", i), Span::call_site()));

                    let is_file = param.python_type.r#type == "filepath";
                    let (rust_type, call_expr) =
                        map_type(&param.python_type.r#type, is_file, &ident);

                    (quote! { #ident: #rust_type }, call_expr)
                })
                .unzip();

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
                    self.client.predict_sync(#name, vec![#(#args_call),*])
                }

                #[doc = #bg_doc]
                pub fn #background_name(&self, #(#args_def),*) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
                    self.client.submit_sync(#name, vec![#(#args_call),*])
                }
            },
            Syncity::Async => quote! {
                #(#doc_attrs)*
                pub async fn #method_name(&self, #(#args_def),*) -> Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> {
                    self.client.predict(#name, vec![#(#args_call),*]).await
                }

                #[doc = #bg_doc]
                pub async fn #background_name(&self, #(#args_def),*) -> Result<gradio::PredictionStream, gradio::anyhow::Error> {
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
