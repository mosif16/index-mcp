use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Serialize, Clone)]
pub struct GraphNode {
    pub id: String,
    pub path: Option<String>,
    pub kind: String,
    pub name: String,
    pub signature: Option<String>,
    pub range_start: Option<i64>,
    pub range_end: Option<i64>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GraphEdge {
    pub id: String,
    pub source_id: String,
    pub target_id: String,
    pub edge_type: String,
    pub source_path: Option<String>,
    pub target_path: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize, Clone)]
pub struct GraphExtraction {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

pub fn extract_graph(relative_path: &str, source: &str) -> Option<GraphExtraction> {
    let extension = Path::new(relative_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|value| value.to_ascii_lowercase());

    match extension.as_deref() {
        Some("rs") => rust_simple::extract(relative_path, source),
        Some("py") | Some("pyw") => python_simple::extract(relative_path, source),
        Some("swift") => swift_simple::extract(relative_path, source),
        Some("ts") | Some("tsx") | Some("js") | Some("jsx") => {
            typescript::extract(relative_path, source)
        }
        _ => None,
    }
}

fn stable_id(inputs: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for input in inputs {
        hasher.update(input.as_bytes());
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

fn find_matching_brace(source: &str, open_brace: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open_brace;
    let bytes = source.as_bytes();
    while index < bytes.len() {
        match bytes[index] as char {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

static CALL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap());

struct SimpleExtractor<'a> {
    file_path: &'a str,
    source: &'a str,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    scope_stack: Vec<String>,
    symbol_index: HashMap<String, String>,
    known_functions: HashSet<String>,
}

impl<'a> SimpleExtractor<'a> {
    fn new(file_path: &'a str, source: &'a str) -> Self {
        let file_id = stable_id(&["file", file_path]);
        let file_node = GraphNode {
            id: file_id.clone(),
            path: Some(file_path.to_string()),
            kind: "file".to_string(),
            name: file_path.to_string(),
            signature: None,
            range_start: None,
            range_end: None,
            metadata: None,
        };
        Self {
            file_path,
            source,
            nodes: vec![file_node],
            edges: Vec::new(),
            scope_stack: vec![file_id],
            symbol_index: HashMap::new(),
            known_functions: HashSet::new(),
        }
    }

    fn add_function(
        &mut self,
        name: &str,
        kind: &str,
        byte_start: usize,
        byte_end: usize,
        signature: Option<String>,
        metadata: Option<Value>,
    ) -> String {
        let id = stable_id(&[kind, self.file_path, name, &byte_start.to_string()]);
        self.nodes.push(GraphNode {
            id: id.clone(),
            path: Some(self.file_path.to_string()),
            kind: kind.to_string(),
            name: name.to_string(),
            signature,
            range_start: Some(byte_start as i64),
            range_end: Some(byte_end as i64),
            metadata,
        });
        self.symbol_index
            .entry(name.to_string())
            .or_insert(id.clone());
        self.known_functions.insert(name.to_string());
        id
    }

    fn push_scope(&mut self, id: String) {
        self.scope_stack.push(id);
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
    }

    fn ensure_symbol(&mut self, name: &str) -> String {
        if let Some(existing) = self.symbol_index.get(name) {
            return existing.clone();
        }
        let id = stable_id(&["symbol", name]);
        self.nodes.push(GraphNode {
            id: id.clone(),
            path: None,
            kind: "symbol".to_string(),
            name: name.to_string(),
            signature: None,
            range_start: None,
            range_end: None,
            metadata: None,
        });
        self.symbol_index.insert(name.to_string(), id.clone());
        id
    }

    fn record_call(&mut self, name: &str, byte_offset: usize) {
        if name.is_empty() {
            return;
        }
        let scope_id = match self.scope_stack.last() {
            Some(id) => id.clone(),
            None => return,
        };

        let target_id = self.ensure_symbol(name);
        let edge_id = stable_id(&[
            "edge",
            "calls",
            &scope_id,
            &target_id,
            &byte_offset.to_string(),
        ]);
        self.edges.push(GraphEdge {
            id: edge_id,
            source_id: scope_id,
            target_id,
            edge_type: "calls".to_string(),
            source_path: Some(self.file_path.to_string()),
            target_path: None,
            metadata: Some(serde_json::json!({ "offset": byte_offset })),
        });
    }

    fn scan_calls(&mut self, start: usize, end: usize, forbidden: &[&str]) {
        if start >= end || end > self.source.len() {
            return;
        }
        let body = &self.source[start..end];
        for capture in CALL_RE.captures_iter(body) {
            if let Some(name_match) = capture.get(1) {
                let name = name_match.as_str();
                if forbidden.contains(&name) {
                    continue;
                }
                let global_offset = start + name_match.start();
                self.record_call(name, global_offset);
            }
        }
    }

    fn finish(self) -> Option<GraphExtraction> {
        if self.nodes.len() <= 1 {
            None
        } else {
            Some(GraphExtraction {
                nodes: self.nodes,
                edges: self.edges,
            })
        }
    }
}

mod rust_simple {
    use super::{find_matching_brace, SimpleExtractor};
    use once_cell::sync::Lazy;
    use regex::Regex;

    static FN_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?m)^(?P<indent>\s*)(?P<prefix>pub\s+)?(?P<async>async\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\((?P<params>[^)]*)\)").unwrap()
    });

    const FORBIDDEN: &[&str] = &[
        "if", "match", "while", "loop", "for", "return", "println", "format",
    ];

    pub(super) fn extract(path: &str, source: &str) -> Option<super::GraphExtraction> {
        let mut extractor = SimpleExtractor::new(path, source);
        for capture in FN_RE.captures_iter(source) {
            let name = capture.name("name").unwrap().as_str();
            let params = capture.name("params").map(|m| m.as_str()).unwrap_or("");
            let full_match = capture.get(0).unwrap();
            let async_flag = capture.name("async").is_some();
            let visibility = if capture.name("prefix").is_some() {
                Some("public")
            } else {
                Some("private")
            };

            let brace_pos = source[full_match.end()..]
                .find('{')
                .map(|offset| full_match.end() + offset)
                .unwrap_or(full_match.end());

            let body_start = match source[brace_pos..].chars().next() {
                Some('{') => brace_pos,
                _ => continue,
            };

            let body_end = match find_matching_brace(source, body_start) {
                Some(end) => end,
                None => source.len(),
            };

            let metadata = serde_json::json!({
                "async": async_flag,
                "visibility": visibility,
            });
            let signature = format!("fn {}({})", name, params.trim());
            let function_id = extractor.add_function(
                name,
                "function",
                full_match.start(),
                body_end,
                Some(signature),
                Some(metadata),
            );
            extractor.push_scope(function_id);
            extractor.scan_calls(body_start, body_end, FORBIDDEN);
            extractor.pop_scope();
        }
        extractor.finish()
    }
}

mod python_simple {
    use super::SimpleExtractor;
    use once_cell::sync::Lazy;
    use regex::Regex;

    static DEF_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?m)^(?P<indent>[ \t]*)(?P<async>async\s+)?def\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\((?P<params>[^)]*)\):").unwrap()
    });

    const FORBIDDEN: &[&str] = &[
        "if", "for", "while", "return", "elif", "else", "with", "class", "await",
    ];

    pub(super) fn extract(path: &str, source: &str) -> Option<super::GraphExtraction> {
        let mut extractor = SimpleExtractor::new(path, source);
        for capture in DEF_RE.captures_iter(source) {
            let name = capture.name("name").unwrap().as_str();
            let params = capture.name("params").map(|m| m.as_str()).unwrap_or("");
            let def_match = capture.get(0).unwrap();
            let indent = capture
                .name("indent")
                .map(|m| m.as_str().len())
                .unwrap_or(0);
            let async_flag = capture.name("async").is_some();

            let body_start = match source[def_match.end()..].find('\n') {
                Some(offset) => def_match.end() + offset + 1,
                None => source.len(),
            };

            let mut scan_index = body_start;
            let mut body_end = source.len();
            let mut last_non_empty = body_start;
            while scan_index < source.len() {
                let next_newline = source[scan_index..]
                    .find('\n')
                    .map(|offset| scan_index + offset)
                    .unwrap_or(source.len());
                let line = &source[scan_index..next_newline];
                let trimmed = line.trim();

                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    let line_indent = line.len() - line.trim_start_matches([' ', '\t']).len();
                    if line_indent <= indent {
                        body_end = last_non_empty;
                        break;
                    }
                    last_non_empty = next_newline;
                }
                scan_index = if next_newline == source.len() {
                    next_newline
                } else {
                    next_newline + 1
                };
            }
            if body_end == source.len() {
                body_end = last_non_empty;
            }
            if body_start >= body_end {
                body_end = body_start;
            }

            let metadata = serde_json::json!({ "async": async_flag });
            let signature = format!("def {}({})", name, params.trim());
            let function_id = extractor.add_function(
                name,
                "function",
                def_match.start(),
                body_end,
                Some(signature),
                Some(metadata),
            );
            extractor.push_scope(function_id);
            extractor.scan_calls(body_start, body_end, FORBIDDEN);
            extractor.pop_scope();
        }
        extractor.finish()
    }
}

mod swift_simple {
    use super::{find_matching_brace, SimpleExtractor};
    use once_cell::sync::Lazy;
    use regex::Regex;

    static FUNC_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?m)^(?P<indent>\s*)(?P<prefix>[A-Za-z0-9_\s@:<>=\(\)\[\]]*?)func\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\((?P<params>[^)]*)\)").unwrap()
    });

    const FORBIDDEN: &[&str] = &[
        "if", "for", "while", "switch", "catch", "guard", "return", "init", "deinit",
    ];

    pub(super) fn extract(path: &str, source: &str) -> Option<super::GraphExtraction> {
        let mut extractor = SimpleExtractor::new(path, source);
        for capture in FUNC_RE.captures_iter(source) {
            let name = capture.name("name").unwrap().as_str();
            let params = capture.name("params").map(|m| m.as_str()).unwrap_or("");
            let full_match = capture.get(0).unwrap();

            let brace_pos = source[full_match.end()..]
                .find('{')
                .map(|offset| full_match.end() + offset)
                .unwrap_or(full_match.end());

            let body_start = match source[brace_pos..].chars().next() {
                Some('{') => brace_pos,
                _ => continue,
            };

            let body_end = match find_matching_brace(source, body_start) {
                Some(end) => end,
                None => source.len(),
            };

            let signature = format!("func {}({})", name, params.trim());
            let function_id = extractor.add_function(
                name,
                "function",
                full_match.start(),
                body_end,
                Some(signature),
                None,
            );
            extractor.push_scope(function_id);
            extractor.scan_calls(body_start, body_end, FORBIDDEN);
            extractor.pop_scope();
        }
        extractor.finish()
    }
}

mod typescript {
    use super::{stable_id, GraphEdge, GraphExtraction, GraphNode};
    use swc_common::{sync::Lrc, FileName, SourceMap, Span};
    use swc_ecma_ast::*;
    use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
    use swc_ecma_visit::{noop_visit_type, Visit, VisitWith};

    pub(super) fn extract(path: &str, source: &str) -> Option<GraphExtraction> {
        let cm: Lrc<SourceMap> = Default::default();
        let fm = cm.new_source_file(
            FileName::Custom(path.to_string()).into(),
            source.to_string(),
        );

        let lexer = Lexer::new(
            Syntax::Typescript(TsSyntax {
                tsx: path.ends_with(".tsx"),
                decorators: true,
                dts: false,
                no_early_errors: false,
                disallow_ambiguous_jsx_like: false,
            }),
            EsVersion::EsNext,
            StringInput::from(&*fm),
            None,
        );

        let mut parser = Parser::new_from(lexer);
        let module = match parser.parse_module() {
            Ok(module) => module,
            Err(_) => return None,
        };

        let mut extractor = GraphExtractor::new(path.to_string());
        module.visit_with(&mut extractor);
        let (nodes, edges) = extractor.into_parts();
        if nodes.len() <= 1 {
            None
        } else {
            Some(GraphExtraction { nodes, edges })
        }
    }

    struct GraphExtractor {
        file_path: String,
        nodes: Vec<GraphNode>,
        edges: Vec<GraphEdge>,
        scope_stack: Vec<String>,
        symbol_index: std::collections::HashMap<String, String>,
    }

    impl GraphExtractor {
        fn new(file_path: String) -> Self {
            let file_id = stable_id(&["file", &file_path]);
            let file_node = GraphNode {
                id: file_id.clone(),
                path: Some(file_path.clone()),
                kind: "file".to_string(),
                name: file_path.clone(),
                signature: None,
                range_start: None,
                range_end: None,
                metadata: None,
            };
            Self {
                file_path,
                nodes: vec![file_node],
                edges: Vec::new(),
                scope_stack: vec![file_id],
                symbol_index: std::collections::HashMap::new(),
            }
        }

        fn into_parts(self) -> (Vec<GraphNode>, Vec<GraphEdge>) {
            (self.nodes, self.edges)
        }

        fn current_scope(&self) -> Option<&String> {
            self.scope_stack.last()
        }

        fn push_scope(&mut self, id: String) {
            self.scope_stack.push(id);
        }

        fn pop_scope(&mut self) {
            self.scope_stack.pop();
        }

        fn span_offsets(&self, span: Span) -> (Option<i64>, Option<i64>) {
            (Some(span.lo.0 as i64), Some(span.hi.0 as i64))
        }

        fn create_function_node(
            &mut self,
            name: &str,
            kind: &str,
            param_count: usize,
            is_async: bool,
            is_generator: bool,
            span: Span,
        ) -> String {
            let (start, end) = self.span_offsets(span);
            let signature = Some(format!("{}({} params)", name, param_count));
            let metadata = serde_json::json!({
                "async": is_async,
                "generator": is_generator,
            });
            let id = stable_id(&[kind, &self.file_path, name, &format!("{:?}", start)]);
            self.nodes.push(GraphNode {
                id: id.clone(),
                path: Some(self.file_path.clone()),
                kind: kind.to_string(),
                name: name.to_string(),
                signature,
                range_start: start,
                range_end: end,
                metadata: Some(metadata),
            });
            self.symbol_index
                .entry(name.to_string())
                .or_insert(id.clone());
            id
        }

        fn ensure_symbol(&mut self, name: &str) -> String {
            if let Some(id) = self.symbol_index.get(name) {
                return id.clone();
            }
            let id = stable_id(&["symbol", name]);
            self.nodes.push(GraphNode {
                id: id.clone(),
                path: None,
                kind: "symbol".to_string(),
                name: name.to_string(),
                signature: None,
                range_start: None,
                range_end: None,
                metadata: None,
            });
            self.symbol_index.insert(name.to_string(), id.clone());
            id
        }

        fn record_call(&mut self, callee: &Expr, span: Span) {
            let name = match callee {
                Expr::Ident(ident) => ident.sym.to_string(),
                Expr::Member(member) => match &member.prop {
                    MemberProp::Ident(ident) => ident.sym.to_string(),
                    _ => return,
                },
                _ => return,
            };

            let target_id = self.ensure_symbol(&name);
            if let Some(scope_id) = self.current_scope() {
                let edge_id = stable_id(&[
                    "edge",
                    "calls",
                    scope_id,
                    &target_id,
                    &format!("{:?}", span.lo()),
                ]);
                self.edges.push(GraphEdge {
                    id: edge_id,
                    source_id: scope_id.clone(),
                    target_id,
                    edge_type: "calls".to_string(),
                    source_path: Some(self.file_path.clone()),
                    target_path: None,
                    metadata: None,
                });
            }
        }
    }

    impl Visit for GraphExtractor {
        noop_visit_type!();

        fn visit_fn_decl(&mut self, node: &FnDecl) {
            if node.declare || node.function.body.is_none() {
                return;
            }
            let fn_id = self.create_function_node(
                node.ident.sym.as_ref(),
                "function",
                node.function.params.len(),
                node.function.is_async,
                node.function.is_generator,
                node.function.span,
            );
            self.push_scope(fn_id);
            node.function.visit_with(self);
            self.pop_scope();
        }

        fn visit_class_method(&mut self, node: &ClassMethod) {
            if node.function.body.is_none() {
                return;
            }
            if let PropName::Ident(name) = &node.key {
                let fn_id = self.create_function_node(
                    name.sym.as_ref(),
                    "method",
                    node.function.params.len(),
                    node.function.is_async,
                    node.function.is_generator,
                    node.function.span,
                );
                self.push_scope(fn_id);
                node.function.visit_with(self);
                self.pop_scope();
            } else {
                node.function.visit_with(self);
            }
        }

        fn visit_constructor(&mut self, node: &Constructor) {
            if node.body.is_none() {
                return;
            }
            let fn_id = self.create_function_node(
                "constructor",
                "constructor",
                node.params.len(),
                false,
                false,
                node.span,
            );
            self.push_scope(fn_id.clone());
            node.visit_children_with(self);
            self.pop_scope();
        }

        fn visit_arrow_expr(&mut self, node: &ArrowExpr) {
            let name = format!("lambda_{}", self.nodes.len());
            let fn_id = self.create_function_node(
                &name,
                "lambda",
                node.params.len(),
                node.is_async,
                node.is_generator,
                node.span,
            );
            self.push_scope(fn_id.clone());
            node.visit_children_with(self);
            self.pop_scope();
        }

        fn visit_call_expr(&mut self, node: &CallExpr) {
            if let Callee::Expr(expr) = &node.callee {
                self.record_call(expr, node.span);
            }
            node.visit_children_with(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_extractor_discovers_functions() {
        let absolute = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bundle.rs");
        let source = std::fs::read_to_string(&absolute).expect("bundle.rs should exist");
        let relative = absolute
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('\\', "/");
        let extraction =
            rust_simple::extract(&relative, &source).expect("rust extractor should produce nodes");
        assert!(
            extraction
                .nodes
                .iter()
                .any(|node| node.name == "collect_snippets"),
            "collect_snippets not found in extraction"
        );
    }
}
