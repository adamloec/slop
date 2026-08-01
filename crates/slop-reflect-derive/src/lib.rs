//! `#[derive(Reflect)]` — the compile-time front end to `slop-reflect`.
//!
//! Depend on `slop-reflect` rather than on this crate; it re-exports the macro.
//! A proc macro must live in its own crate, which is a Rust restriction and not
//! a design decision.
//!
//! # What the macro will not let you get wrong
//!
//! `docs/DESIGN.md` §2.4 makes [`TypeInfo`] a value, and three of its fields are
//! trusted by the ECS in ways that are memory-unsafe or ABI-unsafe to get wrong.
//! None of them is taken from the author:
//!
//! - **Layout** is `Layout::new::<Self>()`. Never stated.
//! - **The destructor** is installed if and only if `std::mem::needs_drop::<Self>()`,
//!   which is exact and `const`. A type that gains a `String` field gains a
//!   destructor with no edit here.
//! - **Blittability** is *computed*: `#[repr(C)]`, no destructor, and every
//!   field blittable. All three fold at compile time. A struct containing a
//!   `String` cannot claim to cross into a guest's linear memory however it is
//!   annotated, and `#[repr(Rust)]` field ordering is unspecified — so a type
//!   without `#[repr(C)]` is never blittable even if its fields all are.
//!
//! What the author *does* control is the path, because identity is a decision
//! rather than a fact — see the `path` attribute below.
//!
//! [`TypeInfo`]: https://docs.rs/slop-reflect

use proc_macro::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, LitStr, Meta, parse_macro_input};

/// Derive [`Reflect`] for a struct with named fields.
///
/// # Attributes
///
/// `#[reflect(path = "game::Inventory")]` overrides the canonical path, which
/// otherwise comes from `module_path!()` and the type's name.
///
/// The override exists because **moving a type between modules changes its
/// identity**, and identity is what every save file, scene file, and network
/// packet is written against. Refactoring is routine; silently invalidating
/// stored data is not. Pinning the path lets a type move without a migration.
///
/// # Panics
///
/// Compilation fails, with an explanation, for enums, unions, tuple structs,
/// unit structs, and generic types.
#[proc_macro_derive(Reflect, attributes(reflect))]
pub fn derive_reflect(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let name = &input.ident;

    // Generics would need the path to encode the monomorphization —
    // `game::Slot<u32>` and `game::Slot<f32>` are different types with different
    // layouts and must not share an id. That is a real design question about how
    // a guest module names an instantiation, and guessing at it now would be
    // designing against imagined requirements. Rejected loudly rather than
    // silently producing one id for every instantiation.
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "Reflect cannot yet be derived for a generic type: its path would have to encode \
             the type arguments, and two instantiations sharing one identity would alias in \
             the registry",
        ));
    }

    let fields = named_fields(input)?;
    let path = canonical_path(input, name)?;

    let field_infos = fields.iter().map(|field| {
        let ident = field.ident.as_ref().expect("named fields were checked");
        let ty = &field.ty;

        quote! {
            ::slop_reflect::FieldInfo::new(
                ::core::stringify!(#ident),
                ::core::mem::offset_of!(Self, #ident),
                <#ty as ::slop_reflect::Reflect>::type_id(),
            )
        }
    });

    // Every field's blittability, folded into one constant. An empty struct
    // yields `true`, which is correct: it has no bytes to misinterpret.
    let field_types: Vec<&syn::Type> = fields.iter().map(|field| &field.ty).collect();
    let fields_blittable = if field_types.is_empty() {
        quote!(true)
    } else {
        quote! {
            #( <#field_types as ::slop_reflect::Reflect>::TRANSFER.is_blittable() )&&*
        }
    };

    let repr_c = has_repr_c(input);

    Ok(quote! {
        // SAFETY: `layout` is taken directly from `Self`; the destructor is
        // installed exactly when `needs_drop::<Self>()` reports one and does
        // nothing but `drop_in_place` on a correctly typed pointer; and the
        // field offsets come from `offset_of!` on `Self`. None of the three is
        // supplied by the author, so none can disagree with the real type.
        unsafe impl ::slop_reflect::Reflect for #name {
            const PATH: &'static str = #path;

            const TRANSFER: ::slop_reflect::Transfer = {
                // `#[repr(Rust)]` leaves field order unspecified, so offsets
                // are not reproducible across compilations and mean nothing to
                // a separately compiled guest module. A destructor means the
                // bytes own something a raw copy would alias.
                if #repr_c
                    && !::core::mem::needs_drop::<#name>()
                    && #fields_blittable
                {
                    ::slop_reflect::Transfer::Blittable
                } else {
                    ::slop_reflect::Transfer::Owning
                }
            };

            fn type_info() -> ::slop_reflect::TypeInfo {
                let kind = ::slop_reflect::TypeKind::Struct {
                    fields: ::std::vec![ #(#field_infos),* ],
                };

                if ::core::mem::needs_drop::<Self>() {
                    // SAFETY: the closure casts to `Self`, which is exactly the
                    // type `Layout::new::<Self>()` describes, and does nothing
                    // but drop it in place.
                    unsafe {
                        ::slop_reflect::TypeInfo::with_drop(
                            Self::PATH,
                            ::core::alloc::Layout::new::<Self>(),
                            Self::TRANSFER,
                            kind,
                            |pointer| ::core::ptr::drop_in_place(pointer.cast::<Self>()),
                        )
                    }
                } else {
                    ::slop_reflect::TypeInfo::new(
                        Self::PATH,
                        ::core::alloc::Layout::new::<Self>(),
                        Self::TRANSFER,
                        kind,
                    )
                }
            }
        }
    })
}

/// The named fields of a struct, or an error naming what was found instead.
fn named_fields(input: &DeriveInput) -> syn::Result<Vec<&syn::Field>> {
    let Data::Struct(data) = &input.data else {
        // Enums need a variant model in `TypeKind` before they can be
        // described; unions have no safe field access at all.
        return Err(syn::Error::new(
            input.span(),
            "Reflect can only be derived for structs with named fields; enums are not yet \
             modelled in TypeKind and unions cannot be reflected safely",
        ));
    };

    match &data.fields {
        Fields::Named(named) => Ok(named.named.iter().collect()),
        // A tuple struct's fields have no names, and a name is what a property
        // panel and a serialized field key both need. Positional names could be
        // synthesized, but "0" and "1" are not stable under reordering, which
        // makes them a worse identity than no identity.
        Fields::Unnamed(_) => Err(syn::Error::new(
            data.fields.span(),
            "Reflect needs named fields: a tuple struct's positional keys are not stable \
             under reordering, so serialized data written against them cannot be migrated",
        )),
        Fields::Unit => Err(syn::Error::new(
            input.span(),
            "Reflect on a unit struct describes nothing; if it is a marker component, the ECS \
             can store it without reflection",
        )),
    }
}

/// The path expression: an explicit override, or `module_path!()` and the name.
fn canonical_path(input: &DeriveInput, name: &syn::Ident) -> syn::Result<proc_macro2::TokenStream> {
    let mut explicit: Option<LitStr> = None;

    for attribute in &input.attrs {
        if !attribute.path().is_ident("reflect") {
            continue;
        }

        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("path") {
                explicit = Some(meta.value()?.parse()?);
                return Ok(());
            }

            Err(meta.error("unrecognized reflect attribute; expected `path = \"...\"`"))
        })?;
    }

    Ok(match explicit {
        Some(literal) => quote!(#literal),
        // Resolved where the derive is used, so it names the module the type is
        // actually declared in rather than this crate.
        None => quote!(::core::concat!(
            ::core::module_path!(),
            "::",
            ::core::stringify!(#name)
        )),
    })
}

/// Whether the type is laid out predictably enough to be blittable.
///
/// `#[repr(C)]` and `#[repr(transparent)]` both give a defined field order.
/// `#[repr(packed)]` deliberately does not count on its own: it changes
/// alignment in ways a guest's own struct definition would have to match
/// exactly, and silently disagreeing is worse than not being blittable.
fn has_repr_c(input: &DeriveInput) -> bool {
    input.attrs.iter().any(|attribute| {
        if !attribute.path().is_ident("repr") {
            return false;
        }

        let Meta::List(list) = &attribute.meta else {
            return false;
        };

        let mut found = false;
        // `parse_nested_meta` errors on reprs it cannot parse as paths; a
        // failure simply means "not C", which is the safe answer.
        let _ = list.parse_nested_meta(|meta| {
            if meta.path.is_ident("C") || meta.path.is_ident("transparent") {
                found = true;
            }

            Ok(())
        });

        found
    })
}
