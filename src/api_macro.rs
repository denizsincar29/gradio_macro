use gradio::ClientOptions;
use heck::ToUpperCamelCase;
use proc_macro2::{Ident, Span};
use proc_macro::TokenStream;
use syn::{parse_macro_input, punctuated::Punctuated, Expr, ItemStruct, Meta};
use quote::quote;

use crate::cache::{encode_url_for_cache, get_api_info};
use crate::codegen::{
    ascii_snake, build_api_string, build_doc_attrs, build_setter_doc, make_default_expr, map_type,
    parse_literal_variants, safe_ident, Syncity,
};

/// Implementation of the `gradio_api` proc-macro attribute.
///
/// Called by the thin `#[proc_macro_attribute]` entry point in `lib.rs`.
pub(crate) fn gradio_api_impl(args: TokenStream, input: TokenStream) -> TokenStream {
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
                _ => return crate::make_compile_error(
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
        return crate::make_compile_error("url is required");
    };

    let mut grad_opts = ClientOptions::default();
    let mut grad_auth = None;
    if grad_token.is_some() {
        grad_opts.hf_token = grad_token.clone();
    }
    if grad_login.is_some() ^ grad_password.is_some() {
        return crate::make_compile_error("Both login and password must be present!");
    } else if grad_login.is_some() && grad_password.is_some() {
        grad_auth = Some((grad_login.clone().unwrap(), grad_password.clone().unwrap()));
        grad_opts.auth = grad_auth.clone();
    }

    let Some(option) = option else {
        return crate::make_compile_error("option is required");
    };

    let api_info = match get_api_info(&url, grad_opts) {
        Ok(api) => api,
        Err(e) => return crate::make_compile_error(&format!(
            "Failed to fetch Gradio API for \"{}\": {}", url, e
        )),
    };
    let api = api_info.named_endpoints;

    // Build the two static strings that `.endpoints()` and `.api()` will return.
    let endpoints_json = match serde_json::to_string(&api) {
        Ok(json) => json,
        Err(e) => return crate::make_compile_error(&format!(
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

        let call_methods = build_call_methods(option, name, &output_struct_ident, &extract_fields, &validations, &call_exprs, &bg_doc);

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
    let (custom_builder_struct, custom_builder_impl) = build_custom_endpoint_builder(
        &struct_name_str, option,
    );

    let custom_builder_ident = Ident::new(
        &format!("{}CustomEndpointBuilder", struct_name_str),
        Span::call_site(),
    );

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

    // ── check_cache() method ─────────────────────────────────────────────
    let check_cache_method = build_check_cache_method(&url, &endpoints_json, &struct_name_str);

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

                #check_cache_method

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

                #check_cache_method

                #(#functions)*
            }
        },
    };

    api_struct.into()
}

/// Build the call/call_background/call_cli methods for a builder.
fn build_call_methods(
    option: Syncity,
    name: &str,
    output_struct_ident: &Ident,
    extract_fields: &[proc_macro2::TokenStream],
    validations: &[proc_macro2::TokenStream],
    call_exprs: &[&proc_macro2::TokenStream],
    bg_doc: &str,
) -> proc_macro2::TokenStream {
    match option {
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
                                let __raw: Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> = output.try_into();
                                if !success {
                                    return Err(__raw.err().unwrap_or_else(|| gradio::anyhow::anyhow!("prediction failed")));
                                }
                                return std::convert::TryFrom::try_from(__raw?);
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
    }
}

/// Build the custom endpoint builder struct and impl for the given syncity.
fn build_custom_endpoint_builder(
    struct_name_str: &str,
    option: Syncity,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let custom_builder_ident = Ident::new(
        &format!("{}CustomEndpointBuilder", struct_name_str),
        Span::call_site(),
    );

    let builder_struct = quote! {
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

    let builder_impl = match option {
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
                                    let __raw: Result<Vec<gradio::PredictionOutput>, gradio::anyhow::Error> = output.try_into();
                                    if !success {
                                        return Err(__raw.err().unwrap_or_else(|| gradio::anyhow::anyhow!("prediction failed")));
                                    }
                                    return __raw;
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

    (builder_struct, builder_impl)
}

/// Generate the `check_cache()` method and its debug-only implementation helper.
///
/// In debug builds (`#[cfg(debug_assertions)]`):
/// - Fetches the current API spec from the network using `self.client.view_api()`.
/// - Compares it with the compile-time embedded spec.
/// - Prints a human-readable diff (added/removed/changed endpoints and parameters).
/// - Asks the user whether to write the diff to `gradio_spec_diff.txt`.
/// - Updates the local cache file (`.gradio_cache/<url>.json`) with the fresh spec.
/// - If differences were found and the user chose to write to file: exits the process.
/// - If differences were found and the user chose to continue: returns `false`.
/// - If specs match: returns `true`.
///
/// In release builds: always returns `true` immediately with zero overhead.
pub(crate) fn build_check_cache_method(
    url: &str,
    endpoints_json: &str,
    struct_name_str: &str,
) -> proc_macro2::TokenStream {
    let encoded_url = encode_url_for_cache(url);

    let check_impl_fn_name = Ident::new(
        &format!("__gradio_check_cache_impl_{}", ascii_snake(struct_name_str)),
        Span::call_site(),
    );
    let diff_fn_name = Ident::new(
        &format!("__gradio_diff_endpoints_{}", ascii_snake(struct_name_str)),
        Span::call_site(),
    );

    quote! {
        /// Compare the compile-time embedded API spec against the live upstream spec.
        ///
        /// **Debug builds only** (`#[cfg(debug_assertions)]`):
        /// - Fetches the current spec from the Gradio space via `client.view_api()`.
        /// - Prints any differences (added/removed/changed endpoints and parameters).
        /// - Asks whether to write the diff to `gradio_spec_diff.txt`.
        ///   - If **y**: writes the diff file, updates cache, and exits the process so you can
        ///     recompile with the new spec.
        ///   - If **n**: updates cache on disk and returns `false` so the caller can decide what
        ///     to do (e.g. log a warning and continue).
        /// - Returns `true` when the spec is unchanged.
        ///
        /// **Release builds**: always returns `true` immediately (zero overhead).
        #[cfg(debug_assertions)]
        pub fn check_cache(&self) -> bool {
            Self::#check_impl_fn_name(&self.client)
        }

        /// No-op in release builds — always returns `true`.
        #[cfg(not(debug_assertions))]
        pub fn check_cache(&self) -> bool {
            true
        }

        #[cfg(debug_assertions)]
        fn #check_impl_fn_name(client: &gradio::Client) -> bool {
            const __EMBEDDED_JSON: &str = #endpoints_json;
            const __URL: &str = #url;
            const __ENCODED_URL: &str = #encoded_url;

            let embedded: serde_json::Value = serde_json::from_str(__EMBEDDED_JSON)
                .unwrap_or(serde_json::Value::Null);

            let fresh_api = client.view_api();
            let fresh: serde_json::Value = serde_json::to_value(&fresh_api.named_endpoints)
                .unwrap_or(serde_json::Value::Null);

            if embedded == fresh {
                println!("[gradio_macro] check_cache: API spec for '{}' is up to date.", __URL);
                return true;
            }

            let diffs = Self::#diff_fn_name(&embedded, &fresh);
            println!("[gradio_macro] check_cache: API spec for '{}' has changed!", __URL);
            println!("Differences:");
            for diff in &diffs {
                println!("  {}", diff);
            }

            // Save the fresh spec to the local cache file so the next
            // `cargo build --features gradio_macro/update_cache` picks it up.
            {
                // env!("CARGO_MANIFEST_DIR") is expanded at compile time of the consuming
                // crate, giving the crate root that the proc-macro also uses for its cache
                // lookup.  This is more reliable than std::env::var() which is typically
                // unset at runtime.
                let cache_dir =
                    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".gradio_cache");
                let _ = std::fs::create_dir_all(&cache_dir);
                let cache_path = cache_dir.join(format!("{}.json", __ENCODED_URL));
                let timestamp_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Serialize the full ApiInfo (not just named_endpoints) so that
                // load_api_from_cache() can deserialize it back correctly.
                if let Ok(api_value) = serde_json::to_value(&fresh_api) {
                    let envelope = serde_json::json!({
                        "timestamp_secs": timestamp_secs,
                        "api": api_value,
                    });
                    if let Ok(content) = serde_json::to_string_pretty(&envelope) {
                        let _ = std::fs::write(&cache_path, content);
                    }
                }
            }

            // Ask whether to persist the diff to a text file.
            print!("Write differences to 'gradio_spec_diff.txt'? [y/n]: ");
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let mut input = String::new();
            let _ = std::io::stdin().read_line(&mut input);

            if input.trim().eq_ignore_ascii_case("y") {
                let diff_text = diffs.join("\n");
                match std::fs::write("gradio_spec_diff.txt", &diff_text) {
                    Ok(_) => println!("Differences written to 'gradio_spec_diff.txt'."),
                    Err(e) => eprintln!("Failed to write diff file: {}", e),
                }
                println!(
                    "[gradio_macro] Cache updated. Recompile with \
                     `cargo build --features gradio_macro/update_cache` to pick up the new spec."
                );
                std::process::exit(0);
            }

            println!("[gradio_macro] Cache updated on disk. Proceeding with execution.");
            false
        }

        /// Compute a human-readable diff between two endpoint spec JSON objects.
        #[cfg(debug_assertions)]
        fn #diff_fn_name(
            old_spec: &serde_json::Value,
            new_spec: &serde_json::Value,
        ) -> Vec<String> {
            let mut diffs: Vec<String> = Vec::new();
            let empty_map = serde_json::Map::new();
            let old_map = old_spec.as_object().unwrap_or(&empty_map);
            let new_map = new_spec.as_object().unwrap_or(&empty_map);

            // Removed endpoints
            let mut removed: Vec<&String> = old_map
                .keys()
                .filter(|k| !new_map.contains_key(*k))
                .collect();
            removed.sort();
            for key in removed {
                diffs.push(format!("[REMOVED] endpoint: {}", key));
            }

            // Added endpoints
            let mut added: Vec<&String> = new_map
                .keys()
                .filter(|k| !old_map.contains_key(*k))
                .collect();
            added.sort();
            for key in added {
                diffs.push(format!("[ADDED] endpoint: {}", key));
            }

            // Changed endpoints (present in both but with different spec)
            let mut shared_keys: Vec<&String> = old_map
                .keys()
                .filter(|k| new_map.contains_key(*k))
                .collect();
            shared_keys.sort();

            for key in shared_keys {
                let old_ep = &old_map[key];
                let new_ep = &new_map[key];
                if old_ep == new_ep {
                    continue;
                }

                diffs.push(format!("[CHANGED] endpoint: {}", key));

                // Compare parameters
                let old_params = old_ep
                    .get("parameters")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let new_params = new_ep
                    .get("parameters")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                let param_name = |p: &serde_json::Value| -> String {
                    p.get("parameter_name")
                        .or_else(|| p.get("label"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string()
                };
                let old_param_names: Vec<String> = old_params.iter().map(&param_name).collect();
                let new_param_names: Vec<String> = new_params.iter().map(&param_name).collect();

                for name in &old_param_names {
                    if !new_param_names.contains(name) {
                        diffs.push(format!("    [REMOVED] parameter: {}", name));
                    }
                }
                for name in &new_param_names {
                    if !old_param_names.contains(name) {
                        diffs.push(format!("    [ADDED] parameter: {}", name));
                    }
                }

                // Type changes for parameters present in both (by position)
                for (old_p, new_p) in old_params.iter().zip(new_params.iter()) {
                    let p_name = param_name(old_p);
                    let old_type = old_p
                        .get("type")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let new_type = new_p
                        .get("type")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    if old_type != new_type {
                        diffs.push(format!(
                            "    [TYPE CHANGE] parameter '{}': {} -> {}",
                            p_name, old_type, new_type
                        ));
                    }
                }

                // Compare returns
                let old_returns = old_ep
                    .get("returns")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let new_returns = new_ep
                    .get("returns")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                if old_returns.len() != new_returns.len() {
                    diffs.push(format!(
                        "    [CHANGED] return count: {} -> {}",
                        old_returns.len(),
                        new_returns.len()
                    ));
                }
                for (old_r, new_r) in old_returns.iter().zip(new_returns.iter()) {
                    let r_name = old_r
                        .get("label")
                        .and_then(|v| v.as_str())
                        .unwrap_or("output");
                    let old_type = old_r
                        .get("type")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let new_type = new_r
                        .get("type")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    if old_type != new_type {
                        diffs.push(format!(
                            "    [TYPE CHANGE] return '{}': {} -> {}",
                            r_name, old_type, new_type
                        ));
                    }
                }
            }

            diffs
        }
    }
}
