//! The symbols of one module, read from its syntax tree: top-level declarations and the module initialisation, what each of them uses at runtime, and the module's imports, exports and loads.

use std::collections::{BTreeSet, HashMap, HashSet};

use oxc_ast::AstKind;
use oxc_ast::ast::{
    AccessorProperty, Argument, ArrowFunctionBody, ArrowFunctionExpression, AssignmentExpression,
    AwaitExpression, BindingPattern, CallExpression, Class, ComputedMemberExpression, Declaration,
    Decorator, ExportDefaultDeclarationKind, Expression, FormalParameters, Function, FunctionType,
    IdentifierReference, ImportDeclarationSpecifier, ImportExpression, ImportOrExportKind,
    JSXMemberExpression, JSXMemberExpressionObject, ModuleExportName, NewExpression, Program,
    PropertyDefinition, Statement, StaticBlock, StaticMemberExpression, TSClassImplements,
    TSModuleReference, TSType, TSTypeAnnotation, TSTypeParameterDeclaration,
    TSTypeParameterInstantiation, TaggedTemplateExpression, UnaryExpression, UnaryOperator,
    UpdateExpression, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Token;
use oxc_semantic::{ReferenceFlags, ScopeFlags, Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, Span};
use prober_core::symbols::{FallbackCause, LineRange, MODULE_INIT_NAME, SymbolKind};

use crate::imports::{is_require, literal_text, loads_module};
use crate::symbols::fingerprint::Fingerprint;
use crate::symbols::parameters::{FunctionFacts, position};
use crate::symbols::{
    ExportTarget, ImportBinding, Imported, LoadArgument, LocalSymbol, ModuleSymbols, Parameter, Use,
};

const DEFAULT_EXPORT: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    Local(usize),
    Import(usize),
}

struct Draft {
    name: String,
    kind: SymbolKind,
    spans: Vec<Span>,
    names: Vec<Span>,
    uses: BTreeSet<Use>,
    load_uses: BTreeSet<Use>,
    load_arguments: Vec<usize>,
    parameters: Vec<Parameter>,
    runs_at_load: bool,
}

impl Draft {
    fn new(name: String, kind: SymbolKind) -> Self {
        Self {
            name,
            kind,
            spans: Vec::new(),
            names: Vec::new(),
            uses: BTreeSet::new(),
            load_uses: BTreeSet::new(),
            load_arguments: Vec::new(),
            parameters: Vec::new(),
            runs_at_load: false,
        }
    }
}

struct DraftArgument {
    callee: Use,
    index: usize,
    uses: BTreeSet<Use>,
}

enum PendingExport {
    Binding(SymbolId),
    Target(ExportTarget),
}

pub(crate) fn extract_symbols<'a>(
    program: &Program<'a>,
    source_text: &str,
    tokens: &[Token],
) -> Result<ModuleSymbols, FallbackCause> {
    let semantic = SemanticBuilder::new().build(program).semantic;
    let scoping = semantic.scoping();
    let mut collector = SymbolCollector {
        scoping,
        drafts: vec![Draft::new(
            MODULE_INIT_NAME.to_owned(),
            SymbolKind::ModuleInit,
        )],
        bindings: HashMap::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        star_exports: Vec::new(),
        loads: Vec::new(),
        owner: ModuleSymbols::INIT,
        deferred: 0,
        narrowed_loads: HashSet::new(),
        ancestors: Vec::new(),
        function: None,
        arguments: Vec::new(),
        argument: None,
        load_call: None,
        fallback: None,
    };

    for statement in &program.body {
        collector.register(statement);
    }

    for statement in &program.body {
        collector.collect(statement);
    }

    if let Some(fallback) = collector.fallback {
        return Err(fallback);
    }

    let fingerprint = Fingerprint::new(source_text, tokens);
    let line_starts = line_starts(source_text);
    let exports = collector.resolved_exports();
    let SymbolCollector {
        mut drafts,
        imports,
        star_exports,
        loads,
        arguments,
        ..
    } = collector;
    let mut load_uses = BTreeSet::new();
    let mut load_spans = Vec::new();
    let mut load_arguments = Vec::new();

    for draft in drafts.iter().skip(1).filter(|draft| draft.runs_at_load) {
        load_uses.extend(draft.load_uses.iter().cloned());
        load_spans.extend(draft.spans.iter().copied());
        load_arguments.extend(draft.load_arguments.iter().map(|&argument| {
            let argument = &arguments[argument];

            LoadArgument {
                callee: argument.callee.clone(),
                index: argument.index,
                uses: argument.uses.iter().cloned().collect(),
            }
        }));
    }

    let init = &mut drafts[ModuleSymbols::INIT];

    init.uses.extend(load_uses);
    init.spans.extend(load_spans);
    init.spans.sort();

    let symbols = drafts
        .into_iter()
        .map(|draft| LocalSymbol {
            hash: fingerprint.hash(&draft.spans, &draft.names),
            lines: draft
                .spans
                .iter()
                .map(|&span| line_range(&line_starts, span))
                .collect(),
            name: draft.name,
            kind: draft.kind,
            uses: draft.uses.into_iter().collect(),
            parameters: draft.parameters,
        })
        .collect();

    Ok(ModuleSymbols {
        symbols,
        imports,
        exports,
        star_exports,
        loads,
        load_arguments,
    })
}

struct SymbolCollector<'s, 'a> {
    scoping: &'s Scoping,
    drafts: Vec<Draft>,
    bindings: HashMap<SymbolId, Binding>,
    imports: Vec<ImportBinding>,
    exports: Vec<(String, PendingExport)>,
    star_exports: Vec<String>,
    loads: Vec<String>,
    owner: usize,
    deferred: usize,
    narrowed_loads: HashSet<Span>,
    ancestors: Vec<AstKind<'a>>,
    function: Option<FunctionFacts>,
    arguments: Vec<DraftArgument>,
    argument: Option<usize>,
    load_call: Option<(Span, Vec<Option<usize>>)>,
    fallback: Option<FallbackCause>,
}

impl<'a> SymbolCollector<'_, 'a> {
    fn register(&mut self, statement: &Statement) {
        match statement {
            Statement::ImportDeclaration(import) => {
                if import.import_kind == ImportOrExportKind::Type {
                    return;
                }

                let specifier = import.source.value.to_string();

                self.loads.push(specifier.clone());

                for import_specifier in import.specifiers.iter().flatten() {
                    let (local, imported) = match import_specifier {
                        ImportDeclarationSpecifier::ImportSpecifier(named) => {
                            if named.import_kind == ImportOrExportKind::Type {
                                continue;
                            }

                            (&named.local, Imported::Name(export_name(&named.imported)))
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(default) => {
                            (&default.local, Imported::Name(DEFAULT_EXPORT.to_owned()))
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(namespace) => {
                            (&namespace.local, Imported::Namespace)
                        }
                    };

                    self.bind_import(local.symbol_id.get(), specifier.clone(), imported);
                }
            }
            Statement::ExportDeclaration(export) => {
                for (name, index) in self.register_declaration(&export.declaration) {
                    self.exports
                        .push((name, PendingExport::Target(ExportTarget::Local(index))));
                }
            }
            Statement::ExportNamedDeclaration(export) => {
                if export.export_kind == ImportOrExportKind::Type {
                    return;
                }

                for export_specifier in &export.specifiers {
                    if export_specifier.export_kind == ImportOrExportKind::Type {
                        continue;
                    }

                    if let ModuleExportName::IdentifierReference(local) = &export_specifier.local
                        && let Some(symbol) = self.resolved_symbol(local)
                    {
                        self.exports.push((
                            export_name(&export_specifier.exported),
                            PendingExport::Binding(symbol),
                        ));
                    }
                }
            }
            Statement::ExportFromDeclaration(export) => {
                if export.export_kind == ImportOrExportKind::Type {
                    return;
                }

                let specifier = export.source.value.to_string();

                self.loads.push(specifier.clone());

                for export_specifier in &export.specifiers {
                    if export_specifier.export_kind == ImportOrExportKind::Type {
                        continue;
                    }

                    let reexport = ExportTarget::Reexport {
                        specifier: specifier.clone(),
                        name: export_name(&export_specifier.local),
                    };

                    self.exports.push((
                        export_name(&export_specifier.exported),
                        PendingExport::Target(reexport),
                    ));
                }
            }
            Statement::ExportAllDeclaration(export) => {
                if export.export_kind == ImportOrExportKind::Type {
                    return;
                }

                let specifier = export.source.value.to_string();

                self.loads.push(specifier.clone());

                match &export.exported {
                    Some(exported) => self.exports.push((
                        export_name(exported),
                        PendingExport::Target(ExportTarget::Namespace(specifier)),
                    )),
                    None => self.star_exports.push(specifier),
                }
            }
            Statement::ExportDefaultDeclaration(export) => {
                let default_symbol = match &export.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                        self.register_function(function, DEFAULT_EXPORT)
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                        self.register_class(class, DEFAULT_EXPORT)
                    }
                    ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => None,
                    declaration => {
                        let expression = declaration.to_expression();

                        if let Expression::Identifier(identifier) =
                            expression.get_inner_expression()
                            && let Some(symbol) = self.resolved_symbol(identifier)
                            && self.scoping.symbol_scope_id(symbol) == self.scoping.root_scope_id()
                        {
                            self.exports
                                .push((DEFAULT_EXPORT.to_owned(), PendingExport::Binding(symbol)));

                            return;
                        }

                        let index = self.add_draft(
                            DEFAULT_EXPORT.to_owned(),
                            expression_kind(expression),
                            expression.span(),
                        );

                        Some(index)
                    }
                };

                if let Some(index) = default_symbol {
                    self.exports.push((
                        DEFAULT_EXPORT.to_owned(),
                        PendingExport::Target(ExportTarget::Local(index)),
                    ));
                }
            }
            Statement::TSExportAssignment(_) => {
                self.fallback.get_or_insert(FallbackCause::ExportAssignment);
            }
            Statement::TSNamespaceExportDeclaration(_) => {}
            statement => match statement.as_declaration() {
                Some(declaration) => {
                    self.register_declaration(declaration);
                }
                None => self.drafts[ModuleSymbols::INIT]
                    .spans
                    .push(statement.span()),
            },
        }
    }

    fn register_declaration(&mut self, declaration: &Declaration) -> Vec<(String, usize)> {
        match declaration {
            Declaration::VariableDeclaration(variables) if !variables.declare => variables
                .declarations
                .iter()
                .flat_map(|declarator| self.register_declarator(declarator))
                .collect(),
            Declaration::FunctionDeclaration(function) => self
                .register_function(function, DEFAULT_EXPORT)
                .map(|index| (self.drafts[index].name.clone(), index))
                .into_iter()
                .collect(),
            Declaration::ClassDeclaration(class) => self
                .register_class(class, DEFAULT_EXPORT)
                .map(|index| (self.drafts[index].name.clone(), index))
                .into_iter()
                .collect(),
            Declaration::TSEnumDeclaration(enumeration) if !enumeration.declare => {
                let index = self.register_named(
                    &enumeration.id.name,
                    enumeration.id.symbol_id.get(),
                    enumeration.id.span,
                    SymbolKind::Variable,
                    enumeration.span,
                );

                vec![(enumeration.id.name.to_string(), index)]
            }
            Declaration::TSNamespaceDeclaration(namespace) if !namespace.declare => {
                let index = self.register_named(
                    &namespace.id.name,
                    namespace.id.symbol_id.get(),
                    namespace.id.span,
                    SymbolKind::Variable,
                    namespace.span,
                );

                self.drafts[index].runs_at_load = true;

                vec![(namespace.id.name.to_string(), index)]
            }
            Declaration::TSImportEqualsDeclaration(import_equals)
                if import_equals.import_kind == ImportOrExportKind::Value =>
            {
                match &import_equals.module_reference {
                    TSModuleReference::ExternalModuleReference(module_reference) => {
                        let specifier = module_reference.expression.value.to_string();

                        self.loads.push(specifier.clone());
                        self.bind_import(
                            import_equals.id.symbol_id.get(),
                            specifier,
                            Imported::Module,
                        );

                        Vec::new()
                    }
                    TSModuleReference::IdentifierReference(_)
                    | TSModuleReference::QualifiedName(_) => {
                        let index = self.register_named(
                            &import_equals.id.name,
                            import_equals.id.symbol_id.get(),
                            import_equals.id.span,
                            SymbolKind::Variable,
                            import_equals.span,
                        );

                        vec![(import_equals.id.name.to_string(), index)]
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    fn register_declarator(&mut self, declarator: &VariableDeclarator) -> Vec<(String, usize)> {
        let mut bound = Vec::new();

        binding_identifiers(&declarator.id, &mut bound);

        let existing = bound
            .iter()
            .find_map(|(_, symbol, _)| symbol.and_then(|symbol| self.local_binding(symbol)));
        let index = match existing {
            Some(index) => {
                self.drafts[index].spans.push(declarator.span);
                index
            }
            None => {
                let names: Vec<&str> = bound.iter().map(|(name, _, _)| name.as_str()).collect();
                let kind = declarator
                    .init
                    .as_ref()
                    .map_or(SymbolKind::Variable, expression_kind);

                self.add_draft(names.join(", "), kind, declarator.span)
            }
        };

        if let BindingPattern::BindingIdentifier(identifier) = &declarator.id {
            self.drafts[index].names.push(identifier.span);
        }

        for (_, symbol, _) in &bound {
            if let Some(symbol) = symbol {
                self.bindings.insert(*symbol, Binding::Local(index));
            }
        }

        bound
            .into_iter()
            .map(|(name, _, _)| (name, index))
            .collect()
    }

    fn register_function(&mut self, function: &Function, anonymous_name: &str) -> Option<usize> {
        if function.declare || function.r#type == FunctionType::TSDeclareFunction {
            return None;
        }

        Some(
            self.register_optional_name(
                function
                    .id
                    .as_ref()
                    .map(|id| (&*id.name, id.symbol_id.get(), id.span)),
                anonymous_name,
                SymbolKind::Function,
                function.span,
            ),
        )
    }

    fn register_class(&mut self, class: &Class, anonymous_name: &str) -> Option<usize> {
        if class.declare {
            return None;
        }

        Some(
            self.register_optional_name(
                class
                    .id
                    .as_ref()
                    .map(|id| (&*id.name, id.symbol_id.get(), id.span)),
                anonymous_name,
                SymbolKind::Class,
                class.span,
            ),
        )
    }

    fn register_optional_name(
        &mut self,
        id: Option<(&str, Option<SymbolId>, Span)>,
        anonymous_name: &str,
        kind: SymbolKind,
        span: Span,
    ) -> usize {
        match id {
            Some((name, symbol, name_span)) => {
                self.register_named(name, symbol, name_span, kind, span)
            }
            None => self.add_draft(anonymous_name.to_owned(), kind, span),
        }
    }

    fn register_named(
        &mut self,
        name: &str,
        symbol: Option<SymbolId>,
        name_span: Span,
        kind: SymbolKind,
        span: Span,
    ) -> usize {
        let index = match symbol.and_then(|symbol| self.local_binding(symbol)) {
            Some(index) => {
                self.drafts[index].spans.push(span);
                index
            }
            None => self.add_draft(name.to_owned(), kind, span),
        };

        self.drafts[index].names.push(name_span);

        if let Some(symbol) = symbol {
            self.bindings.insert(symbol, Binding::Local(index));
        }

        index
    }

    fn add_draft(&mut self, name: String, kind: SymbolKind, span: Span) -> usize {
        let mut draft = Draft::new(name, kind);

        draft.spans.push(span);
        self.drafts.push(draft);

        self.drafts.len() - 1
    }

    fn bind_import(&mut self, symbol: Option<SymbolId>, specifier: String, imported: Imported) {
        let Some(symbol) = symbol else {
            return;
        };

        self.imports.push(ImportBinding {
            specifier,
            imported,
        });
        self.bindings
            .insert(symbol, Binding::Import(self.imports.len() - 1));
    }

    fn local_binding(&self, symbol: SymbolId) -> Option<usize> {
        match self.bindings.get(&symbol) {
            Some(Binding::Local(index)) => Some(*index),
            Some(Binding::Import(_)) | None => None,
        }
    }

    fn resolved_symbol(&self, reference: &IdentifierReference) -> Option<SymbolId> {
        let reference_id = reference.reference_id.get()?;

        self.scoping.get_reference(reference_id).symbol_id()
    }

    fn resolved_exports(&self) -> Vec<(String, ExportTarget)> {
        let mut exports: Vec<(String, ExportTarget)> = self
            .exports
            .iter()
            .filter_map(|(name, pending)| {
                let target = match pending {
                    PendingExport::Target(target) => target.clone(),
                    PendingExport::Binding(symbol) => match self.bindings.get(symbol)? {
                        Binding::Local(index) => ExportTarget::Local(*index),
                        Binding::Import(index) => ExportTarget::Import(*index),
                    },
                };

                Some((name.clone(), target))
            })
            .collect();

        exports.dedup();

        exports
    }

    fn collect(&mut self, statement: &Statement<'a>) {
        match statement {
            Statement::ImportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_) => {}
            Statement::ExportDeclaration(export) => self.collect_declaration(&export.declaration),
            Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                    self.collect_function(function, DEFAULT_EXPORT);
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    self.collect_class(class, DEFAULT_EXPORT);
                }
                ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {}
                declaration => {
                    let expression = declaration.to_expression();
                    let default_draft = self.drafts.iter().rposition(|draft| {
                        draft.name == DEFAULT_EXPORT && draft.spans.contains(&expression.span())
                    });

                    if let Some(index) = default_draft {
                        self.owner = index;
                        self.visit_expression(expression);
                    }
                }
            },
            statement => match statement.as_declaration() {
                Some(declaration) => self.collect_declaration(declaration),
                None => {
                    self.owner = ModuleSymbols::INIT;
                    self.visit_statement(statement);
                }
            },
        }

        self.owner = ModuleSymbols::INIT;
    }

    fn collect_declaration(&mut self, declaration: &Declaration<'a>) {
        match declaration {
            Declaration::VariableDeclaration(variables) if !variables.declare => {
                for declarator in &variables.declarations {
                    let mut bound = Vec::new();

                    binding_identifiers(&declarator.id, &mut bound);

                    let owner = bound.iter().find_map(|(_, symbol, _)| {
                        symbol.and_then(|symbol| self.local_binding(symbol))
                    });

                    if let Some(owner) = owner {
                        self.owner = owner;
                        self.collect_declarator(declarator);
                    }
                }
            }
            Declaration::FunctionDeclaration(function) => {
                self.collect_function(function, DEFAULT_EXPORT);
            }
            Declaration::ClassDeclaration(class) => self.collect_class(class, DEFAULT_EXPORT),
            Declaration::TSEnumDeclaration(enumeration) if !enumeration.declare => {
                if let Some(owner) = enumeration
                    .id
                    .symbol_id
                    .get()
                    .and_then(|symbol| self.local_binding(symbol))
                {
                    self.owner = owner;
                    self.visit_ts_enum_body(&enumeration.body);
                }
            }
            Declaration::TSNamespaceDeclaration(namespace) if !namespace.declare => {
                if let Some(owner) = namespace
                    .id
                    .symbol_id
                    .get()
                    .and_then(|symbol| self.local_binding(symbol))
                {
                    self.owner = owner;
                    self.visit_ts_namespace_declaration_body(&namespace.body);
                }
            }
            Declaration::TSImportEqualsDeclaration(import_equals)
                if import_equals.import_kind == ImportOrExportKind::Value =>
            {
                if let Some(owner) = import_equals
                    .id
                    .symbol_id
                    .get()
                    .and_then(|symbol| self.local_binding(symbol))
                {
                    self.owner = owner;
                    self.visit_ts_module_reference(&import_equals.module_reference);
                }
            }
            _ => {}
        }
    }

    fn collect_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
        match declarator
            .init
            .as_ref()
            .map(Expression::get_inner_expression)
        {
            Some(Expression::ArrowFunctionExpression(arrow)) => {
                let body: &[Statement] = match &arrow.body {
                    ArrowFunctionBody::FunctionBody(body) => &body.statements,
                    _ => &[],
                };

                self.function = Some(FunctionFacts::new(arrow.span, &arrow.params, body));
            }
            Some(Expression::FunctionExpression(function)) => {
                let body = function
                    .body
                    .as_ref()
                    .map_or(&[][..], |body| &body.statements);

                self.function = Some(FunctionFacts::new(function.span, &function.params, body));
            }
            Some(Expression::CallExpression(call)) => {
                if let Some(callee) = self.callee_use(call) {
                    let held = call
                        .arguments
                        .iter()
                        .enumerate()
                        .map(|(index, argument)| {
                            argument
                                .as_expression()
                                .is_some_and(may_be_kept)
                                .then(|| self.add_argument(callee.clone(), index))
                        })
                        .collect();

                    self.load_call = Some((call.span, held));
                }
            }
            _ => {}
        }

        self.visit_variable_declarator(declarator);
        self.load_call = None;
        self.finish_function();
    }

    fn add_argument(&mut self, callee: Use, index: usize) -> usize {
        self.arguments.push(DraftArgument {
            callee,
            index,
            uses: BTreeSet::new(),
        });
        self.drafts[self.owner]
            .load_arguments
            .push(self.arguments.len() - 1);

        self.arguments.len() - 1
    }

    fn finish_function(&mut self) {
        if let Some(function) = self.function.take() {
            self.drafts[self.owner].parameters = function.into_parameters();
        }
    }

    fn collect_function(&mut self, function: &Function<'a>, anonymous_name: &str) {
        if function.declare || function.r#type == FunctionType::TSDeclareFunction {
            return;
        }

        if let Some(owner) = self.owner_of(
            function.id.as_ref().and_then(|id| id.symbol_id.get()),
            anonymous_name,
            function.span,
        ) {
            let body = function
                .body
                .as_ref()
                .map_or(&[][..], |body| &body.statements);

            self.owner = owner;
            self.function = Some(FunctionFacts::new(function.span, &function.params, body));
            self.visit_function(function, ScopeFlags::Function);
            self.finish_function();
        }
    }

    fn collect_class(&mut self, class: &Class<'a>, anonymous_name: &str) {
        if class.declare {
            return;
        }

        if let Some(owner) = self.owner_of(
            class.id.as_ref().and_then(|id| id.symbol_id.get()),
            anonymous_name,
            class.span,
        ) {
            self.owner = owner;
            self.visit_class(class);
        }
    }

    fn owner_of(
        &self,
        symbol: Option<SymbolId>,
        anonymous_name: &str,
        span: Span,
    ) -> Option<usize> {
        match symbol {
            Some(symbol) => self.local_binding(symbol),
            None => self
                .drafts
                .iter()
                .rposition(|draft| draft.name == anonymous_name && draft.spans.contains(&span)),
        }
    }

    fn record(&mut self, binding: Binding, member: Option<String>) {
        let symbol_use = match binding {
            Binding::Local(index) if index == self.owner => return,
            Binding::Local(index) => Use::Local(index),
            Binding::Import(index) => Use::Import {
                binding: index,
                member,
            },
        };

        self.add_use(symbol_use);
    }

    fn add_use(&mut self, symbol_use: Use) {
        let draft = &mut self.drafts[self.owner];

        match self.argument {
            Some(argument) => {
                self.arguments[argument].uses.insert(symbol_use.clone());
            }
            None => {
                draft.load_uses.insert(symbol_use.clone());
            }
        }

        draft.uses.insert(symbol_use);
    }

    fn callee_use(&self, call: &CallExpression) -> Option<Use> {
        match call.callee.get_inner_expression() {
            Expression::Identifier(callee) => match self.runtime_binding(callee)? {
                Binding::Local(index) => Some(Use::Local(index)),
                Binding::Import(index) => Some(Use::Import {
                    binding: index,
                    member: None,
                }),
            },
            Expression::StaticMemberExpression(member) => match &member.object {
                Expression::Identifier(object) => match self.runtime_binding(object)? {
                    Binding::Import(index) => Some(Use::Import {
                        binding: index,
                        member: Some(member.property.name.to_string()),
                    }),
                    Binding::Local(_) => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    fn watch_reference(&mut self, reference: &IdentifierReference) {
        let Some(symbol) = self.resolved_symbol(reference) else {
            return;
        };
        let Some(function) = &self.function else {
            return;
        };

        if !function.watches(symbol) {
            return;
        }

        let reference_position =
            position(&self.ancestors, function.span(), reference.span, &|call| {
                self.callee_use(call)
            });

        if let Some(function) = &mut self.function {
            function.record(symbol, reference_position);
        }
    }

    fn runtime_binding(&self, reference: &IdentifierReference) -> Option<Binding> {
        let reference_id = reference.reference_id.get()?;
        let resolved = self.scoping.get_reference(reference_id);
        let flags = resolved.flags();

        if !is_runtime(flags) {
            return None;
        }

        self.bindings.get(&resolved.symbol_id()?).copied()
    }

    fn is_unresolved(&self, reference: &IdentifierReference) -> bool {
        reference.reference_id.get().is_some_and(|reference_id| {
            self.scoping
                .get_reference(reference_id)
                .symbol_id()
                .is_none()
        })
    }

    fn runs_now(&mut self) {
        if self.deferred == 0 {
            self.drafts[self.owner].runs_at_load = true;
        }
    }

    fn later(&mut self, visit: impl FnOnce(&mut Self)) {
        self.deferred += 1;
        visit(self);
        self.deferred -= 1;
    }

    fn use_module(&mut self, specifier: &str) {
        self.add_use(Use::Module(specifier.to_owned()));
    }

    fn load_narrowed(&mut self, specifier: &str, load: Span) -> usize {
        self.narrowed_loads.insert(load);
        self.imports.push(ImportBinding {
            specifier: specifier.to_owned(),
            imported: Imported::Dynamic,
        });
        self.add_use(Use::Load(specifier.to_owned()));

        self.imports.len() - 1
    }

    fn use_loaded_members(&mut self, specifier: &str, load: Span, members: Vec<String>) {
        let binding = self.load_narrowed(specifier, load);

        for member in members {
            self.record(Binding::Import(binding), Some(member));
        }
    }

    fn bind_loaded_module(
        &mut self,
        specifier: &str,
        load: Span,
        pattern: &BindingPattern,
    ) -> bool {
        match pattern {
            BindingPattern::ObjectPattern(_) => match destructured_names(pattern) {
                Some(members) => {
                    self.use_loaded_members(specifier, load, members);

                    true
                }
                None => false,
            },
            BindingPattern::BindingIdentifier(identifier) => {
                let Some(symbol) = identifier.symbol_id.get() else {
                    return false;
                };

                if self.bindings.contains_key(&symbol) {
                    return false;
                }

                let binding = self.load_narrowed(specifier, load);

                self.bindings.insert(symbol, Binding::Import(binding));

                true
            }
            BindingPattern::ArrayPattern(_) | BindingPattern::AssignmentPattern(_) => false,
        }
    }
}

impl<'a> Visit<'a> for SymbolCollector<'_, 'a> {
    fn enter_node(&mut self, kind: AstKind<'a>) {
        self.ancestors.push(kind);
    }

    fn leave_node(&mut self, _: AstKind<'a>) {
        self.ancestors.pop();
    }

    fn visit_identifier_reference(&mut self, reference: &IdentifierReference<'a>) {
        self.watch_reference(reference);

        if reference.name == "exports" && self.is_unresolved(reference) {
            self.fallback.get_or_insert(FallbackCause::CommonJs);
        }

        if let Some(binding) = self.runtime_binding(reference) {
            self.record(binding, None);
        }
    }

    fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
        if let Some((specifier, load)) = declarator.init.as_ref().and_then(loaded_module) {
            self.bind_loaded_module(specifier, load, &declarator.id);
        }

        walk::walk_variable_declarator(self, declarator);
    }

    fn visit_static_member_expression(&mut self, member: &StaticMemberExpression<'a>) {
        if let Some((specifier, load)) = loaded_module(&member.object) {
            self.use_loaded_members(specifier, load, vec![member.property.name.to_string()]);
        }

        if let Expression::Identifier(object) = &member.object {
            if object.name == "module"
                && member.property.name == "exports"
                && self.is_unresolved(object)
            {
                self.fallback.get_or_insert(FallbackCause::CommonJs);
            }

            if let Some(binding @ Binding::Import(_)) = self.runtime_binding(object) {
                self.record(binding, Some(member.property.name.to_string()));

                return;
            }
        }

        walk::walk_static_member_expression(self, member);
    }

    fn visit_computed_member_expression(&mut self, member: &ComputedMemberExpression<'a>) {
        if let Some((specifier, load)) = loaded_module(&member.object)
            && let Some(name) = literal_text(&member.expression)
        {
            self.use_loaded_members(specifier, load, vec![name.to_owned()]);
        }

        if let Expression::Identifier(object) = &member.object
            && let Some(binding @ Binding::Import(_)) = self.runtime_binding(object)
        {
            self.record(binding, literal_text(&member.expression).map(str::to_owned));
            self.visit_expression(&member.expression);

            return;
        }

        walk::walk_computed_member_expression(self, member);
    }

    fn visit_jsx_member_expression(&mut self, member: &JSXMemberExpression<'a>) {
        if let JSXMemberExpressionObject::IdentifierReference(object) = &member.object
            && let Some(binding @ Binding::Import(_)) = self.runtime_binding(object)
        {
            self.record(binding, Some(member.property.name.to_string()));

            return;
        }

        walk::walk_jsx_member_expression(self, member);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if !call.pure {
            self.runs_now();
        }

        if let Expression::Identifier(callee) = &call.callee
            && callee.name == "eval"
            && self.is_unresolved(callee)
        {
            self.fallback.get_or_insert(FallbackCause::Eval);
        }

        if let Some((specifier, load)) = loaded_then(call) {
            let parameters = call
                .arguments
                .first()
                .and_then(Argument::as_expression)
                .and_then(callback_parameters);
            let first_parameter = parameters.and_then(|parameters| parameters.items.first());

            if let Some(parameter) = first_parameter {
                self.bind_loaded_module(specifier, load, &parameter.pattern);
            }
        }

        let literal_specifier = call
            .arguments
            .first()
            .and_then(Argument::as_expression)
            .and_then(literal_text);

        if let Some(specifier) = literal_specifier
            && loads_module(&call.callee)
            && !self.narrowed_loads.contains(&call.span)
        {
            self.use_module(specifier);
        }

        let held = self
            .load_call
            .take_if(|(span, _)| *span == call.span)
            .map(|(_, held)| held);

        match held {
            Some(held) => {
                let kind = AstKind::CallExpression(self.alloc(call));

                self.enter_node(kind);
                self.visit_expression(&call.callee);

                for (argument, held_argument) in call.arguments.iter().zip(held) {
                    self.argument = held_argument;
                    self.visit_argument(argument);
                    self.argument = None;
                }

                self.leave_node(kind);
            }
            None => walk::walk_call_expression(self, call),
        }
    }

    fn visit_import_expression(&mut self, import_expression: &ImportExpression<'a>) {
        self.runs_now();

        if let Some(specifier) = literal_text(&import_expression.source)
            && !self.narrowed_loads.contains(&import_expression.span)
        {
            self.use_module(specifier);
        }

        walk::walk_import_expression(self, import_expression);
    }

    fn visit_new_expression(&mut self, new_expression: &NewExpression<'a>) {
        if !new_expression.pure {
            self.runs_now();
        }

        walk::walk_new_expression(self, new_expression);
    }

    fn visit_tagged_template_expression(&mut self, tagged: &TaggedTemplateExpression<'a>) {
        self.runs_now();
        walk::walk_tagged_template_expression(self, tagged);
    }

    fn visit_assignment_expression(&mut self, assignment: &AssignmentExpression<'a>) {
        self.runs_now();
        walk::walk_assignment_expression(self, assignment);
    }

    fn visit_update_expression(&mut self, update: &UpdateExpression<'a>) {
        self.runs_now();
        walk::walk_update_expression(self, update);
    }

    fn visit_unary_expression(&mut self, unary: &UnaryExpression<'a>) {
        if unary.operator == UnaryOperator::Delete {
            self.runs_now();
        }

        walk::walk_unary_expression(self, unary);
    }

    fn visit_await_expression(&mut self, await_expression: &AwaitExpression<'a>) {
        self.runs_now();
        walk::walk_await_expression(self, await_expression);
    }

    fn visit_decorator(&mut self, decorator: &Decorator<'a>) {
        self.runs_now();
        walk::walk_decorator(self, decorator);
    }

    fn visit_static_block(&mut self, static_block: &StaticBlock<'a>) {
        self.runs_now();
        walk::walk_static_block(self, static_block);
    }

    fn visit_function(&mut self, function: &Function<'a>, flags: ScopeFlags) {
        self.later(|collector| walk::walk_function(collector, function, flags));
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'a>) {
        self.later(|collector| walk::walk_arrow_function_expression(collector, arrow));
    }

    fn visit_property_definition(&mut self, property: &PropertyDefinition<'a>) {
        for decorator in &property.decorators {
            self.visit_decorator(decorator);
        }

        self.visit_property_key(&property.key);

        if let Some(value) = &property.value {
            if property.r#static {
                self.visit_expression(value);
            } else {
                self.later(|collector| collector.visit_expression(value));
            }
        }
    }

    fn visit_accessor_property(&mut self, property: &AccessorProperty<'a>) {
        for decorator in &property.decorators {
            self.visit_decorator(decorator);
        }

        self.visit_property_key(&property.key);

        if let Some(value) = &property.value {
            if property.r#static {
                self.visit_expression(value);
            } else {
                self.later(|collector| collector.visit_expression(value));
            }
        }
    }

    fn visit_ts_type(&mut self, _: &TSType<'a>) {}

    fn visit_ts_type_annotation(&mut self, _: &TSTypeAnnotation<'a>) {}

    fn visit_ts_type_parameter_declaration(&mut self, _: &TSTypeParameterDeclaration<'a>) {}

    fn visit_ts_type_parameter_instantiation(&mut self, _: &TSTypeParameterInstantiation<'a>) {}

    fn visit_ts_class_implements(&mut self, _: &TSClassImplements<'a>) {}
}

fn may_be_kept(argument: &Expression) -> bool {
    matches!(
        argument.get_inner_expression(),
        Expression::ArrowFunctionExpression(_)
            | Expression::FunctionExpression(_)
            | Expression::Identifier(_)
            | Expression::StaticMemberExpression(_)
    )
}

fn loaded_module<'e>(expression: &'e Expression<'e>) -> Option<(&'e str, Span)> {
    match expression.get_inner_expression() {
        Expression::AwaitExpression(awaited) => match awaited.argument.get_inner_expression() {
            Expression::ImportExpression(import) => {
                Some((literal_text(&import.source)?, import.span))
            }
            _ => None,
        },
        Expression::CallExpression(call) if is_require(&call.callee) => {
            let specifier = call
                .arguments
                .first()?
                .as_expression()
                .and_then(literal_text)?;

            Some((specifier, call.span))
        }
        _ => None,
    }
}

fn loaded_then<'e>(call: &'e CallExpression<'e>) -> Option<(&'e str, Span)> {
    let Expression::StaticMemberExpression(then) = call.callee.get_inner_expression() else {
        return None;
    };
    let Expression::ImportExpression(import) = then.object.get_inner_expression() else {
        return None;
    };

    if then.property.name != "then" {
        return None;
    }

    Some((literal_text(&import.source)?, import.span))
}

fn callback_parameters<'e>(callback: &'e Expression<'e>) -> Option<&'e FormalParameters<'e>> {
    match callback.get_inner_expression() {
        Expression::ArrowFunctionExpression(arrow) => Some(&arrow.params),
        Expression::FunctionExpression(function) => Some(&function.params),
        _ => None,
    }
}

fn destructured_names(pattern: &BindingPattern) -> Option<Vec<String>> {
    let BindingPattern::ObjectPattern(object) = pattern else {
        return None;
    };

    if object.rest.is_some() {
        return None;
    }

    object
        .properties
        .iter()
        .map(|property| property.key.static_name().map(|name| name.to_string()))
        .collect()
}

fn is_runtime(flags: ReferenceFlags) -> bool {
    (flags.is_read() || flags.is_write()) && !flags.is_value_as_type() && !flags.is_type()
}

fn export_name(name: &ModuleExportName) -> String {
    name.name().to_string()
}

fn expression_kind(expression: &Expression) -> SymbolKind {
    match expression.get_inner_expression() {
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
            SymbolKind::Function
        }
        Expression::ClassExpression(_) => SymbolKind::Class,
        _ => SymbolKind::Variable,
    }
}

fn binding_identifiers(
    pattern: &BindingPattern,
    bound: &mut Vec<(String, Option<SymbolId>, Span)>,
) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            bound.push((
                identifier.name.to_string(),
                identifier.symbol_id.get(),
                identifier.span,
            ));
        }
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                binding_identifiers(&property.value, bound);
            }

            if let Some(rest) = &object.rest {
                binding_identifiers(&rest.argument, bound);
            }
        }
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                binding_identifiers(element, bound);
            }

            if let Some(rest) = &array.rest {
                binding_identifiers(&rest.argument, bound);
            }
        }
        BindingPattern::AssignmentPattern(assignment) => {
            binding_identifiers(&assignment.left, bound);
        }
    }
}

fn line_starts(source_text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            source_text
                .match_indices('\n')
                .map(|(newline_offset, _)| newline_offset + 1),
        )
        .collect()
}

fn line_range(line_starts: &[usize], span: Span) -> LineRange {
    let line_of = |offset: u32| {
        let line_index = line_starts.partition_point(|&line_start| line_start <= offset as usize);

        u32::try_from(line_index).unwrap_or(u32::MAX)
    };

    LineRange {
        start: line_of(span.start),
        end: line_of(span.end.saturating_sub(1).max(span.start)),
    }
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_parser::{Parser, config::TokensParserConfig};
    use oxc_span::SourceType;

    use super::*;

    fn extracted(path: &str, source_text: &str) -> Result<ModuleSymbols, FallbackCause> {
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(path).unwrap();
        let parsed = Parser::new(&allocator, source_text, source_type)
            .with_config(TokensParserConfig)
            .parse();

        assert!(!parsed.diagnostics.has_errors(), "{source_text}");

        extract_symbols(&parsed.program, source_text, &parsed.tokens)
    }

    fn module_symbols(source_text: &str) -> ModuleSymbols {
        extracted("src/module.tsx", source_text).unwrap()
    }

    fn describe_use(module: &ModuleSymbols, symbol_use: &Use) -> String {
        match symbol_use {
            Use::Local(index) => module.symbols[*index].name.clone(),
            Use::Import { binding, member } => {
                let binding = &module.imports[*binding];
                let imported = match &binding.imported {
                    Imported::Name(name) => name.clone(),
                    Imported::Namespace => "*".to_owned(),
                    Imported::Dynamic => "import()".to_owned(),
                    Imported::Module => "=".to_owned(),
                };
                let member = member
                    .as_ref()
                    .map(|member| format!(".{member}"))
                    .unwrap_or_default();

                format!("{}#{imported}{member}", binding.specifier)
            }
            Use::Module(specifier) => format!("module {specifier}"),
            Use::Load(specifier) => format!("load {specifier}"),
        }
    }

    fn uses(source_text: &str) -> Vec<(String, Vec<String>)> {
        let module = module_symbols(source_text);

        module
            .symbols
            .iter()
            .map(|symbol| {
                let uses = symbol
                    .uses
                    .iter()
                    .map(|symbol_use| describe_use(&module, symbol_use))
                    .collect();

                (symbol.name.clone(), uses)
            })
            .collect()
    }

    fn used(name: &str, uses: &[&str]) -> (String, Vec<String>) {
        (
            name.to_owned(),
            uses.iter()
                .map(|&symbol_use| symbol_use.to_owned())
                .collect(),
        )
    }

    fn exports(module: &ModuleSymbols) -> Vec<(String, String)> {
        module
            .exports
            .iter()
            .map(|(name, target)| {
                let target = match target {
                    ExportTarget::Local(index) => module.symbols[*index].name.clone(),
                    ExportTarget::Import(binding) => {
                        format!("import {}", module.imports[*binding].specifier)
                    }
                    ExportTarget::Reexport { specifier, name } => format!("{specifier}#{name}"),
                    ExportTarget::Namespace(specifier) => format!("{specifier}#*"),
                };

                (name.clone(), target)
            })
            .collect()
    }

    fn exported(name: &str, target: &str) -> (String, String) {
        (name.to_owned(), target.to_owned())
    }

    #[test]
    fn runtime_declarations_are_symbols_and_type_declarations_are_not() {
        let source_text = r#"
            import type { Config } from "./config";
            export interface Options { a: number }
            export type Mode = "a" | "b";
            declare const injected: string;
            export function load(config: Config): Options { return { a: helper() }; }
            function helper() { return VALUE; }
            export class Store extends Base { save() { return load(DEFAULTS); } }
            class Base {}
            export const VALUE = 1, DEFAULTS = { a: VALUE };
            export const { left, right } = pair();
            function pair() { return { left: 1, right: 2 }; }
            export enum Color { Red }
            export namespace Tools { export const x = 1; }
            export function overloaded(a: string): void;
            export function overloaded(a: number): void;
            export function overloaded(a: unknown) { return a; }
        "#;
        let module = module_symbols(source_text);
        let kinds: Vec<(&str, SymbolKind)> = module
            .symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect();

        assert_eq!(
            kinds,
            [
                (MODULE_INIT_NAME, SymbolKind::ModuleInit),
                ("load", SymbolKind::Function),
                ("helper", SymbolKind::Function),
                ("Store", SymbolKind::Class),
                ("Base", SymbolKind::Class),
                ("VALUE", SymbolKind::Variable),
                ("DEFAULTS", SymbolKind::Variable),
                ("left, right", SymbolKind::Variable),
                ("pair", SymbolKind::Function),
                ("Color", SymbolKind::Variable),
                ("Tools", SymbolKind::Variable),
                ("overloaded", SymbolKind::Function),
            ]
        );
        assert_eq!(
            uses(source_text),
            [
                used(MODULE_INIT_NAME, &["pair"]),
                used("load", &["helper"]),
                used("helper", &["VALUE"]),
                used("Store", &["load", "Base", "DEFAULTS"]),
                used("Base", &[]),
                used("VALUE", &[]),
                used("DEFAULTS", &["VALUE"]),
                used("left, right", &["pair"]),
                used("pair", &[]),
                used("Color", &[]),
                used("Tools", &[]),
                used("overloaded", &[]),
            ]
        );
        assert_eq!(
            exports(&module),
            [
                exported("load", "load"),
                exported("Store", "Store"),
                exported("VALUE", "VALUE"),
                exported("DEFAULTS", "DEFAULTS"),
                exported("left", "left, right"),
                exported("right", "left, right"),
                exported("Color", "Color"),
                exported("Tools", "Tools"),
                exported("overloaded", "overloaded"),
            ]
        );
        assert_eq!(module.loads, Vec::<String>::new());
    }

    #[test]
    fn module_initialisation_uses_top_level_statements_and_declarations_running_at_load() {
        let source_text = r#"
            import { register } from "./registry";
            import { createLogger } from "./logger";
            const log = createLogger("app");
            const handlers = { start: () => register("start") };
            register(handlers);
            export const ready = Promise.resolve(log);
            export class Widget { static instance = new Widget(); render() { return log; } }
            export class Lazy { instance = new Lazy(); }
            @decorate class Decorated {}
            function decorate(target: unknown) { return target; }
            export const isTag = /*#__PURE__*/ makeMap("div");
            function makeMap(names: string) { return (name: string) => name === names; }
        "#;

        assert_eq!(
            uses(source_text),
            [
                used(
                    MODULE_INIT_NAME,
                    &[
                        "log",
                        "handlers",
                        "decorate",
                        "./registry#register",
                        "./logger#createLogger"
                    ]
                ),
                used("log", &["./logger#createLogger"]),
                used("handlers", &["./registry#register"]),
                used("ready", &["log"]),
                used("Widget", &["log"]),
                used("Lazy", &[]),
                used("Decorated", &["decorate"]),
                used("decorate", &[]),
                used("isTag", &["makeMap"]),
                used("makeMap", &[]),
            ]
        );
    }

    #[test]
    fn namespace_members_are_kept_and_other_uses_of_a_namespace_are_not() {
        let source_text = r#"
            import * as utils from "./utils";
            export function a() { return utils.format(1); }
            export function b() { return utils["parse"]("x"); }
            export function c() { return Object.keys(utils); }
            export function d(key: string) { return utils[key]; }
            export const E = () => <utils.Button />;
        "#;

        assert_eq!(
            uses(source_text),
            [
                used(MODULE_INIT_NAME, &[]),
                used("a", &["./utils#*.format"]),
                used("b", &["./utils#*.parse"]),
                used("c", &["./utils#*"]),
                used("d", &["./utils#*"]),
                used("E", &["./utils#*.Button"]),
            ]
        );
    }

    #[test]
    fn type_positions_and_shadowing_names_are_not_uses() {
        let source_text = r#"
            import { Model, build, check } from "./model";
            export function make(input: Model): typeof build {
                const build = (value: Model) => value;
                return build(input) as Model satisfies Model;
            }
            export const validate = (value: unknown): value is Model => check(value);
        "#;

        assert_eq!(
            uses(source_text),
            [
                used(MODULE_INIT_NAME, &[]),
                used("make", &[]),
                used("validate", &["./model#check"]),
            ]
        );
    }

    #[test]
    fn literal_dynamic_imports_requires_and_mocks_use_whole_modules() {
        let source_text = r#"
            export async function load() { return (await import("./plugin")).default; }
            const config = require("./config.json");
            vi.mock("./db", () => ({ query: vi.fn() }));
            export const read = () => config;
        "#;

        assert_eq!(
            uses(source_text),
            [
                used(MODULE_INIT_NAME, &["module ./config.json", "module ./db"]),
                used("load", &["./plugin#import().default", "load ./plugin"]),
                used("config", &["module ./config.json"]),
                used("read", &["config"]),
            ]
        );
    }

    #[test]
    fn loaded_modules_narrow_to_the_names_read_and_keep_their_initialisation() {
        let source_text = r#"
            export async function a() {
                const { format, parse: read } = await import("./utils");
                return format(read());
            }
            export async function b() { const utils = await import("./utils"); return utils.format(); }
            export function c() { return import("./utils").then(({ parse }) => parse()); }
            export function d() { return import("./utils").then((utils) => Object.keys(utils)); }
            export async function e() { const { ...all } = await import("./utils"); return all; }
            export function f() { return require("./legacy").run(); }
            export const g = () => import("./lazy");
        "#;

        assert_eq!(
            uses(source_text),
            [
                used(MODULE_INIT_NAME, &[]),
                used(
                    "a",
                    &[
                        "./utils#import().format",
                        "./utils#import().parse",
                        "load ./utils"
                    ]
                ),
                used("b", &["./utils#import().format", "load ./utils"]),
                used("c", &["./utils#import().parse", "load ./utils"]),
                used("d", &["./utils#import()", "load ./utils"]),
                used("e", &["module ./utils"]),
                used("f", &["./legacy#import().run", "load ./legacy"]),
                used("g", &["module ./lazy"]),
            ]
        );
    }

    type DescribedParameters = (String, Vec<(bool, Vec<String>)>);

    fn parameters_of(source_text: &str) -> Vec<DescribedParameters> {
        let module = module_symbols(source_text);

        module
            .symbols
            .iter()
            .filter(|symbol| !symbol.parameters.is_empty())
            .map(|symbol| {
                let parameters = symbol
                    .parameters
                    .iter()
                    .map(|parameter| {
                        let passed = parameter
                            .passed
                            .iter()
                            .map(|(callee, argument)| {
                                format!("{} #{argument}", describe_use(&module, callee))
                            })
                            .collect();

                        (parameter.kept, passed)
                    })
                    .collect();

                (symbol.name.clone(), parameters)
            })
            .collect()
    }

    fn parameters(name: &str, parameters: &[(bool, &[&str])]) -> DescribedParameters {
        (
            name.to_owned(),
            parameters
                .iter()
                .map(|(kept, passed)| {
                    (
                        *kept,
                        passed.iter().map(|&passed| passed.to_owned()).collect(),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn parameters_are_kept_when_they_only_reach_what_the_function_returns() {
        let source_text = r#"
            import { remember } from "./memory";
            const listeners: Array<() => void> = [];
            export function lazy(load: () => unknown) { let cached: unknown; return () => (cached ??= load()); }
            export function eager(create: () => unknown) { return create(); }
            export function forwarded(load: () => unknown) { return lazy(load); }
            export function wrapped(load: () => Promise<unknown>) { return remember(() => load().then((loaded) => loaded)); }
            export function stored(callback: () => void) { listeners.push(callback); }
            export function viaLocal(load: (options: unknown) => void, options: unknown) {
                const run = () => load(options);
                return { run() { return run(); } };
            }
            export const arrow = (load: () => void) => () => load();
            export function unused(_: unknown, { a }: { a: number }) { return a; }
        "#;

        assert_eq!(
            parameters_of(source_text),
            [
                parameters("lazy", &[(true, &[])]),
                parameters("eager", &[(false, &[])]),
                parameters("forwarded", &[(true, &["lazy #0"])]),
                parameters("wrapped", &[(true, &["./memory#remember #0"])]),
                parameters("stored", &[(false, &[])]),
                parameters("viaLocal", &[(true, &[]), (true, &[])]),
                parameters("arrow", &[(true, &[])]),
                parameters("unused", &[(true, &[]), (false, &[])]),
            ]
        );
    }

    #[test]
    fn declarations_running_at_load_hand_their_callbacks_to_the_functions_they_call() {
        let module = module_symbols(
            r#"
                import { createLazyRuntimeModule } from "./lazy";
                const loadRuntime = createLazyRuntimeModule(() => import("./runtime"));
                const value = compute(helper, { limit: LIMIT });
                const LIMIT = 3;
                function helper() { return 1; }
                function compute(callback: () => number, options: { limit: number }) { return callback() + options.limit; }
                export { loadRuntime, value };
            "#,
        );
        let load_arguments: Vec<(String, usize, Vec<String>)> = module
            .load_arguments
            .iter()
            .map(|argument| {
                let uses = argument
                    .uses
                    .iter()
                    .map(|symbol_use| describe_use(&module, symbol_use))
                    .collect();

                (
                    describe_use(&module, &argument.callee),
                    argument.index,
                    uses,
                )
            })
            .collect();
        let init = &module.symbols[ModuleSymbols::INIT];

        assert_eq!(
            load_arguments,
            [
                (
                    "./lazy#createLazyRuntimeModule".to_owned(),
                    0,
                    vec!["module ./runtime".to_owned()]
                ),
                ("compute".to_owned(), 0, vec!["helper".to_owned()]),
            ]
        );
        assert_eq!(
            init.uses
                .iter()
                .map(|symbol_use| describe_use(&module, symbol_use))
                .collect::<Vec<_>>(),
            ["LIMIT", "compute", "./lazy#createLazyRuntimeModule"]
        );
        assert_eq!(init.lines.len(), 2);
    }

    #[test]
    fn default_exports_are_symbols_or_aliases_of_one() {
        let named = module_symbols(
            "export default function main() { return helper(); }\nfunction helper() {}",
        );
        let object = module_symbols("export default { name: \"x\", run };\nfunction run() {}");
        let alias = module_symbols(
            "const app = create();\nexport default app;\nfunction create() { return 1; }",
        );
        let forward = module_symbols("export default start;\nfunction start() {}");
        let listed = module_symbols("function helper() {}\nexport { helper as default, helper };");

        assert_eq!(exports(&named), [exported("default", "main")]);
        assert_eq!(exports(&object), [exported("default", "default")]);
        assert_eq!(object.symbols[1].kind, SymbolKind::Variable);
        assert_eq!(exports(&alias), [exported("default", "app")]);
        assert_eq!(exports(&forward), [exported("default", "start")]);
        assert_eq!(
            exports(&listed),
            [exported("default", "helper"), exported("helper", "helper")]
        );
    }

    #[test]
    fn re_exports_star_exports_and_imports_load_their_modules() {
        let module = module_symbols(
            r#"
                export { a, b as c } from "./ab";
                export * from "./all";
                export * as ns from "./ns";
                export type { T } from "./types";
                export type * from "./more-types";
                import "./side-effect";
                import { x } from "./x";
                import { type Y } from "./y";
                export { x as y };
            "#,
        );

        assert_eq!(
            exports(&module),
            [
                exported("a", "./ab#a"),
                exported("c", "./ab#b"),
                exported("ns", "./ns#*"),
                exported("y", "import ./x"),
            ]
        );
        assert_eq!(module.star_exports, ["./all"]);
        assert_eq!(
            module.loads,
            ["./ab", "./all", "./ns", "./side-effect", "./x", "./y"]
        );
    }

    #[test]
    fn commonjs_exports_export_assignments_and_direct_eval_fall_back_to_file_level() {
        let fallbacks = [
            (
                "src/a.cjs",
                "module.exports = { a: 1 };",
                FallbackCause::CommonJs,
            ),
            ("src/a.js", "exports.a = 1;", FallbackCause::CommonJs),
            (
                "src/a.ts",
                "const foo = 1;\nexport = foo;",
                FallbackCause::ExportAssignment,
            ),
            (
                "src/a.ts",
                "export function run(code: string) { return eval(code); }",
                FallbackCause::Eval,
            ),
        ];

        for (path, source_text, cause) in fallbacks {
            assert_eq!(extracted(path, source_text), Err(cause), "{source_text}");
        }

        for source_text in [
            "const m = require(\"./m\");\nexport const x = m.x;",
            "const module = { exports: {} };\nmodule.exports = 1;\nexport { module };",
            "const evaluate = (eval);\nexport function run(eval: (code: string) => void) { eval(\"1\"); }",
        ] {
            assert!(extracted("src/a.ts", source_text).is_ok(), "{source_text}");
        }
    }

    #[test]
    fn lines_cover_each_declaration_and_each_top_level_statement() {
        let module = module_symbols(
            "import { a } from \"./a\";\n\nexport function f() {\n  return a;\n}\n\na();\nconst x = 1,\n  y = 2;\nlog(x);\n",
        );
        let lines: Vec<(&str, Vec<(u32, u32)>)> = module
            .symbols
            .iter()
            .map(|symbol| {
                let lines = symbol
                    .lines
                    .iter()
                    .map(|lines| (lines.start, lines.end))
                    .collect();

                (symbol.name.as_str(), lines)
            })
            .collect();

        assert_eq!(
            lines,
            [
                (MODULE_INIT_NAME, vec![(7, 7), (10, 10)]),
                ("f", vec![(3, 5)]),
                ("x", vec![(8, 8)]),
                ("y", vec![(9, 9)]),
            ]
        );
    }

    #[test]
    fn hashes_ignore_comments_formatting_and_the_declared_name() {
        let hashes = |source_text: &str| -> Vec<u64> {
            module_symbols(source_text)
                .symbols
                .iter()
                .map(|symbol| symbol.hash)
                .collect()
        };
        let original = hashes("export function load(id: string) { return fetch(id); }\nstart();");
        let reformatted = hashes(
            "// Loads one item.\nexport function load(\n    id: string\n) {\n    return fetch(id); // remote\n}\n\nstart();",
        );
        let renamed =
            hashes("export function fetchOne(id: string) { return fetch(id); }\nstart();");
        let changed =
            hashes("export function load(id: string) { return fetch(id, {}); }\nstart(1);");

        assert_eq!(original, reformatted);
        assert_eq!(original, renamed);
        assert_ne!(original[0], changed[0]);
        assert_ne!(original[1], changed[1]);
    }
}
