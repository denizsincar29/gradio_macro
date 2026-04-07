use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::Ident;
use proc_macro2::Span;
use quote::quote;

#[derive(Clone, Copy)]
pub(crate) enum Syncity {
    Sync,
    Async,
}

/// Parse `Literal['a', 'b', 'c']` or `Literal["x", "y"]` Python type annotations into a list of
/// string variants. Returns `None` when the type string is not a Literal type.
pub(crate) fn parse_literal_variants(python_type: &str) -> Option<Vec<String>> {
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

/// Holds all code-generation fragments for a single API parameter.
pub(crate) struct ParamCodegen {
    /// The Rust type for the function parameter (may use `impl Trait`).
    pub rust_type: proc_macro2::TokenStream,
    /// Concrete Rust type for struct/builder fields.
    pub field_type: proc_macro2::TokenStream,
    /// Optional binding to convert from `rust_type` to `field_type`.
    pub binding: Option<proc_macro2::TokenStream>,
    /// Optional runtime validation expression (e.g. file-existence check).
    pub validation: Option<proc_macro2::TokenStream>,
    /// Expression that constructs a `gradio::PredictionInput` from the (bound) ident.
    pub call_expr: proc_macro2::TokenStream,
    /// Rust field type used in the generated clap CLI struct.
    pub cli_type: proc_macro2::TokenStream,
    /// Extra clap `#[arg(...)]` attributes for the generated CLI field.
    pub cli_arg_attrs: Vec<proc_macro2::TokenStream>,
    /// Optional enum type definition to emit (for Literal types in `gradio_api`).
    pub enum_def: Option<proc_macro2::TokenStream>,
}

/// Map a Gradio API parameter to a [`ParamCodegen`] with all code fragments.
pub(crate) fn map_type(
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
pub(crate) const RUST_KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const",
    "continue", "crate", "do", "dyn", "else", "enum", "extern", "false",
    "final", "fn", "for", "if", "impl", "in", "let", "loop", "macro",
    "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "try", "type", "typeof", "union", "unsafe", "unsized", "use", "virtual",
    "where", "while", "yield",
];

/// Strip non-ASCII characters from a snake_case string, collapsing multiple
/// consecutive underscores into one and trimming leading/trailing underscores.
/// ASCII uppercase letters are lowercased so they are preserved as valid identifier
/// characters even if `to_snake_case()` somehow leaves them in mixed case.
pub(crate) fn ascii_snake(s: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_underscore = false;
        } else if c == '_' || (!c.is_ascii() && c.is_alphabetic()) {
            // Underscores and non-ASCII alphabetic characters are treated as
            // word separators: emit at most one underscore, and never at the start.
            if !last_underscore && !out.is_empty() {
                out.push('_');
            }
            last_underscore = true;
        }
        // All other characters (ASCII punctuation, non-alphabetic symbols) are dropped
    }
    // Trim trailing underscore
    out.trim_end_matches('_').to_string()
}

/// Convert a name string into a valid Rust snake_case identifier.
/// - Prefixes with `arg_` when the result starts with a digit or is empty.
/// - Appends `_` when the result is a Rust keyword.
/// - Non-ASCII characters are stripped to keep identifiers ASCII-only.
pub(crate) fn safe_ident(name: &str, fallback: &str) -> Ident {
    let snake_cased = ascii_snake(&name.to_snake_case());
    let with_fallback = if snake_cased.is_empty() {
        ascii_snake(&fallback.to_snake_case())
    } else {
        snake_cased
    };
    // If still empty after ASCII filtering, use positional fallback directly
    // (positional fallbacks like "arg0", "output0" are already ASCII-safe).
    let with_fallback = if with_fallback.is_empty() {
        ascii_snake(fallback)
    } else {
        with_fallback
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
/// Non-ASCII characters are replaced with spaces before camel-casing so that heck only sees
/// known ASCII characters, keeping generated identifiers ASCII-only.
pub(crate) fn safe_variant_ident(name: &str, fallback: &str) -> Ident {
    // Replace non-ASCII characters with spaces so heck treats them as word separators.
    let ascii_name: String = name
        .chars()
        .map(|c| if c.is_ascii() { c } else { ' ' })
        .collect();
    let camel = ascii_name.trim().to_upper_camel_case();
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
pub(crate) fn make_default_expr(
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
pub(crate) fn build_doc_attrs(
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
pub(crate) fn build_setter_doc(param: &gradio::structs::ApiData, index: usize) -> String {
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
pub(crate) fn build_api_string(
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
