#![recursion_limit = "256"]
// Copyright (c) 2020 Google LLC All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use parse_attrs::has_argh_attrs;
use syn::ext::IdentExt as _;

/// Implementation of the `FromArgs` and `argh(...)` derive attributes.
///
/// For more thorough documentation, see the `argh` crate itself.
extern crate proc_macro;

use {
    crate::{
        errors::Errors,
        parse_attrs::{check_long_name, FieldAttrs, FieldKind, TypeAttrs},
    },
    proc_macro2::{Span, TokenStream},
    quote::{quote, quote_spanned, ToTokens},
    std::{collections::HashMap, str::FromStr},
    syn::{spanned::Spanned, GenericArgument, LitStr, PathArguments, Type},
};

mod args_info;
mod errors;
mod help;
mod parse_attrs;

/// Entrypoint for `#[derive(FromArgs)]`.
#[proc_macro_derive(FromArgs, attributes(argh))]
pub fn argh_derive(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let ast = syn::parse_macro_input!(input as syn::DeriveInput);
    let gen = impl_from_args(&ast);
    gen.into()
}

/// Entrypoint for `#[derive(FromArgValue)]`.
#[proc_macro_derive(FromArgValue, attributes(argh))]
pub fn argh_value_derive(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let ast = syn::parse_macro_input!(input as syn::DeriveInput);
    let gen = impl_from_arg_value(&ast);
    gen.into()
}

/// Entrypoint for `#[derive(ArgsInfo)]`.
#[proc_macro_derive(ArgsInfo, attributes(argh))]
pub fn args_info_derive(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let ast = syn::parse_macro_input!(input as syn::DeriveInput);
    let gen = args_info::impl_args_info(&ast);
    gen.into()
}

/// Transform the input into a token stream containing any generated implementations,
/// as well as all errors that occurred.
fn impl_from_args(input: &syn::DeriveInput) -> TokenStream {
    let errors = &Errors::default();
    let type_attrs = &TypeAttrs::parse(errors, input);
    let mut output_tokens = match &input.data {
        syn::Data::Struct(ds) => {
            impl_from_args_struct(errors, &input.ident, type_attrs, &input.generics, ds)
        }
        syn::Data::Enum(de) => {
            impl_from_args_enum(errors, &input.ident, type_attrs, &input.generics, de)
        }
        syn::Data::Union(_) => {
            errors.err(input, "`#[derive(FromArgs)]` cannot be applied to unions");
            TokenStream::new()
        }
    };
    errors.to_tokens(&mut output_tokens);
    output_tokens
}

fn impl_from_arg_value(input: &syn::DeriveInput) -> TokenStream {
    let errors = &Errors::default();
    let mut output_tokens = match &input.data {
        syn::Data::Enum(de) => impl_from_arg_value_enum(errors, &input.ident, &input.generics, de),
        _ => {
            errors.err(input, "`#[derive(FromArgValue)]` can only be applied to `enum`s");
            TokenStream::new()
        }
    };
    if has_argh_attrs(&input.attrs) {
        errors.err(
            &input.ident,
            "`#[derive(FromArgValue)]` `enum`s do not support `#[argh(...)]` attributes",
        );
    }
    errors.to_tokens(&mut output_tokens);
    output_tokens
}

/// The kind of optionality a parameter has.
enum Optionality {
    None,
    Defaulted(TokenStream),
    Optional,
    Repeating,
    DefaultedRepeating(TokenStream),
}

impl PartialEq<Optionality> for Optionality {
    fn eq(&self, other: &Optionality) -> bool {
        use Optionality::*;
        // NB: (Defaulted, Defaulted) can't contain the same token streams
        matches!((self, other), (Optional, Optional) | (Repeating, Repeating))
    }
}

impl Optionality {
    /// Whether or not this is `Optionality::None`
    fn is_required(&self) -> bool {
        matches!(self, Optionality::None)
    }
}

/// A field of a `#![derive(FromArgs)]` struct with attributes and some other
/// notable metadata appended.
struct StructField<'a> {
    /// The original parsed field
    field: &'a syn::Field,
    /// The parsed attributes of the field
    attrs: FieldAttrs,
    /// The field name. This is contained optionally inside `field`,
    /// but is duplicated non-optionally here to indicate that all field that
    /// have reached this point must have a field name, and it no longer
    /// needs to be unwrapped.
    name: &'a syn::Ident,
    /// Similar to `name` above, this is contained optionally inside `FieldAttrs`,
    /// but here is fully present to indicate that we only have to consider fields
    /// with a valid `kind` at this point.
    kind: FieldKind,
    // If `field.ty` is `Vec<T>` or `Option<T>`, this is `T`, otherwise it's `&field.ty`.
    // This is used to enable consistent parsing code between optional and non-optional
    // keyed and subcommand fields.
    ty_without_wrapper: &'a syn::Type,
    // Whether the field represents an optional value, such as an `Option` subcommand field
    // or an `Option` or `Vec` keyed argument, or if it has a `default`.
    optionality: Optionality,
    // The `--`-prefixed name of the option, if one exists.
    long_name: Option<String>,
}

impl<'a> StructField<'a> {
    /// Attempts to parse a field of a `#[derive(FromArgs)]` struct, pulling out the
    /// fields required for code generation.
    fn new(errors: &Errors, field: &'a syn::Field, attrs: FieldAttrs) -> Option<Self> {
        let name = field.ident.as_ref().expect("missing ident for named field");

        // Ensure that one "kind" is present (switch, option, subcommand, positional)
        let kind = if let Some(field_type) = &attrs.field_type {
            field_type.kind
        } else {
            errors.err(
                field,
                concat!(
                    "Missing `argh` field kind attribute.\n",
                    "Expected one of: `switch`, `option`, `remaining`, `subcommand`, `positional`",
                ),
            );
            return None;
        };

        // Parse out whether a field is optional (`Option` or `Vec`).
        let optionality;
        let ty_without_wrapper;
        match kind {
            FieldKind::Switch => {
                if !ty_expect_switch(errors, &field.ty) {
                    return None;
                }
                optionality = Optionality::Optional;
                ty_without_wrapper = &field.ty;
            }
            FieldKind::Option | FieldKind::Positional => {
                if let Some(default) = &attrs.default {
                    let tokens = match TokenStream::from_str(&default.value()) {
                        Ok(tokens) => tokens,
                        Err(_) => {
                            errors.err(&default, "Invalid tokens: unable to lex `default` value");
                            return None;
                        }
                    };
                    // Set the span of the generated tokens to the string literal
                    let tokens: TokenStream = tokens
                        .into_iter()
                        .map(|mut tree| {
                            tree.set_span(default.span());
                            tree
                        })
                        .collect();
                    let inner = if let Some(x) = ty_inner(&["Vec"], &field.ty) {
                        optionality = Optionality::DefaultedRepeating(tokens);
                        x
                    } else {
                        optionality = Optionality::Defaulted(tokens);
                        &field.ty
                    };
                    ty_without_wrapper = inner;
                } else {
                    let mut inner = None;
                    optionality = if let Some(x) = ty_inner(&["Option"], &field.ty) {
                        inner = Some(x);
                        Optionality::Optional
                    } else if let Some(x) = ty_inner(&["Vec"], &field.ty) {
                        inner = Some(x);
                        Optionality::Repeating
                    } else {
                        Optionality::None
                    };
                    ty_without_wrapper = inner.unwrap_or(&field.ty);
                }
            }
            FieldKind::SubCommand => {
                let inner = ty_inner(&["Option"], &field.ty);
                optionality =
                    if inner.is_some() { Optionality::Optional } else { Optionality::None };
                ty_without_wrapper = inner.unwrap_or(&field.ty);
            }
        }

        // Determine the "long" name of options and switches.
        // Defaults to the kebab-case'd field name if `#[argh(long = "...")]` is omitted.
        let long_name = match kind {
            FieldKind::Switch | FieldKind::Option => {
                let long_name = attrs.long.as_ref().map(syn::LitStr::value).unwrap_or_else(|| {
                    let kebab_name = to_kebab_case(&name.unraw().to_string());
                    check_long_name(errors, name, &kebab_name);
                    kebab_name
                });
                if long_name == "help" {
                    errors.err(field, "Custom `--help` flags are not supported.");
                }
                let long_name = format!("--{}", long_name);
                Some(long_name)
            }
            FieldKind::SubCommand | FieldKind::Positional => None,
        };

        Some(StructField { field, attrs, kind, optionality, ty_without_wrapper, name, long_name })
    }

    pub(crate) fn positional_arg_name(&self) -> String {
        self.attrs
            .arg_name
            .as_ref()
            .map(LitStr::value)
            .unwrap_or_else(|| self.name.to_string().trim_matches('_').to_owned())
    }
}

fn to_kebab_case(s: &str) -> String {
    let words = s.split('_').filter(|word| !word.is_empty());
    let mut res = String::with_capacity(s.len());
    for word in words {
        if !res.is_empty() {
            res.push('-')
        }
        res.push_str(word)
    }
    res
}

#[test]
fn test_kebabs() {
    #[track_caller]
    fn check(s: &str, want: &str) {
        let got = to_kebab_case(s);
        assert_eq!(got.as_str(), want)
    }
    check("", "");
    check("_", "");
    check("foo", "foo");
    check("__foo_", "foo");
    check("foo_bar", "foo-bar");
    check("foo__Bar", "foo-Bar");
    check("foo_bar__baz_", "foo-bar-baz");
}

/// Implements `FromArgs` and `TopLevelCommand` or `SubCommand` for a `#[derive(FromArgs)]` struct.
fn impl_from_args_struct(
    errors: &Errors,
    name: &syn::Ident,
    type_attrs: &TypeAttrs,
    generic_args: &syn::Generics,
    ds: &syn::DataStruct,
) -> TokenStream {
    let fields = match &ds.fields {
        syn::Fields::Named(fields) => fields,
        syn::Fields::Unnamed(_) => {
            errors.err(
                &ds.struct_token,
                "`#![derive(FromArgs)]` is not currently supported on tuple structs",
            );
            return TokenStream::new();
        }
        syn::Fields::Unit => {
            errors.err(&ds.struct_token, "#![derive(FromArgs)]` cannot be applied to unit structs");
            return TokenStream::new();
        }
    };

    let fields: Vec<_> = fields
        .named
        .iter()
        .filter_map(|field| {
            let attrs = FieldAttrs::parse(errors, field);
            StructField::new(errors, field, attrs)
        })
        .collect();

    ensure_unique_names(errors, &fields);
    ensure_only_last_positional_is_optional(errors, &fields);

    let impl_span = Span::call_site();

    let from_args_method = impl_from_args_struct_from_args(errors, type_attrs, &fields);

    let redact_arg_values_method =
        impl_from_args_struct_redact_arg_values(errors, type_attrs, &fields);

    let top_or_sub_cmd_impl = top_or_sub_cmd_impl(errors, name, type_attrs, generic_args);

    let (impl_generics, ty_generics, where_clause) = generic_args.split_for_impl();
    let trait_impl = quote_spanned! { impl_span =>
        #[automatically_derived]
        impl #impl_generics argh::FromArgs for #name #ty_generics #where_clause {
            #from_args_method

            #redact_arg_values_method
        }

        #top_or_sub_cmd_impl
    };

    trait_impl
}

fn impl_from_args_struct_from_args<'a>(
    errors: &Errors,
    type_attrs: &TypeAttrs,
    fields: &'a [StructField<'a>],
) -> TokenStream {
    let init_fields = declare_local_storage_for_from_args_fields(fields);
    let unwrap_fields = unwrap_from_args_fields(fields);
    let positional_fields: Vec<&StructField<'_>> =
        fields.iter().filter(|field| field.kind == FieldKind::Positional).collect();
    let positional_field_idents = positional_fields.iter().map(|field| &field.field.ident);
    let positional_field_names = positional_fields.iter().map(|field| field.name.to_string());
    let last_positional_is_repeating = positional_fields
        .last()
        .map(|field| field.optionality == Optionality::Repeating)
        .unwrap_or(false);
    let last_positional_is_greedy = positional_fields
        .last()
        .map(|field| field.kind == FieldKind::Positional && field.attrs.greedy.is_some())
        .unwrap_or(false);

    let flag_output_table = fields.iter().filter_map(|field| {
        let field_name = &field.field.ident;
        match field.kind {
            FieldKind::Option => Some(quote! { argh::ParseStructOption::Value(&mut #field_name) }),
            FieldKind::Switch => Some(quote! { argh::ParseStructOption::Flag(&mut #field_name) }),
            FieldKind::SubCommand | FieldKind::Positional => None,
        }
    });

    let flag_str_to_output_table_map = flag_str_to_output_table_map_entries(fields);

    let mut subcommands_iter =
        fields.iter().filter(|field| field.kind == FieldKind::SubCommand).fuse();

    let subcommand: Option<&StructField<'_>> = subcommands_iter.next();
    for dup_subcommand in subcommands_iter {
        errors.duplicate_attrs("subcommand", subcommand.unwrap().field, dup_subcommand.field);
    }

    let impl_span = Span::call_site();

    let missing_requirements_ident = syn::Ident::new("__missing_requirements", impl_span);

    let append_missing_requirements =
        append_missing_requirements(&missing_requirements_ident, fields);

    let parse_subcommands = if let Some(subcommand) = subcommand {
        let name = subcommand.name;
        let ty = subcommand.ty_without_wrapper;
        quote_spanned! { impl_span =>
            Some(argh::ParseStructSubCommand {
                subcommands: <#ty as argh::SubCommands>::COMMANDS,
                dynamic_subcommands: &<#ty as argh::SubCommands>::dynamic_commands(),
                parse_func: &mut |__command, __remaining_args| {
                    #name = Some(<#ty as argh::FromArgs>::from_args(__command, __remaining_args)?);
                    ::core::result::Result::Ok(())
                },
            })
        }
    } else {
        quote_spanned! { impl_span => None }
    };

    let help_triggers = get_help_triggers(type_attrs);

    let help = if cfg!(feature = "help") {
        // Identifier referring to a value containing the name of the current command as an `&[&str]`.
        let cmd_name_str_array_ident = syn::Ident::new("__cmd_name", impl_span);
        help::help(errors, cmd_name_str_array_ident, type_attrs, fields, subcommand, &help_triggers)
    } else {
        quote! { String::new() }
    };

    let method_impl = quote_spanned! { impl_span =>
        fn from_args(__cmd_name: &[&str], __args: &[&str])
            -> ::core::result::Result<Self, argh::EarlyExit>
        {
            #![allow(clippy::unwrap_in_result)]

            #( #init_fields )*

            argh::parse_struct_args(
                __cmd_name,
                __args,
                argh::ParseStructOptions {
                    arg_to_slot: &[ #( #flag_str_to_output_table_map ,)* ],
                    slots: &mut [ #( #flag_output_table, )* ],
                    help_triggers: &[ #( #help_triggers ),* ],
                },
                argh::ParseStructPositionals {
                    positionals: &mut [
                        #(
                            argh::ParseStructPositional {
                                name: #positional_field_names,
                                slot: &mut #positional_field_idents as &mut argh::ParseValueSlot,
                            },
                        )*
                    ],
                    last_is_repeating: #last_positional_is_repeating,
                    last_is_greedy: #last_positional_is_greedy,
                },
                #parse_subcommands,
                &|| #help,
            )?;

            let mut #missing_requirements_ident = argh::MissingRequirements::default();
            #(
                #append_missing_requirements
            )*
            #missing_requirements_ident.err_on_any()?;

            ::core::result::Result::Ok(Self {
                #( #unwrap_fields, )*
            })
        }
    };

    method_impl
}

/// get help triggers vector from type_attrs.help_triggers as a [`Vec<String>`]
///
/// Defaults to vec!["--help", "help"] if type_attrs.help_triggers is None
fn get_help_triggers(type_attrs: &TypeAttrs) -> Vec<String> {
    let help_triggers = type_attrs.help_triggers.as_ref().map_or_else(
        || vec!["--help".to_owned(), "help".to_owned()],
        |s| {
            s.iter()
                .filter_map(|s| {
                    let trigger = s.value();
                    let trigger_trimmed = trigger.trim().to_owned();
                    if trigger_trimmed.is_empty() {
                        None
                    } else {
                        Some(trigger_trimmed)
                    }
                })
                .collect::<Vec<_>>()
        },
    );
    help_triggers
}

fn impl_from_args_struct_redact_arg_values<'a>(
    errors: &Errors,
    type_attrs: &TypeAttrs,
    fields: &'a [StructField<'a>],
) -> TokenStream {
    let init_fields = declare_local_storage_for_redacted_fields(fields);
    let unwrap_fields = unwrap_redacted_fields(fields);

    let positional_fields: Vec<&StructField<'_>> =
        fields.iter().filter(|field| field.kind == FieldKind::Positional).collect();
    let positional_field_idents = positional_fields.iter().map(|field| &field.field.ident);
    let positional_field_names = positional_fields.iter().map(|field| field.name.to_string());
    let last_positional_is_repeating = positional_fields
        .last()
        .map(|field| field.optionality == Optionality::Repeating)
        .unwrap_or(false);
    let last_positional_is_greedy = positional_fields
        .last()
        .map(|field| field.kind == FieldKind::Positional && field.attrs.greedy.is_some())
        .unwrap_or(false);

    let flag_output_table = fields.iter().filter_map(|field| {
        let field_name = &field.field.ident;
        match field.kind {
            FieldKind::Option => Some(quote! { argh::ParseStructOption::Value(&mut #field_name) }),
            FieldKind::Switch => Some(quote! { argh::ParseStructOption::Flag(&mut #field_name) }),
            FieldKind::SubCommand | FieldKind::Positional => None,
        }
    });

    let flag_str_to_output_table_map = flag_str_to_output_table_map_entries(fields);

    let mut subcommands_iter =
        fields.iter().filter(|field| field.kind == FieldKind::SubCommand).fuse();

    let subcommand: Option<&StructField<'_>> = subcommands_iter.next();
    for dup_subcommand in subcommands_iter {
        errors.duplicate_attrs("subcommand", subcommand.unwrap().field, dup_subcommand.field);
    }

    let impl_span = Span::call_site();

    let missing_requirements_ident = syn::Ident::new("__missing_requirements", impl_span);

    let append_missing_requirements =
        append_missing_requirements(&missing_requirements_ident, fields);

    let redact_subcommands = if let Some(subcommand) = subcommand {
        let name = subcommand.name;
        let ty = subcommand.ty_without_wrapper;
        quote_spanned! { impl_span =>
            Some(argh::ParseStructSubCommand {
                subcommands: <#ty as argh::SubCommands>::COMMANDS,
                dynamic_subcommands: &<#ty as argh::SubCommands>::dynamic_commands(),
                parse_func: &mut |__command, __remaining_args| {
                    #name = Some(<#ty as argh::FromArgs>::redact_arg_values(__command, __remaining_args)?);
                    ::core::result::Result::Ok(())
                },
            })
        }
    } else {
        quote_spanned! { impl_span => None }
    };

    let unwrap_cmd_name_err_string = if type_attrs.is_subcommand.is_none() {
        quote! { "no command name" }
    } else {
        quote! { "no subcommand name" }
    };

    let help_triggers = get_help_triggers(type_attrs);

    let help = if cfg!(feature = "help") {
        // Identifier referring to a value containing the name of the current command as an `&[&str]`.
        let cmd_name_str_array_ident = syn::Ident::new("__cmd_name", impl_span);
        help::help(errors, cmd_name_str_array_ident, type_attrs, fields, subcommand, &help_triggers)
    } else {
        quote! { String::new() }
    };

    let method_impl = quote_spanned! { impl_span =>
        fn redact_arg_values(__cmd_name: &[&str], __args: &[&str]) -> std::result::Result<Vec<String>, argh::EarlyExit> {
            #( #init_fields )*

            argh::parse_struct_args(
                __cmd_name,
                __args,
                argh::ParseStructOptions {
                    arg_to_slot: &[ #( #flag_str_to_output_table_map ,)* ],
                    slots: &mut [ #( #flag_output_table, )* ],
                    help_triggers: &[ #( #help_triggers ),* ],
                },
                argh::ParseStructPositionals {
                    positionals: &mut [
                        #(
                            argh::ParseStructPositional {
                                name: #positional_field_names,
                                slot: &mut #positional_field_idents as &mut argh::ParseValueSlot,
                            },
                        )*
                    ],
                    last_is_repeating: #last_positional_is_repeating,
                    last_is_greedy: #last_positional_is_greedy,
                },
                #redact_subcommands,
                &|| #help,
            )?;

            let mut #missing_requirements_ident = argh::MissingRequirements::default();
            #(
                #append_missing_requirements
            )*
            #missing_requirements_ident.err_on_any()?;

            let mut __redacted = vec![
                if let Some(cmd_name) = __cmd_name.last() {
                    (*cmd_name).to_owned()
                } else {
                    return ::core::result::Result::Err(argh::EarlyExit::from(#unwrap_cmd_name_err_string.to_owned()));
                }
            ];

            #( #unwrap_fields )*

            ::core::result::Result::Ok(__redacted)
        }
    };

    method_impl
}

/// Ensures that only the last positional arg is non-required.
fn ensure_only_last_positional_is_optional(errors: &Errors, fields: &[StructField<'_>]) {
    let mut first_non_required_span = None;
    for field in fields {
        if field.kind == FieldKind::Positional {
            if let Some(first) = first_non_required_span {
                errors.err_span(
                    first,
                    "Only the last positional argument may be `Option`, `Vec`, or defaulted.",
                );
                errors.err(&field.field, "Later positional argument declared here.");
                return;
            }
            if !field.optionality.is_required() {
                first_non_required_span = Some(field.field.span());
            }
        }
    }
}

/// Ensures that only one short or long name is used.
fn ensure_unique_names(errors: &Errors, fields: &[StructField<'_>]) {
    let mut seen_short_names = HashMap::new();
    let mut seen_long_names = HashMap::new();

    for field in fields {
        if let Some(short_name) = &field.attrs.short {
            let short_name = short_name.value();
            if let Some(first_use_field) = seen_short_names.get(&short_name) {
                errors.err_span_tokens(
                    first_use_field,
                    &format!("The short name of \"-{}\" was already used here.", short_name),
                );
                errors.err_span_tokens(field.field, "Later usage here.");
            }

            seen_short_names.insert(short_name, &field.field);
        }

        if let Some(long_name) = &field.long_name {
            if let Some(first_use_field) = seen_long_names.get(&long_name) {
                errors.err_span_tokens(
                    *first_use_field,
                    &format!("The long name of \"{}\" was already used here.", long_name),
                );
                errors.err_span_tokens(field.field, "Later usage here.");
            }

            seen_long_names.insert(long_name, field.field);
        }
    }
}

/// Implement `argh::TopLevelCommand` or `argh::SubCommand` as appropriate.
fn top_or_sub_cmd_impl(
    errors: &Errors,
    name: &syn::Ident,
    type_attrs: &TypeAttrs,
    generic_args: &syn::Generics,
) -> TokenStream {
    let description = if cfg!(feature = "help") {
        help::require_description(errors, name.span(), &type_attrs.description, "type")
    } else {
        String::new()
    };
    let (impl_generics, ty_generics, where_clause) = generic_args.split_for_impl();
    if type_attrs.is_subcommand.is_none() {
        // Not a subcommand
        quote! {
            #[automatically_derived]
            impl #impl_generics argh::TopLevelCommand for #name #ty_generics #where_clause {}
        }
    } else {
        let empty_str = syn::LitStr::new("", Span::call_site());
        let subcommand_name = type_attrs.name.as_ref().unwrap_or_else(|| {
            errors.err(name, "`#[argh(name = \"...\")]` attribute is required for subcommands");
            &empty_str
        });
        quote! {
            #[automatically_derived]
            impl #impl_generics argh::SubCommand for #name #ty_generics #where_clause {
                const COMMAND: &'static argh::CommandInfo = &argh::CommandInfo {
                    name: #subcommand_name,
                    description: #description,
                };
            }
        }
    }
}

/// Declare a local slots to store each field in during parsing.
///
/// Most fields are stored in `Option<FieldType>` locals.
/// `argh(option)` fields are stored in a `ParseValueSlotTy` along with a
/// function that knows how to decode the appropriate value.
fn declare_local_storage_for_from_args_fields<'a>(
    fields: &'a [StructField<'a>],
) -> impl Iterator<Item = TokenStream> + 'a {
    fields.iter().map(|field| {
        let field_name = &field.field.ident;
        let field_type = &field.ty_without_wrapper;

        // Wrap field types in `Option` if they aren't already `Option` or `Vec`-wrapped.
        let field_slot_type = match field.optionality {
            Optionality::Optional | Optionality::Repeating => (&field.field.ty).into_token_stream(),
            Optionality::None | Optionality::Defaulted(_) => {
                quote! { std::option::Option<#field_type> }
            }
            Optionality::DefaultedRepeating(_) => {
                quote! { std::option::Option<std::vec::Vec<#field_type>> }
            }
        };

        match field.kind {
            FieldKind::Option | FieldKind::Positional => {
                let from_str_fn = match &field.attrs.from_str_fn {
                    Some(from_str_fn) => from_str_fn.into_token_stream(),
                    None => {
                        quote! {
                            <#field_type as argh::FromArgValue>::from_arg_value
                        }
                    }
                };

                quote! {
                    let mut #field_name: argh::ParseValueSlotTy<#field_slot_type, #field_type>
                        = argh::ParseValueSlotTy {
                            slot: std::default::Default::default(),
                            parse_func: |_, value| { #from_str_fn(value) },
                        };
                }
            }
            FieldKind::SubCommand => {
                quote! { let mut #field_name: #field_slot_type = None; }
            }
            FieldKind::Switch => {
                quote! { let mut #field_name: #field_slot_type = argh::Flag::default(); }
            }
        }
    })
}

/// Unwrap non-optional fields and take options out of their tuple slots.
fn unwrap_from_args_fields<'a>(
    fields: &'a [StructField<'a>],
) -> impl Iterator<Item = TokenStream> + 'a {
    fields.iter().map(|field| {
        let field_name = field.name;
        match field.kind {
            FieldKind::Option | FieldKind::Positional => match &field.optionality {
                Optionality::None => quote! {
                    #field_name: #field_name.slot.unwrap()
                },
                Optionality::Optional | Optionality::Repeating => {
                    quote! { #field_name: #field_name.slot }
                }
                Optionality::Defaulted(tokens) | Optionality::DefaultedRepeating(tokens) => {
                    quote! {
                        #field_name: #field_name.slot.unwrap_or_else(|| #tokens)
                    }
                }
            },
            FieldKind::Switch => field_name.into_token_stream(),
            FieldKind::SubCommand => match field.optionality {
                Optionality::None => quote! { #field_name: #field_name.unwrap() },
                Optionality::Optional | Optionality::Repeating => field_name.into_token_stream(),
                Optionality::Defaulted(_) | Optionality::DefaultedRepeating(_) => unreachable!(),
            },
        }
    })
}

/// Declare a local slots to store each field in during parsing.
///
/// Most fields are stored in `Option<FieldType>` locals.
/// `argh(option)` fields are stored in a `ParseValueSlotTy` along with a
/// function that knows how to decode the appropriate value.
fn declare_local_storage_for_redacted_fields<'a>(
    fields: &'a [StructField<'a>],
) -> impl Iterator<Item = TokenStream> + 'a {
    fields.iter().map(|field| {
        let field_name = &field.field.ident;

        match field.kind {
            FieldKind::Switch => {
                quote! {
                    let mut #field_name = argh::RedactFlag {
                        slot: None,
                    };
                }
            }
            FieldKind::Option => {
                let field_slot_type = match field.optionality {
                    Optionality::Repeating => {
                        quote! { std::vec::Vec<String> }
                    }
                    Optionality::DefaultedRepeating(_) => {
                        quote! { std::option::Option<std::vec::Vec<String>> }
                    }
                    Optionality::None | Optionality::Optional | Optionality::Defaulted(_) => {
                        quote! { std::option::Option<String> }
                    }
                };

                quote! {
                    let mut #field_name: argh::ParseValueSlotTy::<#field_slot_type, String> =
                        argh::ParseValueSlotTy {
                        slot: std::default::Default::default(),
                        parse_func: |arg, _| { ::core::result::Result::Ok(arg.to_owned()) },
                    };
                }
            }
            FieldKind::Positional => {
                let field_slot_type = match field.optionality {
                    Optionality::Repeating => {
                        quote! { std::vec::Vec<String> }
                    }
                    Optionality::DefaultedRepeating(_) => {
                        quote! { std::option::Option<std::vec::Vec<String>> }
                    }
                    Optionality::None | Optionality::Optional | Optionality::Defaulted(_) => {
                        quote! { std::option::Option<String> }
                    }
                };

                let arg_name = field.positional_arg_name();
                quote! {
                    let mut #field_name: argh::ParseValueSlotTy::<#field_slot_type, String> =
                        argh::ParseValueSlotTy {
                        slot: std::default::Default::default(),
                        parse_func: |_, _| { ::core::result::Result::Ok(#arg_name.to_owned()) },
                    };
                }
            }
            FieldKind::SubCommand => {
                quote! { let mut #field_name: std::option::Option<std::vec::Vec<String>> = None; }
            }
        }
    })
}

/// Unwrap non-optional fields and take options out of their tuple slots.
fn unwrap_redacted_fields<'a>(
    fields: &'a [StructField<'a>],
) -> impl Iterator<Item = TokenStream> + 'a {
    fields.iter().map(|field| {
        let field_name = field.name;

        match field.kind {
            FieldKind::Switch => {
                quote! {
                    if let Some(__field_name) = #field_name.slot {
                        __redacted.push(__field_name);
                    }
                }
            }
            FieldKind::Option => match field.optionality {
                Optionality::Repeating => {
                    quote! {
                        __redacted.extend(#field_name.slot.into_iter());
                    }
                }
                Optionality::DefaultedRepeating(_) => {
                    quote! {
                        if let Some(__field_name) = #field_name.slot {
                            __redacted.extend(__field_name.into_iter());
                        }
                    }
                }
                Optionality::None | Optionality::Optional | Optionality::Defaulted(_) => {
                    quote! {
                        if let Some(__field_name) = #field_name.slot {
                            __redacted.push(__field_name);
                        }
                    }
                }
            },
            FieldKind::Positional => {
                quote! {
                    __redacted.extend(#field_name.slot.into_iter());
                }
            }
            FieldKind::SubCommand => {
                quote! {
                    if let Some(__subcommand_args) = #field_name {
                        __redacted.extend(__subcommand_args.into_iter());
                    }
                }
            }
        }
    })
}

/// Entries of tokens like `("--some-flag-key", 5)` that map from a flag key string
/// to an index in the output table.
fn flag_str_to_output_table_map_entries<'a>(fields: &'a [StructField<'a>]) -> Vec<TokenStream> {
    let mut flag_str_to_output_table_map = vec![];
    for (i, (field, long_name)) in fields
        .iter()
        .filter_map(|field| field.long_name.as_ref().map(|long_name| (field, long_name)))
        .enumerate()
    {
        if let Some(short) = &field.attrs.short {
            let short = format!("-{}", short.value());
            flag_str_to_output_table_map.push(quote! { (#short, #i) });
        }

        flag_str_to_output_table_map.push(quote! { (#long_name, #i) });
    }
    flag_str_to_output_table_map
}

/// For each non-optional field, add an entry to the `argh::MissingRequirements`.
fn append_missing_requirements<'a>(
    // missing_requirements_ident
    mri: &syn::Ident,
    fields: &'a [StructField<'a>],
) -> impl Iterator<Item = TokenStream> + 'a {
    let mri = mri.clone();
    fields.iter().filter(|f| f.optionality.is_required()).map(move |field| {
        let field_name = field.name;
        match field.kind {
            FieldKind::Switch => unreachable!("switches are always optional"),
            FieldKind::Positional => {
                let name = field.positional_arg_name();
                quote! {
                    if #field_name.slot.is_none() {
                        #mri.missing_positional_arg(#name)
                    }
                }
            }
            FieldKind::Option => {
                let name = field.long_name.as_ref().expect("options always have a long name");
                quote! {
                    if #field_name.slot.is_none() {
                        #mri.missing_option(#name)
                    }
                }
            }
            FieldKind::SubCommand => {
                let ty = field.ty_without_wrapper;
                quote! {
                    if #field_name.is_none() {
                        #mri.missing_subcommands(
                            <#ty as argh::SubCommands>::COMMANDS
                                .iter()
                                .cloned()
                                .chain(
                                    <#ty as argh::SubCommands>::dynamic_commands()
                                        .iter()
                                        .copied()
                                ),
                        )
                    }
                }
            }
        }
    })
}

/// Require that a type can be a `switch`.
/// Throws an error for all types except booleans and integers
fn ty_expect_switch(errors: &Errors, ty: &syn::Type) -> bool {
    fn ty_can_be_switch(ty: &syn::Type) -> bool {
        if let syn::Type::Path(path) = ty {
            if path.qself.is_some() {
                return false;
            }
            if path.path.segments.len() != 1 {
                return false;
            }
            let ident = &path.path.segments[0].ident;
            // `Option<bool>` can be used as a `switch`.
            if ident == "Option" {
                if let PathArguments::AngleBracketed(args) = &path.path.segments[0].arguments {
                    if let GenericArgument::Type(Type::Path(p)) = &args.args[0] {
                        if p.path.segments[0].ident == "bool" {
                            return true;
                        }
                    }
                }
            }
            ["bool", "u8", "u16", "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128"]
                .iter()
                .any(|path| ident == path)
        } else {
            false
        }
    }

    let res = ty_can_be_switch(ty);
    if !res {
        errors.err(ty, "switches must be of type `bool`, `Option<bool>`, or integer type");
    }
    res
}

/// Returns `Some(T)` if a type is `wrapper_name<T>` for any `wrapper_name` in `wrapper_names`.
fn ty_inner<'a>(wrapper_names: &[&str], ty: &'a syn::Type) -> Option<&'a syn::Type> {
    if let syn::Type::Path(path) = ty {
        if path.qself.is_some() {
            return None;
        }
        // Since we only check the last path segment, it isn't necessarily the case that
        // we're referring to `std::vec::Vec` or `std::option::Option`, but there isn't
        // a fool proof way to check these since name resolution happens after macro expansion,
        // so this is likely "good enough" (so long as people don't have their own types called
        // `Option` or `Vec` that take one generic parameter they're looking to parse).
        let last_segment = path.path.segments.last()?;
        if !wrapper_names.iter().any(|name| last_segment.ident == *name) {
            return None;
        }
        if let syn::PathArguments::AngleBracketed(gen_args) = &last_segment.arguments {
            let generic_arg = gen_args.args.first()?;
            if let syn::GenericArgument::Type(ty) = &generic_arg {
                return Some(ty);
            }
        }
    }
    None
}

/// Implements `FromArgs` and `SubCommands` for a `#![derive(FromArgs)]` enum.
fn impl_from_args_enum(
    errors: &Errors,
    name: &syn::Ident,
    type_attrs: &TypeAttrs,
    generic_args: &syn::Generics,
    de: &syn::DataEnum,
) -> TokenStream {
    parse_attrs::check_enum_type_attrs(errors, type_attrs, &de.enum_token.span);

    // An enum variant like `<name>(<ty>)`
    struct SubCommandVariant<'a> {
        name: &'a syn::Ident,
        ty: &'a syn::Type,
    }

    let mut dynamic_type_and_variant = None;

    let variants: Vec<SubCommandVariant<'_>> = de
        .variants
        .iter()
        .filter_map(|variant| {
            let name = &variant.ident;
            let ty = enum_only_single_field_unnamed_variants(errors, &variant.fields)?;
            if parse_attrs::VariantAttrs::parse(errors, variant).is_dynamic.is_some() {
                if dynamic_type_and_variant.is_some() {
                    errors.err(variant, "Only one variant can have the `dynamic` attribute");
                }
                dynamic_type_and_variant = Some((ty, name));
                None
            } else {
                Some(SubCommandVariant { name, ty })
            }
        })
        .collect();

    let name_repeating = std::iter::repeat(name.clone());
    let variant_ty = variants.iter().map(|x| x.ty).collect::<Vec<_>>();
    let variant_names = variants.iter().map(|x| x.name).collect::<Vec<_>>();
    let dynamic_from_args =
        dynamic_type_and_variant.as_ref().map(|(dynamic_type, dynamic_variant)| {
            quote! {
                if let Some(result) = <#dynamic_type as argh::DynamicSubCommand>::try_from_args(
                    command_name, args) {
                    return result.map(#name::#dynamic_variant);
                }
            }
        });
    let dynamic_redact_arg_values = dynamic_type_and_variant.as_ref().map(|(dynamic_type, _)| {
        quote! {
            if let Some(result) = <#dynamic_type as argh::DynamicSubCommand>::try_redact_arg_values(
                command_name, args) {
                return result;
            }
        }
    });
    let dynamic_commands = dynamic_type_and_variant.as_ref().map(|(dynamic_type, _)| {
        quote! {
            fn dynamic_commands() -> &'static [&'static argh::CommandInfo] {
                <#dynamic_type as argh::DynamicSubCommand>::commands()
            }
        }
    });

    let (impl_generics, ty_generics, where_clause) = generic_args.split_for_impl();
    quote! {
        impl #impl_generics argh::FromArgs for #name #ty_generics #where_clause {
            fn from_args(command_name: &[&str], args: &[&str])
                -> std::result::Result<Self, argh::EarlyExit>
            {
                let subcommand_name = if let Some(subcommand_name) = command_name.last() {
                    *subcommand_name
                } else {
                    return ::core::result::Result::Err(argh::EarlyExit::from("no subcommand name".to_owned()));
                };

                #(
                    if subcommand_name == <#variant_ty as argh::SubCommand>::COMMAND.name {
                        return ::core::result::Result::Ok(#name_repeating::#variant_names(
                            <#variant_ty as argh::FromArgs>::from_args(command_name, args)?
                        ));
                    }
                )*

                #dynamic_from_args

                ::core::result::Result::Err(argh::EarlyExit::from("no subcommand matched".to_owned()))
            }

            fn redact_arg_values(command_name: &[&str], args: &[&str]) -> std::result::Result<Vec<String>, argh::EarlyExit> {
                let subcommand_name = if let Some(subcommand_name) = command_name.last() {
                    *subcommand_name
                } else {
                    return ::core::result::Result::Err(argh::EarlyExit::from("no subcommand name".to_owned()));
                };

                #(
                    if subcommand_name == <#variant_ty as argh::SubCommand>::COMMAND.name {
                        return <#variant_ty as argh::FromArgs>::redact_arg_values(command_name, args);
                    }
                )*

                #dynamic_redact_arg_values

                ::core::result::Result::Err(argh::EarlyExit::from("no subcommand matched".to_owned()))
            }
        }

        impl #impl_generics argh::SubCommands for #name #ty_generics #where_clause {
            const COMMANDS: &'static [&'static argh::CommandInfo] = &[#(
                <#variant_ty as argh::SubCommand>::COMMAND,
            )*];

            #dynamic_commands
        }
    }
}

/// Returns `Some(Bar)` if the field is a single-field unnamed variant like `Foo(Bar)`.
/// Otherwise, generates an error.
fn enum_only_single_field_unnamed_variants<'a>(
    errors: &Errors,
    variant_fields: &'a syn::Fields,
) -> Option<&'a syn::Type> {
    macro_rules! with_enum_suggestion {
        ($help_text:literal) => {
            concat!(
                $help_text,
                "\nInstead, use a variant with a single unnamed field for each subcommand:\n",
                "    enum MyCommandEnum {\n",
                "        SubCommandOne(SubCommandOne),\n",
                "        SubCommandTwo(SubCommandTwo),\n",
                "    }",
            )
        };
    }

    match variant_fields {
        syn::Fields::Named(fields) => {
            errors.err(
                fields,
                with_enum_suggestion!(
                    "`#![derive(FromArgs)]` `enum`s do not support variants with named fields."
                ),
            );
            None
        }
        syn::Fields::Unit => {
            errors.err(
                variant_fields,
                with_enum_suggestion!(
                    "`#![derive(FromArgs)]` does not support `enum`s with no variants."
                ),
            );
            None
        }
        syn::Fields::Unnamed(fields) => {
            if fields.unnamed.len() != 1 {
                errors.err(
                    fields,
                    with_enum_suggestion!(
                        "`#![derive(FromArgs)]` `enum` variants must only contain one field."
                    ),
                );
                None
            } else {
                // `unwrap` is okay because of the length check above.
                let first_field = fields.unnamed.first().unwrap();
                Some(&first_field.ty)
            }
        }
    }
}

/// Implements `FromArgValue` for a `#![derive(FromArgValue)]` enum (a choice enum).
fn impl_from_arg_value_enum(
    errors: &Errors,
    name: &syn::Ident,
    generic_args: &syn::Generics,
    de: &syn::DataEnum,
) -> TokenStream {
    // An enum variant like `<name>`
    struct ChoiceVariant<'a> {
        ident: &'a syn::Ident,
        name: syn::LitStr,
    }

    let variants: Vec<ChoiceVariant<'_>> = de
        .variants
        .iter()
        .map(|variant| {
            let ident = &variant.ident;
            choice_enum_only_fieldless_variant(errors, &variant.fields);
            let attrs = parse_attrs::ChoiceVariantAttrs::parse(errors, variant);
            let name = match attrs.name_override {
                Some(lit) => lit,
                None => {
                    let name_str = pascal_to_snake_case(&format!("{}", ident));
                    syn::LitStr::new(&name_str, ident.span())
                }
            };
            ChoiceVariant { ident, name }
        })
        .collect();

    if variants.is_empty() {
        errors.err(&de.variants, "Choice enums must have at least one variant");
    }

    let name_repeating = std::iter::repeat(name.clone());
    let variant_idents = variants.iter().map(|x| x.ident);
    let variant_names = variants.iter().map(|x| &x.name).collect::<Vec<_>>();
    let err_literal = {
        let mut err = "expected ".to_string();
        for (i, name) in variant_names.iter().enumerate() {
            if i == 0 {
            } else if i == variant_names.len() - 1 {
                err.push_str(" or ");
            } else {
                err.push_str(", ");
            }
            err.push_str(&format!("{:?}", name.value()));
        }
        LitStr::new(&err, name.span())
    };
    let (impl_generics, ty_generics, where_clause) = generic_args.split_for_impl();
    quote! {
        impl #impl_generics argh::FromArgValue for #name #ty_generics #where_clause {
            fn from_arg_value(value: &str)
                -> ::core::result::Result<Self, String>
            {
                ::core::result::Result::Ok(match value {
                    #(
                        #variant_names => #name_repeating::#variant_idents,
                    )*
                    _ => {
                        return ::core::result::Result::Err(#err_literal.to_owned())
                    }
                })
            }
        }
    }
}

/// Generates an error if the variant is not a field-less variant like `Foo`.
fn choice_enum_only_fieldless_variant(errors: &Errors, variant_fields: &syn::Fields) {
    match variant_fields {
        syn::Fields::Unit => {}
        _ => {
            errors.err(
                variant_fields,
                "Choice `enum`s tagged with `#![derive(FromArgValue)]` do not support variants with associated data.",
            );
        }
    }
}

fn pascal_to_snake_case(camel: &str) -> String {
    let mut out = String::with_capacity(camel.len() + 8);
    for (i, c) in camel.chars().enumerate() {
        if c.is_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
