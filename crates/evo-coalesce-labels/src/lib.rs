//! `#[derive(CoalesceLabels)]` proc-macro for the evo framework's
//! per-subscriber happenings-coalescing surface.
//!
//! The framework's `Happening` enum derives this trait on every
//! variant. Each struct field becomes a label whose name is the
//! field name and whose value is the field's `Display`
//! representation. A subscriber declaring a coalesce label list on
//! `subscribe_happenings` collapses same-label events within a
//! window; the labels come from this trait's runtime extraction.
//!
//! ## Field-type contract
//!
//! Every label-bearing field is converted via `field.to_string()`,
//! which requires `core::fmt::Display`. Fields whose types do NOT
//! implement `Display` MUST be annotated with
//! `#[coalesce_labels(skip)]` or the derive will fail to compile.
//! `Display` (rather than a `Debug` fallback) is the deliberate
//! engineering trade-off: it forces contributors to commit to a
//! stable wire-form representation per type rather than letting
//! `Debug`'s formatting (`"foo"` for a `String`) silently break
//! label matching.
//!
//! The framework's authoritative reference for the variant-author
//! contract — including the list of types currently annotated
//! `skip`, the rationale for choosing `Display` over `Debug`, and
//! the rules for adding new fields — lives in
//! `docs/engineering/HAPPENINGS.md` section 3.4. A future variant
//! author MUST consult that section before adding a field whose
//! type is not in the documented `skip` set.
//!
//! ## Field attributes
//!
//! - `#[coalesce_labels(skip)]` — exclude this field from the label
//!   set. Apply to fields whose type does not implement `Display`,
//!   or whose value is unhelpful for coalescing (large opaque
//!   payloads, timestamps, vectors, error message strings).
//! - `#[coalesce_labels(flatten)]` — for `serde_json::Value` fields,
//!   treat top-level object keys as labels (with their values
//!   stringified). Used by `Happening::PluginEvent` to expose
//!   plugin-defined payload fields as coalesce-eligible labels.
//!   The static-labels enumeration does NOT include flattened
//!   keys; they are runtime-determined and plugin-specific.
//!
//! ## Generated impl
//!
//! For every variant of an enum (or every named struct field of a
//! struct), the macro generates:
//!
//! ```ignore
//! impl ::evo_plugin_sdk::happenings::CoalesceLabels for MyType {
//!     fn labels(&self) -> ::std::collections::BTreeMap<&'static str, ::std::string::String> {
//!         let mut out = ::std::collections::BTreeMap::new();
//!         match self {
//!             MyType::Variant { field_a, field_b, .. } => {
//!                 out.insert("variant", "variant".to_string());
//!                 out.insert("field_a", field_a.to_string());
//!                 out.insert("field_b", field_b.to_string());
//!             }
//!             // ...
//!         }
//!         out
//!     }
//!
//!     fn static_labels(kind: &str) -> &'static [&'static str] {
//!         match kind {
//!             "variant" => &["variant", "field_a", "field_b"],
//!             // ...
//!             _ => &[],
//!         }
//!     }
//! }
//! ```
//!
//! The `static_labels` associated function returns the canonical
//! label set per variant kind without requiring an instance.
//! `describe_capabilities` uses this to advertise the framework's
//! coalesce labels at runtime.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, Attribute, Data, DataEnum, DeriveInput, Fields, Variant,
};

/// Derive `CoalesceLabels` on an enum or named-struct.
///
/// See the crate-level documentation for the generated impl shape
/// and the field-attribute vocabulary.
#[proc_macro_derive(CoalesceLabels, attributes(coalesce_labels))]
pub fn derive_coalesce_labels(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match &input.data {
        Data::Enum(data) => derive_for_enum(&input, data).into(),
        Data::Struct(_) => syn::Error::new_spanned(
            &input,
            "CoalesceLabels derive on structs is not yet supported; \
             apply it to enums (the framework's Happening variant \
             surface). Future ADR may extend the macro.",
        )
        .to_compile_error()
        .into(),
        Data::Union(_) => syn::Error::new_spanned(
            &input,
            "CoalesceLabels derive on unions is not supported.",
        )
        .to_compile_error()
        .into(),
    }
}

fn derive_for_enum(input: &DeriveInput, data: &DataEnum) -> TokenStream2 {
    let ty = &input.ident;
    let (impl_generics, ty_generics, where_clause) =
        input.generics.split_for_impl();

    let mut variant_arms = Vec::new();
    let mut static_arms = Vec::new();

    for variant in &data.variants {
        let kind_str = variant_kind_str(&variant.ident);
        let var_ident = &variant.ident;

        let (extractor, static_labels, bound_fields) =
            variant_label_extractor(&kind_str, variant);

        // Build the match arm for the runtime extractor. Only the
        // fields the extractor actually uses are bound; everything
        // else is absorbed by the `..` rest pattern. This keeps the
        // generated code free of unused-variable warnings.
        let pattern_fields = variant_pattern_fields(variant, &bound_fields);
        variant_arms.push(quote! {
            #ty::#var_ident #pattern_fields => {
                #extractor
            }
        });

        // Build the static-labels match arm.
        static_arms.push(quote! {
            #kind_str => &[#(#static_labels),*],
        });
    }

    quote! {
        impl #impl_generics ::evo_plugin_sdk::happenings::CoalesceLabels
            for #ty #ty_generics #where_clause
        {
            fn labels(&self) -> ::std::collections::BTreeMap<
                &'static str,
                ::std::string::String,
            > {
                let mut out: ::std::collections::BTreeMap<
                    &'static str,
                    ::std::string::String,
                > = ::std::collections::BTreeMap::new();
                match self {
                    #(#variant_arms)*
                }
                out
            }

            fn static_labels(kind: &str) -> &'static [&'static str] {
                match kind {
                    #(#static_arms)*
                    _ => &[],
                }
            }
        }
    }
}

/// Translate an enum variant ident `MyVariant` into the snake_case
/// kind string `my_variant` the wire form uses.
fn variant_kind_str(ident: &syn::Ident) -> String {
    let s = ident.to_string();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push('_');
        }
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

/// Build the runtime extractor body for a variant, plus its static
/// label set and the list of field names the extractor actually
/// references (so the caller can build a minimal pattern that
/// avoids unused-binding warnings).
fn variant_label_extractor(
    kind_str: &str,
    variant: &Variant,
) -> (TokenStream2, Vec<String>, Vec<syn::Ident>) {
    let mut body = TokenStream2::new();
    let mut static_labels = vec!["variant".to_string()];
    let mut bound_fields: Vec<syn::Ident> = Vec::new();

    // Always insert the variant label.
    body.extend(quote! {
        out.insert("variant", #kind_str.to_string());
    });

    if let Fields::Named(named) = &variant.fields {
        for field in &named.named {
            let attr = parse_field_attr(&field.attrs);
            if attr.skip {
                continue;
            }
            let name = field.ident.as_ref().expect("named field");
            let name_str = name.to_string();
            bound_fields.push(name.clone());
            if attr.flatten {
                // Treat the field as a serde_json::Value and flatten
                // top-level object entries into labels. The runtime
                // labels are plugin-specific; the static-labels list
                // does not enumerate them.
                body.extend(quote! {
                    if let ::serde_json::Value::Object(map) = #name {
                        for (k, v) in map {
                            // BTreeMap key is &'static str, but
                            // payload keys are runtime strings —
                            // leak the &'static lifetime via Box::leak
                            // to bridge. The cost is amortised across
                            // process lifetime; payload key sets are
                            // plugin-stable in practice.
                            let leaked: &'static str =
                                ::std::boxed::Box::leak(
                                    k.clone().into_boxed_str(),
                                );
                            let value_str = match v {
                                ::serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            out.insert(leaked, value_str);
                        }
                    }
                });
                // Note: static_labels does not enumerate flattened
                // runtime keys — they are not knowable at compile
                // time.
            } else {
                body.extend(quote! {
                    out.insert(#name_str, #name.to_string());
                });
                static_labels.push(name_str);
            }
        }
    }

    (body, static_labels, bound_fields)
}

/// Build the variant's `match` pattern. Binds only the fields
/// listed in `bound_fields` (those the extractor body references)
/// and absorbs the rest with `..`. Unit variants get no
/// pattern fields. Tuple variants are unsupported and
/// compile-error.
fn variant_pattern_fields(
    variant: &Variant,
    bound_fields: &[syn::Ident],
) -> TokenStream2 {
    match &variant.fields {
        Fields::Named(_) => {
            if bound_fields.is_empty() {
                quote! { { .. } }
            } else {
                quote! { { #(#bound_fields),*, .. } }
            }
        }
        Fields::Unit => quote! {},
        Fields::Unnamed(_) => {
            quote! {
                compile_error!(
                    "CoalesceLabels derive does not support tuple \
                     variants; use named-field variants instead."
                )
            }
        }
    }
}

#[derive(Default)]
struct FieldAttr {
    skip: bool,
    flatten: bool,
}

fn parse_field_attr(attrs: &[Attribute]) -> FieldAttr {
    let mut out = FieldAttr::default();
    for attr in attrs {
        if !attr.path().is_ident("coalesce_labels") {
            continue;
        }
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                out.skip = true;
            } else if meta.path.is_ident("flatten") {
                out.flatten = true;
            }
            Ok(())
        });
    }
    out
}
