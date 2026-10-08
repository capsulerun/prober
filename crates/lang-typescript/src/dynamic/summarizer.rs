//! Builds a module summary from the syntax tree, resolving every name through oxc_semantic.

use std::collections::{HashMap, HashSet};

use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::{
    Argument, ArrayExpression, ArrayExpressionElement, ArrowFunctionBody, AssignmentTarget,
    BinaryOperator, BindingPattern, CallExpression, ChainElement, Declaration,
    ExportDefaultDeclarationKind, Expression, FormalParameter, IdentifierReference,
    ImportDeclarationSpecifier, ImportOrExportKind, ModuleExportName, NewExpression,
    ObjectExpression, ObjectPattern, ObjectPropertyKind, Program, PropertyKind, Statement,
    StaticMemberExpression, TemplateLiteral, VariableDeclarator,
};
use oxc_parser::Parser;
use oxc_semantic::{AstNode, AstNodes, NodeId, Scoping, Semantic, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, SourceType, Span};
use prober_core::files::RepoPath;

use crate::dynamic::summary::{
    Access, Action, Comparison, Effect, Expr, FunctionIndex, FunctionSummary, ImportUse,
    ImportedName, ModuleSummary, Reach, UseKind,
};

const ARRAY_ITERATION_METHODS: [&str; 9] = [
    "map",
    "forEach",
    "flatMap",
    "filter",
    "some",
    "every",
    "find",
    "findIndex",
    "findLast",
];

const WORKER_CONSTRUCTORS: [&str; 2] = ["Worker", "SharedWorker"];

const FILE_ACCESS_FUNCTIONS: &[&str] = &[
    "access",
    "accessSync",
    "copy",
    "copyFile",
    "copyFileSync",
    "copySync",
    "cp",
    "cpSync",
    "createReadStream",
    "exists",
    "existsSync",
    "fastGlob",
    "fg",
    "glob",
    "globSync",
    "globby",
    "globbySync",
    "lstat",
    "lstatSync",
    "opendir",
    "opendirSync",
    "readFile",
    "readFileSync",
    "readJSON",
    "readJSONSync",
    "readJson",
    "readJsonSync",
    "readdir",
    "readdirSync",
    "stat",
    "statSync",
    "sync",
];

const MAX_EXPRESSION_DEPTH: usize = 64;

const MAX_EXPRESSION_TEXT_LENGTH: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryScope {
    Loads,
    Accesses,
}

pub(crate) fn summarize_module(
    path: &RepoPath,
    source_text: &str,
    scope: SummaryScope,
) -> ModuleSummary {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path.as_str()).unwrap_or_default();
    let parsed = Parser::new(&allocator, source_text, source_type).parse();

    summarize_program(&parsed.program, source_text, scope)
}

pub(crate) fn summarize_program<'a>(
    program: &'a Program<'a>,
    source_text: &'a str,
    scope: SummaryScope,
) -> ModuleSummary {
    let semantic = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(program)
        .semantic;

    Summarizer::new(&semantic, source_text, scope).summarize(&program.body)
}

struct Summarizer<'s, 'a> {
    semantic: &'s Semantic<'a>,
    source_text: &'a str,
    scope: SummaryScope,
    line_starts: Vec<usize>,
    function_indices: HashMap<NodeId, FunctionIndex>,
    function_parents: Vec<Option<FunctionIndex>>,
    symbol_values: HashMap<SymbolId, Expr>,
    symbols_in_progress: Vec<SymbolId>,
    depth: usize,
}

impl<'s, 'a> Summarizer<'s, 'a> {
    fn new(semantic: &'s Semantic<'a>, source_text: &'a str, scope: SummaryScope) -> Self {
        let line_starts = std::iter::once(0)
            .chain(
                source_text
                    .match_indices('\n')
                    .map(|(newline_offset, _)| newline_offset + 1),
            )
            .collect();

        Self {
            semantic,
            source_text,
            scope,
            line_starts,
            function_indices: HashMap::new(),
            function_parents: Vec::new(),
            symbol_values: HashMap::new(),
            symbols_in_progress: Vec::new(),
            depth: 0,
        }
    }

    fn nodes(&self) -> &'s AstNodes<'a> {
        self.semantic.nodes()
    }

    fn scoping(&self) -> &'s Scoping {
        self.semantic.scoping()
    }

    fn summarize(mut self, program_body: &'a [Statement<'a>]) -> ModuleSummary {
        let function_nodes = self.index_functions();
        let mut summary = ModuleSummary {
            functions: function_nodes
                .iter()
                .map(|&function_node| self.function_summary(function_node))
                .collect(),
            ..ModuleSummary::default()
        };

        for node in self.nodes().iter() {
            self.record_node(node, &mut summary);
        }

        for statement in program_body {
            self.record_module_statement(statement, &mut summary);
        }

        if self.scope == SummaryScope::Accesses {
            self.record_reachable_returns(&function_nodes, &mut summary);
        }

        summary
    }

    fn record_reachable_returns(&mut self, function_nodes: &[NodeId], summary: &mut ModuleSummary) {
        let mut return_statements: HashMap<FunctionIndex, Vec<NodeId>> = HashMap::new();

        for node in self.nodes().iter() {
            if let AstKind::ReturnStatement(_) = node.kind()
                && let Some(function) = self.enclosing_function(node.id())
            {
                return_statements
                    .entry(function)
                    .or_default()
                    .push(node.id());
            }
        }

        let mut pending = Vec::new();
        let mut recorded = HashSet::new();

        for access in &summary.accesses {
            referenced_functions(&access.callee, &mut pending);

            for argument in &access.arguments {
                referenced_functions(argument, &mut pending);
            }
        }

        for (_, exported) in &summary.exports {
            referenced_functions(exported, &mut pending);
        }

        while let Some(function) = pending.pop() {
            if !recorded.insert(function) {
                continue;
            }

            let returns = self.returns_of(function_nodes[function], &return_statements, function);

            for returned in returns.iter().chain(
                summary.functions[function]
                    .parameter_defaults
                    .iter()
                    .flatten(),
            ) {
                referenced_functions(returned, &mut pending);
            }

            summary.functions[function].returns = returns;
        }
    }

    fn returns_of(
        &mut self,
        function_node: NodeId,
        return_statements: &HashMap<FunctionIndex, Vec<NodeId>>,
        function: FunctionIndex,
    ) -> Vec<Expr> {
        if let AstKind::ArrowFunctionExpression(arrow) = self.nodes().kind(function_node)
            && let Some(expression_body) = match &arrow.body {
                ArrowFunctionBody::FunctionBody(_) => None,
                expression_body => expression_body.as_expression(),
            }
        {
            return vec![self.expression(expression_body)];
        }

        let mut returns = Vec::new();

        for &statement_id in return_statements.get(&function).into_iter().flatten() {
            if let AstKind::ReturnStatement(return_statement) = self.nodes().kind(statement_id)
                && let Some(argument) = &return_statement.argument
            {
                returns.push(self.expression(argument));
            }
        }

        returns
    }

    fn index_functions(&mut self) -> Vec<NodeId> {
        let function_nodes: Vec<NodeId> = self
            .nodes()
            .iter()
            .filter(|node| is_function_node(node.kind()))
            .map(AstNode::id)
            .collect();

        for (function_index, &function_node) in function_nodes.iter().enumerate() {
            self.function_indices.insert(function_node, function_index);
        }

        self.function_parents = function_nodes
            .iter()
            .map(|&function_node| self.enclosing_function(function_node))
            .collect();

        function_nodes
    }

    fn enclosing_function(&self, node_id: NodeId) -> Option<FunctionIndex> {
        self.nodes()
            .ancestor_ids(node_id)
            .find_map(|ancestor_id| self.function_indices.get(&ancestor_id).copied())
    }

    fn function_summary(&mut self, function_node: NodeId) -> FunctionSummary {
        let index = self.function_indices[&function_node];
        let (parameters, expression_body) = match self.nodes().kind(function_node) {
            AstKind::Function(function) => (&function.params.items, None),
            AstKind::ArrowFunctionExpression(arrow) => (
                &arrow.params.items,
                match &arrow.body {
                    ArrowFunctionBody::FunctionBody(_) => None,
                    expression_body => expression_body.as_expression(),
                },
            ),
            _ => unreachable!("function nodes are functions or arrow functions"),
        };
        let parameter_defaults = parameters
            .iter()
            .map(|parameter| {
                parameter
                    .initializer
                    .as_deref()
                    .map(|initializer| self.expression(initializer))
            })
            .collect();
        if self.scope == SummaryScope::Accesses {
            return FunctionSummary {
                parent: self.function_parents[index],
                parameter_defaults,
                returns: Vec::new(),
                reach: Reach::Nested,
            };
        }

        let returns = expression_body
            .map(|expression| self.expression(expression))
            .into_iter()
            .collect();

        FunctionSummary {
            parent: self.function_parents[index],
            parameter_defaults,
            returns,
            reach: self.reach(function_node),
        }
    }

    fn reach(&self, function_node: NodeId) -> Reach {
        let holder_id = self.holder(function_node);
        let declared_name = match self.nodes().kind(function_node) {
            AstKind::Function(function) => function.id.as_ref(),
            _ => None,
        };

        match self.nodes().kind(holder_id) {
            AstKind::VariableDeclarator(declarator) => match &declarator.id {
                BindingPattern::BindingIdentifier(binding) => {
                    let declaration_id = self.nodes().parent_id(holder_id);
                    let exported_as = self
                        .is_exported_declaration(declaration_id)
                        .then(|| binding.name.to_string());

                    self.named_reach(binding.symbol_id.get(), exported_as)
                }
                _ => Reach::Nested,
            },
            AstKind::FormalParameter(parameter) => {
                self.parameter_default_reach(holder_id, parameter)
            }
            AstKind::ExportDefaultDeclaration(_) => self.named_reach(
                declared_name.and_then(|binding| binding.symbol_id.get()),
                Some("default".to_owned()),
            ),
            AstKind::ExportDeclaration(_) => self.named_reach(
                declared_name.and_then(|binding| binding.symbol_id.get()),
                declared_name.map(|binding| binding.name.to_string()),
            ),
            _ => match self.nodes().kind(function_node) {
                AstKind::Function(function) if function.is_declaration() => self.named_reach(
                    declared_name.and_then(|binding| binding.symbol_id.get()),
                    None,
                ),
                _ => Reach::Nested,
            },
        }
    }

    fn named_reach(&self, symbol: Option<SymbolId>, exported_as: Option<String>) -> Reach {
        let mut exported_names: Vec<String> = exported_as.into_iter().collect();
        let mut escapes = false;

        for use_kind in symbol
            .into_iter()
            .flat_map(|symbol| self.symbol_uses(symbol))
        {
            match use_kind {
                UseKind::Call => {}
                UseKind::Exported(exported_name) => exported_names.push(exported_name),
                UseKind::ReexportAll | UseKind::Escape => escapes = true,
            }
        }

        Reach::Named {
            exported_as: exported_names,
            escapes,
        }
    }

    fn parameter_default_reach(
        &self,
        parameter_id: NodeId,
        parameter: &FormalParameter<'a>,
    ) -> Reach {
        let Some((function, index)) = self.parameter_position(parameter_id) else {
            return Reach::Nested;
        };
        let escapes = match &parameter.pattern {
            BindingPattern::BindingIdentifier(binding) => binding
                .symbol_id
                .get()
                .into_iter()
                .flat_map(|symbol| self.symbol_uses(symbol))
                .any(|use_kind| use_kind != UseKind::Call),
            _ => true,
        };

        Reach::ParameterDefault {
            function,
            index,
            escapes,
        }
    }

    fn parameter_position(&self, parameter_id: NodeId) -> Option<(FunctionIndex, usize)> {
        let parameters_id = self.nodes().parent_id(parameter_id);
        let AstKind::FormalParameters(parameters) = self.nodes().kind(parameters_id) else {
            return None;
        };
        let parameter_span = self.nodes().kind(parameter_id).span();
        let index = parameters
            .items
            .iter()
            .position(|item| item.span == parameter_span)?;
        let function = *self
            .function_indices
            .get(&self.nodes().parent_id(parameters_id))?;

        Some((function, index))
    }

    fn is_exported_declaration(&self, declaration_id: NodeId) -> bool {
        matches!(
            self.nodes().parent_kind(declaration_id),
            AstKind::ExportDeclaration(_)
        )
    }

    fn holder(&self, node_id: NodeId) -> NodeId {
        let mut child_id = node_id;

        loop {
            let parent_id = self.nodes().parent_id(child_id);

            if !is_transparent_wrapper(self.nodes().kind(parent_id)) {
                return parent_id;
            }

            child_id = parent_id;
        }
    }

    fn symbol_uses(&self, symbol: SymbolId) -> Vec<UseKind> {
        self.scoping()
            .get_resolved_references(symbol)
            .filter(|reference| !reference.is_type())
            .map(|reference| self.use_kind(reference.node_id()))
            .collect()
    }

    fn use_kind(&self, node_id: NodeId) -> UseKind {
        let mut child_id = node_id;

        loop {
            let parent_id = self.nodes().parent_id(child_id);

            match self.nodes().kind(parent_id) {
                parent_kind if is_transparent_wrapper(parent_kind) => child_id = parent_id,
                AstKind::CallExpression(call) => {
                    return if call.callee.span() == self.nodes().kind(child_id).span() {
                        UseKind::Call
                    } else {
                        UseKind::Escape
                    };
                }
                AstKind::ExportSpecifier(specifier) => {
                    return UseKind::Exported(specifier.exported.name().to_string());
                }
                AstKind::ExportDefaultDeclaration(_) => {
                    return UseKind::Exported("default".to_owned());
                }
                _ => return UseKind::Escape,
            }
        }
    }

    fn namespace_uses(&self, symbol: SymbolId) -> Vec<(ImportedName, UseKind)> {
        self.scoping()
            .get_resolved_references(symbol)
            .filter(|reference| !reference.is_type())
            .map(|reference| {
                let reference_id = reference.node_id();
                let parent_id = self.nodes().parent_id(reference_id);

                match self.nodes().kind(parent_id) {
                    AstKind::StaticMemberExpression(member)
                        if member.object.span() == self.nodes().kind(reference_id).span() =>
                    {
                        (
                            ImportedName::Named(member.property.name.to_string()),
                            self.use_kind(parent_id),
                        )
                    }
                    _ => (ImportedName::Namespace, UseKind::Escape),
                }
            })
            .collect()
    }

    fn record_node(&mut self, node: &AstNode<'a>, summary: &mut ModuleSummary) {
        if self.scope == SummaryScope::Accesses {
            if let AstKind::CallExpression(call) = node.kind() {
                self.record_access(call, summary);
            }

            return;
        }

        match node.kind() {
            AstKind::ReturnStatement(return_statement) => {
                let Some(argument) = &return_statement.argument else {
                    return;
                };

                if let Some(function) = self.enclosing_function(node.id()) {
                    let returned = self.expression(argument);

                    summary.functions[function].returns.push(returned);
                }
            }
            AstKind::ImportExpression(import_expression) => {
                match literal_text(&import_expression.source) {
                    Some(specifier) => summary.import_uses.push(module_object_use(specifier)),
                    None => {
                        let specifier = self.expression(&import_expression.source);

                        self.record_effect(
                            summary,
                            node.id(),
                            import_expression.source.span(),
                            Action::Import(specifier),
                        );
                    }
                }
            }
            AstKind::CallExpression(call) => self.record_call(node.id(), call, summary),
            AstKind::NewExpression(new_expression) => {
                self.record_worker(node.id(), new_expression, summary);
            }
            _ => {}
        }
    }

    fn record_call(
        &mut self,
        node_id: NodeId,
        call: &'a CallExpression<'a>,
        summary: &mut ModuleSummary,
    ) {
        if call.arguments.is_empty() {
            return;
        }

        let callee = call.callee.get_inner_expression();

        if let Expression::Identifier(reference) = callee
            && self.symbol_of(reference).is_none()
            && reference.name == "require"
            && let Some(specifier) = call
                .arguments
                .first()
                .and_then(Argument::as_expression)
                .and_then(literal_text)
        {
            self.record_required_module_uses(node_id, specifier, summary);

            return;
        }

        if !self.may_load_through(callee) {
            return;
        }

        let action = Action::Call {
            callee: self.expression(&call.callee),
            arguments: self.arguments(&call.arguments),
        };

        self.record_effect(summary, node_id, call.span, action);
    }

    fn record_access(&mut self, call: &'a CallExpression<'a>, summary: &mut ModuleSummary) {
        if call.arguments.is_empty() || !may_access_files(call.callee.get_inner_expression()) {
            return;
        }

        let access = Access {
            line: self.line_of(call.span.start),
            callee: self.expression(&call.callee),
            arguments: self.arguments(&call.arguments),
        };

        summary.accesses.push(access);
    }

    fn may_load_through(&self, callee: &Expression<'a>) -> bool {
        match callee {
            Expression::Identifier(reference) => {
                self.symbol_of(reference).is_some() || reference.name == "require"
            }
            Expression::StaticMemberExpression(member) => {
                match member.object.get_inner_expression() {
                    Expression::Identifier(object) => {
                        self.symbol_of(object).is_some_and(|symbol| {
                            matches!(
                                self.declaration_kind(symbol),
                                AstKind::ImportNamespaceSpecifier(_)
                                    | AstKind::ImportDefaultSpecifier(_)
                                    | AstKind::ImportSpecifier(_)
                            )
                        })
                    }
                    _ => false,
                }
            }
            Expression::CallExpression(_) => true,
            _ => false,
        }
    }

    fn record_required_module_uses(
        &self,
        call_id: NodeId,
        specifier: &str,
        summary: &mut ModuleSummary,
    ) {
        let holder_id = self.holder(call_id);
        let destructured = match self.nodes().kind(holder_id) {
            AstKind::VariableDeclarator(declarator) => match &declarator.id {
                BindingPattern::ObjectPattern(pattern) => Some(pattern),
                _ => None,
            },
            AstKind::ExpressionStatement(_) => return,
            _ => None,
        };
        let Some(pattern) = destructured else {
            summary.import_uses.push(module_object_use(specifier));

            return;
        };

        for property in &pattern.properties {
            let (Some(key), BindingPattern::BindingIdentifier(binding)) =
                (property.key.static_name(), &property.value)
            else {
                summary.import_uses.push(module_object_use(specifier));

                continue;
            };

            for use_kind in binding
                .symbol_id
                .get()
                .into_iter()
                .flat_map(|symbol| self.symbol_uses(symbol))
            {
                summary.import_uses.push(ImportUse {
                    specifier: specifier.to_owned(),
                    name: ImportedName::Named(key.to_string()),
                    kind: use_kind,
                });
            }
        }
    }

    fn record_worker(
        &mut self,
        node_id: NodeId,
        new_expression: &'a NewExpression<'a>,
        summary: &mut ModuleSummary,
    ) {
        let Expression::Identifier(constructor) = new_expression.callee.get_inner_expression()
        else {
            return;
        };
        let is_worker = self.symbol_of(constructor).is_none()
            && WORKER_CONSTRUCTORS.contains(&constructor.name.as_str());
        let Some(script) = new_expression
            .arguments
            .first()
            .and_then(Argument::as_expression)
        else {
            return;
        };

        if !is_worker || evaluates_code(new_expression.arguments.get(1)) {
            return;
        }

        let specifier = self.expression(script);

        self.record_effect(summary, node_id, script.span(), Action::Worker(specifier));
    }

    fn record_effect(
        &self,
        summary: &mut ModuleSummary,
        node_id: NodeId,
        span: Span,
        action: Action,
    ) {
        let unit = self
            .enclosing_function(node_id)
            .and_then(|function| summary.unit_of(function));

        summary.effects.push(Effect {
            line: self.line_of(span.start),
            unit,
            expression: self.text_of(span),
            action,
        });
    }

    fn record_module_statement(
        &mut self,
        statement: &'a Statement<'a>,
        summary: &mut ModuleSummary,
    ) {
        match statement {
            Statement::ImportDeclaration(import_declaration) => {
                if import_declaration.import_kind == ImportOrExportKind::Type
                    || self.scope == SummaryScope::Accesses
                {
                    return;
                }

                let specifier = import_declaration.source.value.to_string();

                for import_specifier in import_declaration.specifiers.iter().flatten() {
                    self.record_import_specifier_uses(&specifier, import_specifier, summary);
                }
            }
            Statement::ExportDeclaration(export_declaration) => {
                self.record_declaration_exports(&export_declaration.declaration, summary);
            }
            Statement::ExportNamedDeclaration(export_declaration) => {
                if export_declaration.export_kind == ImportOrExportKind::Type {
                    return;
                }

                for export_specifier in &export_declaration.specifiers {
                    if let ModuleExportName::IdentifierReference(local) = &export_specifier.local {
                        let value = self.identifier(local);

                        summary
                            .exports
                            .push((export_specifier.exported.name().to_string(), value));
                    }
                }
            }
            Statement::ExportFromDeclaration(export_declaration) => {
                if export_declaration.export_kind == ImportOrExportKind::Type {
                    return;
                }

                let specifier = export_declaration.source.value.to_string();

                for export_specifier in &export_declaration.specifiers {
                    let exported_name = export_specifier.exported.name().to_string();
                    let name = imported_name(&export_specifier.local.name());

                    summary.import_uses.push(ImportUse {
                        specifier: specifier.clone(),
                        name: name.clone(),
                        kind: UseKind::Exported(exported_name.clone()),
                    });
                    summary.exports.push((
                        exported_name,
                        Expr::Imported {
                            specifier: specifier.clone(),
                            name,
                        },
                    ));
                }
            }
            Statement::ExportAllDeclaration(export_declaration) => {
                if export_declaration.export_kind == ImportOrExportKind::Type {
                    return;
                }

                let specifier = export_declaration.source.value.to_string();

                match &export_declaration.exported {
                    Some(exported) => {
                        summary.import_uses.push(module_object_use(&specifier));
                        summary.exports.push((
                            exported.name().to_string(),
                            Expr::Imported {
                                specifier,
                                name: ImportedName::Namespace,
                            },
                        ));
                    }
                    None => {
                        summary.import_uses.push(ImportUse {
                            specifier: specifier.clone(),
                            name: ImportedName::Namespace,
                            kind: UseKind::ReexportAll,
                        });
                        summary.star_exports.push(specifier);
                    }
                }
            }
            Statement::ExpressionStatement(expression_statement) => {
                if let Expression::AssignmentExpression(assignment) =
                    &expression_statement.expression
                    && is_module_exports(&assignment.left)
                {
                    let value = self.expression(&assignment.right);

                    summary.exports.push(("default".to_owned(), value));
                }
            }
            Statement::TSExportAssignment(export_assignment) => {
                let value = self.expression(&export_assignment.expression);

                summary.exports.push(("default".to_owned(), value));
            }
            Statement::ExportDefaultDeclaration(export_declaration) => {
                let value = match &export_declaration.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                        self.function_at(function.node_id.get())
                    }
                    declaration => declaration
                        .as_expression()
                        .map_or(Expr::Unknown, |expression| self.expression(expression)),
                };

                summary.exports.push(("default".to_owned(), value));
            }
            _ => {}
        }
    }

    fn record_import_specifier_uses(
        &self,
        specifier: &str,
        import_specifier: &ImportDeclarationSpecifier<'a>,
        summary: &mut ModuleSummary,
    ) {
        let (name, binding) = match import_specifier {
            ImportDeclarationSpecifier::ImportSpecifier(named) => {
                if named.import_kind == ImportOrExportKind::Type {
                    return;
                }

                (imported_name(&named.imported.name()), &named.local)
            }
            ImportDeclarationSpecifier::ImportDefaultSpecifier(default) => {
                (ImportedName::Default, &default.local)
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(namespace) => {
                (ImportedName::Namespace, &namespace.local)
            }
        };
        let Some(symbol) = binding.symbol_id.get() else {
            return;
        };
        let uses = match name {
            ImportedName::Namespace => self.namespace_uses(symbol),
            name => self
                .symbol_uses(symbol)
                .into_iter()
                .map(|use_kind| (name.clone(), use_kind))
                .collect(),
        };

        for (name, kind) in uses {
            summary.import_uses.push(ImportUse {
                specifier: specifier.to_owned(),
                name,
                kind,
            });
        }
    }

    fn record_declaration_exports(
        &mut self,
        declaration: &'a Declaration<'a>,
        summary: &mut ModuleSummary,
    ) {
        match declaration {
            Declaration::FunctionDeclaration(function) => {
                if let Some(binding) = &function.id {
                    let value = self.function_at(function.node_id.get());

                    summary.exports.push((binding.name.to_string(), value));
                }
            }
            Declaration::VariableDeclaration(variable_declaration) => {
                for declarator in &variable_declaration.declarations {
                    if let BindingPattern::BindingIdentifier(binding) = &declarator.id
                        && let Some(symbol) = binding.symbol_id.get()
                    {
                        let value = self.symbol_value(symbol);

                        summary.exports.push((binding.name.to_string(), value));
                    }
                }
            }
            _ => {}
        }
    }

    fn expression(&mut self, expression: &'a Expression<'a>) -> Expr {
        if self.depth >= MAX_EXPRESSION_DEPTH {
            return Expr::Unknown;
        }

        self.depth += 1;

        let value = self.expression_value(expression);

        self.depth -= 1;

        value
    }

    fn expression_value(&mut self, expression: &'a Expression<'a>) -> Expr {
        match expression.get_inner_expression() {
            Expression::StringLiteral(literal) => Expr::Text(literal.value.to_string()),
            Expression::BooleanLiteral(literal) => Expr::Boolean(literal.value),
            Expression::TemplateLiteral(template) => self.template(template),
            Expression::BinaryExpression(binary) if binary.operator == BinaryOperator::Addition => {
                Expr::Concat(vec![
                    self.expression(&binary.left),
                    self.expression(&binary.right),
                ])
            }
            Expression::LogicalExpression(logical) => Expr::Choice(vec![
                self.expression(&logical.left),
                self.expression(&logical.right),
            ]),
            Expression::ConditionalExpression(conditional) => Expr::Conditional {
                comparison: self.comparison(&conditional.test),
                consequent: Box::new(self.expression(&conditional.consequent)),
                alternate: Box::new(self.expression(&conditional.alternate)),
            },
            Expression::AwaitExpression(await_expression) => {
                self.expression(&await_expression.argument)
            }
            Expression::SequenceExpression(sequence) => sequence
                .expressions
                .last()
                .map_or(Expr::Unknown, |last| self.expression(last)),
            Expression::Identifier(reference) => self.identifier(reference),
            Expression::StaticMemberExpression(member) => self.static_member(member),
            Expression::ComputedMemberExpression(member) => {
                match literal_text(&member.expression) {
                    Some(property) => Expr::Member {
                        object: Box::new(self.expression(&member.object)),
                        property: property.to_owned(),
                    },
                    None => Expr::Unknown,
                }
            }
            Expression::CallExpression(call) => self.call(call),
            Expression::ChainExpression(chain) => match &chain.expression {
                ChainElement::CallExpression(call) => self.call(call),
                ChainElement::StaticMemberExpression(member) => self.static_member(member),
                _ => Expr::Unknown,
            },
            Expression::NewExpression(new_expression) => Expr::New {
                callee: Box::new(self.expression(&new_expression.callee)),
                arguments: self.arguments(&new_expression.arguments),
            },
            Expression::ArrayExpression(array) => self.array(array),
            Expression::ObjectExpression(object) => self.object(object),
            Expression::ArrowFunctionExpression(arrow) => self.function_at(arrow.node_id.get()),
            Expression::FunctionExpression(function) => self.function_at(function.node_id.get()),
            _ => Expr::Unknown,
        }
    }

    fn template(&mut self, template: &'a TemplateLiteral<'a>) -> Expr {
        let mut parts = Vec::new();

        for (position, quasi) in template.quasis.iter().enumerate() {
            parts.push(
                quasi
                    .value
                    .cooked
                    .as_ref()
                    .map_or(Expr::Unknown, |cooked| Expr::Text(cooked.to_string())),
            );

            if let Some(expression) = template.expressions.get(position) {
                parts.push(self.expression(expression));
            }
        }

        Expr::Concat(parts)
    }

    fn comparison(&mut self, test: &'a Expression<'a>) -> Option<Comparison> {
        let Expression::BinaryExpression(binary) = test.get_inner_expression() else {
            return None;
        };
        let equal = match binary.operator {
            BinaryOperator::Equality | BinaryOperator::StrictEquality => true,
            BinaryOperator::Inequality | BinaryOperator::StrictInequality => false,
            _ => return None,
        };

        Some(Comparison {
            left: Box::new(self.expression(&binary.left)),
            right: Box::new(self.expression(&binary.right)),
            equal,
        })
    }

    fn static_member(&mut self, member: &'a StaticMemberExpression<'a>) -> Expr {
        if let Expression::ImportMeta(_) = member.object.get_inner_expression() {
            return match member.property.name.as_str() {
                "url" | "filename" => Expr::ModuleFile,
                "dirname" => Expr::ModuleDirectory,
                "resolve" => Expr::ModuleResolve,
                _ => Expr::Unknown,
            };
        }

        Expr::Member {
            object: Box::new(self.expression(&member.object)),
            property: member.property.name.to_string(),
        }
    }

    fn call(&mut self, call: &'a CallExpression<'a>) -> Expr {
        Expr::Call {
            callee: Box::new(self.expression(&call.callee)),
            arguments: self.arguments(&call.arguments),
        }
    }

    fn arguments(&mut self, arguments: &'a [Argument<'a>]) -> Vec<Expr> {
        arguments
            .iter()
            .map(|argument| {
                argument
                    .as_expression()
                    .map_or(Expr::Unknown, |expression| self.expression(expression))
            })
            .collect()
    }

    fn array(&mut self, array: &'a ArrayExpression<'a>) -> Expr {
        let elements = array
            .elements
            .iter()
            .map(|element| match element {
                ArrayExpressionElement::SpreadElement(spread) => {
                    Expr::Spread(Box::new(self.expression(&spread.argument)))
                }
                ArrayExpressionElement::Elision(_) => Expr::Unknown,
                element => element
                    .as_expression()
                    .map_or(Expr::Unknown, |expression| self.expression(expression)),
            })
            .collect();

        Expr::Array(elements)
    }

    fn object(&mut self, object: &'a ObjectExpression<'a>) -> Expr {
        let mut parts = Vec::new();
        let mut properties = Vec::new();

        for property in &object.properties {
            let property = match property {
                ObjectPropertyKind::ObjectProperty(property) => property,
                ObjectPropertyKind::SpreadProperty(spread) => {
                    if !properties.is_empty() {
                        parts.push(Expr::Object(std::mem::take(&mut properties)));
                    }

                    parts.push(self.expression(&spread.argument));

                    continue;
                }
            };

            if property.kind != PropertyKind::Init {
                return Expr::Unknown;
            }

            let Some(key) = property.key.static_name() else {
                return Expr::Unknown;
            };
            let value = self.expression(&property.value);

            properties.push((key.to_string(), value));
        }

        if parts.is_empty() {
            return Expr::Object(properties);
        }

        if !properties.is_empty() {
            parts.push(Expr::Object(properties));
        }

        Expr::Merge(parts)
    }

    fn function_at(&self, node_id: NodeId) -> Expr {
        self.function_indices
            .get(&node_id)
            .map_or(Expr::Unknown, |&function| Expr::Function(function))
    }

    fn symbol_of(&self, reference: &IdentifierReference<'a>) -> Option<SymbolId> {
        reference
            .reference_id
            .get()
            .and_then(|reference_id| self.scoping().get_reference(reference_id).symbol_id())
    }

    fn declaration_kind(&self, symbol: SymbolId) -> AstKind<'a> {
        self.nodes().kind(self.scoping().symbol_declaration(symbol))
    }

    fn identifier(&mut self, reference: &'a IdentifierReference<'a>) -> Expr {
        match self.symbol_of(reference) {
            Some(symbol) => self.symbol_value(symbol),
            None => match reference.name.as_str() {
                "__dirname" => Expr::ModuleDirectory,
                "__filename" => Expr::ModuleFile,
                name => Expr::Global(name.to_owned()),
            },
        }
    }

    fn symbol_value(&mut self, symbol: SymbolId) -> Expr {
        if let Some(value) = self.symbol_values.get(&symbol) {
            return value.clone();
        }

        if self.symbols_in_progress.contains(&symbol) {
            return Expr::Unknown;
        }

        self.symbols_in_progress.push(symbol);

        let value = self.declared_value(symbol);

        self.symbols_in_progress.pop();
        self.symbol_values.insert(symbol, value.clone());

        value
    }

    fn declared_value(&mut self, symbol: SymbolId) -> Expr {
        let declaration_id = self.scoping().symbol_declaration(symbol);

        match self.nodes().kind(declaration_id) {
            AstKind::VariableDeclarator(declarator) => {
                self.declarator_value(declaration_id, declarator, symbol)
            }
            AstKind::FormalParameter(parameter) => match &parameter.pattern {
                BindingPattern::BindingIdentifier(_) => self.parameter_value(declaration_id),
                BindingPattern::ObjectPattern(pattern) => {
                    self.destructured_parameter_value(declaration_id, pattern, symbol)
                }
                _ => Expr::Unknown,
            },
            AstKind::Function(_) => self.function_at(declaration_id),
            AstKind::ImportSpecifier(named) if named.import_kind == ImportOrExportKind::Value => {
                self.imported_value(declaration_id, imported_name(&named.imported.name()))
            }
            AstKind::ImportDefaultSpecifier(_) => {
                self.imported_value(declaration_id, ImportedName::Default)
            }
            AstKind::ImportNamespaceSpecifier(_) => {
                self.imported_value(declaration_id, ImportedName::Namespace)
            }
            _ => Expr::Unknown,
        }
    }

    fn imported_value(&self, specifier_id: NodeId, name: ImportedName) -> Expr {
        match self.nodes().parent_kind(specifier_id) {
            AstKind::ImportDeclaration(import_declaration)
                if import_declaration.import_kind == ImportOrExportKind::Value =>
            {
                Expr::Imported {
                    specifier: import_declaration.source.value.to_string(),
                    name,
                }
            }
            _ => Expr::Unknown,
        }
    }

    fn declarator_value(
        &mut self,
        declarator_id: NodeId,
        declarator: &'a VariableDeclarator<'a>,
        symbol: SymbolId,
    ) -> Expr {
        let declaration_id = self.nodes().parent_id(declarator_id);

        if let AstKind::ForOfStatement(for_of) = self.nodes().parent_kind(declaration_id) {
            return match &declarator.id {
                BindingPattern::BindingIdentifier(_) => {
                    Expr::ElementOf(Box::new(self.expression(&for_of.right)))
                }
                _ => Expr::Unknown,
            };
        }

        if self.scoping().symbol_is_mutated(symbol) {
            return Expr::Unknown;
        }

        let Some(initializer) = &declarator.init else {
            return Expr::Unknown;
        };

        match &declarator.id {
            BindingPattern::BindingIdentifier(_) => self.expression(initializer),
            BindingPattern::ObjectPattern(pattern) => {
                self.destructured_value(initializer, pattern, symbol)
            }
            _ => Expr::Unknown,
        }
    }

    fn destructured_value(
        &mut self,
        initializer: &'a Expression<'a>,
        pattern: &'a ObjectPattern<'a>,
        symbol: SymbolId,
    ) -> Expr {
        for property in &pattern.properties {
            let Some(key) = property.key.static_name() else {
                continue;
            };
            let default = match &property.value {
                BindingPattern::BindingIdentifier(binding)
                    if binding.symbol_id.get() == Some(symbol) =>
                {
                    None
                }
                BindingPattern::AssignmentPattern(assignment) => match &assignment.left {
                    BindingPattern::BindingIdentifier(binding)
                        if binding.symbol_id.get() == Some(symbol) =>
                    {
                        Some(&assignment.right)
                    }
                    _ => continue,
                },
                _ => continue,
            };
            let member = Expr::Member {
                object: Box::new(self.expression(initializer)),
                property: key.to_string(),
            };

            return match default {
                Some(default) => Expr::Choice(vec![member, self.expression(default)]),
                None => member,
            };
        }

        Expr::Unknown
    }

    fn parameter_value(&mut self, parameter_id: NodeId) -> Expr {
        let Some((function, index)) = self.parameter_position(parameter_id) else {
            return Expr::Unknown;
        };
        let function_id = self.nodes().parent_id(self.nodes().parent_id(parameter_id));

        if index == 0
            && let Some(array) = self.iterated_array(function_id)
        {
            return Expr::ElementOf(Box::new(array));
        }

        Expr::Parameter { function, index }
    }

    fn destructured_parameter_value(
        &mut self,
        parameter_id: NodeId,
        pattern: &'a ObjectPattern<'a>,
        symbol: SymbolId,
    ) -> Expr {
        let Some((_, 0)) = self.parameter_position(parameter_id) else {
            return Expr::Unknown;
        };
        let function_id = self.nodes().parent_id(self.nodes().parent_id(parameter_id));
        let Some(array) = self.iterated_array(function_id) else {
            return Expr::Unknown;
        };
        let key = pattern
            .properties
            .iter()
            .find_map(|property| match &property.value {
                BindingPattern::BindingIdentifier(binding)
                    if binding.symbol_id.get() == Some(symbol) =>
                {
                    property.key.static_name()
                }
                _ => None,
            });

        match key {
            Some(key) => Expr::Member {
                object: Box::new(Expr::ElementOf(Box::new(array))),
                property: key.to_string(),
            },
            None => Expr::Unknown,
        }
    }

    fn iterated_array(&mut self, function_id: NodeId) -> Option<Expr> {
        let holder_id = self.holder(function_id);
        let AstKind::CallExpression(call) = self.nodes().kind(holder_id) else {
            return None;
        };
        let function_span = self.nodes().kind(function_id).span();
        let is_first_argument = call
            .arguments
            .first()
            .and_then(Argument::as_expression)
            .is_some_and(|argument| argument.get_inner_expression().span() == function_span);
        let Expression::StaticMemberExpression(method) = call.callee.get_inner_expression() else {
            return None;
        };

        if !is_first_argument || !ARRAY_ITERATION_METHODS.contains(&method.property.name.as_str()) {
            return None;
        }

        Some(self.expression(&method.object))
    }

    fn line_of(&self, offset: u32) -> u32 {
        let line_index = self
            .line_starts
            .partition_point(|&line_start| line_start <= offset as usize);

        u32::try_from(line_index).unwrap_or(u32::MAX)
    }

    fn text_of(&self, span: Span) -> String {
        let text = self
            .source_text
            .get(span.start as usize..span.end as usize)
            .unwrap_or_default();
        let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");

        if single_line.chars().count() <= MAX_EXPRESSION_TEXT_LENGTH {
            return single_line;
        }

        let truncated: String = single_line
            .chars()
            .take(MAX_EXPRESSION_TEXT_LENGTH - 1)
            .collect();

        format!("{truncated}…")
    }
}

fn referenced_functions(expr: &Expr, functions: &mut Vec<FunctionIndex>) {
    match expr {
        Expr::Function(function) => functions.push(*function),
        Expr::Concat(exprs) | Expr::Choice(exprs) | Expr::Array(exprs) | Expr::Merge(exprs) => {
            for expr in exprs {
                referenced_functions(expr, functions);
            }
        }
        Expr::Conditional {
            consequent,
            alternate,
            ..
        } => {
            referenced_functions(consequent, functions);
            referenced_functions(alternate, functions);
        }
        Expr::Spread(expr) | Expr::ElementOf(expr) => referenced_functions(expr, functions),
        Expr::Object(properties) => {
            for (_, expr) in properties {
                referenced_functions(expr, functions);
            }
        }
        Expr::Member { object, .. } => referenced_functions(object, functions),
        Expr::Call { callee, arguments } | Expr::New { callee, arguments } => {
            referenced_functions(callee, functions);

            for argument in arguments {
                referenced_functions(argument, functions);
            }
        }
        Expr::Text(_)
        | Expr::Parameter { .. }
        | Expr::Boolean(_)
        | Expr::ModuleFile
        | Expr::ModuleDirectory
        | Expr::ModuleResolve
        | Expr::Imported { .. }
        | Expr::Global(_)
        | Expr::Unknown => {}
    }
}

fn may_access_files(callee: &Expression) -> bool {
    let function_name = match callee {
        Expression::Identifier(reference) => reference.name.as_str(),
        Expression::StaticMemberExpression(member) => member.property.name.as_str(),
        _ => return false,
    };

    FILE_ACCESS_FUNCTIONS.contains(&function_name)
}

fn is_module_exports(target: &AssignmentTarget) -> bool {
    let AssignmentTarget::StaticMemberExpression(member) = target else {
        return false;
    };

    member.property.name == "exports"
        && matches!(&member.object, Expression::Identifier(object) if object.name == "module")
}

fn is_function_node(kind: AstKind) -> bool {
    matches!(
        kind,
        AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
    )
}

fn is_transparent_wrapper(kind: AstKind) -> bool {
    matches!(
        kind,
        AstKind::ParenthesizedExpression(_)
            | AstKind::TSAsExpression(_)
            | AstKind::TSSatisfiesExpression(_)
            | AstKind::TSNonNullExpression(_)
            | AstKind::TSTypeAssertion(_)
            | AstKind::TSInstantiationExpression(_)
            | AstKind::AwaitExpression(_)
    )
}

fn literal_text<'e>(expression: &'e Expression) -> Option<&'e str> {
    match expression.get_inner_expression() {
        Expression::StringLiteral(literal) => Some(&literal.value),
        Expression::TemplateLiteral(template) if template.expressions.is_empty() => {
            template.quasis.first()?.value.cooked.as_deref()
        }
        _ => None,
    }
}

fn evaluates_code(options: Option<&Argument>) -> bool {
    let Some(Expression::ObjectExpression(options)) = options
        .and_then(Argument::as_expression)
        .map(Expression::get_inner_expression)
    else {
        return false;
    };

    options.properties.iter().any(|property| match property {
        ObjectPropertyKind::ObjectProperty(property) => {
            property.key.static_name().as_deref() == Some("eval")
                && matches!(property.value.get_inner_expression(), Expression::BooleanLiteral(literal) if literal.value)
        }
        ObjectPropertyKind::SpreadProperty(_) => false,
    })
}

fn imported_name(name: &str) -> ImportedName {
    match name {
        "default" => ImportedName::Default,
        name => ImportedName::Named(name.to_owned()),
    }
}

fn module_object_use(specifier: &str) -> ImportUse {
    ImportUse {
        specifier: specifier.to_owned(),
        name: ImportedName::Namespace,
        kind: UseKind::Escape,
    }
}
