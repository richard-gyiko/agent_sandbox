use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields};

/// Derive macro that generates `service_snapshot()` and `service_restore()`
/// method implementations for `TwinService`.
///
/// # Field Attributes
///
/// - `#[twin_snapshot(encode = "base64")]` — For `BTreeMap<K, Vec<u8>>` fields,
///   base64-encode values in the snapshot and decode on restore. Without this
///   attribute, fields are serialized/deserialized via serde_json as-is.
///
/// # Example
///
/// ```rust,ignore
/// #[derive(TwinSnapshot)]
/// pub struct MyTwinService {
///     items: BTreeMap<String, MyItem>,
///     #[twin_snapshot(encode = "base64")]
///     blobs: BTreeMap<String, Vec<u8>>,
///     counter: u64,
/// }
/// ```
///
/// Generates `service_snapshot()` that produces:
/// ```json
/// { "items": { ... }, "blobs": { "key": "<base64>" }, "counter": 42 }
/// ```
///
/// And `service_restore()` that reverses the process.
#[proc_macro_derive(TwinSnapshot, attributes(twin_snapshot))]
pub fn derive_twin_snapshot(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match impl_twin_snapshot(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn is_base64_field(field: &syn::Field) -> syn::Result<bool> {
    for attr in &field.attrs {
        if !attr.path().is_ident("twin_snapshot") {
            continue;
        }
        let mut found_base64 = false;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("encode") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                if lit.value() == "base64" {
                    found_base64 = true;
                } else {
                    return Err(meta.error(format!(
                        "unsupported encoding {:?}, only \"base64\" is supported",
                        lit.value()
                    )));
                }
                Ok(())
            } else {
                Err(meta
                    .error("unsupported twin_snapshot attribute, expected `encode = \"base64\"`"))
            }
        })?;
        if found_base64 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn impl_twin_snapshot(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    input,
                    "TwinSnapshot can only be derived for structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "TwinSnapshot can only be derived for structs",
            ));
        }
    };

    let mut snapshot_inserts = Vec::new();
    let mut restore_fields = Vec::new();

    for field in fields.iter() {
        let field_name = field.ident.as_ref().unwrap();
        let field_name_str = field_name.to_string();
        let base64 = is_base64_field(field)?;

        if base64 {
            // Snapshot: base64-encode each value in the BTreeMap
            snapshot_inserts.push(quote! {
                {
                    use ::base64::Engine as _;
                    let encoded: ::std::collections::BTreeMap<&str, String> = self
                        .#field_name
                        .iter()
                        .map(|(k, v)| (k.as_str(), ::base64::engine::general_purpose::STANDARD.encode(v)))
                        .collect();
                    map.insert(
                        #field_name_str.to_string(),
                        ::serde_json::to_value(encoded)
                            .expect(concat!("TwinSnapshot: failed to serialize field `", #field_name_str, "`")),
                    );
                }
            });

            // Restore: base64-decode each value
            restore_fields.push(quote! {
                let #field_name = if let Some(val) = snapshot.get(#field_name_str) {
                    use ::base64::Engine as _;
                    let encoded: ::std::collections::BTreeMap<String, String> =
                        ::serde_json::from_value(val.clone()).map_err(|e| {
                            ::twin_service::TwinError::Operation(format!(
                                "failed to deserialize field `{}`: {e}",
                                #field_name_str
                            ))
                        })?;
                    encoded
                        .into_iter()
                        .map(|(k, v)| {
                            let bytes = ::base64::engine::general_purpose::STANDARD
                                .decode(&v)
                                .map_err(|e| {
                                    ::twin_service::TwinError::Operation(format!(
                                        "failed to decode base64 for key {k} in field `{}`: {e}",
                                        #field_name_str
                                    ))
                                })?;
                            Ok((k, bytes))
                        })
                        .collect::<Result<::std::collections::BTreeMap<_, _>, ::twin_service::TwinError>>()?
                } else {
                    ::std::collections::BTreeMap::new()
                };
            });
        } else {
            // Snapshot: serialize via serde_json
            snapshot_inserts.push(quote! {
                map.insert(
                    #field_name_str.to_string(),
                    ::serde_json::to_value(&self.#field_name)
                        .expect(concat!("TwinSnapshot: failed to serialize field `", #field_name_str, "`")),
                );
            });

            // Restore: deserialize via serde_json
            restore_fields.push(quote! {
                let #field_name = ::serde_json::from_value(
                    snapshot
                        .get(#field_name_str)
                        .cloned()
                        .ok_or_else(|| {
                            ::twin_service::TwinError::Operation(format!(
                                "snapshot missing field `{}`",
                                #field_name_str
                            ))
                        })?,
                )
                .map_err(|e| {
                    ::twin_service::TwinError::Operation(format!(
                        "failed to deserialize field `{}`: {e}",
                        #field_name_str
                    ))
                })?;
            });
        }
    }

    let field_names: Vec<_> = fields.iter().map(|f| f.ident.as_ref().unwrap()).collect();

    Ok(quote! {
        impl #impl_generics #name #ty_generics #where_clause {
            /// Auto-generated by `#[derive(TwinSnapshot)]`.
            pub fn _twin_snapshot(&self) -> ::serde_json::Value {
                let mut map = ::serde_json::Map::new();
                #(#snapshot_inserts)*
                ::serde_json::Value::Object(map)
            }

            /// Auto-generated by `#[derive(TwinSnapshot)]`.
            pub fn _twin_restore(&mut self, snapshot: &::serde_json::Value) -> Result<(), ::twin_service::TwinError> {
                #(#restore_fields)*
                #(self.#field_names = #field_names;)*
                Ok(())
            }
        }
    })
}
