//! `#[derive(RecordDecode)]` — generated `TryFrom<&GrpcRecord>` / `FromGrpcValue`
//! for model structs, decoding an ArcadeDB `GrpcRecord` (or an embedded/map
//! `GrpcValue`) directly into a typed Rust DTO — no intermediate JSON.
//!
//! `#[derive(RecordEncode)]` — the write-side mirror: generates
//! `ToGrpcRecord`, mapping each field's Rust type onto the matching
//! `GrpcValue` wire kind. Same `rename`/`rename_all` naming; field
//! attributes below apply with encode-side semantics (`skip` spellings,
//! `Option` omission, nested maps — see [`RecordEncode`]).
//!
//! ## Field attributes
//!
//! Both derives understand serde's `rename`, `rename_all`, `default`,
//! `default = "path"`, and `flatten`, plus `#[record(...)]` spellings:
//!
//! - `#[serde(rename = "logical_key")]` / `#[record(rename = "...")]` — wire property name.
//! - `#[serde(default)]` / `#[record(default)]` — missing property → `Default::default()`.
//! - `#[serde(default = "path")]` / `#[record(default = "path")]` — custom default fn.
//! - `#[record(with = "path")]` / `#[record(deserialize_with = "path")]` — custom
//!   conversion `fn(&GrpcValue) -> Result<T, RecordDecodeError>` for types
//!   the macro cannot map itself.
//! - `#[record(skip)]` — `Default::default()`, property never read.
//! - `#[serde(flatten)]` — decode the *remaining* properties into a nested
//!   `FromGrpcValue` struct.
//! - `#[record(with = "path")]` + `#[serde(default)]` — run the custom fn when
//!   the property is present, fall back to the default when absent.
//!
//! Custom types used across many DTOs can instead implement
//! `FromGrpcValue` directly — then no per-field attribute is needed at all.
//!
//! ## Type support
//!
//! Any field type implementing `FromGrpcValue` works: `String`, `bool`,
//! integers (`i8`–`i64`, `u8`–`u64`, range-checked), `f32`/`f64`,
//! `Option<T>`, `Vec<T>` (list values), `HashMap<String, T>` (maps),
//! `chrono::DateTime<Utc>` (timestamps), `serde_json::Value` (any kind),
//! `Vec<u8>` (byte blobs), `Link` (`#bucket:pos` rids), single-field tuple
//! structs (transparent newtypes), and nested structs deriving
//! `RecordDecode`. Unit-variant enums decode from strings honoring
//! `#[serde(rename)]` + `#[serde(rename_all)]`; internally-tagged
//! struct-variant enums (`#[serde(tag = "...")]`) decode from embedded/map
//! values via the discriminator key.
//!
//! ## Naming parity with serde
//!
//! `rename_all` uses serde's exact case dialect: fields are assumed
//! snake_case, variants PascalCase, acronyms are NOT regrouped
//! (`HTTPServer` + `snake_case` → `h_t_t_p_server`). Un-renamed names are
//! the ident as-is; unknown rule names are compile errors.
//!
//! `@rid` / `@type` synthetic metadata: a field whose wire name is `@rid` or
//! `@type` reads the property if the query returned it, else falls back to
//! `GrpcRecord.rid` / `GrpcRecord.type`.
//!
//! ## Naming the runtime crate
//!
//! The generated code references `::arcadedb_protocol` by default; redirect
//! with `#[record(crate_path = "crate::db::arcadedb")]` when the runtime
//! is re-exported elsewhere.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{
    parse_macro_input, Data, DataEnum, DeriveInput, Fields, GenericArgument, Ident, PathArguments,
    Type,
};

mod sql;

/// `sql!` — write the statement as a literal, get a syntax check for free.
///
/// ```ignore
/// use arcadedb_protocol::{params, sql};
///
/// client.execute_with_values(
///     sql!("UPDATE users SET status = :status UPSERT WHERE user_id = :user_id"),
///     params! { status: "active", user_id },
/// ).await?;
/// ```
///
/// Catches at compile time: UPDATE clause order, unbalanced delimiters,
/// reserved-word placeholders (`:after`), pasted scripts, `DELETE` without
/// `WHERE`, `INSERT` without `INTO`, `CREATE EDGE` without `FROM`/`TO`.
/// Expands to the `&str` unchanged. Bound params are `params!`'s job, not
/// this macro's; non-literal statements stay plain strings.
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    sql::expand_sql(input)
}

/// `#[derive(RecordDecode)]`.
// `serde` is registered here too (registration is additive across derives —
// serde's own Deserialize/Serialize both register it), so decode-only DTOs
// without a serde derive can still spell `#[serde(rename = "...")]`.
#[proc_macro_derive(RecordDecode, attributes(record, serde))]
pub fn derive_record_decode(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let output = match expand_derive(&input) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error(),
    };
    output.into()
}

/// `#[derive(RecordEncode)]` — the encode-side mirror of [`RecordDecode`]:
/// generates `ToGrpcRecord` (see the module docs for the attribute reference).
///
/// The `serde` attribute namespace is registered here too — unlike
/// [`RecordDecode`], these DTOs are write-only and typically have no serde
/// derive to register it.
#[proc_macro_derive(RecordEncode, attributes(record, serde))]
pub fn derive_record_encode(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let output = match expand_encode_derive(&input) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error(),
    };
    output.into()
}

// ---------------------------------------------------------------------------
// Expand
// ---------------------------------------------------------------------------

fn expand_derive(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    // Generic *lifetime* parameters are allowed (borrowed decode: a
    // `Row<'a>` holding `&'a str` / `Cow<'a, str>` / `&'a [u8]` fields
    // borrows from the record being decoded). Type/const generics remain
    // unsupported — the wire mapping is fully concrete per type.
    let mut lifetimes: Vec<&syn::LifetimeParam> = input
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            syn::GenericParam::Lifetime(lt) => Some(lt),
            _ => None,
        })
        .collect();
    if lifetimes.len() > 1 {
        return Err(syn::Error::new(
            input.generics.span(),
            "RecordDecode supports at most one lifetime parameter",
        ));
    }

    let borrowed = lifetimes.pop();
    let borrow = match borrowed {
        Some(lt) => {
            let life = &lt.lifetime;
            BorrowCfg {
                clause: quote! { <#life> },
                inherent_clause: quote! { <#life> },
                self_ty: quote! { #name<#life> },
                helper_clause: quote! {},
                lt: life.clone(),
                owned_try_from: false,
            }
        }
        None => BorrowCfg {
            clause: quote! { <'de> },
            inherent_clause: quote! {},
            self_ty: quote! { #name },
            helper_clause: quote! { <'de> },
            lt: syn::Lifetime::new("'de", proc_macro2::Span::call_site()),
            owned_try_from: true,
        },
    };
    if let Some(p) = input
        .generics
        .params
        .iter()
        .find(|p| !matches!(p, syn::GenericParam::Lifetime(_)))
    {
        return Err(syn::Error::new(
            p.span(),
            "RecordDecode does not support type/const generic parameters",
        ));
    }

    let container = ContainerCfg::parse(input)?;

    match &input.data {
        Data::Struct(data) => expand_struct(name, data, &container, &borrow),
        Data::Enum(data) => expand_enum(name, data, &container, &borrow),
        Data::Union(data) => Err(syn::Error::new(
            data.union_token.span(),
            "RecordDecode does not support unions",
        )),
    }
}

/// Lifetime scaffolding shared by every generated impl.
struct BorrowCfg {
    /// `impl<'de>` (owning DTOs) or `impl<'a>` (borrowed DTOs).
    clause: TokenStream2,
    /// The inherent helper's `impl` clause: `impl Name` (owning — the helper
    /// declares its own `'de`) resp. `impl<'a> Name<'a>` (borrowed — the helper
    /// uses the block's `'a`).
    inherent_clause: TokenStream2,
    /// `#name` resp. `#name<'a>` — the impl self type.
    self_ty: TokenStream2,
    /// Extra generic clause on the inherent decode helper when the DTO owns
    /// its data: `fn __arcade_decode_from_map<'de>(...)`.
    helper_clause: TokenStream2,
    /// The lifetime ident (`'de`/`'a`) used on the trait and record types.
    lt: syn::Lifetime,
    /// Emit `TryFrom<GrpcRecord>` (owned) too — only possible when the DTO
    /// owns its data (a borrowing struct can't outlive the owned record).
    owned_try_from: bool,
}

fn expand_struct(
    name: &Ident,
    data: &syn::DataStruct,
    container: &ContainerCfg,
    borrow: &BorrowCfg,
) -> syn::Result<TokenStream2> {
    let Fields::Named(fields) = &data.fields else {
        return expand_tuple_struct(name, data, container, borrow);
    };

    let crate_path = &container.crate_path;

    // Parse + validate each field.
    let mut field_lets = Vec::new();
    let mut flat_fields: Vec<(Ident, Type, Option<Type>)> = Vec::new();
    let mut non_flat_keys: Vec<String> = Vec::new();
    let mut wire_kinds: Vec<(String, String)> = Vec::new();

    for field in &fields.named {
        let fident = field.ident.as_ref().expect("named field ident").clone();
        let cfg = FieldCfg::parse(field)?;
        let ty = &field.ty;

        // Wire name: explicit rename > container rule (serde's exact
        // `apply_to_field`) > the ident as-is (serde never transforms
        // un-renamed names).
        let wire = match (&cfg.rename, container.rename_all) {
            (Some(r), _) => r.clone(),
            (None, Some(rule)) => rule.apply_to_field(&fident.to_string()),
            (None, None) => fident.to_string(),
        };

        if cfg.skip_decode {
            field_lets.push(quote! {
                let #fident: #ty = ::core::default::Default::default();
            });
            continue;
        }

        // Expected ArcadeDB column type for drift checks — fields with a
        // known mapping only (custom `with` conversions and dynamic values
        // are excluded; flatten targets describe their own kinds).
        if cfg.with.is_none() {
            if let Some(db_type) = db_type_of(ty) {
                wire_kinds.push((wire.clone(), db_type));
            }
        }

        // `#[serde(flatten)]` / `#[record(flatten)]` — decode remaining props.
        if cfg.flatten {
            if !flat_fields.is_empty() {
                return Err(syn::Error::new(
                    field.span(),
                    "RecordDecode supports at most one flattened field per struct",
                ));
            }
            if cfg.with.is_some() {
                return Err(syn::Error::new(
                    field.span(),
                    "RecordDecode: `with` cannot be combined with a flattened field",
                ));
            }
            let opt_inner = option_inner(ty);
            flat_fields.push((fident, ty.clone(), opt_inner));
            continue;
        }

        non_flat_keys.push(wire.clone());

        // Synthetic `@rid` / `@type`: read the real property when present,
        // otherwise fall back to the record's own `rid`/`type` — lazily, via
        // the decode helper's synth args, instead of cloning the entire
        // property map (the old approach). Synth fallbacks support owned
        // targets only (a borrowed target like `Cow<'de, str>` couldn't
        // outlive the fallback value).
        if wire == "@rid" || wire == "@type" {
            let syn = if wire == "@rid" {
                quote! { __arcade_syn_rid }
            } else {
                quote! { __arcade_syn_type }
            };
            if cfg.with.is_some() {
                return Err(syn::Error::new(
                    field.span(),
                    "RecordDecode: `with` cannot be combined with `@rid`/`@type` synthesis",
                ));
            }
            field_lets.push(synth_field_let(
                &fident,
                ty,
                &wire,
                &cfg.default,
                &syn,
                crate_path,
            )?);
            continue;
        }

        // `#[record(with = "...")]` — custom conversion fn. Combined with
        // `#[serde(default)]` the property is optional, falling back to the
        // default when absent (e.g. custom containers).
        if let Some(with_path) = &cfg.with {
            let call = if let Some(inner) = option_inner(ty) {
                quote! {
                    #[allow(clippy::redundant_closure_call)]
                    let #fident: #ty = #crate_path::record::get_opt_with::<#inner>(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                        #with_path,
                    )?;
                }
            } else if cfg.default.is_some() {
                let fallback = default_fallback(&cfg.default);
                quote! {
                    #[allow(clippy::redundant_closure_call)]
                    let #fident: #ty = #crate_path::record::get_with_or::<#ty>(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                        #with_path,
                        #fallback,
                    )?;
                }
            } else {
                quote! {
                    #[allow(clippy::redundant_closure_call)]
                    let #fident: #ty = #crate_path::record::get_with::<#ty>(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                        #with_path,
                    )?;
                }
            };
            field_lets.push(call);
            continue;
        }

        // `Option<T>` — missing property or explicit null → `None`.
        if let Some(inner) = option_inner(ty) {
            let call = if vec_u8(&inner) {
                // `Option<Vec<u8>>` — a nullable byte blob.
                quote! {
                    let #fident: #ty = #crate_path::record::get_opt_bytes(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                    )?;
                }
            } else {
                quote! {
                    let #fident: #ty = #crate_path::record::get_opt(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                    )?;
                }
            };
            field_lets.push(call);
            continue;
        }

        // `Vec<u8>` fields are byte blobs (wire `bytes` kind, or a list of
        // small ints) — not generic int lists.
        if vec_u8(ty) {
            let call = match &cfg.default {
                None => quote! {
                    let #fident: #ty = #crate_path::record::get_bytes(__arcade_props, __arcade_taken, #wire)?;
                },
                Some(None) => quote! {
                    let #fident: #ty = #crate_path::record::get_bytes_or(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                        ::core::default::Default::default,
                    )?;
                },
                Some(Some(path)) => quote! {
                    let #fident: #ty = #crate_path::record::get_bytes_or(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                        #path,
                    )?;
                },
            };
            field_lets.push(call);
            continue;
        }

        // Missing-property policy: serde `default` → default value, else error.
        match &cfg.default {
            None => {
                field_lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                    )?;
                });
            }
            Some(None) => {
                field_lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get_or(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                        ::core::default::Default::default,
                    )?;
                });
            }
            Some(Some(path)) => {
                field_lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get_or(
                        __arcade_props,
                        __arcade_taken,
                        #wire,
                        #path,
                    )?;
                });
            }
        }
    }

    let wire_kind_pairs: Vec<TokenStream2> = wire_kinds
        .iter()
        .map(|(w, k)| quote! { (#w, #k) })
        .collect();

    // Flattened fields consume everything not taken by named fields —
    // ZERO-COPY: no rest map is materialized. The child decodes straight
    // from the record's own map via `DecodeFromMap`, with the parent's
    // consumed keys hidden (`taken`), so borrowed (`Cow<'de>`) flatten
    // targets work and no entries are cloned.
    for (fident, ty, opt_inner) in &flat_fields {
        // The child must not see the parent's keys; a nested flatten child
        // must not see this level's keys either — extend the incoming list.
        let own_taken = &non_flat_keys;
        let own_len = own_taken.len();
        let taken_stmt = quote! {
            let mut __arcade_t: ::std::vec::Vec<&str> =
                ::std::vec::Vec::with_capacity(__arcade_taken.len() + #own_len);
            __arcade_t.extend(__arcade_taken.iter().copied());
            __arcade_t.extend([#(#own_taken),*]);
        };
        if let Some(inner) = opt_inner {
            field_lets.push(quote! {
                let #fident: #ty = {
                    #taken_stmt
                    // Visible-to-the-child check uses the CHILD's taken set —
                    // the parent's incoming list alone would let an
                    // all-consumed map through to a doomed decode.
                    if __arcade_props
                        .keys()
                        .any(|__arcade_k| !__arcade_t.contains(&__arcade_k.as_str()))
                    {
                        ::core::option::Option::Some(
                            <#inner as #crate_path::record::DecodeFromMap>::from_map(
                                __arcade_props,
                                &__arcade_t,
                                ::core::option::Option::None,
                                ::core::option::Option::None,
                            )
                            .map_err(|e| e.with_field(stringify!(#fident)))?
                        )
                    } else {
                        ::core::option::Option::None
                    }
                };
            });
        } else {
            field_lets.push(quote! {
                #taken_stmt
                let #fident: #ty = <#ty as #crate_path::record::DecodeFromMap>::from_map(
                    __arcade_props,
                    &__arcade_t,
                    ::core::option::Option::None,
                    ::core::option::Option::None,
                )
                .map_err(|e| e.with_field(stringify!(#fident)))?;
            });
        }
    }

    let assigns = fields.named.iter().map(|f| {
        let ident = f.ident.as_ref().expect("named field ident");
        quote! { #ident }
    });

    // The record path borrows the property map directly — `@rid`/`@type`
    // fields read a fallback from the `rid`/`type` args lazily, so no whole
    // property-map clone is needed when synthesizing metadata.
    let synth = quote! {
        let __arcade_props = &rec.properties;
    };

    let BorrowCfg {
        clause,
        inherent_clause,
        self_ty,
        helper_clause,
        lt,
        owned_try_from,
    } = borrow;
    // Record-level `TryFrom<GrpcRecord>` only makes sense when the DTO owns
    // its data: a borrowing struct cannot outlive the owned record it would
    // borrow from. The record-level path (repo rows) is owned anyway.
    let owned_try_from = owned_try_from.then(|| {
        quote! {
            #[automatically_derived]
            impl ::core::convert::TryFrom<#crate_path::GrpcRecord> for #self_ty {
                type Error = #crate_path::record::RecordDecodeError;

                fn try_from(
                    rec: #crate_path::GrpcRecord,
                ) -> ::std::result::Result<Self, Self::Error> {
                    Self::try_from(&rec)
                }
            }
        }
    });

    Ok(quote! {
        #[automatically_derived]
        // Field idents are re-used as local bindings in the decode helper;
        // unconventional DTO names (e.g. acronym fields) must not warn from
        // generated code.
        #[allow(non_snake_case)]
        impl #inherent_clause #self_ty {
            #[doc(hidden)]
            #[allow(clippy::too_many_arguments)]
            fn __arcade_decode_from_map #helper_clause (
                __arcade_props: & #lt ::std::collections::HashMap<
                    ::std::string::String,
                    #crate_path::GrpcValue,
                >,
                __arcade_taken: &[&str],
                __arcade_syn_rid: ::core::option::Option<&::std::string::String>,
                __arcade_syn_type: ::core::option::Option<&::std::string::String>,
            ) -> ::std::result::Result<Self, #crate_path::record::RecordDecodeError> {
                #(#field_lets)*
                ::std::result::Result::Ok(Self { #(#assigns),* })
            }

            /// Wire names of every readable column, in declaration order —
            /// the canonical SELECT list for this DTO. Skipped fields are
            /// not read; flattened fields consume "the rest" (their own
            /// `WIRES` describe their columns).
            pub const WIRES: &'static [&'static str] = &[#(#non_flat_keys),*];

            /// [`Self::WIRES`] as an inline SQL SELECT list (`"a, b, c"`),
            /// computed once per DTO — callers invoke this per query, and
            /// the join need not repeat. The leaked `String` lives for the
            /// process lifetime, exactly once per type.
            pub fn select_list() -> &'static str {
                static LIST: ::std::sync::OnceLock<&'static str> = ::std::sync::OnceLock::new();
                LIST.get_or_init(|| Self::WIRES.join(", ").leak())
            }

            /// Expected ArcadeDB column types, for schema-drift checks
            /// (`arcadedb-migrate`'s `assert_dto_kinds`): `(wire name,
            /// canonical type)` for every field with a known mapping.
            /// Custom `with` fields, flatten targets, and dynamic values are
            /// absent — they cannot be statically typed against a column.
            pub const WIRE_KINDS: &'static [(&'static str, &'static str)] =
                &[#(#wire_kind_pairs),*];
        }

        #[automatically_derived]
        impl #clause #crate_path::record::DecodeFromMap<#lt> for #self_ty {
            fn from_map(
                __arcade_props: & #lt ::std::collections::HashMap<
                    ::std::string::String,
                    #crate_path::GrpcValue,
                >,
                __arcade_taken: &[&str],
                __arcade_syn_rid: ::core::option::Option<&::std::string::String>,
                __arcade_syn_type: ::core::option::Option<&::std::string::String>,
            ) -> ::std::result::Result<Self, #crate_path::record::RecordDecodeError> {
                Self::__arcade_decode_from_map(
                    __arcade_props,
                    __arcade_taken,
                    __arcade_syn_rid,
                    __arcade_syn_type,
                )
            }
        }

        #[automatically_derived]
        impl #clause #crate_path::record::FromGrpcValue<#lt> for #self_ty {
            fn from_grpc_value(
                __arcade_v: & #lt #crate_path::GrpcValue,
            ) -> ::std::result::Result<Self, #crate_path::record::RecordDecodeError> {
                match &__arcade_v.kind {
                    ::core::option::Option::Some(#crate_path::Kind::EmbeddedValue(__arcade_e)) => {
                        Self::__arcade_decode_from_map(
                            &__arcade_e.fields,
                            &[],
                            ::core::option::Option::None,
                            ::core::option::Option::None,
                        )
                    }
                    ::core::option::Option::Some(#crate_path::Kind::MapValue(__arcade_m)) => {
                        Self::__arcade_decode_from_map(
                            &__arcade_m.entries,
                            &[],
                            ::core::option::Option::None,
                            ::core::option::Option::None,
                        )
                    }
                    _ => ::std::result::Result::Err(
                        #crate_path::record::RecordDecodeError::type_mismatch(
                            "embedded record",
                            #crate_path::record::kind_name(&__arcade_v.kind),
                        ),
                    ),
                }
            }
        }

        #[automatically_derived]
        impl #clause ::core::convert::TryFrom<&#lt #crate_path::GrpcRecord>
            for #self_ty
        {
            type Error = #crate_path::record::RecordDecodeError;

            fn try_from(rec: &#lt #crate_path::GrpcRecord) -> ::std::result::Result<Self, Self::Error> {
                #synth
                Self::__arcade_decode_from_map(
                    __arcade_props,
                    &[],
                    (!rec.rid.is_empty()).then_some(&rec.rid),
                    (!rec.r#type.is_empty()).then_some(&rec.r#type),
                )
            }
        }

        #owned_try_from
    })
}

// ---------------------------------------------------------------------------
// Tuple structs
// ---------------------------------------------------------------------------

/// Single-field tuple structs (`#[serde(transparent)]` newtypes, like an id
/// wrapper) delegate to their inner type: a field of `Wrapper(Inner)` decodes
/// exactly like a field of `Inner`. A record-level `TryFrom` is intentionally
/// NOT generated — a newtype has no property map of its own, so a missing
/// impl fails at compile time.
fn expand_tuple_struct(
    name: &Ident,
    data: &syn::DataStruct,
    container: &ContainerCfg,
    borrow: &BorrowCfg,
) -> syn::Result<TokenStream2> {
    let crate_path = &container.crate_path;
    match &data.fields {
        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            let inner = &fields.unnamed[0].ty;
            let BorrowCfg {
                clause,
                self_ty,
                lt,
                ..
            } = borrow;

            Ok(quote! {
                #[automatically_derived]
                impl #clause #crate_path::record::FromGrpcValue<#lt>
                    for #self_ty
                {
                    fn from_grpc_value(
                        __arcade_v: & #lt #crate_path::GrpcValue,
                    ) -> ::std::result::Result<Self, #crate_path::record::RecordDecodeError> {
                        let __arcade_inner =
                            <#inner as #crate_path::record::FromGrpcValue<#lt>>::from_grpc_value(
                                __arcade_v,
                            )?;
                        ::std::result::Result::Ok(#name(__arcade_inner))
                    }
                }
            })
        }
        // Unit structs and multi-field tuple structs map to no property shape —
        // no impls (a use site then fails to compile).
        _ => Ok(quote! {}),
    }
}

// ---------------------------------------------------------------------------
// Enum expansion (unit variants only; string kind on the wire)
// ---------------------------------------------------------------------------

fn expand_enum(
    name: &Ident,
    data: &DataEnum,
    container: &ContainerCfg,
    borrow: &BorrowCfg,
) -> syn::Result<TokenStream2> {
    let crate_path = &container.crate_path;

    // `#[serde(tag = "...")]` — internally-tagged: the wire is a map whose
    // discriminator key selects the variant, then the remaining keys decode as
    // the variant's struct fields.
    if let Some(tag) = &container.tag {
        return expand_internally_tagged_enum(name, data, container, tag, borrow);
    }

    let mut variants = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new(
                variant.span(),
                "RecordDecode on enums supports unit variants only; \
                 for data-carrying variants implement `FromGrpcValue` manually \
                 (or decode with `#[record(with = \"...\")]`)",
            ));
        }
        let vident = &variant.ident;
        let wire = variant_wire_name(variant, container)?;
        variants.push(quote! { #wire => ::core::result::Result::Ok(Self::#vident) });
    }

    let BorrowCfg {
        clause,
        self_ty,
        lt,
        ..
    } = borrow;
    Ok(quote! {
        #[automatically_derived]
        impl #clause #crate_path::record::FromGrpcValue<#lt> for #self_ty {
            fn from_grpc_value(
                __arcade_v: & #lt #crate_path::GrpcValue,
            ) -> ::std::result::Result<Self, #crate_path::record::RecordDecodeError> {
                let __arcade_s = match &__arcade_v.kind {
                    ::core::option::Option::Some(#crate_path::Kind::StringValue(__arcade_s)) => {
                        __arcade_s.as_str()
                    }
                    _ => {
                        return ::std::result::Result::Err(
                            #crate_path::record::RecordDecodeError::type_mismatch(
                                "string",
                                #crate_path::record::kind_name(&__arcade_v.kind),
                            ),
                        )
                    }
                };
                match __arcade_s {
                    #(#variants,)*
                    __other => ::std::result::Result::Err(
                        #crate_path::record::RecordDecodeError::unknown_variant(__other),
                    ),
                }
            }
        }
    })
}

/// Internally-tagged enums (`#[serde(tag = "...")]`): the wire value is an
/// embedded/map record carrying a discriminator key, followed by the selected
/// variant's fields.
///
/// Emits BOTH decode routes: `FromGrpcValue` (embedded/map values) and
/// `DecodeFromMap` (the zero-copy flatten path — reads the record's own map
/// with the parent's taken keys hidden).
fn expand_internally_tagged_enum(
    _name: &Ident,
    data: &DataEnum,
    container: &ContainerCfg,
    tag: &str,
    borrow: &BorrowCfg,
) -> syn::Result<TokenStream2> {
    let crate_path = &container.crate_path;
    let props = syn::Ident::new("__arcade_map", proc_macro2::Span::call_site());

    let mut arms = Vec::new();
    for variant in &data.variants {
        let vident = &variant.ident;
        let wire = variant_wire_name(variant, container)?;
        match &variant.fields {
            Fields::Unit => {
                arms.push(quote! { #wire => ::std::result::Result::Ok(Self::#vident) });
            }
            Fields::Named(fields) => {
                let lets = variant_field_lets(fields, container, &props)?;
                let assigns = fields
                    .named
                    .iter()
                    .map(|f| f.ident.as_ref().expect("named field ident"));
                arms.push(quote! {
                    #wire => {
                        #(#lets)*
                        ::std::result::Result::Ok(Self::#vident { #(#assigns),* })
                    }
                });
            }
            Fields::Unnamed(_) => {
                return Err(syn::Error::new(
                    variant.span(),
                    "RecordDecode on internally-tagged enums supports struct variants only",
                ));
            }
        }
    }

    let BorrowCfg {
        clause,
        self_ty,
        lt,
        ..
    } = borrow;

    // The discriminator walk: shared by the value path (from_grpc_value)
    // and the map path (DecodeFromMap — the zero-copy flatten route).
    let walk = quote! {
        let __arcade_tag = match #crate_path::record::visible(__arcade_map, #tag, __arcade_taken) {
            ::core::option::Option::Some(__arcade_t) => __arcade_t,
            ::core::option::Option::None => {
                return ::std::result::Result::Err(
                    #crate_path::record::RecordDecodeError::missing_field(#tag),
                )
            }
        };
        let __arcade_wire = match &__arcade_tag.kind {
            ::core::option::Option::Some(#crate_path::Kind::StringValue(__arcade_s)) => {
                __arcade_s.as_str()
            }
            _ => {
                return ::std::result::Result::Err(
                    #crate_path::record::RecordDecodeError::type_mismatch(
                        "string",
                        #crate_path::record::kind_name(&__arcade_tag.kind),
                    ),
                )
            }
        };
        match __arcade_wire {
            #(#arms,)*
            __other => ::std::result::Result::Err(
                #crate_path::record::RecordDecodeError::unknown_variant(__other),
            ),
        }
    };

    Ok(quote! {
        #[automatically_derived]
        impl #clause #crate_path::record::FromGrpcValue<#lt> for #self_ty {
            fn from_grpc_value(
                __arcade_v: & #lt #crate_path::GrpcValue,
            ) -> ::std::result::Result<Self, #crate_path::record::RecordDecodeError> {
                // Via the value path no keys are ever hidden from the enum.
                let __arcade_taken: &[&str] = &[];
                let __arcade_map = match &__arcade_v.kind {
                    ::core::option::Option::Some(#crate_path::Kind::EmbeddedValue(__arcade_e)) => {
                        &__arcade_e.fields
                    }
                    ::core::option::Option::Some(#crate_path::Kind::MapValue(__arcade_m)) => {
                        &__arcade_m.entries
                    }
                    _ => {
                        return ::std::result::Result::Err(
                            #crate_path::record::RecordDecodeError::type_mismatch(
                                "record/map",
                                #crate_path::record::kind_name(&__arcade_v.kind),
                            ),
                        )
                    }
                };
                #walk
            }
        }

        #[automatically_derived]
        impl #clause #crate_path::record::DecodeFromMap<#lt> for #self_ty {
            fn from_map(
                __arcade_props: & #lt ::std::collections::HashMap<
                    ::std::string::String,
                    #crate_path::GrpcValue,
                >,
                __arcade_taken: &[&str],
                _syn_rid: ::core::option::Option<&::std::string::String>,
                _syn_type: ::core::option::Option<&::std::string::String>,
            ) -> ::std::result::Result<Self, #crate_path::record::RecordDecodeError> {
                let __arcade_map = __arcade_props;
                #walk
            }
        }
    })
}

/// Per-field decoders for a struct variant. Same policy as struct fields
/// (rename/`Option`/`default`/`with`/`skip`) minus flatten, which is not
/// supported inside variants.
fn variant_field_lets(
    fields: &syn::FieldsNamed,
    container: &ContainerCfg,
    props: &syn::Ident,
) -> syn::Result<Vec<TokenStream2>> {
    let crate_path = &container.crate_path;

    let mut lets = Vec::new();
    for field in &fields.named {
        let fident = field.ident.as_ref().expect("named field ident").clone();
        let cfg = FieldCfg::parse(field)?;
        let ty = &field.ty;

        if cfg.flatten {
            return Err(syn::Error::new(
                field.span(),
                "RecordDecode: `flatten` is not supported inside enum variants",
            ));
        }
        if cfg.skip_decode {
            lets.push(quote! {
                let #fident: #ty = ::core::default::Default::default();
            });
            continue;
        }

        let wire = match (&cfg.rename, container.rename_all_fields) {
            (Some(r), _) => r.clone(),
            (None, Some(rule)) => rule.apply_to_field(&fident.to_string()),
            (None, None) => fident.to_string(),
        };

        // `#[record(with = "...")]` — honor `Option<T>` and `#[serde(default)]`.
        if let Some(with_path) = &cfg.with {
            if let Some(inner) = option_inner(ty) {
                lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get_opt_with::<#inner>(
                        #props,
                        __arcade_taken,
                        #wire,
                        #with_path,
                    )?;
                });
            } else if cfg.default.is_some() {
                let fallback = default_fallback(&cfg.default);
                lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get_with_or::<#ty>(
                        #props,
                        __arcade_taken,
                        #wire,
                        #with_path,
                        #fallback,
                    )?;
                });
            } else {
                lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get_with::<#ty>(
                        #props,
                        __arcade_taken,
                        #wire,
                        #with_path,
                    )?;
                });
            }
            continue;
        }

        if option_inner(ty).is_some() {
            lets.push(quote! {
                let #fident: #ty = #crate_path::record::get_opt(
                    #props,
                    __arcade_taken,
                    #wire,
                )?;
            });
            continue;
        }

        match &cfg.default {
            None => {
                lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get(#props, __arcade_taken, #wire)?;
                });
            }
            Some(None) => {
                lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get_or(
                        #props,
                        __arcade_taken,
                        #wire,
                        ::core::default::Default::default,
                    )?;
                });
            }
            Some(Some(path)) => {
                lets.push(quote! {
                    let #fident: #ty = #crate_path::record::get_or(
                        #props,
                        __arcade_taken,
                        #wire,
                        #path,
                    )?;
                });
            }
        }
    }
    Ok(lets)
}

/// Fallback expression for a parsed `#[serde(default)]` (the `Some(_)` side
/// only — callers guard on the default being present).
fn default_fallback(default: &Option<Option<syn::Path>>) -> TokenStream2 {
    match default {
        Some(None) => quote! { ::core::default::Default::default },
        Some(Some(path)) => quote! { #path },
        // Caller guarantees this branch only fires when a default is declared.
        None => quote! { ::core::default::Default::default },
    }
}

/// Wire form of an enum variant: explicit `#[serde(rename)]` wins, else the
/// container rule (serde's exact `apply_to_variant`), else the variant ident
/// as-is.
fn variant_wire_name(variant: &syn::Variant, container: &ContainerCfg) -> syn::Result<String> {
    for attr in &variant.attrs {
        if attr.path().is_ident("serde") {
            let mut rename = None;
            let mut serialize_rename = None;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    parse_rename(&meta, &mut rename, &mut serialize_rename)?;
                } else {
                    ignore_meta(&meta)?;
                }
                Ok(())
            })?;
            if let Some(r) = rename {
                return Ok(r);
            }
        }
    }
    if let Some(rule) = container.rename_all {
        return Ok(rule.apply_to_variant(&variant.ident.to_string()));
    }
    Ok(variant.ident.to_string())
}

// ---------------------------------------------------------------------------
// Attribute parsing
// ---------------------------------------------------------------------------

/// Container-level `#[record(...)]` / `#[serde(...)]` config.
struct ContainerCfg {
    crate_path: syn::Path,
    rename_all: Option<RenameRule>,
    /// `#[serde(rename_all = "...")]` applied to *variant fields* (serde's
    /// `rename_all_fields`) — names variant fields, unlike `rename_all`,
    /// which names variants.
    rename_all_fields: Option<RenameRule>,
    /// `#[serde(tag = "...")]` — the enum is internally tagged; the wire is a
    /// map carrying a discriminator key, followed by the variant's fields.
    tag: Option<String>,
}

impl ContainerCfg {
    fn parse(input: &DeriveInput) -> syn::Result<Self> {
        let mut crate_path: Option<syn::Path> = None;
        let mut rename_all: Option<RenameRule> = None;
        let mut rename_all_fields: Option<RenameRule> = None;
        let mut tag: Option<String> = None;

        for attr in &input.attrs {
            if attr.path().is_ident("serde") {
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("rename_all") || meta.path.is_ident("rename_all_fields") {
                        let value = meta.value()?;
                        let s: syn::LitStr = value.parse()?;
                        let rule = RenameRule::parse(&s.value()).ok_or_else(|| {
                            meta.error(format!(
                                "unknown rename_all rule `{}`; expected one of `lowercase`, \
                                 `UPPERCASE`, `PascalCase`, `camelCase`, `snake_case`, \
                                 `SCREAMING_SNAKE_CASE`, `kebab-case`, `SCREAMING-KEBAB-CASE`",
                                s.value(),
                            ))
                        })?;
                        if meta.path.is_ident("rename_all") {
                            rename_all = Some(rule);
                        } else {
                            rename_all_fields = Some(rule);
                        }
                        Ok(())
                    } else if meta.path.is_ident("tag") {
                        let value = meta.value()?;
                        let s: syn::LitStr = value.parse()?;
                        tag = Some(s.value());
                        Ok(())
                    } else {
                        // Foreign serde meta (`untagged`, `deny_unknown_fields`, ...).
                        ignore_meta(&meta)
                    }
                })?;
            } else if attr.path().is_ident("record") {
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("crate_path") {
                        let value = meta.value()?;
                        let s: syn::LitStr = value.parse()?;
                        crate_path = Some(s.parse()?);
                        Ok(())
                    } else {
                        Err(meta.error("unknown `record` container attribute"))
                    }
                })?;
            }
        }

        Ok(Self {
            crate_path: crate_path.unwrap_or_else(|| syn::parse_quote!(::arcadedb_protocol)),
            rename_all,
            rename_all_fields,
            tag,
        })
    }
}

/// Field-level `#[record(...)]` + relevant `#[serde(...)]` config.
struct FieldCfg {
    /// Wire property name (the `deserialize` side of a two-sided `rename`).
    rename: Option<String>,
    /// The `serialize` side of a two-sided `rename(serialize = "...",
    /// deserialize = "...")`; the `=` form sets both. `RecordEncode` prefers
    /// this and falls back to `rename` when only one side is declared.
    serialize_rename: Option<String>,
    /// `None` = no default attr; `Some(None)` = bare `default`;
    /// `Some(Some(path))` = `default = "path"`.
    default: Option<Option<syn::Path>>,
    /// Custom conversion: decode `fn(&GrpcValue) -> Result<T, RecordDecodeError>`,
    /// encode `fn(&T) -> GrpcValue`. Direction depends on which derive reads it.
    with: Option<syn::Path>,
    /// Skip the field when DECODING (serde's `skip` and `skip_deserializing`).
    skip_decode: bool,
    /// Skip the field when ENCODING (serde's `skip` and `skip_serializing`).
    skip_encode: bool,
    /// `#[serde(skip_serializing_if = "path")]` — omit the property at encode
    /// time when the predicate returns true.
    skip_serializing_if: Option<syn::Path>,
    /// `#[record(key)]` — the field is part of the bulk-conflict identity
    /// (`ToGrpcRecord::KEY_COLUMNS`). Record-namespace only: serde has no
    /// `key` spelling, and the tolerance policy must not swallow it.
    key: bool,
    flatten: bool,
}

impl FieldCfg {
    fn parse(field: &syn::Field) -> syn::Result<Self> {
        let mut cfg = FieldCfg {
            rename: None,
            serialize_rename: None,
            default: None,
            with: None,
            skip_decode: false,
            skip_encode: false,
            skip_serializing_if: None,
            key: false,
            flatten: false,
        };

        for attr in &field.attrs {
            if attr.path().is_ident("serde") {
                attr.parse_nested_meta(|meta| Self::parse_meta(&mut cfg, &meta, false))?;
            } else if attr.path().is_ident("record") {
                attr.parse_nested_meta(|meta| Self::parse_meta(&mut cfg, &meta, true))?;
            }
        }
        Ok(cfg)
    }

    /// One dispatcher for both namespaces. Shared keys (`rename`, `default`,
    /// `flatten`) behave identically; `with`/`skip`/`crate_path` are
    /// `record`-only. The unknown-key policy differs by design: the `serde`
    /// namespace *tolerates* foreign serde meta we don't model
    /// (`skip_serializing_if`, `borrow`, ...), while the `record` namespace
    /// *rejects* unknown keys — typos of our own attributes must not pass
    /// silently.
    fn parse_meta(
        cfg: &mut FieldCfg,
        meta: &syn::meta::ParseNestedMeta<'_>,
        record_ns: bool,
    ) -> syn::Result<()> {
        if meta.path.is_ident("rename") {
            parse_rename(meta, &mut cfg.rename, &mut cfg.serialize_rename)
        } else if meta.path.is_ident("default") {
            parse_default(meta, &mut cfg.default)
        } else if meta.path.is_ident("flatten") {
            cfg.flatten = true;
            Ok(())
        } else if meta.path.is_ident("skip") {
            // serde's bare `skip` is bidirectional; the directional spellings
            // (`skip_serializing`, `skip_deserializing`) must stay directional.
            cfg.skip_decode = true;
            cfg.skip_encode = true;
            Ok(())
        } else if meta.path.is_ident("skip_serializing") {
            cfg.skip_encode = true;
            Ok(())
        } else if meta.path.is_ident("skip_deserializing") {
            cfg.skip_decode = true;
            Ok(())
        } else if meta.path.is_ident("skip_serializing_if") {
            let value = meta.value()?;
            let s: syn::LitStr = value.parse()?;
            cfg.skip_serializing_if = Some(s.parse()?);
            Ok(())
        } else if record_ns
            && (meta.path.is_ident("with") || meta.path.is_ident("deserialize_with"))
        {
            let value = meta.value()?;
            let s: syn::LitStr = value.parse()?;
            cfg.with = Some(s.parse()?);
            Ok(())
        } else if record_ns && meta.path.is_ident("key") {
            // Bulk-conflict identity (RecordEncode): the field's wire name
            // lands in `ToGrpcRecord::KEY_COLUMNS`.
            cfg.key = true;
            Ok(())
        } else if record_ns && meta.path.is_ident("crate_path") {
            Err(meta.error("`crate_path` is a container-level attribute"))
        } else if record_ns {
            Err(meta.error("unknown `record` attribute"))
        } else {
            // Foreign serde meta (`borrow`, `untagged`, ...) — tolerate rather
            // than reject.
            ignore_meta(meta)
        }
    }
}

/// `rename = "..."` plus the two-sided `rename(serialize = "...",
/// deserialize = "...")` form (decode reads the deserialize side, encode the
/// serialize side). Shared by field and variant attribute parsing.
fn parse_rename(
    meta: &syn::meta::ParseNestedMeta<'_>,
    rename: &mut Option<String>,
    serialize_rename: &mut Option<String>,
) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        let value = meta.value()?;
        let s: syn::LitStr = value.parse()?;
        let name = s.value();
        *rename = Some(name.clone());
        *serialize_rename = Some(name);
    } else {
        meta.parse_nested_meta(|inner| {
            if inner.path.is_ident("deserialize") {
                let value = inner.value()?;
                let s: syn::LitStr = value.parse()?;
                *rename = Some(s.value());
            } else if inner.path.is_ident("serialize") {
                let value = inner.value()?;
                let s: syn::LitStr = value.parse()?;
                *serialize_rename = Some(s.value());
            }
            ignore_meta(&inner)
        })?;
    }
    Ok(())
}

/// `default` (bare) / `default = "path"`.
fn parse_default(
    meta: &syn::meta::ParseNestedMeta<'_>,
    default: &mut Option<Option<syn::Path>>,
) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        let value = meta.value()?;
        let s: syn::LitStr = value.parse()?;
        *default = Some(Some(s.parse()?));
    } else {
        *default = Some(None);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Consume and discard an unknown nested meta so the surrounding
/// `parse_nested_meta` doesn't fail on its trailing tokens. Handles bare
/// `name`, `name = value`, and `name(...)` forms; used to tolerate foreign
/// `serde` attributes (`skip_serializing_if`, `borrow`, `untagged`, ...).
fn ignore_meta(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        let _: syn::Expr = meta.value()?.parse()?;
    } else if meta.input.peek(syn::token::Paren) {
        let content;
        syn::parenthesized!(content in meta.input);
        let _: syn::punctuated::Punctuated<syn::Meta, syn::Token![,]> =
            content.parse_terminated(syn::parse::Parse::parse, syn::Token![,])?;
    }
    Ok(())
}

/// If `ty` is `Option<inner>` (any `Option` spelling), return `inner`.
fn option_inner(ty: &Type) -> Option<Type> {
    let Type::Path(tp) = ty else { return None };
    let seg = tp.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.last() {
        Some(GenericArgument::Type(inner)) => Some(inner.clone()),
        _ => None,
    }
}

/// If `ty` is `Vec<u8>` (any `Vec` spelling with a single `u8` argument),
/// true — such fields are byte blobs (wire `bytes`), not int lists.
fn vec_u8(ty: &Type) -> bool {
    let Type::Path(tp) = ty else { return false };
    let Some(seg) = tp.path.segments.last() else {
        return false;
    };
    if seg.ident != "Vec" {
        return false;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return false;
    };
    matches!(
        args.args.last(),
        Some(GenericArgument::Type(Type::Path(t))) if t.path.is_ident("u8")
    )
}

/// Does `ty` mention a (borrowed) lifetime anywhere? Borrowed field targets
/// (`&'de str`, `Cow<'de, str>`, …) cannot be the target of the `@rid`/`@type`
/// synth fallback — the fallback value is carried by a temporary that would
/// not outlive a borrowed view into it.
fn type_has_lifetime(ty: &Type) -> bool {
    match ty {
        Type::Path(tp) => tp.path.segments.iter().any(|seg| {
            let PathArguments::AngleBracketed(args) = &seg.arguments else {
                return false;
            };
            args.args.iter().any(|arg| match arg {
                GenericArgument::Lifetime(_) => true,
                GenericArgument::Type(inner) => type_has_lifetime(inner),
                _ => false,
            })
        }),
        Type::Reference(r) => r.lifetime.is_some() || type_has_lifetime(&r.elem),
        Type::Tuple(tuple) => tuple.elems.iter().any(type_has_lifetime),
        Type::Paren(paren) => type_has_lifetime(&paren.elem),
        Type::Group(group) => type_has_lifetime(&group.elem),
        _ => false,
    }
}

/// Field-let for a synthetic `@rid` / `@type` property: decode the real
/// property when present, otherwise fall back to the record's own `rid` /
/// `type` (reached through the `__arcade_syn_*` args — no whole-map clone).
/// Honors `Option<T>` and `#[serde(default)]` like the regular field paths.
fn synth_field_let(
    fident: &Ident,
    ty: &Type,
    wire: &str,
    default: &Option<Option<syn::Path>>,
    syn: &TokenStream2,
    crate_path: &syn::Path,
) -> syn::Result<TokenStream2> {
    let owned_guard = |target: &Type| {
        if type_has_lifetime(target) {
            Err(syn::Error::new(
                fident.span(),
                "RecordDecode: `@rid`/`@type` synthesis supports owned targets \
                 only (a borrowed target can't outlive the fallback value)",
            ))
        } else {
            Ok(())
        }
    };

    // `Option<T>` — absent + no synth → `None`.
    if let Some(inner) = option_inner(ty) {
        owned_guard(&inner)?;
        return Ok(quote! {
            let #fident: #ty = match __arcade_props.get(#wire) {
                ::core::option::Option::Some(__arcade_v) => {
                    <::core::option::Option<#inner> as #crate_path::record::FromGrpcValue>::from_grpc_value(
                        __arcade_v,
                    )
                    .map_err(|e| e.with_field(#wire))?
                }
                ::core::option::Option::None => match #syn {
                    ::core::option::Option::Some(__arcade_s) => {
                        let __arcade_syn_gv = #crate_path::GrpcValue {
                            kind: ::core::option::Option::Some(
                                #crate_path::Kind::StringValue(__arcade_s.clone()),
                            ),
                            logical_type: ::std::string::String::new(),
                        };
                        ::core::option::Option::Some(
                            <#inner as #crate_path::record::FromGrpcValue>::from_grpc_value(
                                &__arcade_syn_gv,
                            )
                            .map_err(|e| e.with_field(#wire))?,
                        )
                    }
                    ::core::option::Option::None => ::core::option::Option::None,
                },
            };
        });
    }

    owned_guard(ty)?;

    let synth_occurrence = quote! {
        let __arcade_syn_gv = #crate_path::GrpcValue {
            kind: ::core::option::Option::Some(#crate_path::Kind::StringValue(
                __arcade_s.clone(),
            )),
            logical_type: ::std::string::String::new(),
        };
        <#ty as #crate_path::record::FromGrpcValue>::from_grpc_value(&__arcade_syn_gv)
            .map_err(|e| e.with_field(#wire))?
    };
    let absent = match default {
        None => quote! {
            ::std::result::Result::Err(#crate_path::record::RecordDecodeError::missing_field(
                #wire,
            ))?
        },
        Some(_) => default_fallback(default),
    };

    Ok(quote! {
        let #fident: #ty = match __arcade_props.get(#wire) {
            ::core::option::Option::Some(__arcade_v) => {
                <#ty as #crate_path::record::FromGrpcValue>::from_grpc_value(__arcade_v)
                    .map_err(|e| e.with_field(#wire))?
            }
            ::core::option::Option::None => match #syn {
                // Braced: the synth occurrence is a statement block, not a
                // bare expression (required-`@rid` path).
                ::core::option::Option::Some(__arcade_s) => { #synth_occurrence }
                ::core::option::Option::None => #absent,
            },
        };
    })
}

// ---------------------------------------------------------------------------
// RecordEncode — the write-side mirror of RecordDecode
// ---------------------------------------------------------------------------

fn expand_encode_derive(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    // Same generics policy as the decode side: at most one lifetime parameter
    // (write DTOs borrow `&str` fields from their inputs), no type/const
    // generics — the wire mapping is fully concrete per type.
    let lifetimes: Vec<&syn::LifetimeParam> = input
        .generics
        .params
        .iter()
        .filter_map(|p| match p {
            syn::GenericParam::Lifetime(lt) => Some(lt),
            _ => None,
        })
        .collect();
    if lifetimes.len() > 1 {
        return Err(syn::Error::new(
            input.generics.span(),
            "RecordEncode supports at most one lifetime parameter",
        ));
    }
    if let Some(p) = input
        .generics
        .params
        .iter()
        .find(|p| !matches!(p, syn::GenericParam::Lifetime(_)))
    {
        return Err(syn::Error::new(
            p.span(),
            "RecordEncode does not support type/const generic parameters",
        ));
    }
    let (impl_clause, self_ty) = match lifetimes.first() {
        Some(lt) => {
            let life = &lt.lifetime;
            (quote! { <#life> }, quote! { #name<#life> })
        }
        None => (quote! {}, quote! { #name }),
    };

    let container = ContainerCfg::parse(input)?;

    match &input.data {
        Data::Struct(data) => expand_encode_struct(data, &container, &impl_clause, &self_ty),
        Data::Enum(data) => expand_encode_enum(data, &container, &impl_clause, &self_ty),
        Data::Union(data) => Err(syn::Error::new(
            data.union_token.span(),
            "RecordEncode does not support unions",
        )),
    }
}

/// Expand a `RecordEncode` enum — the write mirror of `RecordDecode`'s enum
/// support. Enums are VALUE-shaped: `to_grpc_value` is the meaningful method
/// (unit variants → their wire string; `#[serde(tag = "...")]` variants → a
/// map carrying the discriminator plus the variant's fields, exactly what
/// decode reads back). `props()` mirrors the tagged form's pairs so a tagged
/// enum can also be stored as a record; unit enums return no pairs.
fn expand_encode_enum(
    data: &DataEnum,
    container: &ContainerCfg,
    impl_clause: &TokenStream2,
    self_ty: &TokenStream2,
) -> syn::Result<TokenStream2> {
    let crate_path = &container.crate_path;
    let tagged = container.tag.as_deref();

    let mut value_arms = Vec::new();
    let mut props_arms = Vec::new();
    for variant in &data.variants {
        let vident = &variant.ident;
        let wire = variant_wire_name(variant, container)?;
        match &variant.fields {
            Fields::Unit => {
                value_arms.push(quote! {
                    Self::#vident => #crate_path::__private::str_v(#wire)
                });
                props_arms.push(quote! {
                    Self::#vident => ::std::vec::Vec::new()
                });
            }
            Fields::Named(fields) => {
                let Some(tag) = tagged else {
                    return Err(syn::Error::new(
                        variant.span(),
                        "RecordEncode: data-carrying variants require `#[serde(tag = \"...\")]` \
                         (internally-tagged); plain unit-variant enums need no tag",
                    ));
                };
                let bindings = fields
                    .named
                    .iter()
                    .map(|f| f.ident.as_ref().expect("named field ident"))
                    .collect::<Vec<_>>();
                let pairs = variant_field_pairs(fields, container)?;
                // Capacity hint: discriminator + fields.
                let pairs_len = fields.named.len() + 1;
                // Value form: discriminator + fields, as a map.
                value_arms.push(quote! {
                    Self::#vident { #(#bindings),* } => {
                        let mut __arcade_pairs: ::std::vec::Vec<(
                            &str,
                            #crate_path::GrpcValue,
                        )> = ::std::vec::Vec::with_capacity(#pairs_len);
                        __arcade_pairs.push((#tag, #crate_path::__private::str_v(#wire)));
                        #pairs
                        #crate_path::GrpcValue {
                            kind: ::core::option::Option::Some(#crate_path::Kind::MapValue(
                                #crate_path::GrpcMap {
                                    entries: __arcade_pairs
                                        .into_iter()
                                        .map(|(k, v)| (k.to_string(), v))
                                        .collect(),
                                },
                            )),
                            logical_type: ::std::string::String::new(),
                        }
                    }
                });
                // Record form: the same pairs.
                props_arms.push(quote! {
                    Self::#vident { #(#bindings),* } => {
                        let mut __arcade_pairs: ::std::vec::Vec<(
                            &str,
                            #crate_path::GrpcValue,
                        )> = ::std::vec::Vec::with_capacity(#pairs_len);
                        __arcade_pairs.push((#tag, #crate_path::__private::str_v(#wire)));
                        #pairs
                        __arcade_pairs
                    }
                });
            }
            Fields::Unnamed(_) => {
                return Err(syn::Error::new(
                    variant.span(),
                    "RecordEncode on enums supports struct variants only",
                ));
            }
        }
    }

    Ok(quote! {
        #[automatically_derived]
        impl #impl_clause #crate_path::record::ToGrpcRecord for #self_ty {
            fn props(&self) -> ::std::vec::Vec<(&str, #crate_path::GrpcValue)> {
                match self {
                    #(#props_arms,)*
                }
            }

            fn to_grpc_value(&self) -> #crate_path::GrpcValue {
                match self {
                    #(#value_arms,)*
                }
            }
        }
    })
}

/// `(name, value)` pair pushes for one struct variant's fields — the
/// encode-side mirror of `variant_field_lets` (same policy: rename/
/// `Option`-omission/`with`/skip; `flatten` unsupported inside variants).
/// Pairs append to a `__arcade_pairs` binding declared by the caller.
fn variant_field_pairs(
    fields: &syn::FieldsNamed,
    container: &ContainerCfg,
) -> syn::Result<TokenStream2> {
    let crate_path = &container.crate_path;
    let mut pushes = Vec::new();
    for field in &fields.named {
        let fident = field.ident.as_ref().expect("named field ident").clone();
        let cfg = FieldCfg::parse(field)?;
        let ty = &field.ty;

        if cfg.flatten {
            return Err(syn::Error::new(
                field.span(),
                "RecordEncode: `flatten` is not supported inside enum variants",
            ));
        }
        if cfg.key {
            return Err(syn::Error::new(
                field.span(),
                "RecordEncode: `key` is not supported inside enum variants",
            ));
        }

        let wire = match (
            &cfg.serialize_rename,
            &cfg.rename,
            container.rename_all_fields,
        ) {
            (Some(r), _, _) => r.clone(),
            (None, Some(r), _) => r.clone(),
            (None, None, Some(rule)) => rule.apply_to_field(&fident.to_string()),
            (None, None, None) => fident.to_string(),
        };

        if cfg.skip_encode {
            continue;
        }

        if let Some(with) = &cfg.with {
            if option_inner(ty).is_some() {
                pushes.push(quote! {
                    if let ::core::option::Option::Some(__arcade_f) = #fident {
                        __arcade_pairs.push((#wire, #with(__arcade_f)));
                    }
                });
            } else {
                pushes.push(quote! {
                    __arcade_pairs.push((#wire, #with(#fident)));
                });
            }
            continue;
        }

        if let Some(inner) = option_inner(ty) {
            let val = encode_field_value(&inner, &quote! { __arcade_f }, crate_path)?;
            pushes.push(quote! {
                if let ::core::option::Option::Some(__arcade_f) = #fident {
                    __arcade_pairs.push((#wire, #val));
                }
            });
            continue;
        }

        let val = encode_field_value(ty, &quote! { #fident }, crate_path)?;
        let push = quote! { __arcade_pairs.push((#wire, #val)); };
        if let Some(pred) = &cfg.skip_serializing_if {
            pushes.push(quote! {
                if !#pred(#fident) {
                    #push
                }
            });
        } else {
            pushes.push(push);
        }
    }
    Ok(quote! { #(#pushes)* })
}

/// Expand a `RecordEncode` struct: `props()` over the named fields, mapping
/// each Rust type onto the matching `GrpcValue` wire kind.
fn expand_encode_struct(
    data: &syn::DataStruct,
    container: &ContainerCfg,
    impl_clause: &TokenStream2,
    self_ty: &TokenStream2,
) -> syn::Result<TokenStream2> {
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new(
            data.fields.span(),
            "RecordEncode supports named-field structs only; newtype wrappers \
             plug in through `#[record(with = \"...\")]` on the field instead",
        ));
    };

    let crate_path = &container.crate_path;
    let mut pushes = Vec::new();
    let mut move_pushes = Vec::new();
    let mut diff_conds = Vec::new();
    let mut key_wires: Vec<String> = Vec::new();
    for field in &fields.named {
        let fident = field.ident.as_ref().expect("named field ident").clone();
        let cfg = FieldCfg::parse(field)?;
        let ty = &field.ty;

        let wire = match (&cfg.serialize_rename, &cfg.rename, container.rename_all) {
            (Some(r), _, _) => r.clone(),
            (None, Some(r), _) => r.clone(),
            (None, None, Some(rule)) => rule.apply_to_field(&fident.to_string()),
            (None, None, None) => fident.to_string(),
        };

        if cfg.key {
            if cfg.skip_encode {
                return Err(syn::Error::new(
                    fident.span(),
                    "RecordEncode: `key` cannot be combined with `skip`",
                ));
            }
            key_wires.push(wire.clone());
        }

        pushes.push(encode_field_push(&fident, ty, &wire, &cfg, crate_path)?);
        move_pushes.push(encode_field_push_move(
            &fident, ty, &wire, &cfg, crate_path,
        )?);
        diff_conds.push(diff_cond(&fident, ty, &wire, &cfg, crate_path));
    }
    // Upper bound for `props()`: one pair per field (skips push nothing) —
    // exact for skip-free DTOs, and never a growth reallocation either way.
    let props_capacity = fields.named.len();

    Ok(quote! {
        #[automatically_derived]
        // Generated code widens a couple of ints onto kinds the wire supports;
        // `automatically_derived`-style allowances keep consumer clippy quiet.
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        impl #impl_clause #crate_path::record::ToGrpcRecord for #self_ty {
            /// Conflict identity from the `#[record(key)]` fields — see
            /// `ToGrpcRecord::KEY_COLUMNS`.
            const KEY_COLUMNS: &'static [&'static str] = &[#(#key_wires),*];

            fn props(&self) -> ::std::vec::Vec<(&str, #crate_path::GrpcValue)> {
                let mut __out: ::std::vec::Vec<(&str, #crate_path::GrpcValue)> =
                    ::std::vec::Vec::with_capacity(#props_capacity);
                #(#pushes)*
                __out
            }

            /// Rust-value diff — no wire encoding; allocation proportional
            /// to the changed-column count (identical rows allocate nothing).
            fn changed_wires(&self, other: &Self) -> ::std::vec::Vec<::std::string::String> {
                let mut __arcade_changed: ::std::vec::Vec<::std::string::String> =
                    ::std::vec::Vec::new();
                #(#diff_conds)*
                __arcade_changed
            }

            /// The moving encode: owned payloads (`String`, `Vec<u8>`,
            /// `Vec<String>`, `Cow<str>`) become wire values by MOVE, not
            /// clone. Types without a moving mapping encode through a borrow,
            /// as in `props()`.
            fn into_grpc_record(self, class: &str) -> #crate_path::GrpcRecord {
                let mut __out: ::std::vec::Vec<(&str, #crate_path::GrpcValue)> =
                    ::std::vec::Vec::with_capacity(#props_capacity);
                #(#move_pushes)*
                #crate_path::rec(class, __out)
            }
        }
    })
}

/// The `changed_wires` condition for one field. `self` = old, `other` = new:
/// a column the NEW snapshot omits (`skip_serializing_if`) is never reported,
/// and an `Option` going `Some → None` is not SET-expressible — both match
/// `props_diff`'s semantics.
fn diff_cond(
    fident: &Ident,
    ty: &Type,
    wire: &str,
    cfg: &FieldCfg,
    crate_path: &syn::Path,
) -> TokenStream2 {
    let wire = syn::LitStr::new(wire, proc_macro2::Span::call_site());
    if cfg.skip_encode {
        return quote! {};
    }
    let is_option = option_inner(ty).is_some();
    let differs = if cfg.flatten {
        // The flatten target has no field-wise comparison here — fall back
        // to its encoded form (allocates; flatten is the rare shape).
        quote! {
            <#ty as #crate_path::record::ToGrpcRecord>::to_grpc_value(&self.#fident)
                != <#ty as #crate_path::record::ToGrpcRecord>::to_grpc_value(&other.#fident)
        }
    } else if let Some(w) = &cfg.with {
        if is_option {
            quote! { other.#fident.is_some() && #w(&self.#fident) != #w(&other.#fident) }
        } else {
            quote! { #w(&self.#fident) != #w(&other.#fident) }
        }
    } else if is_option {
        quote! { other.#fident.is_some() && self.#fident != other.#fident }
    } else {
        quote! { self.#fident != other.#fident }
    };
    let guarded = match &cfg.skip_serializing_if {
        Some(pred) => quote! { !#pred(&other.#fident) && (#differs) },
        None => differs,
    };
    quote! {
        if #guarded {
            __arcade_changed.push(#wire.to_string());
        }
    }
}

/// One `into_grpc_record` push: the moving analogue of [`encode_field_push`].
/// Owned payloads of movable types are MOVED into the wire value; everything
/// else encodes through a borrow exactly like `props()`.
fn encode_field_push_move(
    fident: &Ident,
    ty: &Type,
    wire: &str,
    cfg: &FieldCfg,
    crate_path: &syn::Path,
) -> syn::Result<TokenStream2> {
    let wire = syn::LitStr::new(wire, proc_macro2::Span::call_site());
    if cfg.skip_encode {
        return Ok(quote! {});
    }

    // Flatten and `with` have no moving form — borrow as in `props()`.
    if cfg.flatten || cfg.with.is_some() {
        let borrow = encode_field_push(fident, ty, wire.value().as_str(), cfg, crate_path)?;
        return Ok(borrow);
    }

    let val_plain = encode_field_value_move(ty, &quote! { self.#fident }, crate_path);
    let push_plain = quote! { __out.push((#wire, #val_plain)); };
    if let Some(inner) = option_inner(ty) {
        let val_opt = encode_field_value_move(&inner, &quote! { __arcade_f }, crate_path);
        return Ok(quote! {
            if let ::core::option::Option::Some(__arcade_f) = self.#fident {
                __out.push((#wire, #val_opt));
            }
        });
    }
    if let Some(pred) = &cfg.skip_serializing_if {
        // The predicate borrows BEFORE the field moves.
        return Ok(quote! {
            if !#pred(&self.#fident) {
                #push_plain
            }
        });
    }
    Ok(push_plain)
}

/// A moved-value wire expression: `String`/`Cow<str>`/`Vec<u8>`/
/// `Vec<String>`/scalars move; every other type falls back to the borrowing
/// mapping on a reference (same wire output, clone cost as in `props()`).
fn encode_field_value_move(ty: &Type, expr: &TokenStream2, crate_path: &syn::Path) -> TokenStream2 {
    if let Type::Reference(r) = ty {
        return encode_field_value(&r.elem, expr, crate_path)
            .unwrap_or_else(|e| e.to_compile_error());
    }
    let Type::Path(tp) = ty else {
        return encode_field_value(ty, expr, crate_path).unwrap_or_else(|e| e.to_compile_error());
    };
    let Some(seg) = tp.path.segments.last() else {
        return encode_field_value(ty, expr, crate_path).unwrap_or_else(|e| e.to_compile_error());
    };
    match seg.ident.to_string().as_str() {
        "String" => quote! { #crate_path::__private::str_v(#expr) },
        "char" => quote! { #crate_path::__private::str_v((#expr).to_string()) },
        "Cow" => {
            if let Ok(inner) = container_inner(ty, seg, "Cow") {
                match inner {
                    Type::Path(p) if p.path.is_ident("str") => {
                        quote! { #crate_path::__private::str_v((#expr).into_owned()) }
                    }
                    Type::Slice(sl) if matches!(&*sl.elem, Type::Path(p) if p.path.is_ident("u8")) =>
                    {
                        quote! { #crate_path::__private::bytes_v((#expr).into_owned()) }
                    }
                    _ => encode_field_value(ty, &quote! { &#expr }, crate_path)
                        .unwrap_or_else(|e| e.to_compile_error()),
                }
            } else {
                encode_field_value(ty, &quote! { &#expr }, crate_path)
                    .unwrap_or_else(|e| e.to_compile_error())
            }
        }
        "bool" => quote! { #crate_path::__private::bool_v(#expr) },
        "i32" => quote! { #crate_path::__private::i32_v(#expr) },
        "i8" | "i16" | "u8" | "u16" => quote! { #crate_path::__private::i32_v(i32::from(#expr)) },
        "i64" | "u32" => quote! { #crate_path::__private::i64_v(i64::from(#expr)) },
        "u64" => quote! { #crate_path::__private::i64_v(#expr as i64) },
        "f32" => quote! { #crate_path::__private::f32_v(#expr) },
        "f64" => quote! { #crate_path::__private::f64_v(#expr) },
        "DateTime" => quote! {
            #crate_path::GrpcValue {
                kind: ::core::option::Option::Some(#crate_path::Kind::TimestampValue(
                    #crate_path::Timestamp {
                        seconds: (#expr).timestamp(),
                        nanos: (#expr).timestamp_subsec_nanos() as i32,
                    },
                )),
                logical_type: ::std::string::String::new(),
            }
        },
        "Value" => quote! { #crate_path::json_to_grpc_value(&#expr) },
        "Link" => quote! {
            #crate_path::GrpcValue {
                kind: ::core::option::Option::Some(#crate_path::Kind::LinkValue(
                    #crate_path::GrpcLink {
                        rid: (#expr).to_string(),
                        r#type: ::std::string::String::new(),
                    },
                )),
                logical_type: ::std::string::String::new(),
            }
        },
        _ => {
            // `Vec<u8>` moves as a blob; `Vec<String>` moves element-wise;
            // any other container encodes through a borrow. The `Vec` checks
            // are gated on the segment ident — `container_inner` would
            // happily read the value arg of a `HashMap<K, V>` otherwise.
            if seg.ident != "Vec" {
                encode_field_value(ty, &quote! { &#expr }, crate_path)
                    .unwrap_or_else(|e| e.to_compile_error())
            } else if vec_u8(ty) {
                quote! { #crate_path::__private::bytes_v(#expr) }
            } else if matches!(container_inner(ty, seg, "Vec"), Ok(inner)
                if matches!(inner, Type::Path(p) if p.path.is_ident("String")))
            {
                quote! {
                    #crate_path::GrpcValue {
                        kind: ::core::option::Option::Some(#crate_path::Kind::ListValue(
                            #crate_path::GrpcList {
                                values: (#expr)
                                    .into_iter()
                                    .map(|__arcade_e| #crate_path::__private::str_v(__arcade_e))
                                    .collect(),
                            },
                        )),
                        logical_type: ::std::string::String::new(),
                    }
                }
            } else {
                encode_field_value(ty, &quote! { &#expr }, crate_path)
                    .unwrap_or_else(|e| e.to_compile_error())
            }
        }
    }
}

/// One `props()` push for a field: skip policy, `Option` → property omitted
/// when `None`, custom `with` fn, or the type-driven wire mapping.
fn encode_field_push(
    fident: &Ident,
    ty: &Type,
    wire: &str,
    cfg: &FieldCfg,
    crate_path: &syn::Path,
) -> syn::Result<TokenStream2> {
    let wire = syn::LitStr::new(wire, proc_macro2::Span::call_site());
    if cfg.skip_encode {
        return Ok(quote! {});
    }

    // Flattened nested record — splice its property map into ours.
    if cfg.flatten {
        // Mirror the decode-side validation: a flattened field's wire shape
        // is the nested props, so `with` could never apply.
        if cfg.with.is_some() {
            return Err(syn::Error::new(
                fident.span(),
                "RecordEncode: `with` cannot be combined with a flattened field",
            ));
        }
        return if let Some(inner) = option_inner(ty) {
            Ok(quote! {
                if let ::core::option::Option::Some(__arcade_f) = &self.#fident {
                    __out.extend(<#inner as #crate_path::record::ToGrpcRecord>::props(
                        __arcade_f,
                    ));
                }
            })
        } else {
            Ok(quote! {
                __out.extend(<#ty as #crate_path::record::ToGrpcRecord>::props(
                    &self.#fident,
                ));
            })
        };
    }

    // `Option<T>` — `None` means "don't write the property" (new rows get the
    // schema default; conflict rows keep the stored value).
    if let Some(inner) = option_inner(ty) {
        let val = match &cfg.with {
            Some(w) => quote! { #w(__arcade_f) },
            None => encode_field_value(&inner, &quote! { __arcade_f }, crate_path)?,
        };
        return Ok(quote! {
            if let ::core::option::Option::Some(__arcade_f) = &self.#fident {
                __out.push((#wire, #val));
            }
        });
    }

    let val = match &cfg.with {
        Some(w) => quote! { #w(&self.#fident) },
        None => encode_field_value(ty, &quote! { &self.#fident }, crate_path)?,
    };
    let push = quote! { __out.push((#wire, #val)); };
    if let Some(pred) = &cfg.skip_serializing_if {
        return Ok(quote! {
            if !#pred(&self.#fident) {
                #push
            }
        });
    }
    Ok(push)
}

// ===========================================================================
// Canonical record-type classification
// ===========================================================================
//

/// The single type argument of a generic container (`Vec<T>`, `Cow<T>`, …).
fn container_inner<'a>(
    ty: &'a Type,
    seg: &'a syn::PathSegment,
    what: &str,
) -> syn::Result<&'a Type> {
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return Err(syn::Error::new(
            ty.span(),
            format!("`{what}` needs a type argument"),
        ));
    };
    let Some(GenericArgument::Type(inner)) = args.args.last() else {
        return Err(syn::Error::new(
            ty.span(),
            format!("`{what}` needs a type argument"),
        ));
    };
    Ok(inner)
}

/// The value type of a `HashMap<String, T>` / `BTreeMap<String, T>` (map keys
/// must be `String` — the wire map only carries string keys).
fn map_value_type<'a>(ty: &'a Type, seg: &'a syn::PathSegment) -> syn::Result<&'a Type> {
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return Err(syn::Error::new(ty.span(), "`map` needs type arguments"));
    };
    let mut types = args.args.iter().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    });
    let key = types
        .next()
        .ok_or_else(|| syn::Error::new(ty.span(), "`map` needs a key type"))?;
    if !matches!(key, Type::Path(p) if p.path.is_ident("String")) {
        return Err(syn::Error::new(
            key.span(),
            "RecordEncode: map keys must be `String` (the wire map is string-keyed)",
        ));
    }
    let value = types
        .next()
        .ok_or_else(|| syn::Error::new(ty.span(), "`map` needs a value type"))?;
    if types.next().is_some() {
        return Err(syn::Error::new(
            ty.span(),
            "`map` expects exactly two type arguments",
        ));
    }
    Ok(value)
}

/// The single table enumerating the Rust types the record derives support.
/// Both consumers — wire-kind/schema rendering ([`db_type_of`] via
/// [`render`]) and encode codegen ([`value_from_db`]) — dispatch on this
/// enum, so adding a supported type means adding **one variant here** plus
/// one arm each in `render` and `value_from_db`. Nothing else in the macro
/// matches on type idents.
///
/// Contract ripple when extending: `classify` (this file) → `render` →
/// `WIRE_KINDS` const → migrate-side `dto_drift` interpretation.
enum DbType {
    /// `str` / `String` / `Cow<str>`.
    Str,
    /// `char` (encodes as a one-character string).
    Char,
    Bool,
    /// Wire `Int32` family — exact source width drives the conversion.
    Int32(Int32W),
    /// Wire `Int64` family.
    Int64(Int64W),
    F32,
    F64,
    /// Byte blob (`Vec<u8>` / `Cow<[u8]>`) — wire `bytes`, no schema mapping
    /// (the column shape is ambiguous).
    Bytes,
    /// `serde_json::Value` dynamic catch-all.
    Json,
    Link,
    DateTime,
    NaiveDate,
    /// `rust_decimal::Decimal` (protocol `decimal` feature). Codegen emits
    /// `decimal_v` directly: a DTO naming the type without the feature fails
    /// loudly there, which is stricter than the old silent fallback.
    Decimal,
    /// `Vec<T>` / `HashSet<T>` / `BTreeSet<T>` / `&[T]` — all wire lists
    /// (`SET OF` vs `LIST OF` intent lives in the DTO field's Rust type).
    /// Inner `None` = foreign element (a nested `RecordEncode` struct),
    /// encoded through its own fallback.
    List(Box<Option<DbType>>),
    /// `HashMap<String, T>` / `BTreeMap<String, T>` — wire maps.
    Map(Box<Option<DbType>>),
}

/// How an `Int32`-family value is obtained from the field expression.
enum Int32W {
    /// Already `i32`.
    Direct,
    /// `i8` / `i16` / `u8` / `u16` — losslessly widened.
    Widened,
}

/// How an `Int64`-family value is obtained from the field expression.
enum Int64W {
    /// Already `i64`.
    Direct,
    /// `u32`.
    FromU32,
    /// `u64` narrowed (`as i64`; ArcadeDB has no unsigned 64-bit kind).
    NarrowedU64,
}

/// Outcome of classifying a field type. `Foreign` types fall back to the
/// `ToGrpcRecord` path (nested derive structs); `Err` is a malformed usage
/// of a *known* family (e.g. `Cow<f64>`) and must surface as a compile error.
enum Classification {
    Known(DbType),
    Foreign,
}

/// The one matcher over `syn::Type` for the record universe. Handles the
/// transparent wrappers first (references, slices as list views, `Option`
/// unwrapping), then dispatches on the last path segment.
fn classify(ty: &Type) -> syn::Result<Classification> {
    if let Type::Reference(r) = ty {
        return classify(&r.elem);
    }
    if let Type::Slice(s) = ty {
        // `&[T]` — a borrowed list view.
        return Ok(Classification::Known(DbType::List(Box::new(
            elem_classification(&s.elem)?,
        ))));
    }
    if let Some(ref inner) = option_inner(ty) {
        return classify(inner);
    }
    let Type::Path(tp) = ty else {
        return Ok(Classification::Foreign);
    };
    let seg = tp.path.segments.last().expect("path always has segments");

    if seg.ident == "Cow" {
        let inner = container_inner(ty, seg, "Cow")?;
        return match inner {
            Type::Path(p) if p.path.is_ident("str") => Ok(Classification::Known(DbType::Str)),
            Type::Slice(s) if matches!(&*s.elem, Type::Path(p) if p.path.is_ident("u8")) => {
                Ok(Classification::Known(DbType::Bytes))
            }
            _ => Err(syn::Error::new(
                ty.span(),
                "RecordEncode: `Cow` fields must be `Cow<str>` or `Cow<[u8]>`",
            )),
        };
    }

    let db = match seg.ident.to_string().as_str() {
        "str" | "String" => DbType::Str,
        "char" => DbType::Char,
        "bool" => DbType::Bool,
        "i8" => DbType::Int32(Int32W::Widened),
        "i16" => DbType::Int32(Int32W::Widened),
        "i32" => DbType::Int32(Int32W::Direct),
        "u8" => DbType::Int32(Int32W::Widened),
        "u16" => DbType::Int32(Int32W::Widened),
        "i64" => DbType::Int64(Int64W::Direct),
        "u32" => DbType::Int64(Int64W::FromU32),
        // ArcadeDB has no unsigned 64-bit kind; values over i64::MAX cannot
        // be round-tripped (ordinary ids are well below it).
        "u64" => DbType::Int64(Int64W::NarrowedU64),
        "f32" => DbType::F32,
        "f64" => DbType::F64,
        "Value" => DbType::Json,
        "Link" => DbType::Link,
        "DateTime" => DbType::DateTime,
        "NaiveDate" => DbType::NaiveDate,
        "Decimal" => DbType::Decimal,
        "Vec" | "HashSet" | "BTreeSet" => {
            if vec_u8(ty) {
                DbType::Bytes
            } else {
                let elem_ty = container_inner(ty, seg, "list container")?;
                DbType::List(Box::new(elem_classification(elem_ty)?))
            }
        }
        "HashMap" | "BTreeMap" => {
            let value_ty = map_value_type(ty, seg)?;
            DbType::Map(Box::new(elem_classification(value_ty)?))
        }
        _ => return Ok(Classification::Foreign),
    };
    Ok(Classification::Known(db))
}

/// Classify a container's element/value type: known → `Some`, foreign (a
/// nested `RecordEncode` struct) → `None`, malformed → error.
fn elem_classification(ty: &Type) -> syn::Result<Option<DbType>> {
    match classify(ty)? {
        Classification::Known(db) => Ok(Some(db)),
        Classification::Foreign => Ok(None),
    }
}

/// Schema column type for drift checks — the vocabulary migrate's
/// `dto_drift` interprets. `None` = no clean column correspondence (byte
/// blobs, dynamic values, foreign elements).
fn render(db: &DbType) -> Option<String> {
    match db {
        DbType::Str | DbType::Char => Some("STRING".to_string()),
        DbType::Bool => Some("BOOLEAN".to_string()),
        DbType::Int32(_) => Some("INTEGER".to_string()),
        DbType::Int64(_) => Some("LONG".to_string()),
        DbType::F32 => Some("FLOAT".to_string()),
        DbType::F64 => Some("DOUBLE".to_string()),
        DbType::Bytes | DbType::Json => None,
        DbType::Link => Some("LINK".to_string()),
        DbType::DateTime => Some("DATETIME".to_string()),
        DbType::NaiveDate => Some("DATE".to_string()),
        // Emitted whenever a DTO names the rust_decimal type — implies the
        // consumer enabled the protocol's `decimal` feature.
        DbType::Decimal => Some("DECIMAL".to_string()),
        // The declaration cannot distinguish LIST OF vs SET OF; the drift
        // checker treats both spellings as one class.
        DbType::List(inner) => render_inner(inner).map(|s| format!("LIST OF {s}")),
        DbType::Map(_) => Some("MAP".to_string()),
    }
}

fn render_inner(inner: &Option<DbType>) -> Option<String> {
    inner.as_ref().and_then(render)
}

/// Encode codegen for a classified type. Total over `DbType` — `classify`
/// has already rejected malformed usages. Numeric arms preserve the exact
/// expressions the previous per-width matcher emitted (`*#expr`,
/// `i32::from(*#expr)`, …).
fn value_from_db(db: &DbType, expr: &TokenStream2, crate_path: &syn::Path) -> TokenStream2 {
    match db {
        DbType::Str | DbType::Char => quote! { #crate_path::__private::str_v((#expr).to_string()) },
        DbType::Bool => quote! { #crate_path::__private::bool_v(*#expr) },
        DbType::Int32(Int32W::Direct) => quote! { #crate_path::__private::i32_v(*#expr) },
        DbType::Int32(Int32W::Widened) => {
            quote! { #crate_path::__private::i32_v(i32::from(*#expr)) }
        }
        DbType::Int64(Int64W::Direct) => quote! { #crate_path::__private::i64_v(*#expr) },
        DbType::Int64(Int64W::FromU32) => {
            quote! { #crate_path::__private::i64_v(i64::from(*#expr)) }
        }
        DbType::Int64(Int64W::NarrowedU64) => {
            quote! { #crate_path::__private::i64_v(*#expr as i64) }
        }
        DbType::F32 => quote! { #crate_path::__private::f32_v(*#expr) },
        DbType::F64 => quote! { #crate_path::__private::f64_v(*#expr) },
        DbType::Bytes => quote! { #crate_path::__private::bytes_v((#expr).clone()) },
        DbType::Json => quote! { #crate_path::json_to_grpc_value(#expr) },
        DbType::Link => quote! {
            #crate_path::GrpcValue {
                kind: ::core::option::Option::Some(#crate_path::Kind::LinkValue(
                    #crate_path::GrpcLink {
                        rid: (#expr).to_string(),
                        r#type: ::std::string::String::new(),
                    },
                )),
                logical_type: ::std::string::String::new(),
            }
        },
        // chrono `DateTime` (any timezone) → the protobuf well-known
        // `Timestamp` — the wire's date/datetime kind. Sub-second nanos are
        // always < 2³¹, so the `as i32` narrowing is total.
        DbType::DateTime => quote! {
            #crate_path::GrpcValue {
                kind: ::core::option::Option::Some(#crate_path::Kind::TimestampValue(
                    #crate_path::Timestamp {
                        seconds: (#expr).timestamp(),
                        nanos: (#expr).timestamp_subsec_nanos() as i32,
                    },
                )),
                logical_type: ::std::string::String::new(),
            }
        },
        DbType::NaiveDate => quote! { #crate_path::__private::date_v(*#expr) },
        DbType::Decimal => quote! { #crate_path::__private::decimal_v(*#expr) },
        DbType::List(inner) => {
            let inner: &Option<DbType> = inner;
            let elem = elem_tokens(inner.as_ref(), &quote! { __arcade_e }, crate_path);
            quote! {
                #crate_path::GrpcValue {
                    kind: ::core::option::Option::Some(#crate_path::Kind::ListValue(
                        #crate_path::GrpcList {
                            values: (#expr)
                                .iter()
                                .map(|__arcade_e| #elem)
                                .collect(),
                        },
                    )),
                    logical_type: ::std::string::String::new(),
                }
            }
        }
        DbType::Map(inner) => {
            let inner: &Option<DbType> = inner;
            let value = elem_tokens(inner.as_ref(), &quote! { __arcade_v }, crate_path);
            quote! {
                #crate_path::GrpcValue {
                    kind: ::core::option::Option::Some(#crate_path::Kind::MapValue(
                        #crate_path::GrpcMap {
                            entries: (#expr)
                                .iter()
                                .map(|(__arcade_k, __arcade_v)| (__arcade_k.clone(), #value))
                                .collect(),
                        },
                    )),
                    logical_type: ::std::string::String::new(),
                }
            }
        }
    }
}

/// Element/value tokens inside a container: known kinds encode per their
/// variant (bound to the iteration variable); foreign elements fall back to
/// the record-map form.
fn elem_tokens(inner: Option<&DbType>, var: &TokenStream2, crate_path: &syn::Path) -> TokenStream2 {
    match inner {
        Some(db) => value_from_db(db, var, crate_path),
        None => quote! {
            <_ as #crate_path::record::ToGrpcRecord>::to_grpc_value(#var)
        },
    }
}

/// Wire-value codegen for one field expression. `Foreign` types route to
/// their `ToGrpcRecord` impl (nested derive structs).
fn encode_field_value(
    ty: &Type,
    expr: &TokenStream2,
    crate_path: &syn::Path,
) -> syn::Result<TokenStream2> {
    match classify(ty)? {
        Classification::Known(db) => Ok(value_from_db(&db, expr, crate_path)),
        Classification::Foreign => Ok(quote! {
            <#ty as #crate_path::record::ToGrpcRecord>::to_grpc_value(#expr)
        }),
    }
}

/// The expected ArcadeDB column type for a decode field, when statically
/// knowable — feeds `WIRE_KINDS` for schema-drift checks. Returns `None`
/// for types without a clean column correspondence (custom types, dynamic
/// values, byte blobs — `Vec<u8>` legitimately lives in several column
/// shapes). `Option<T>` unwraps to `T`; nested `Vec<T>` renders as
/// `LIST OF <T>`; the migrate-side checker treats the integer and float
/// families as decodable across their widths (the decoder is range-checked
/// and float-tolerant, so `i64` reading an `INTEGER` column is fine).
fn db_type_of(ty: &Type) -> Option<String> {
    match classify(ty) {
        Ok(Classification::Known(db)) => render(&db),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// rename_all rules — serde's exact case dialect
// ---------------------------------------------------------------------------

/// A `rename_all` case rule, with serde's exact transformation semantics
/// (ported from `serde_derive`'s `internals/case.rs`). Parity with serde is
/// the point: wire keys are produced by serde's `Serialize`, so the decode
/// side must apply the identical rules — including serde's deliberate
/// no-acronym-grouping (`HTTPServer` → snake_case → `h_t_t_p_server`).
#[derive(Clone, Copy)]
enum RenameRule {
    Lower,
    Upper,
    Pascal,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

impl RenameRule {
    /// Parse one of serde's eight rule names; anything else is a compile
    /// error, mirroring serde's own unknown-`rename_all` error.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "lowercase" => Some(Self::Lower),
            "UPPERCASE" => Some(Self::Upper),
            "PascalCase" => Some(Self::Pascal),
            "camelCase" => Some(Self::Camel),
            "snake_case" => Some(Self::Snake),
            "SCREAMING_SNAKE_CASE" => Some(Self::ScreamingSnake),
            "kebab-case" => Some(Self::Kebab),
            "SCREAMING-KEBAB-CASE" => Some(Self::ScreamingKebab),
            _ => None,
        }
    }

    /// Apply to a *variant* name (PascalCase input, serde's
    /// `apply_to_variant`): snake/kebab split before every uppercase char.
    fn apply_to_variant(self, variant: &str) -> String {
        match self {
            Self::Pascal => variant.to_owned(),
            Self::Lower => variant.to_ascii_lowercase(),
            Self::Upper => variant.to_ascii_uppercase(),
            Self::Camel => lower_first(variant),
            Self::Snake => {
                let mut snake = String::with_capacity(variant.len() + 4);
                for (i, ch) in variant.char_indices() {
                    if i > 0 && ch.is_uppercase() {
                        snake.push('_');
                    }
                    snake.push(ch.to_ascii_lowercase());
                }
                snake
            }
            Self::ScreamingSnake => Self::Snake.apply_to_variant(variant).to_ascii_uppercase(),
            Self::Kebab => Self::Snake.apply_to_variant(variant).replace('_', "-"),
            Self::ScreamingKebab => Self::ScreamingSnake
                .apply_to_variant(variant)
                .replace('_', "-"),
        }
    }

    /// Apply to a *field* name (snake_case input, serde's `apply_to_field`):
    /// `lowercase`/`snake_case` are identities — serde assumes Rust's
    /// snake_case convention and never regroups case runs.
    fn apply_to_field(self, field: &str) -> String {
        match self {
            Self::Lower | Self::Snake => field.to_owned(),
            Self::Upper => field.to_ascii_uppercase(),
            Self::Pascal => {
                let mut pascal = String::with_capacity(field.len());
                let mut capitalize = true;
                for ch in field.chars() {
                    if ch == '_' {
                        capitalize = true;
                    } else if capitalize {
                        pascal.push(ch.to_ascii_uppercase());
                        capitalize = false;
                    } else {
                        pascal.push(ch);
                    }
                }
                pascal
            }
            Self::Camel => lower_first(&Self::Pascal.apply_to_field(field)),
            Self::ScreamingSnake => field.to_ascii_uppercase(),
            Self::Kebab => field.replace('_', "-"),
            Self::ScreamingKebab => Self::ScreamingSnake.apply_to_field(field).replace('_', "-"),
        }
    }
}

/// Lowercase the first char, keep the rest (serde's camelCase final step).
fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_lowercase().collect::<String>())
        .unwrap_or_default()
        + chars.as_str()
}

#[cfg(test)]
mod dbtype_tests {
    use super::*;
    use syn::parse_quote;

    fn rendered(ty: Type) -> Option<String> {
        match classify(&ty).expect("type must classify") {
            Classification::Known(db) => render(&db),
            Classification::Foreign => None,
        }
    }

    /// Every supported type classifies, renders, and agrees with the
    /// schema-drift vocabulary — the matrix that dies if a new `DbType`
    /// variant (or matcher arm) is added to one consumer but not the other.
    #[test]
    fn classification_matrix_is_complete() {
        let cases: Vec<(Type, Option<&str>)> = vec![
            (parse_quote!(String), Some("STRING")),
            (parse_quote!(char), Some("STRING")),
            (parse_quote!(bool), Some("BOOLEAN")),
            (parse_quote!(i8), Some("INTEGER")),
            (parse_quote!(u16), Some("INTEGER")),
            (parse_quote!(i32), Some("INTEGER")),
            (parse_quote!(i64), Some("LONG")),
            (parse_quote!(u32), Some("LONG")),
            (parse_quote!(f32), Some("FLOAT")),
            (parse_quote!(f64), Some("DOUBLE")),
            (
                parse_quote!(chrono::DateTime<chrono::Utc>),
                Some("DATETIME"),
            ),
            (parse_quote!(chrono::NaiveDate), Some("DATE")),
            (parse_quote!(rust_decimal::Decimal), Some("DECIMAL")),
            (parse_quote!(arcadedb_protocol::Link), Some("LINK")),
            (parse_quote!(Vec<i32>), Some("LIST OF INTEGER")),
            (
                parse_quote!(std::collections::HashSet<String>),
                Some("LIST OF STRING"),
            ),
            (
                parse_quote!(std::collections::BTreeSet<u8>),
                Some("LIST OF INTEGER"),
            ),
            (parse_quote!(Vec<Vec<f64>>), Some("LIST OF LIST OF DOUBLE")),
            (
                parse_quote!(std::collections::HashMap<String, i64>),
                Some("MAP"),
            ),
            (
                parse_quote!(std::collections::BTreeMap<String, f64>),
                Some("MAP"),
            ),
            // Excluded from drift, but still encodable.
            (parse_quote!(Vec<u8>), None),
            // Foreign types render nothing (ToGrpcRecord fallback).
            (parse_quote!(SomeExternalStruct), None),
            (parse_quote!(Option<i64>), Some("LONG")),
            (parse_quote!(&[i32]), Some("LIST OF INTEGER")),
        ];
        for (ty, expected) in cases {
            assert_eq!(rendered(ty), expected.map(str::to_string));
        }
    }

    /// Malformed known-family usages are hard errors, never silent fallbacks.
    #[test]
    fn malformed_cow_is_an_error() {
        let ty: Type = parse_quote!(Cow<f64>);
        assert!(classify(&ty).is_err());
    }
}
