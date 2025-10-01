use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use swc_common::{sync::Lrc, FileName, SourceMap, Span};
use swc_ecma_ast::*;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{noop_visit_type, Visit, VisitWith};

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
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        FileName::Custom(relative_path.to_string()).into(),
        source.to_string(),
    );

    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: relative_path.ends_with(".tsx"),
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

    let mut extractor = GraphExtractor::new(relative_path.to_string());
    module.visit_with(&mut extractor);

    let (nodes, edges) = extractor.into_parts();

    Some(GraphExtraction { nodes, edges })
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
            &node.ident.sym.to_string(),
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
                &name.sym.to_string(),
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
        match &node.callee {
            Callee::Expr(expr) => self.record_call(expr, node.span),
            _ => {}
        }
        node.visit_children_with(self);
    }

    fn visit_module_item(&mut self, node: &ModuleItem) {
        node.visit_children_with(self);
    }
}

fn stable_id(inputs: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for input in inputs {
        hasher.update(input.as_bytes());
        hasher.update(&[0xff]);
    }
    format!("{:x}", hasher.finalize())
}
