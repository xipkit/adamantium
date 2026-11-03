# Rule coverage

The questions in `google-rust-decisions.json` translate selected judgment-based portions of the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html). Each question includes its applicability conditions and relevant exceptions. The surrounding project context must be supplied when a decision depends on information outside the candidate.

| Guide area | Decisions question names |
| --- | --- |
| Module organization and imports | `leaf_module_name`, `non_leaf_implementation`, `glob_import_context`, `trait_import_scope`, `qualified_type_without_clear_reason`, `unqualified_external_function` |
| Formatting exceptions | `unjustified_rustfmt_skip` |
| Ownership and borrowing | `unnecessary_shared_ownership`, `unnecessary_deep_clone` |
| API parameters, results, and errors | `eager_library_collection`, `restrictive_library_iteration`, `opaque_boolean_parameter`, `panic_on_recoverable_error`, `lost_recoverable_error_context`, `unnecessary_into_in_application` |
| Type design | `alias_for_distinct_domain_types`, `unnamed_tuple_components` |
| Traits and conversions | `trait_without_polymorphic_purpose`, `library_dynamic_dispatch`, `foreign_extension_trait_ownership`, `missing_clone_for_value_type`, `missing_copy_for_value_type`, `missing_map_key_traits`, `missing_primary_conversion_constructor`, `missing_default_for_options` |
| Macros | `unnecessary_macro` |
| Naming | `opaque_abbreviation`, `opaque_lifetime_name`, `opaque_generic_parameter` |
| Documentation and comments | `uninformative_public_docs`, `redundant_language_comment`, `undocumented_input_panic` |
| Unsafe code | `unnecessary_unsafe`, `inadequate_unsafe_safety_comment`, `undocumented_unsafe_contract`, `missing_unsafe_technical_justification`, `unsafe_mixed_with_business_logic` |
| API compatibility and behavior | `unrelated_operator_overload`, `missing_non_exhaustive_enum`, `missing_non_exhaustive_struct`, `missing_must_use_function`, `missing_must_use_type`, `foreign_doc_hidden_api`, `fragile_public_const_api` |
| Implementation and diagnostics | `unclear_inferred_type`, `non_exhaustive_match_hides_variants`, `catch_unwind_as_control_flow`, `complex_iterator_chain`, `unnecessary_for_each`, `assertion_missing_context`, `unhelpful_assertion_message` |

This is not a complete machine implementation of the guide. Formatting, straightforward import structure, and other syntactic checks belong in rustfmt or Clippy where possible. For example, Clippy already provides checks for `len` without `is_empty`, `new` without `Default`, and several standard-trait and borrowing conventions. Some Decisions rules consider exceptions or semantic context around patterns that a deterministic lint can also detect.

Chromium- and Fuchsia-specific guidance is not enabled as a general rule. Other guidance may depend on repository policy or architectural information outside a candidate. When code or policy is missing, the rules are designed to return `insufficient_context` rather than infer it. Decisions output is a review aid, not proof of correctness or unsafe-code soundness.
