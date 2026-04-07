use gradio::ClientOptions;
use proc_macro::TokenStream;
use syn::{parse_macro_input, punctuated::Punctuated, Expr, ItemStruct, Meta};
use quote::quote;

use crate::cache::get_api_info;
use crate::codegen::{map_type, safe_ident, safe_variant_ident, Syncity};

/// Implementation of the `gradio_cli` proc-macro attribute.
///
/// Called by the thin `#[proc_macro_attribute]` entry point in `lib.rs`.
pub(crate) fn gradio_cli_impl(args: TokenStream, input: TokenStream) -> TokenStream {
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
    let mut grad_auth: Option<(String, String)> = None;
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

    let api = match get_api_info(&url, grad_opts) {
        Ok(api) => api,
        Err(e) => return crate::make_compile_error(&format!(
            "Failed to fetch Gradio API for \"{}\": {}", url, e
        )),
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

    let cmd_enum_name = proc_macro2::Ident::new(
        &format!("{}Command", struct_name),
        proc_macro2::Span::call_site(),
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
        let mut field_idents: Vec<proc_macro2::Ident> = Vec::new();

        for (i, param) in info.parameters.iter().enumerate() {
            let ident = param
                .parameter_name
                .as_deref()
                .or(param.label.as_deref())
                .map(|n| safe_ident(n, &format!("arg{}", i)))
                .unwrap_or_else(|| {
                    proc_macro2::Ident::new(&format!("arg{}", i), proc_macro2::Span::call_site())
                });

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
