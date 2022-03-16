use proc_macro::TokenStream;
use quote::quote;
use syn::spanned::Spanned;

pub fn derive_enum(input: TokenStream) -> TokenStream {
    let ast = syn::parse_macro_input!(input as syn::DeriveInput);

    let struct_name = &ast.ident;
    let variants = match ast.data {
        // syn::Data::Struct(syn::DataStruct {
        //     fields: syn::Fields::Named(named_fields),
        //     ..
        // }) => named_fields,
        syn::Data::Enum(syn::DataEnum { variants, .. }) => variants,
        _ => {
            return syn::Error::new(ast.span(), "Deriving Enum is only supported on enums")
                .to_compile_error()
                .into()
        }
    };

    // The `Enum` trait is super simple: variant names are mapped to their index in the declaration
    // order, and the names are either just the variant name or a `#[name = "..."]` attribute in
    // case the name should contain a space.
    let mut variant_names = Vec::new();
    let mut to_index_tokens = Vec::new();
    let mut from_index_tokens = Vec::new();
    for (variant_idx, variant) in variants.iter().enumerate() {
        if !variant.fields.is_empty() {
            return syn::Error::new(variant.span(), "Variants cannot have any fields")
                .to_compile_error()
                .into();
        }

        let mut name_attr: Option<String> = None;
        for attr in &variant.attrs {
            if attr.path.is_ident("name") {
                match attr.parse_meta() {
                    Ok(syn::Meta::NameValue(syn::MetaNameValue {
                        lit: syn::Lit::Str(s),
                        ..
                    })) => {
                        if name_attr.is_none() {
                            name_attr = Some(s.value());
                        } else {
                            return syn::Error::new(attr.span(), "Duplicate name attribute")
                                .to_compile_error()
                                .into();
                        }
                    }
                    _ => {
                        return syn::Error::new(
                            attr.span(),
                            "The name attribute should be a key-value pair with a string argument: #[name = \"foo bar\"]",
                        )
                        .to_compile_error()
                        .into()
                    }
                };
            }
        }

        match name_attr {
            Some(name) => variant_names.push(name),
            None => variant_names.push(variant.ident.to_string()),
        }

        let variant_ident = &variant.ident;
        to_index_tokens.push(quote! { #struct_name::#variant_ident => #variant_idx, });
        from_index_tokens.push(quote! { #variant_idx => #struct_name::#variant_ident, });
    }

    let from_index_default_tokens = variants.first().map(|v| {
        let variant_ident = &v.ident;
        quote! { _ => #struct_name::#variant_ident, }
    });

    quote! {
        impl Enum for #struct_name {
            fn variants() -> &'static [&'static str] {
                &[#(#variant_names),*]
            }

            fn to_index(self) -> usize {
                match self {
                    #(#to_index_tokens)*
                }
            }

            fn from_index(index: usize) -> Self {
                match index {
                    #(#from_index_tokens)*
                    #from_index_default_tokens
                }
            }
        }
    }
    .into()
}
