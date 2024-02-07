//! Macro for building [wasmCloud capability providers](https://wasmcloud.com/docs/fundamentals/capabilities/create-provider/)
//! from [WIT](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md) contracts.
//!
//! For example, to build a capability provider for the [wasmcloud:keyvalue contract](https://github.com/wasmCloud/interfaces/tree/main/keyvalue):
//!
//! ```rust,ignore
//! wasmcloud_provider_wit_bindgen::generate!({
//!     impl_struct: KvRedisProvider,
//!     contract: "wasmcloud:keyvalue",
//!     wit_bindgen_cfg: "provider-kvredis"
//! });
//!
//! struct YourProvider;
//! ```
//!
//! All content after `wit_bindgen_cfg: ` is fed to the underlying bindgen (wasmtime::component::macro). In this example, "provider-kvredis" refers to the WIT world that your component will inhabit -- expected to be found at `<project root>/wit/<your world name>.wit`. An example world file:
//!
//! ```rust,ignore
//! package wasmcloud:provider-kvredis
//!
//! world provider-kvredis {
//!     import wasmcloud:keyvalue/key-value
//! }
//! ```
//!
//! For more information on the options available to underlying bindgen, see the [wasmtime-component-bindgen documentation](https://docs.rs/wasmtime/latest/wasmtime/component/macro.bindgen.html).
//!

use std::collections::HashMap;

use anyhow::{bail, Context};
use proc_macro2::{Ident, Punct, Span, TokenStream, TokenTree};
use quote::{ToTokens, TokenStreamExt};
use syn::{
    parse_macro_input, punctuated::Punctuated, visit_mut::VisitMut, FnArg, ImplItemFn, ItemEnum,
    ItemStruct, ItemType, LitStr, PathSegment, ReturnType, Token,
};
use tracing::debug;
use tracing_subscriber::EnvFilter;
use wit_parser::WorldKey;

mod bindgen_visitor;
use bindgen_visitor::WitBindgenOutputVisitor;

mod config;
use config::ProviderBindgenConfig;

mod rust;

mod vendor;
use vendor::wasmtime_component_macro::bindgen::expand as expand_wasmtime_component;

mod wit;
use wit::{
    extract_witified_map, WitFunctionName, WitInterfacePath, WitNamespaceName, WitPackageName,
};

/// Rust module name that is used by wit-bindgen to generate all the modules
const EXPORTS_MODULE_NAME: &str = "exports";

type ImplStructName = String;
type WasmcloudContract = String;

/// Information related to an interface function that will be eventually exposed on the lattice
type LatticeExposedInterface = (WitNamespaceName, WitPackageName, WitFunctionName);

type StructName = String;
type StructLookup = HashMap<StructName, (Punctuated<PathSegment, Token![::]>, ItemStruct)>;

type EnumName = String;
type EnumLookup = HashMap<EnumName, (Punctuated<PathSegment, Token![::]>, ItemEnum)>;

type TypeName = String;
type TypeLookup = HashMap<TypeName, (Punctuated<PathSegment, Token![::]>, ItemType)>;

/// A converted Rust Trait method that will go out on the lattice
///
/// This structure is normally produced by bindgen code after relevant parsing.
#[derive(Debug, Clone)]
struct LatticeMethod {
    /// Lattice methods that depend on pre-existing types that are serializable (i.e. [`serde::Serializable`] directly and can be sent out on the lattice as-is.
    ///
    /// These types may be produced by wit-bindgen produced types, but *do not* have to be generated by
    /// this macro.
    ///
    /// Often produced when [`WitFunctionLatticeTranslationStrategy::FirstArgument`] is configured.
    /// The name of the method that would be used on the lattice
    lattice_method_name: LitStr,

    /// The name of the type can be deserialized to perform the invocation
    ///
    /// When this is a bindgen-generated struct, `type_name` will be the name of the struct, and
    /// when the type is known/a standard type, `struct_members` will be empty and this `type_name` will be the
    /// known value (ex. `String`)
    type_name: Option<TokenStream>,

    /// Tokens that represent the struct member declarations for the current lattice method
    ///
    /// This is normally used when there are *multiple* arguments to a function but they are bundled together to be used
    /// across the lattice (i.e. [`WitFunctionLatticeTranslationStrategy::BundleArguments`])
    ///
    /// This is only present when the `type_name` corresponds to a struct that we must generate as part of this macro
    struct_members: Option<TokenStream>,

    /// Function name for the method that will be called after a lattice invocation is received
    func_name: Ident,

    /// Invocation arguments, only names without types
    invocation_arg_names: Vec<Ident>,

    /// Return type of the invocation
    invocation_return: ReturnType,
}

/// This macro generates functionality necessary to use a WIT-enabled Rust providers (binaries that are managed by the host)
#[proc_macro]
pub fn generate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Parse the provider bindgen macro configuration
    let cfg = parse_macro_input!(input as ProviderBindgenConfig);
    let contract_ident = LitStr::new(&cfg.contract, Span::call_site());

    // Extract the parsed upstream WIT bindgen configuration, which (once successfully parsed)
    // contains metadata extracted from WIT files
    let wit_bindgen_cfg = cfg
        .wit_bindgen_cfg
        .as_ref()
        .context("configuration to pass to WIT bindgen is missing")
        .expect("failed to parse WIT bindgen configuration");

    // Track exported iface invocation methods/structs

    // Process the parsed WIT metadata to extract imported interface invocation methods & structs,
    // which will be used to generate InvocationHandlers for external calls that the provider may make
    let mut imported_iface_invocation_methods: Vec<TokenStream> = Vec::new();
    let mut imported_iface_invocation_structs: Vec<TokenStream> = Vec::new();
    for (_, world) in wit_bindgen_cfg.resolve.worlds.iter() {
        for (import_key, _) in world.imports.iter() {
            if let WorldKey::Interface(iface_id) = import_key {
                // Find the parsed interface definition that corresponds to the iface
                let iface = &wit_bindgen_cfg.resolve.interfaces[*iface_id];

                // Some interfaces are known to be *not* coming in from the lattice
                // and should not have invocation handlers generated for them.
                //
                // For example, the wasmcloud:bus interface should not be interpreted
                // as an InvocationHandler generation target
                if iface
                    .package
                    .map(|p| &wit_bindgen_cfg.resolve.packages[p].name)
                    .is_some_and(is_ignored_invocation_handler_pkg)
                {
                    continue;
                }

                // All other interfaces should have their functions processed in order to generate
                // InvocationHandlers in the resulting bindgen output code
                //
                // For each function in an exported interface, we'll need to generate a method
                // on the eventual `InvocationHandler`s that will be built later.
                //
                // Most functions on imported interface to consist of *one* argument which is
                // normally a struct (WIT record type) what represents the information for lattice, ex.:
                //
                //  ```
                //  interface handler {
                //       use types.{some-message}
                //       handle-message: func(msg: some-message) -> result<_, string>
                //   }
                //  ```
                for (iface_fn_name, iface_fn) in iface.functions.iter() {
                    debug!("processing imported interface function: [{iface_fn_name}]");

                    let (invocation_struct_tokens, invocation_method_tokens) = cfg
                        .export_fn_lattice_translation_strategy
                        .translate_import_fn_for_lattice(iface, iface_fn_name, iface_fn, &cfg)
                        .expect("failed to translate export fn");
                    imported_iface_invocation_methods.extend(invocation_method_tokens.into_iter());
                    imported_iface_invocation_structs.extend(invocation_struct_tokens.into_iter());
                }
            }
        }
    }

    // Expand the wasmtime::component macro with the given arguments.
    // We re-use the output of this macro and extract code from it in order to build our own.
    let bindgen_tokens: TokenStream =
        expand_wasmtime_component(wit_bindgen_cfg).unwrap_or_else(syn::Error::into_compile_error);

    // // TODO: REMOVE
    // eprintln!("BINDGEN TOKENS:\n========{bindgen_tokens}\n==========");

    // Parse the bindgen-generated tokens into an AST
    // that will be used in the output (combined with other wasmcloud-specific generated code)
    let mut bindgen_ast: syn::File =
        syn::parse2(bindgen_tokens).expect("failed to parse wit-bindgen generated code as file");

    // Traverse the generated upstream wasmtime::component macro output code,
    // to modify it and extract information from it
    let mut visitor = WitBindgenOutputVisitor::new(&cfg);
    visitor.visit_file_mut(&mut bindgen_ast);

    // Turn the function calls extracted from the wasmtime::component macro code
    // into method declarations that enable receiving invocations from the lattice
    let methods_by_iface = build_lattice_methods_by_wit_interface(
        &visitor.serde_extended_structs,
        &visitor.type_lookup,
        &visitor.export_trait_methods,
        &cfg,
    )
    .expect("failed to build lattice methods from WIT interfaces");

    // Create the implementation struct name as an Ident
    let impl_struct_name = Ident::new_raw(cfg.impl_struct.as_str(), Span::call_site());

    // Build a list of match arms for the invocation dispatch that is required
    let mut interface_dispatch_match_arms: Vec<TokenStream> = Vec::new();
    let mut iface_tokens = TokenStream::new();

    // Go through every method metadata object (`LatticeMethod`) extracted from the
    // wasmtime::component macro output code in order to:
    //
    // - Generate struct declarations
    // - Generate traits for each interface (ex. "wasi:keyvalue/eventual" -> `WasiKeyvalueEventual`)
    //
    for (wit_iface_name, methods) in methods_by_iface.iter() {
        // Convert the WIT interface name into an ident
        let wit_iface = Ident::new(wit_iface_name, Span::call_site());

        // Filter out type names and struct members for structs that should be generated
        //
        // NOTE: struct_members is *either*
        // - the actual member from the GeneratedStruct variant
        // - the typename + type from the single first arg
        let (struct_type_names, struct_members) = methods.clone().into_iter().fold(
            (Vec::<TokenStream>::new(), Vec::<TokenStream>::new()),
            |mut acc, lm| {
                if let (Some(sm), Some(type_name)) = (lm.struct_members, lm.type_name) {
                    acc.0.push(type_name);
                    acc.1.push(sm);
                }
                acc
            },
        );

        // Add generated struct code for the current interface
        iface_tokens.append_all(quote::quote!(
            // START: *Invocation structs & trait for #wit_iface
            #(
                #[derive(Debug, ::wasmcloud_provider_wit_bindgen::deps::serde::Serialize, ::wasmcloud_provider_wit_bindgen::deps::serde::Deserialize)]
                #[serde(crate = "wasmcloud_provider_wit_bindgen::deps::serde")]
                struct #struct_type_names {
                    #struct_members
                }
            )*
        ));

        // Create a list of lattice method names that will trigger provider calls
        let lattice_method_names = methods
            .clone()
            .into_iter()
            .map(|lm| lm.lattice_method_name)
            .collect::<Vec<LitStr>>();

        // Function names that providers will implement for lattice methods (these functions will be called)
        let func_names = methods
            .clone()
            .into_iter()
            .map(|lm| lm.func_name)
            .collect::<Vec<Ident>>();

        // Gather the invocation args with names, which is either:
        // - all struct members if present
        // - the arg name plus type name for a known type
        // - an empty list for zero args
        let invocation_args_with_types = methods
            .clone()
            .into_iter()
            .map(|lm| {
                match (lm.struct_members, &lm.invocation_arg_names[..]) {
                    // If more than one argument was present, we should be dealing with that as
                    // an invocation struct
                    (Some(members), _) => members,
                    // For no arguments, then we don't need to add any invocation args
                    (None, []) => {
                        TokenStream::new()
                    },
                    // If there's one argument then we should add the single argument
                    (None, [first]) => {
                        let type_name = lm.type_name;
                        quote::quote!(#first: #type_name)
                    },
                    // All other combinations are invalid (ex. forcing first-argument parsing when there are muiltiple args to the fn),
                    _ => panic!("unexpectedly found more than 1 invocation arg in function [{}] name, wit_function_lattice_translation-strategy should likely not be set to 'first-argument'", lm.func_name),
                }
            })
            .collect::<Vec<TokenStream>>();

        // Invocation returns of the functions that are called for each lattice method
        let invocation_returns = methods
            .clone()
            .into_iter()
            .map(|lm| lm.invocation_return)
            .collect::<Vec<ReturnType>>();

        // Generate main trait for this interface (ex. `WasiKeyvalueEventual`) that facilitates invocations
        // and pipes through calls to provider impl
        //
        // Create and append the trait for the iface along with
        // the functions that should be implemented by the provider
        iface_tokens.append_all(quote::quote!(
            #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
            pub trait #wit_iface {
                fn contract_id() -> &'static str {
                    #contract_ident
                }

                #(
                    async fn #func_names (
                        &self,
                        ctx: ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::Context,
                        #invocation_args_with_types
                    ) #invocation_returns;
                )*
            }
            // END: *Invocation structs & trait for #wit_iface
        ));

        // Build match arms that do input parsing and argument expressions, for every method
        let (input_parsing_statements, post_self_args) =
            methods
            .clone()
            .into_iter()
            .fold((Vec::new(), Vec::new()), |mut acc, lm| {
                if let Some(type_name) = lm.type_name {
                    // type_name tells us the single type that is coming in over the lattice.
                    //
                    // This can either be:
                    //  - a wit-bindgen-generated type (ex. some record type)
                    //  - a struct we created (a "bundle" generated under [`WitFunctionLatticeTranslationStrategy::BundleArguments`])
                    //  - a pre-existing type (ex. `String`)
                    //
                    // We can use this to generate lines for
                    acc.0.push(quote::quote!(let input: #type_name = ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::deserialize(&body)?;));

                    let invocation_arg_names = lm.invocation_arg_names;
                    acc.1.push(if invocation_arg_names.len() == 1 {
                        // If there is only one invocation argument (and we know the type name)
                        // then it's the input we read over the wire
                        quote::quote!(ctx, input)
                    } else {
                        // If there is more than one arg name, we have a bundle of arguments that was sent over the wire
                        // we must pass the *fields* of that struct in
                        let mut tokens = TokenStream::new();
                        invocation_arg_names.iter().enumerate().fold(&mut tokens, |ts, (idx, i)| {
                            // Append input since if we have multiple arguments they'll be coming in as one envelope over the lattice
                            ts.append_all(quote::quote!(input.#i));
                            if idx != invocation_arg_names.len() - 1 {
                                ts.append(TokenTree::Punct(Punct::new(',', proc_macro2::Spacing::Alone)));
                            }
                            ts
                        });
                        quote::quote!(ctx, #tokens)
                    });
                } else {
                    // If a type name is *not* present, we're dealing with a function that takes *no* input.
                    //
                    // This means that there's no input to be parsed, and only ctx as a post-self argument
                    acc.0.push(TokenStream::new());
                    acc.1.push(Ident::new("ctx", Span::call_site()).to_token_stream());
                }
                acc
            });

        // After building individual invocation structs and traits for each interface
        // we must build & hold on to the usage of these inside the match for the MessageDispatch trait
        interface_dispatch_match_arms.push(quote::quote!(
            #(
                #lattice_method_names => {
                    #input_parsing_statements
                    let result = #wit_iface::#func_names(
                        self,
                        #post_self_args
                    )
                        .await;
                    Ok(::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::serialize(&result)?)
                }
            )*
        ));
    }

    // Build a list of types that should be included in the output code
    let types: Vec<TokenStream> = visitor
        .type_lookup
        .iter()
        .filter_map(|(_, (_, ty))| {
            // If the name of the type is identical to a bindgen-produced struct that will
            // be added later, this was likely a type alias -- we won't need it
            if visitor
                .serde_extended_structs
                .contains_key(&ty.ident.to_string())
            {
                None
            } else {
                Some(ty.to_token_stream())
            }
        })
        .collect();

    // Build a list of structs that should be included
    let structs: Vec<TokenStream> = visitor
        .serde_extended_structs
        .iter()
        .map(|(_, (_, s))| s.to_token_stream())
        .collect();

    // Build a list of enums that should be included
    let enums: Vec<TokenStream> = visitor
        .serde_extended_enums
        .iter()
        .map(|(_, (_, s))| s.to_token_stream())
        .collect();

    // Build the final chunk of code
    let tokens = quote::quote!(
        // START: per-interface codegen
        #iface_tokens
        // END: per-interface codegen

        // START: wit-bindgen generated types
        #(
            #types
        )*
        // END: wit-bindgen generated types

        // START: wit-bindgen generated structs
        #(
            #structs
        )*
        // END: wit-bindgen generated structs

        // START: wit-bindgen generated enums
        #(
            #enums
        )*
        // END: wit-bindgen generated enums

        /// MessageDispatch ensures that your provider can receive and
        /// process messages sent to it over the lattice
        ///
        /// This implementation is a stub and must be filled out by implementers
        ///
        /// It would be preferable to use <T: SomeTrait> here, but the fact that  'd like to use
        #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
        impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::MessageDispatch for #impl_struct_name {
            async fn dispatch<'a>(
                &'a self,
                ctx: ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::Context,
                method: String,
                body: std::borrow::Cow<'a, [u8]>,
            ) -> ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationResult<Vec<u8>> {
                match method.as_str() {
                    #(
                        #interface_dispatch_match_arms
                    )*
                    _ => Err(::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::error::InvocationError::Malformed(format!(
                        "Invalid method name {method}"
                    )).into())
                }
            }
        }

        // START: general provider

        /// This trait categorizes all wasmCloud lattice compatible providers.
        ///
        /// It is a mirror of ProviderHandler for the purposes of ensuring that
        /// at least the following members are is supported.
        #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
        trait WasmcloudCapabilityProvider {
            async fn put_link(&self, ld: &::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::core::LinkDefinition) -> bool;
            async fn delete_link(&self, actor_id: &str);
            async fn shutdown(&self);
        }

        /// ProviderHandler ensures that your provider handles the basic
        /// required functionality of all Providers on a wasmCloud lattice.
        ///
        /// This implementation is a stub and must be filled out by implementers
        #[::wasmcloud_provider_wit_bindgen::deps::async_trait::async_trait]
        impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::ProviderHandler for #impl_struct_name {
            async fn put_link(&self, ld: &::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::core::LinkDefinition) -> bool {
                WasmcloudCapabilityProvider::put_link(self, ld).await
            }

            async fn delete_link(&self, actor_id: &str) {
                WasmcloudCapabilityProvider::delete_link(self, actor_id).await
            }

            async fn shutdown(&self) {
                WasmcloudCapabilityProvider::shutdown(self).await
            }
        }

        /// Given the implementation of ProviderHandler and MessageDispatch,
        /// the implementation for your struct is a guaranteed
        impl ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::Provider for #impl_struct_name {}

        // Structs that are used at Invocation Handling time
        #( #imported_iface_invocation_structs )*

        /// This handler serves to be used for individual invocations of the actor
        /// as performed by the host runtime
        ///
        /// Interfaces imported by the provider can use this to send traffic across the lattice
        pub struct InvocationHandler<'a> {
            ld: &'a ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::core::LinkDefinition,
        }

        impl<'a> InvocationHandler<'a> {
            pub fn new(ld: &'a ::wasmcloud_provider_wit_bindgen::deps::wasmcloud_provider_sdk::core::LinkDefinition) -> Self {
                Self { ld }
            }

            #(
                #imported_iface_invocation_methods
            )*
        }

    );

    tokens.into()
}

/// Build [`LatticeMethod`]s (including related information to facilitate invocations)
/// for the imports of a WIT interface
fn build_lattice_methods_by_wit_interface(
    struct_lookup: &StructLookup,
    type_lookup: &TypeLookup,
    export_trait_methods: &HashMap<WitInterfacePath, Vec<ImplItemFn>>,
    bindgen_cfg: &ProviderBindgenConfig,
) -> anyhow::Result<HashMap<WitInterfacePath, Vec<LatticeMethod>>> {
    let mut methods_by_name: HashMap<WitInterfacePath, Vec<LatticeMethod>> = HashMap::new();

    // For every trait item generated by an imported WIT interface we must generate the appropriate
    // structures that are expected from incoming messages on the lattice.
    for (wit_iface_name, funcs) in export_trait_methods.iter() {
        for trait_method in funcs.iter() {
            // Convert the trait method to code that can be used on the lattice
            let (trait_name, lattice_method) = bindgen_cfg
                .import_fn_lattice_translation_strategy
                .translate_export_fn_for_lattice(
                bindgen_cfg,
                wit_iface_name.into(),
                trait_method,
                struct_lookup,
                type_lookup,
            )?;

            // Add the struct and its members to a list that will be used in another quote
            // it cannot be added directly/composed to a TokenStream here to avoid import conflicts
            // in case bindgen-defined types are used.
            methods_by_name
                .entry(trait_name)
                .or_default()
                .push(lattice_method);
        }
    }
    Ok(methods_by_name)
}

/// Process a first argument to retreive the argument name and type name used
pub(crate) fn process_fn_arg(arg: &FnArg) -> anyhow::Result<(Ident, TokenStream)> {
    // Retrieve the type pattern ascription (i.e. 'arg: Type') out of the first arg
    let pat_type = if let syn::FnArg::Typed(pt) = arg {
        pt
    } else {
        bail!("failed to parse pat type out of ");
    };

    // Retrieve argument name
    let mut arg_name = if let syn::Pat::Ident(n) = pat_type.pat.as_ref() {
        n.ident.clone()
    } else {
        bail!("unexpectedly non-ident pattern in {pat_type:#?}");
    };

    // If the argument name ends in _map, and the type matches a witified map (i.e. list<tuple<T, T>>)
    // then convert the type into a map *before* using it
    let type_name = match (
        arg_name.to_string().ends_with("_map"),
        extract_witified_map(
            &pat_type
                .ty
                .as_ref()
                .to_token_stream()
                .into_iter()
                .collect::<Vec<TokenTree>>(),
        ),
    ) {
        (true, Some(map_type)) => {
            arg_name = Ident::new(
                arg_name.to_string().trim_end_matches("_map"),
                arg_name.span(),
            );
            quote::quote!(#map_type)
        }
        _ => pat_type.ty.as_ref().to_token_stream(),
    };

    Ok((arg_name, type_name))
}

/// Check whether a package should *not* be processed while generating `InvocationHandler`s
fn is_ignored_invocation_handler_pkg(pkg: &wit_parser::PackageName) -> bool {
    matches!(
        (pkg.namespace.as_ref(), pkg.name.as_ref()),
        ("wasmcloud", "bus") | ("wasi", "io")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::{Context, Result};
    use proc_macro2::TokenTree;
    use syn::{parse_quote, ImplItemFn, LitStr};

    use crate::{
        extract_witified_map, wit::WitFunctionLatticeTranslationStrategy, ProviderBindgenConfig,
    };

    /// Token trees that we expect to parse into WIT-ified maps should parse
    #[test]
    fn parse_witified_map_type() -> Result<()> {
        extract_witified_map(
            &quote::quote!(Vec<(String, String)>)
                .into_iter()
                .collect::<Vec<TokenTree>>(),
        )
        .context("failed to parse WIT-ified map type Vec<(String, String)>")?;
        Ok(())
    }

    /// Ensure WIT-ified maps parse correctly in functions
    #[test]
    fn parse_witified_map_in_fn() -> Result<()> {
        let trait_fn: ImplItemFn = parse_quote!(
            fn baz(test_map: Vec<(String, String)>) {}
        );
        let bindgen_cfg = ProviderBindgenConfig {
            impl_struct: "None".into(),
            contract: "wasmcloud:test".into(),
            wit_ns: Some("test".into()),
            wit_pkg: Some("foo".into()),
            exposed_interface_allow_list: Default::default(),
            exposed_interface_deny_list: Default::default(),
            wit_bindgen_cfg: None, // We won't actually run bindgen
            import_fn_lattice_translation_strategy: Default::default(),
            export_fn_lattice_translation_strategy: Default::default(),
            replace_witified_maps: true,
        };
        let (wit_iface_name, lm) =
            WitFunctionLatticeTranslationStrategy::translate_export_fn_via_bundled_args(
                &bindgen_cfg,
                "TestFoo".into(),
                LitStr::new("Foo", proc_macro2::Span::call_site()),
                &trait_fn,
                &HashMap::new(), // structs
                &HashMap::new(), // types
            )?;

        assert_eq!(wit_iface_name, "TestFoo");
        let type_name = lm.type_name.as_ref().context("failed to get type name")?;
        assert_eq!(type_name.to_string(), "TestFooBazInvocation");
        let struct_members = lm.struct_members.context("struct members missing")?;
        assert!(
            matches!(
                &struct_members.into_iter().collect::<Vec<TokenTree>>()[2..], // skip arg name & colon
                [
                    TokenTree::Punct(_),  // ":"
                    TokenTree::Punct(_),  // ":"
                    TokenTree::Ident(i1), // 'std'
                    TokenTree::Punct(_),  // ":"
                    TokenTree::Punct(_),  // ":"
                    TokenTree::Ident(i2), // 'collections'
                    TokenTree::Punct(_),  // ":"
                    TokenTree::Punct(_),  // ":"
                    TokenTree::Ident(i3), // 'HashMap'
                    TokenTree::Punct(b1), // "<"
                    TokenTree::Ident(key_type), // key type
                    TokenTree::Punct(c),  // ","
                    TokenTree::Ident(value_type), // value type
                    TokenTree::Punct(b2), // ">"
                ] if *i1 == "std" &&
                    *i2 == "collections" &&
                    *i3 == "HashMap" &&
                    b1.to_string() == "<" &&
                    c.to_string() == "," &&
                    *key_type == "String" &&
                    *value_type == "String" &&
                    b2.to_string() == ">"
            ),
            "struct members converted type is incorrect",
        );

        Ok(())
    }
}
