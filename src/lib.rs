//! Rust parser plugin - full-parse mode.
//!
//! Handles `.rs` files. Parses source with tree-sitter-rust directly.

use intentumdiff_plugin_sdk::{cst::CstNode, tree::SemanticNode};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct RustParser;

const TRIVIA: &[&str] = &["line_comment", "block_comment", "whitespace"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "source_file",
    // Items
    "function_item",
    "impl_item",
    "trait_item",
    "struct_item",
    "enum_item",
    "union_item",
    "mod_item",
    "type_item",
    "const_item",
    "static_item",
    "use_declaration",
    "extern_crate_declaration",
    "foreign_mod_item",
    "macro_definition",
    "macro_invocation",
    // Impl members
    "associated_type",
    "let_declaration",
    // Enum variants
    "enum_variant",
    // Struct / trait members
    "field_declaration",
    "trait_bound",
    // Statements
    "expression_statement",
    "return_expression",
    "let_declaration",
    "assignment_expression",
    "compound_assignment_expr",
    // Control flow
    "if_expression",
    "match_expression",
    "match_arm",
    "loop_expression",
    "while_expression",
    "for_expression",
    "break_expression",
    "continue_expression",
    // Expressions
    "call_expression",
    "method_call_expression",
    "closure_expression",
    "await_expression",
    "async_block",
    "unsafe_block",
    // Identifiers / literals
    "identifier",
    "type_identifier",
    "string_literal",
    "integer_literal",
    "boolean_literal",
    "attribute_item",
    "inner_attribute_item",
    // Signature facts (issues #4/#5): function qualifiers, borrow/pointer shape and the
    // primitive types they wrap are part of the public contract, not layout.
    "function_modifiers",
    "extern_modifier",
    MODIFIER_TOKEN,
    "self_parameter",
    "reference_type",
    "pointer_type",
    "mutable_specifier",
    "primitive_type",
];

/// Synthetic leaf kind for anonymous keyword tokens that carry meaning but that the
/// named-only shared CST drops (`async`, `unsafe`, `const`, `default`, the `&` of `&self`).
const MODIFIER_TOKEN: &str = "modifier";

/// Kinds whose ANONYMOUS children are semantic. tree-sitter-rust exposes `async`/`unsafe`/
/// `const` only as unnamed tokens under `function_modifiers`, and the `&` of `&self` only as
/// an unnamed token under `self_parameter`; without them `fn f` and `unsafe fn f` (or `self`
/// and `&self`) project — and partly hash — identically.
const TOKEN_BEARING: &[&str] = &["function_modifiers", "self_parameter"];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    match node.node_type.as_str() {
        // `unsafe extern "C"`, `&'a mut self`: the qualifier text itself, whitespace-normalized.
        "function_modifiers" | "self_parameter" => {
            let label = node
                .children
                .iter()
                .map(|c| match c.node_type.as_str() {
                    "extern_modifier" => label_for(c),
                    _ => c.text_or_empty().to_string(),
                })
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            return label.replace("& ", "&");
        }
        "extern_modifier" => {
            return match node
                .children
                .iter()
                .find(|c| c.node_type == "string_literal")
            {
                Some(abi) => format!("extern {}", abi.text_or_empty()),
                None => "extern".to_string(),
            };
        }
        // `&T` / `&mut T` / `*const T` / `*mut T`: the borrow/pointer shape.
        "reference_type" | "pointer_type" => {
            let sigil = if node.node_type == "reference_type" {
                "&"
            } else {
                "*"
            };
            let mutable = node
                .children
                .iter()
                .any(|c| c.node_type == "mutable_specifier");
            return match (sigil, mutable) {
                ("&", true) => "&mut".to_string(),
                ("&", false) => "&".to_string(),
                (_, true) => "*mut".to_string(),
                _ => "*const".to_string(),
            };
        }
        "function_item" | "struct_item" | "enum_item" | "union_item" | "trait_item"
        | "type_item" | "const_item" | "static_item" | "mod_item" | "macro_definition" => {
            for child in &node.children {
                if child.node_type == "identifier" || child.node_type == "type_identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        // impl Trait for Type — use the type name
        "impl_item" => {
            // Walk to find the type name (last type_identifier before the body)
            let mut last_type: Option<String> = None;
            for child in &node.children {
                if child.node_type == "type_identifier"
                    || child.node_type == "scoped_type_identifier"
                {
                    last_type = Some(child.text_or_empty().to_string());
                }
                if child.node_type == "declaration_list" {
                    break;
                }
            }
            if let Some(t) = last_type {
                return t;
            }
        }
        "use_declaration" => {
            for child in &node.children {
                if child.node_type == "scoped_identifier"
                    || child.node_type == "identifier"
                    || child.node_type == "use_wildcard"
                {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "attribute_item" | "inner_attribute_item" => {
            for child in &node.children {
                if child.node_type == "attribute" {
                    for inner in &child.children {
                        if inner.node_type == "identifier" {
                            return inner.text_or_empty().to_string();
                        }
                    }
                }
            }
        }
        _ => {}
    }
    for child in &node.children {
        if child.node_type == "identifier" || child.node_type == "type_identifier" {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "struct_item" | "trait_item" | "enum_item" | "impl_item" | "union_item"
    )
}

fn is_method_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "function_item" | "closure_expression" | "associated_type"
    )
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|_| false,
        &is_semantic,
        &is_class_like,
        &is_method_like,
        &label_for,
    )
}

use intentumdiff_plugin_sdk::ts_convert::{convert_semantic_classed, node_to_cst};

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_rust::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load Rust grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    let mut root = node_to_cst(tree.root_node(), source.as_bytes());
    retain_modifier_tokens(tree.root_node(), &mut root, source.as_bytes());
    Ok(root)
}

/// Re-attach the anonymous keyword tokens of `TOKEN_BEARING` kinds as `MODIFIER_TOKEN` leaves,
/// in source order, so they get labels, exact UTF-8 byte spans and a share of the structural
/// hash. `node_to_cst` keeps named children in `named_child` order, so the two trees walk in
/// lockstep. Extras (comments) stay out.
fn retain_modifier_tokens(ts: tree_sitter::Node<'_>, cst: &mut CstNode, source: &[u8]) {
    if TOKEN_BEARING.contains(&ts.kind()) {
        let mut named = std::mem::take(&mut cst.children).into_iter();
        let mut children = Vec::new();
        for i in 0..ts.child_count() {
            let Some(child) = ts.child(i) else { continue };
            if child.is_named() {
                if let Some(mut converted) = named.next() {
                    retain_modifier_tokens(child, &mut converted, source);
                    children.push(converted);
                }
            } else if !child.is_extra() {
                let text = child.utf8_text(source).unwrap_or("").trim();
                if text.is_empty() {
                    continue;
                }
                children.push(CstNode {
                    node_type: MODIFIER_TOKEN.to_string(),
                    named: false,
                    text: Some(text.to_string()),
                    start_line: child.start_position().row as u32,
                    start_col: child.start_position().column as u32,
                    end_line: child.end_position().row as u32,
                    end_col: child.end_position().column as u32,
                    children: Vec::new(),
                });
            }
        }
        children.extend(named);
        if !children.is_empty() {
            cst.text = None;
        }
        cst.children = children;
        return;
    }
    for (i, child) in cst.children.iter_mut().enumerate() {
        if let Some(ts_child) = ts.named_child(i) {
            retain_modifier_tokens(ts_child, child, source);
        }
    }
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for RustParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "rust".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.ends_with(".rs") {
            "rust".to_string()
        } else {
            String::new()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "fn square(n: i32) -> i32 {\n    n * n\n}\n\nfn main() {\n    println!(\"{}\", square(5));\n}\n".to_string(),
            new: "fn square(n: i32) -> i32 {\n    n.pow(2)\n}\n\nfn cube(n: i32) -> i32 {\n    n.pow(3)\n}\n\nfn main() {\n    println!(\"square: {}\", square(5));\n    println!(\"cube:   {}\", cube(3));\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["rust".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(RustParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!RustParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = RustParser::grammar_id();
        let ids = RustParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = RustParser::detect_language("test.rs".to_string(), "".to_string());
        assert_eq!(r.as_str(), "rust");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            RustParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    // -----------------------------------------------------------------------
    // Signature-fact projection (issues #4 and #5)
    // -----------------------------------------------------------------------

    fn tree(source: &str) -> serde_json::Value {
        let out = process_impl(source);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(value.get("error").is_none(), "parse error: {out}");
        value
    }

    /// Pre-order (node_type, label, [start_line, start_col, end_line, end_col]).
    fn facts(node: &serde_json::Value, out: &mut Vec<(String, String, [u64; 4])>) {
        let p = &node["position"];
        out.push((
            node["node_type"].as_str().unwrap().to_string(),
            node["label"].as_str().unwrap().to_string(),
            [
                p["start_line"].as_u64().unwrap(),
                p["start_col"].as_u64().unwrap(),
                p["end_line"].as_u64().unwrap(),
                p["end_col"].as_u64().unwrap(),
            ],
        ));
        for child in node["children"].as_array().unwrap() {
            facts(child, out);
        }
    }

    fn all_facts(source: &str) -> Vec<(String, String, [u64; 4])> {
        let mut out = Vec::new();
        facts(&tree(source), &mut out);
        out
    }

    /// The projection without positions (what layout must not change).
    fn shape(source: &str) -> Vec<(String, String)> {
        all_facts(source)
            .into_iter()
            .map(|(t, l, _)| (t, l))
            .collect()
    }

    fn has(source: &str, node_type: &str, label: &str, span: [u64; 4]) -> bool {
        all_facts(source)
            .iter()
            .any(|(t, l, s)| t == node_type && l == label && *s == span)
    }

    fn root_hash(source: &str) -> String {
        tree(source)["structural_hash"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The edited token's span must slice the source to exactly that token (UTF-8 bytes).
    fn slice(source: &str, span: [u64; 4]) -> &str {
        assert_eq!(span[0], span[2], "single-line token");
        let line = source.lines().nth(span[0] as usize).unwrap();
        &line[span[1] as usize..span[3] as usize]
    }

    #[test]
    fn issue4_unsafe_fn_pair_emits_modifier_token() {
        let old = "unsafe fn f() {}\n";
        let new = "fn f() {}\n";
        assert!(has(old, "function_modifiers", "unsafe", [0, 0, 0, 6]));
        assert!(has(old, "modifier", "unsafe", [0, 0, 0, 6]));
        assert_eq!(slice(old, [0, 0, 0, 6]), "unsafe");
        assert!(!all_facts(new)
            .iter()
            .any(|(t, _, _)| t == "modifier" || t == "function_modifiers"));
        assert_ne!(shape(old), shape(new));
        assert_ne!(root_hash(old), root_hash(new));
    }

    #[test]
    fn issue4_async_fn_pair_emits_modifier_token() {
        let old = "async fn f() {}\n";
        let new = "fn f() {}\n";
        assert!(has(old, "function_modifiers", "async", [0, 0, 0, 5]));
        assert!(has(old, "modifier", "async", [0, 0, 0, 5]));
        assert_ne!(shape(old), shape(new));
        assert_ne!(root_hash(old), root_hash(new));
    }

    #[test]
    fn issue4_assessment_pairs_emit_modifier_token() {
        let plain = "pub fn answer() -> i32 { 1 }\n";
        let unsafe_fn = "pub unsafe fn answer() -> i32 { 1 }\n";
        let async_fn = "pub async fn answer() -> i32 { 1 }\n";
        assert!(has(unsafe_fn, "modifier", "unsafe", [0, 4, 0, 10]));
        assert!(has(async_fn, "modifier", "async", [0, 4, 0, 9]));
        assert!(has(plain, "primitive_type", "i32", [0, 19, 0, 22]));
        assert_ne!(shape(plain), shape(unsafe_fn));
        assert_ne!(shape(plain), shape(async_fn));
        assert_ne!(shape(unsafe_fn), shape(async_fn));
        assert_ne!(root_hash(plain), root_hash(unsafe_fn));
        assert_ne!(root_hash(unsafe_fn), root_hash(async_fn));
    }

    #[test]
    fn issue4_combined_and_extern_modifiers_hash_every_token() {
        let src = "pub const unsafe extern \"C\" fn f() {}\n";
        assert!(has(
            src,
            "function_modifiers",
            "const unsafe extern \"C\"",
            [0, 4, 0, 27]
        ));
        assert!(has(src, "modifier", "const", [0, 4, 0, 9]));
        assert!(has(src, "modifier", "unsafe", [0, 10, 0, 16]));
        assert!(has(src, "extern_modifier", "extern \"C\"", [0, 17, 0, 27]));
        // Before the fix `unsafe extern "C"` and `extern "C"` hashed identically: the
        // keyword was an unnamed token and the hash only saw the named extern child.
        assert_ne!(
            root_hash("unsafe extern \"C\" fn f() {}\n"),
            root_hash("extern \"C\" fn f() {}\n")
        );
        assert_ne!(
            root_hash("async fn f() {}\n"),
            root_hash("unsafe fn f() {}\n")
        );
    }

    #[test]
    fn issue5_reference_mutability_pair_preserves_mut_token() {
        let old = "pub fn bump(n: &i32) -> i32 { *n + 1 }\n";
        let new = "pub fn bump(n: &mut i32) -> i32 { *n + 1 }\n";
        assert!(has(old, "reference_type", "&", [0, 15, 0, 19]));
        assert!(has(old, "primitive_type", "i32", [0, 16, 0, 19]));
        assert!(has(new, "reference_type", "&mut", [0, 15, 0, 23]));
        assert!(has(new, "mutable_specifier", "mut", [0, 16, 0, 19]));
        assert_eq!(slice(new, [0, 16, 0, 19]), "mut");
        assert!(has(new, "primitive_type", "i32", [0, 20, 0, 23]));
        assert!(!all_facts(old)
            .iter()
            .any(|(t, _, _)| t == "mutable_specifier"));
        assert_ne!(shape(old), shape(new));
        assert_ne!(root_hash(old), root_hash(new));
    }

    #[test]
    fn issue5_self_receiver_borrow_shape_is_projected_and_hashed() {
        let by_value = "impl S { fn f(self) {} }\n";
        let shared = "impl S { fn f(&self) {} }\n";
        let exclusive = "impl S { fn f(&mut self) {} }\n";
        assert!(has(shared, "self_parameter", "&self", [0, 14, 0, 19]));
        assert!(has(shared, "modifier", "&", [0, 14, 0, 15]));
        assert!(has(
            exclusive,
            "self_parameter",
            "&mut self",
            [0, 14, 0, 23]
        ));
        assert!(has(exclusive, "mutable_specifier", "mut", [0, 15, 0, 18]));
        // `self` vs `&self` differed only by an unnamed `&` and hashed identically.
        assert_ne!(root_hash(by_value), root_hash(shared));
        assert_ne!(root_hash(shared), root_hash(exclusive));
        assert_ne!(shape(shared), shape(exclusive));
    }

    #[test]
    fn pointer_mutability_is_projected() {
        assert!(has(
            "fn f(p: *const u8) {}\n",
            "pointer_type",
            "*const",
            [0, 8, 0, 17]
        ));
        assert!(has(
            "fn f(p: *mut u8) {}\n",
            "pointer_type",
            "*mut",
            [0, 8, 0, 15]
        ));
        assert_ne!(
            root_hash("fn f(p: *const u8) {}\n"),
            root_hash("fn f(p: *mut u8) {}\n")
        );
    }

    #[test]
    fn signature_fact_controls_identity_layout_comment() {
        let src = "pub unsafe fn bump(n: &mut i32) -> i32 { *n + 1 }\n";
        // Identity: byte-identical output.
        assert_eq!(process_impl(src), process_impl(src));
        // Layout: same projection and semantic shape; only positions move.
        let spaced = "pub   unsafe\n  fn bump( n : &  mut i32 ) -> i32 {\n    *n + 1\n}\n";
        assert_eq!(shape(src), shape(spaced));
        assert_eq!(root_hash(src), root_hash(spaced));
        // Ordinary comment between qualifiers stays out of the modifier projection.
        let commented = "pub unsafe /* why */ fn bump(n: &mut /* x */ i32) -> i32 { *n + 1 }\n";
        let modifiers: Vec<_> = all_facts(commented)
            .into_iter()
            .filter(|(t, _, _)| {
                t == "function_modifiers" || t == "modifier" || t == "reference_type"
            })
            .map(|(t, l, _)| (t, l))
            .collect();
        assert_eq!(
            modifiers,
            vec![
                ("function_modifiers".to_string(), "unsafe".to_string()),
                ("modifier".to_string(), "unsafe".to_string()),
                ("reference_type".to_string(), "&mut".to_string()),
            ]
        );
    }
}
