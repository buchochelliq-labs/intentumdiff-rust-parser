//! Rust parser plugin - full-parse mode.
//!
//! Handles `.rs` files. Parses source with tree-sitter-rust directly.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

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
];

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
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
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
}
