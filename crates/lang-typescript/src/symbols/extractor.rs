//! The symbols of one module, read from its syntax tree: top-level declarations, the members of classes and object literals, and the module initialisation; what each of them uses, what loading the module, calling a function or constructing a class runs, and the module's imports, exports and loads.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use oxc_ast::AstKind;
use oxc_ast::ast::{
    AccessorProperty, Argument, ArrowFunctionBody, ArrowFunctionExpression, AssignmentExpression,
    AssignmentOperator, AwaitExpression, BindingPattern, CallExpression, Class, ClassElement,
    ComputedMemberExpression, Declaration, Decorator, ExportDefaultDeclarationKind, Expression,
    FormalParameters, Function, FunctionType, IdentifierReference, ImportDeclarationSpecifier,
    ImportExpression, ImportOrExportKind, JSXMemberExpression, JSXMemberExpressionObject,
    MethodDefinition, MethodDefinitionKind, ModuleExportName, NewExpression, ObjectExpression,
    ObjectProperty, ObjectPropertyKind, PrivateFieldExpression, Program, PropertyDefinition,
    PropertyKey, SimpleAssignmentTarget, Statement, StaticBlock, StaticMemberExpression,
    TSClassImplements, TSModuleReference, TSType, TSTypeAnnotation, TSTypeParameterDeclaration,
    TSTypeParameterInstantiation, TaggedTemplateExpression, ThisExpression, UnaryExpression,
    UnaryOperator, UpdateExpression, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Token;
use oxc_semantic::{ReferenceFlags, ScopeFlags, Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, Span};
use prober_core::symbols::{FallbackCause, LineRange, MODULE_INIT_NAME, SymbolKind};

use crate::imports::{is_require, literal_text, loads_module};
use crate::symbols::fingerprint::{Fingerprint, pieces};
use crate::symbols::parameters::{Fate, FunctionFacts, Pending, Question, Step, Verdict};
use crate::symbols::placement::{Bindings, Boundary, End, is_object_assign, placement};
use crate::symbols::{
    ExportTarget, Extends, Held, ImportBinding, Imported, Keeping, LocalSymbol, Membership,
    ModuleSymbols, Parameter, Place, Runs, Shape, Use,
};

const DEFAULT_EXPORT: &str = "default";

const CONSTRUCTOR: &str = "constructor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    Local(usize),
    Import(usize),
}

#[derive(Default)]
struct HeldDraft {
    spans: Vec<Span>,
    uses: BTreeSet<Use>,
}

#[derive(Default)]
struct RunsDraft {
    spans: Vec<Span>,
    holes: Vec<Span>,
    uses: BTreeSet<Use>,
    held: BTreeMap<Vec<Keeping>, HeldDraft>,
}

struct Draft {
    name: String,
    kind: SymbolKind,
    spans: Vec<Span>,
    names: Vec<Span>,
    holes: Vec<Span>,
    uses: BTreeSet<Use>,
    parameters: Vec<Parameter>,
    runs_at_load: bool,
    load: RunsDraft,
    member: Option<Membership>,
    shape: Option<Shape>,
    runs: Option<RunsDraft>,
}

impl Draft {
    fn new(name: String, kind: SymbolKind) -> Self {
        Self {
            name,
            kind,
            spans: Vec::new(),
            names: Vec::new(),
            holes: Vec::new(),
            uses: BTreeSet::new(),
            parameters: Vec::new(),
            runs_at_load: false,
            load: RunsDraft::default(),
            member: None,
            shape: None,
            runs: None,
        }
    }
}

enum PendingExport {
    Binding(SymbolId),
    Target(ExportTarget),
}

#[derive(Debug, Clone, Copy)]
struct Region {
    sink: usize,
    declaration: Option<usize>,
    boundary: Boundary,
    function: Option<Span>,
    boundary_index: usize,
}

impl Region {
    fn statement() -> Self {
        Self {
            sink: ModuleSymbols::INIT,
            declaration: None,
            boundary: Boundary::Declaration,
            function: None,
            boundary_index: 0,
        }
    }

    fn declaration(declaration: usize) -> Self {
        Self {
            declaration: Some(declaration),
            ..Self::statement()
        }
    }

    fn function(sink: usize, boundary: Boundary, function: Span) -> Self {
        Self {
            sink,
            declaration: None,
            boundary,
            function: Some(function),
            boundary_index: usize::MAX,
        }
    }

    fn field(class: usize, boundary_index: usize) -> Self {
        Self {
            sink: class,
            declaration: None,
            boundary: Boundary::Constructor,
            function: None,
            boundary_index,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Run,
    Deferred,
    Kept(Vec<Keeping>, Option<Vec<SymbolId>>),
}

struct Frame {
    index: usize,
    mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThisValue {
    Class { class: usize, place: Place },
    Object(usize),
}

struct Callee {
    call: Span,
    pure: bool,
    wrap: fn(Use) -> Use,
}

enum Condition {
    Always,
    Held(Vec<Keeping>),
    Through(Vec<SymbolId>, Vec<Keeping>, Question),
}

impl Condition {
    fn of(fate: Fate, question: Question) -> Option<Self> {
        match fate.step(question) {
            Step::Counts => Some(Condition::Always),
            Step::Kept if fate.keeping.is_empty() => None,
            Step::Kept => Some(Condition::Held(fate.keeping)),
            Step::Through(locals) => Some(Condition::Through(locals, fate.keeping, question)),
        }
    }
}

struct Context {
    owner: Option<usize>,
    region: Option<Region>,
    frames: Vec<Frame>,
    this_value: Option<ThisValue>,
    member_function: Option<Span>,
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
        members: HashMap::new(),
        member_drafts: HashMap::new(),
        owner: None,
        region: None,
        frames: Vec::new(),
        this_value: None,
        member_function: None,
        class: None,
        collected_class: None,
        narrowed_loads: HashSet::new(),
        ancestors: Vec::new(),
        function: None,
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
        ..
    } = collector;

    for index in 0..drafts.len() {
        if let Some(owner) = drafts[index].member.as_ref().map(|member| member.owner) {
            let spans = drafts[index].spans.clone();

            drafts[owner].holes.extend(spans);
        }
    }

    let mut init_runs = drafts[ModuleSymbols::INIT].runs.take().unwrap_or_default();
    let mut load_spans = Vec::new();

    for draft in drafts.iter_mut().skip(1).filter(|draft| draft.runs_at_load) {
        let load = std::mem::take(&mut draft.load);

        load_spans.extend(draft.spans.iter().copied());
        init_runs.holes.extend(load.holes);
        init_runs.uses.extend(load.uses);

        for (kept_by, held) in load.held {
            let init_held = init_runs.held.entry(kept_by).or_default();

            init_held.spans.extend(held.spans);
            init_held.uses.extend(held.uses);
        }
    }

    let init = &mut drafts[ModuleSymbols::INIT];

    init.spans.extend(load_spans);
    init.spans.sort();
    init.holes = init_runs.holes;
    init.uses = init_runs.uses;

    let held = held_groups(&fingerprint, init_runs.held);
    let symbols = drafts
        .into_iter()
        .map(|draft| LocalSymbol {
            hash: fingerprint.hash(&draft.spans, &draft.holes, &draft.names),
            lines: lines(&line_starts, &draft.spans, &draft.holes),
            runs: draft
                .runs
                .filter(|runs| !runs_as_whole(draft.kind, &draft.uses, runs))
                .map(|runs| Runs {
                    hash: fingerprint.hash(&runs.spans, &runs.holes, &draft.names),
                    lines: lines(&line_starts, &runs.spans, &runs.holes),
                    uses: runs.uses.into_iter().collect(),
                    held: held_groups(&fingerprint, runs.held),
                }),
            name: draft.name,
            kind: draft.kind,
            uses: draft.uses.into_iter().collect(),
            parameters: draft.parameters,
            member: draft.member,
            shape: draft.shape,
        })
        .collect();

    Ok(ModuleSymbols {
        symbols,
        imports,
        exports,
        star_exports,
        loads,
        held,
    })
}

fn lines(line_starts: &[usize], spans: &[Span], holes: &[Span]) -> Vec<LineRange> {
    let mut lines: Vec<LineRange> = pieces(spans, holes)
        .into_iter()
        .map(|piece| line_range(line_starts, piece))
        .collect();

    lines.dedup();

    lines
}

fn runs_as_whole(kind: SymbolKind, uses: &BTreeSet<Use>, runs: &RunsDraft) -> bool {
    kind == SymbolKind::Function
        && runs.holes.is_empty()
        && runs.held.is_empty()
        && runs.uses == *uses
}

fn held_groups(fingerprint: &Fingerprint, held: BTreeMap<Vec<Keeping>, HeldDraft>) -> Vec<Held> {
    held.into_iter()
        .map(|(kept_by, held)| Held {
            kept_by,
            hash: fingerprint.hash(&held.spans, &[], &[]),
            uses: held.uses.into_iter().collect(),
        })
        .collect()
}

struct SymbolCollector<'s, 'a> {
    scoping: &'s Scoping,
    drafts: Vec<Draft>,
    bindings: HashMap<SymbolId, Binding>,
    imports: Vec<ImportBinding>,
    exports: Vec<(String, PendingExport)>,
    star_exports: Vec<String>,
    loads: Vec<String>,
    members: HashMap<Span, usize>,
    member_drafts: HashMap<(usize, String, Place), usize>,
    owner: Option<usize>,
    region: Option<Region>,
    frames: Vec<Frame>,
    this_value: Option<ThisValue>,
    member_function: Option<Span>,
    class: Option<usize>,
    collected_class: Option<Span>,
    narrowed_loads: HashSet<Span>,
    ancestors: Vec<AstKind<'a>>,
    function: Option<(usize, FunctionFacts)>,
    fallback: Option<FallbackCause>,
}

impl<'a> SymbolCollector<'_, 'a> {
    fn register(&mut self, statement: &Statement<'a>) {
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

                        if let Expression::ObjectExpression(object) =
                            expression.get_inner_expression()
                        {
                            self.register_object_members(object, index);
                        }

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

    fn register_declaration(&mut self, declaration: &Declaration<'a>) -> Vec<(String, usize)> {
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

    fn register_declarator(&mut self, declarator: &VariableDeclarator<'a>) -> Vec<(String, usize)> {
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
                let index = self.add_draft(names.join(", "), kind, declarator.span);

                if let (BindingPattern::BindingIdentifier(_), Some(init)) =
                    (&declarator.id, &declarator.init)
                    && let Expression::ObjectExpression(object) = init.get_inner_expression()
                {
                    self.register_object_members(object, index);
                }

                index
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

    fn register_class(&mut self, class: &Class<'a>, anonymous_name: &str) -> Option<usize> {
        if class.declare {
            return None;
        }

        let index = self.register_optional_name(
            class
                .id
                .as_ref()
                .map(|id| (&*id.name, id.symbol_id.get(), id.span)),
            anonymous_name,
            SymbolKind::Class,
            class.span,
        );

        self.register_class_members(class, index);

        Some(index)
    }

    fn register_class_members(&mut self, class: &Class<'a>, owner: usize) {
        let mut construction = RunsDraft::default();

        for element in &class.body.body {
            match element {
                ClassElement::MethodDefinition(method)
                    if method.kind == MethodDefinitionKind::Constructor =>
                {
                    construction.spans.push(method.span);
                    self.add_member(owner, CONSTRUCTOR.to_owned(), Place::Instance, method.span);
                }
                ClassElement::MethodDefinition(method) => {
                    if let Some(key) = member_key(&method.key) {
                        self.add_member(owner, key, place(method.r#static), method.span);
                    }
                }
                ClassElement::PropertyDefinition(property) => {
                    if property.declare {
                        continue;
                    }

                    if !property.r#static && property.value.is_some() {
                        construction.spans.push(property.span);
                    }

                    if let Some(key) = member_key(&property.key) {
                        self.add_member(owner, key, place(property.r#static), property.span);
                    }
                }
                ClassElement::AccessorProperty(property) => {
                    if !property.r#static && property.value.is_some() {
                        construction.spans.push(property.span);
                    }

                    if let Some(key) = member_key(&property.key) {
                        self.add_member(owner, key, place(property.r#static), property.span);
                    }
                }
                ClassElement::StaticBlock(_) | ClassElement::TSIndexSignature(_) => {}
            }
        }

        self.drafts[owner].runs = Some(construction);
    }

    fn register_object_members(&mut self, object: &ObjectExpression<'a>, owner: usize) {
        let mut open = false;

        for property in &object.properties {
            match property {
                ObjectPropertyKind::ObjectProperty(property) => match member_key(&property.key) {
                    Some(key) => self.add_member(owner, key, Place::Own, property.span),
                    None => open = true,
                },
                ObjectPropertyKind::SpreadProperty(_) => open = true,
            }
        }

        self.drafts[owner].shape = Some(Shape::Object { open });
    }

    fn add_member(&mut self, owner: usize, key: String, place: Place, span: Span) {
        let index = match self.member_drafts.get(&(owner, key.clone(), place)) {
            Some(&index) => {
                self.drafts[index].spans.push(span);

                index
            }
            None => {
                let in_class = self.drafts[owner].kind == SymbolKind::Class;
                let name = member_name(&self.drafts[owner].name, &key, place, in_class);
                let index = self.add_draft(name, SymbolKind::Member, span);

                self.drafts[index].member = Some(Membership {
                    owner,
                    key: key.clone(),
                    place,
                    assigns: Vec::new(),
                });
                self.member_drafts.insert((owner, key, place), index);

                index
            }
        };

        self.members.insert(span, index);
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
                        self.collect_value(index, Some(expression));
                        self.visit_expression(expression);
                        self.finish_function();
                    }
                }
            },
            statement => match statement.as_declaration() {
                Some(declaration) => self.collect_declaration(declaration),
                None => {
                    self.region = Some(Region::statement());
                    self.visit_statement(statement);
                }
            },
        }

        self.owner = None;
        self.region = None;
        self.frames.clear();
        self.this_value = None;
        self.member_function = None;
        self.class = None;
        self.collected_class = None;
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
                        self.collect_value(owner, declarator.init.as_ref());
                        self.visit_variable_declarator(declarator);
                        self.finish_function();
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
                    self.collect_value(owner, None);
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
                    self.collect_value(owner, None);
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
                    self.collect_value(owner, None);
                    self.visit_ts_module_reference(&import_equals.module_reference);
                }
            }
            _ => {}
        }
    }

    fn collect_value(&mut self, owner: usize, init: Option<&Expression<'a>>) {
        self.owner = Some(owner);

        match init.map(Expression::get_inner_expression) {
            Some(Expression::ArrowFunctionExpression(arrow)) => {
                let body: &[Statement] = match &arrow.body {
                    ArrowFunctionBody::FunctionBody(body) => &body.statements,
                    _ => &[],
                };

                self.start_function(owner, arrow.span, &arrow.params, body);
            }
            Some(Expression::FunctionExpression(function)) => {
                let body = function
                    .body
                    .as_ref()
                    .map_or(&[][..], |body| &body.statements);

                self.start_function(owner, function.span, &function.params, body);
            }
            init => {
                self.region = Some(Region::declaration(owner));

                if let Some(shape) = init.and_then(|init| self.value_shape(init)) {
                    self.drafts[owner].shape.get_or_insert(shape);
                }
            }
        }
    }

    fn start_function(
        &mut self,
        owner: usize,
        span: Span,
        parameters: &FormalParameters,
        body: &[Statement],
    ) {
        self.drafts[owner].runs = Some(RunsDraft {
            spans: vec![span],
            ..RunsDraft::default()
        });
        self.region = Some(Region::function(owner, Boundary::Function, span));
        self.function = Some((owner, FunctionFacts::new(parameters, body)));
    }

    fn value_shape(&self, init: &Expression) -> Option<Shape> {
        match init {
            Expression::NewExpression(new) => {
                let Expression::Identifier(callee) = new.callee.get_inner_expression() else {
                    return None;
                };

                Some(Shape::Instance(binding_use(
                    self.runtime_binding(callee)?,
                    None,
                )))
            }
            callee if is_object_assign(callee) => Some(Shape::Assign),
            _ => None,
        }
    }

    fn finish_function(&mut self) {
        let Some((owner, function)) = self.function.take() else {
            return;
        };
        let (parameters, pending) = function.finish();

        self.drafts[owner].parameters = parameters;

        for (verdict, pending) in pending {
            let runs = self.drafts[owner]
                .runs
                .get_or_insert_with(RunsDraft::default);

            match verdict {
                Verdict::Counts => runs.uses.extend(pending.uses),
                Verdict::Held(kept_by) => {
                    let held = runs.held.entry(kept_by).or_default();

                    held.uses.extend(pending.uses);
                    held.spans.extend(pending.hole);
                    runs.holes.extend(pending.hole);
                }
                Verdict::Kept => runs.holes.extend(pending.hole),
            }
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

            self.owner = Some(owner);
            self.start_function(owner, function.span, &function.params, body);
            self.visit_function(function, ScopeFlags::Function);
            self.finish_function();
        }
    }

    fn collect_class(&mut self, class: &Class<'a>, anonymous_name: &str) {
        if class.declare {
            return;
        }

        let Some(owner) = self.owner_of(
            class.id.as_ref().and_then(|id| id.symbol_id.get()),
            anonymous_name,
            class.span,
        ) else {
            return;
        };
        let extends = self.extends(class);
        let has_constructor = class.body.body.iter().any(|element| {
            matches!(element, ClassElement::MethodDefinition(method) if method.kind == MethodDefinitionKind::Constructor)
        });

        self.drafts[owner].shape = Some(Shape::Class { extends });

        if !has_constructor && let Some(base) = self.base_construction(owner) {
            self.drafts[owner]
                .runs
                .get_or_insert_with(RunsDraft::default)
                .uses
                .insert(base);
        }

        self.owner = Some(owner);
        self.region = Some(Region::declaration(owner));
        self.class = Some(owner);
        self.collected_class = Some(class.span);
        self.visit_class(class);
    }

    fn base_construction(&self, class: usize) -> Option<Use> {
        match &self.drafts[class].shape {
            Some(Shape::Class {
                extends: Extends::Binding(base),
            }) => Some(Use::New(Box::new(base.clone()))),
            Some(Shape::Class {
                extends: Extends::Other,
            }) => Some(Use::Local(class)),
            _ => None,
        }
    }

    fn extends(&self, class: &Class) -> Extends {
        let Some(heritage) = &class.heritage else {
            return Extends::Nothing;
        };

        match heritage.expression.get_inner_expression() {
            Expression::Identifier(base) => self
                .runtime_binding(base)
                .map_or(Extends::Other, |binding| {
                    Extends::Binding(binding_use(binding, None))
                }),
            Expression::StaticMemberExpression(member) => match &member.object {
                Expression::Identifier(namespace) => match self.runtime_binding(namespace) {
                    Some(binding) => Extends::Binding(binding_use(
                        binding,
                        Some(member.property.name.to_string()),
                    )),
                    None => Extends::Other,
                },
                _ => Extends::Other,
            },
            _ => Extends::Other,
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

    fn enter_context(&mut self, context: Context) -> Context {
        Context {
            owner: std::mem::replace(&mut self.owner, context.owner),
            region: std::mem::replace(&mut self.region, context.region),
            frames: std::mem::replace(&mut self.frames, context.frames),
            this_value: std::mem::replace(&mut self.this_value, context.this_value),
            member_function: std::mem::replace(&mut self.member_function, context.member_function),
        }
    }

    fn restore_context(&mut self, context: Context) {
        self.enter_context(context);
    }

    fn record(&mut self, symbol_use: Use) {
        let Some(owner) = self.owner else {
            return;
        };

        if symbol_use != Use::Local(owner) {
            self.drafts[owner].uses.insert(symbol_use);
        }
    }

    fn use_binding(&mut self, symbol_use: Use, span: Span) {
        let recorded = match self.callee_of(span) {
            Some(callee) if !self.result_escapes(callee.call) => (callee.wrap)(symbol_use.clone()),
            _ => symbol_use.clone(),
        };

        self.record(recorded);
        self.run_occurrence(symbol_use, span);
    }

    fn result_escapes(&self, call: Span) -> bool {
        let outside = &self.ancestors[..self.ancestors.len() - 1];
        let boundary = outside
            .iter()
            .rposition(|kind| {
                matches!(
                    kind,
                    AstKind::Function(_) | AstKind::ArrowFunctionExpression(_)
                )
            })
            .map_or((0, Boundary::Declaration), |index| {
                (index, Boundary::Nested)
            });
        let placement = placement(outside, boundary, call, self);
        let contained = placement.derived || matches!(placement.end, End::Dropped | End::Read);

        !placement.keeping.is_empty() || !contained
    }

    fn mode(&self) -> &Mode {
        self.frames.last().map_or(&Mode::Run, |frame| &frame.mode)
    }

    fn runs(&self) -> bool {
        self.region.is_some() && *self.mode() != Mode::Deferred
    }

    fn boundary(&self) -> (usize, Boundary) {
        match (self.frames.last(), self.region) {
            (Some(frame), _) => (frame.index, Boundary::Nested),
            (None, Some(region)) => (region.boundary_index, region.boundary),
            (None, None) => (self.ancestors.len(), Boundary::Nested),
        }
    }

    fn fate(&self, span: Span) -> Fate {
        self.fate_in(&self.ancestors, span)
    }

    fn fate_in(&self, ancestors: &[AstKind<'a>], span: Span) -> Fate {
        Fate::of(placement(ancestors, self.boundary(), span, self))
    }

    fn reference_fate(&self, span: Span) -> Fate {
        match self.mode() {
            Mode::Deferred => Fate::kept(Vec::new()),
            Mode::Kept(kept_by, None) => Fate::kept(kept_by.clone()),
            Mode::Kept(kept_by, Some(locals)) => Fate::through(locals.clone(), kept_by.clone()),
            Mode::Run if self.callee_of(span).is_some() => Fate::run(),
            Mode::Run => self.fate(span),
        }
    }

    fn callee_of(&self, span: Span) -> Option<Callee> {
        match self.ancestors.last() {
            Some(AstKind::CallExpression(call)) if call.callee.span() == span => Some(Callee {
                call: call.span,
                pure: call.pure,
                wrap: called,
            }),
            Some(AstKind::NewExpression(new)) if new.callee.span() == span => Some(Callee {
                call: new.span,
                pure: new.pure,
                wrap: constructed,
            }),
            _ => None,
        }
    }

    fn run_occurrence(&mut self, symbol_use: Use, span: Span) {
        if !self.runs() {
            return;
        }

        let condition = match self.callee_of(span) {
            Some(callee) => {
                if !callee.pure {
                    self.run(vec![(callee.wrap)(symbol_use.clone())], Condition::Always);
                }

                let outside = &self.ancestors[..self.ancestors.len() - 1];

                Condition::of(self.fate_in(outside, callee.call), Question::Runs)
            }
            None => Condition::of(self.fate(span), Question::Matters),
        };

        if let Some(condition) = condition {
            self.run(vec![symbol_use], condition);
        }
    }

    fn run(&mut self, uses: Vec<Use>, condition: Condition) {
        let Some(region) = self.region else {
            return;
        };
        let condition = match self.mode() {
            Mode::Deferred => return,
            Mode::Kept(kept_by, None) => Condition::Held(kept_by.clone()),
            Mode::Kept(kept_by, Some(locals)) => {
                Condition::Through(locals.clone(), kept_by.clone(), Question::Runs)
            }
            Mode::Run => condition,
        };
        let kept_by = match condition {
            Condition::Always => None,
            Condition::Held(kept_by) => Some(kept_by),
            Condition::Through(locals, kept_by, question) => match &mut self.function {
                Some((_, function)) if function.watches(&locals) => {
                    function.defer(Pending {
                        locals,
                        question,
                        kept_by,
                        uses,
                        hole: None,
                    });

                    return;
                }
                _ => None,
            },
        };
        let runs = self.region_runs(region);

        match kept_by {
            Some(kept_by) => runs.held.entry(kept_by).or_default().uses.extend(uses),
            None => runs.uses.extend(uses),
        }
    }

    fn region_runs(&mut self, region: Region) -> &mut RunsDraft {
        match region.declaration {
            Some(declaration) => &mut self.drafts[declaration].load,
            None => self.drafts[region.sink]
                .runs
                .get_or_insert_with(RunsDraft::default),
        }
    }

    fn add_hole(&mut self, span: Span) {
        if let Some(region) = self.region {
            self.region_runs(region).holes.push(span);
        }
    }

    fn enter_function(
        &mut self,
        span: Span,
        declared: Option<SymbolId>,
        visit: impl FnOnce(&mut Self),
    ) {
        if let Some(region) = &mut self.region
            && region.function == Some(span)
        {
            region.boundary_index = self.ancestors.len();
            visit(self);

            return;
        }

        if self.region.is_none() {
            visit(self);

            return;
        }

        let mode = match self.mode() {
            Mode::Deferred => Mode::Deferred,
            Mode::Kept(kept_by, locals) => Mode::Kept(kept_by.clone(), locals.clone()),
            Mode::Run => {
                let fate = match declared {
                    Some(symbol) => Fate::through(vec![symbol], Vec::new()),
                    None => self.fate(span),
                };

                self.frame_mode(fate, span)
            }
        };

        self.frames.push(Frame {
            index: self.ancestors.len(),
            mode,
        });
        visit(self);
        self.frames.pop();
    }

    fn frame_mode(&mut self, fate: Fate, span: Span) -> Mode {
        let watched = |collector: &Self, locals: &[SymbolId]| {
            collector
                .function
                .as_ref()
                .is_some_and(|(_, function)| function.watches(locals))
        };

        match fate.step(Question::Runs) {
            Step::Counts => Mode::Run,
            Step::Kept if fate.keeping.is_empty() => {
                self.add_hole(span);

                Mode::Deferred
            }
            Step::Kept => {
                self.hold(&fate.keeping, span);

                Mode::Kept(fate.keeping, None)
            }
            Step::Through(locals) if watched(self, &locals) => {
                if let Some((_, function)) = &mut self.function {
                    function.defer(Pending {
                        locals: locals.clone(),
                        question: Question::Runs,
                        kept_by: fate.keeping.clone(),
                        uses: Vec::new(),
                        hole: Some(span),
                    });
                }

                Mode::Kept(fate.keeping, Some(locals))
            }
            Step::Through(_) => Mode::Run,
        }
    }

    fn hold(&mut self, kept_by: &[Keeping], span: Span) {
        let Some(region) = self.region else {
            return;
        };
        let runs = self.region_runs(region);

        runs.holes.push(span);
        runs.held
            .entry(kept_by.to_vec())
            .or_default()
            .spans
            .push(span);
    }

    fn call_use(&self, callee: &Expression) -> Option<Use> {
        match callee.get_inner_expression() {
            Expression::Identifier(callee) => {
                Some(binding_use(self.runtime_binding(callee)?, None))
            }
            Expression::StaticMemberExpression(member) => match &member.object {
                Expression::Identifier(object) => match self.runtime_binding(object)? {
                    Binding::Import(binding) => Some(Use::Import {
                        binding,
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
        let watched = self
            .function
            .as_ref()
            .is_some_and(|(_, function)| function.watches(&[symbol]));

        if !watched {
            return;
        }

        let fate = self.reference_fate(reference.span);

        if let Some((_, function)) = &mut self.function {
            function.record(symbol, fate);
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

    fn this_member(&self, member: String) -> Option<Use> {
        match self.this_value? {
            ThisValue::Class { class, place } => Some(Use::This {
                class,
                member,
                place,
            }),
            ThisValue::Object(object) => Some(Use::Member {
                symbol: object,
                member,
            }),
        }
    }

    fn runs_now(&mut self) {
        if self.frames.is_empty()
            && let Some(Region {
                declaration: Some(declaration),
                ..
            }) = self.region
        {
            self.drafts[declaration].runs_at_load = true;
        }
    }

    fn use_module(&mut self, specifier: &str) {
        let symbol_use = Use::Module(specifier.to_owned());

        self.record(symbol_use.clone());
        self.run(vec![symbol_use], Condition::Always);
    }

    fn load_narrowed(&mut self, specifier: &str, load: Span) -> usize {
        self.narrowed_loads.insert(load);
        self.imports.push(ImportBinding {
            specifier: specifier.to_owned(),
            imported: Imported::Dynamic,
        });

        let symbol_use = Use::Load(specifier.to_owned());

        self.record(symbol_use.clone());
        self.run(vec![symbol_use], Condition::Always);

        self.imports.len() - 1
    }

    fn use_loaded_members(&mut self, specifier: &str, load: Span, members: Vec<String>) {
        let binding = self.load_narrowed(specifier, load);

        for member in members {
            let symbol_use = Use::Import {
                binding,
                member: Some(member),
            };

            self.record(symbol_use.clone());
            self.run(vec![symbol_use], Condition::Always);
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

    fn writes_member(&self, assignment: &AssignmentExpression) -> bool {
        let plain_object = |object: &Expression| match object {
            Expression::ThisExpression(_) => true,
            Expression::Identifier(identifier) => !self.is_unresolved(identifier),
            _ => false,
        };

        assignment.operator == AssignmentOperator::Assign
            && assignment
                .left
                .as_simple_assignment_target()
                .is_some_and(|target| match target {
                    SimpleAssignmentTarget::StaticMemberExpression(member) => {
                        plain_object(&member.object)
                    }
                    SimpleAssignmentTarget::PrivateFieldExpression(field) => {
                        plain_object(&field.object)
                    }
                    SimpleAssignmentTarget::ComputedMemberExpression(member) => {
                        plain_object(&member.object) && literal_text(&member.expression).is_some()
                    }
                    _ => false,
                })
    }

    fn visit_class_field(
        &mut self,
        span: Span,
        is_static: bool,
        decorators: &[Decorator<'a>],
        key: &PropertyKey<'a>,
        value: Option<&Expression<'a>>,
    ) {
        let Some(class) = self.class else {
            return;
        };
        let member = self.members.get(&span).copied().unwrap_or(class);
        let saved_owner = self.owner.replace(member);

        for decorator in decorators {
            self.visit_decorator(decorator);
        }

        self.owner = Some(class);
        self.visit_property_key(key);

        if let Some(value) = value {
            let region = if is_static {
                self.region
            } else {
                self.add_hole(value.span());

                Some(Region::field(class, self.ancestors.len() - 1))
            };
            let saved = self.enter_context(Context {
                owner: Some(member),
                region,
                frames: Vec::new(),
                this_value: Some(ThisValue::Class {
                    class,
                    place: place(is_static),
                }),
                member_function: None,
            });

            self.visit_expression(value);
            self.restore_context(saved);
        }

        self.owner = saved_owner;
    }
}

impl Bindings for SymbolCollector<'_, '_> {
    fn callee_use(&self, callee: &Expression) -> Option<Use> {
        self.call_use(callee)
    }

    fn is_local(&self, reference: &IdentifierReference) -> bool {
        self.resolved_symbol(reference).is_some() && self.runtime_binding(reference).is_none()
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
            self.use_binding(binding_use(binding, None), reference.span);
        }
    }

    fn visit_this_expression(&mut self, this: &ThisExpression) {
        let whole = match self.this_value {
            Some(ThisValue::Class { class, .. }) => Some(class),
            Some(ThisValue::Object(object)) => Some(object),
            None => None,
        };

        if let Some(whole) = whole {
            self.use_binding(Use::Local(whole), this.span);
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

        let name = member.property.name.to_string();
        let symbol_use = match &member.object {
            Expression::Identifier(object) => {
                if object.name == "module" && name == "exports" && self.is_unresolved(object) {
                    self.fallback.get_or_insert(FallbackCause::CommonJs);
                }

                self.runtime_binding(object)
                    .map(|binding| binding_use(binding, Some(name)))
            }
            Expression::ThisExpression(_) => self.this_member(name),
            Expression::Super(_) => match self.this_value {
                Some(ThisValue::Class { class, place }) => Some(Use::Super {
                    class,
                    member: name,
                    place,
                }),
                _ => None,
            },
            _ => None,
        };

        match symbol_use {
            Some(symbol_use) => self.use_binding(symbol_use, member.span),
            None => walk::walk_static_member_expression(self, member),
        }
    }

    fn visit_private_field_expression(&mut self, field: &PrivateFieldExpression<'a>) {
        let symbol_use = match &field.object {
            Expression::ThisExpression(_) => self.this_member(format!("#{}", field.field.name)),
            _ => None,
        };

        match symbol_use {
            Some(symbol_use) => self.use_binding(symbol_use, field.span),
            None => walk::walk_private_field_expression(self, field),
        }
    }

    fn visit_computed_member_expression(&mut self, member: &ComputedMemberExpression<'a>) {
        let key = literal_text(&member.expression).map(str::to_owned);

        if let Some((specifier, load)) = loaded_module(&member.object)
            && let Some(name) = &key
        {
            self.use_loaded_members(specifier, load, vec![name.clone()]);
        }

        let symbol_use = match &member.object {
            Expression::Identifier(object) => self
                .runtime_binding(object)
                .map(|binding| binding_use(binding, key)),
            Expression::ThisExpression(_) => key.and_then(|key| self.this_member(key)),
            _ => None,
        };

        match symbol_use {
            Some(symbol_use) => {
                self.use_binding(symbol_use, member.span);
                self.visit_expression(&member.expression);
            }
            None => walk::walk_computed_member_expression(self, member),
        }
    }

    fn visit_jsx_member_expression(&mut self, member: &JSXMemberExpression<'a>) {
        if let JSXMemberExpressionObject::IdentifierReference(object) = &member.object
            && let Some(binding @ Binding::Import(_)) = self.runtime_binding(object)
        {
            self.use_binding(
                binding_use(binding, Some(member.property.name.to_string())),
                member.span,
            );

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

        if matches!(call.callee, Expression::Super(_))
            && let Some(ThisValue::Class { class, .. }) = self.this_value
            && let Some(base) = self.base_construction(class)
        {
            self.run(vec![base], Condition::Always);
        }

        walk::walk_call_expression(self, call);
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

        let assigned =
            assignment
                .left
                .as_simple_assignment_target()
                .and_then(|target| match target {
                    SimpleAssignmentTarget::StaticMemberExpression(member)
                        if matches!(member.object, Expression::ThisExpression(_)) =>
                    {
                        Some(member.property.name.to_string())
                    }
                    SimpleAssignmentTarget::PrivateFieldExpression(field)
                        if matches!(field.object, Expression::ThisExpression(_)) =>
                    {
                        Some(format!("#{}", field.field.name))
                    }
                    _ => None,
                });

        if let Some(key) = assigned
            && let Some(ThisValue::Class {
                class,
                place: Place::Instance,
            }) = self.this_value
            && let Some(owner) = self.owner
            && let Some(member) = &mut self.drafts[owner].member
            && member.owner == class
            && !member.assigns.contains(&key)
        {
            member.assigns.push(key);
        }

        let kind = AstKind::AssignmentExpression(self.alloc(assignment));

        self.enter_node(kind);

        if !self.writes_member(assignment) {
            self.visit_assignment_target(&assignment.left);
        }

        self.visit_expression(&assignment.right);
        self.leave_node(kind);
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

        let Some(class) = self.class else {
            walk::walk_static_block(self, static_block);

            return;
        };
        let saved_owner = self.owner.replace(class);
        let saved_this = self.this_value.replace(ThisValue::Class {
            class,
            place: Place::Own,
        });

        walk::walk_static_block(self, static_block);
        self.owner = saved_owner;
        self.this_value = saved_this;
    }

    fn visit_class(&mut self, class: &Class<'a>) {
        if self.collected_class == Some(class.span) {
            walk::walk_class(self, class);

            return;
        }

        let saved = self.class.take();

        walk::walk_class(self, class);
        self.class = saved;
    }

    fn visit_method_definition(&mut self, method: &MethodDefinition<'a>) {
        let Some(class) = self.class else {
            walk::walk_method_definition(self, method);

            return;
        };
        let member = self.members.get(&method.span).copied().unwrap_or(class);
        let kind = AstKind::MethodDefinition(self.alloc(method));

        self.enter_node(kind);

        let saved_owner = self.owner.replace(member);

        for decorator in &method.decorators {
            self.visit_decorator(decorator);
        }

        self.owner = Some(class);
        self.visit_property_key(&method.key);
        self.add_hole(method.value.span);

        let (flags, region) = match method.kind {
            MethodDefinitionKind::Constructor => (
                ScopeFlags::Function | ScopeFlags::Constructor,
                Some(Region::function(
                    class,
                    Boundary::Constructor,
                    method.value.span,
                )),
            ),
            MethodDefinitionKind::Get => (ScopeFlags::Function | ScopeFlags::GetAccessor, None),
            MethodDefinitionKind::Set => (ScopeFlags::Function | ScopeFlags::SetAccessor, None),
            MethodDefinitionKind::Method => (ScopeFlags::Function, None),
        };
        let facts = region.map(|_| {
            let body = method
                .value
                .body
                .as_ref()
                .map_or(&[][..], |body| &body.statements);

            (class, FunctionFacts::new(&method.value.params, body))
        });
        let saved_function = std::mem::replace(&mut self.function, facts);
        let saved = self.enter_context(Context {
            owner: Some(member),
            region,
            frames: Vec::new(),
            this_value: Some(ThisValue::Class {
                class,
                place: place(method.r#static),
            }),
            member_function: Some(method.value.span),
        });

        self.visit_function(&method.value, flags);
        self.finish_function();
        self.restore_context(saved);
        self.function = saved_function;
        self.owner = saved_owner;
        self.leave_node(kind);
    }

    fn visit_property_definition(&mut self, property: &PropertyDefinition<'a>) {
        if self.class.is_none() {
            walk::walk_property_definition(self, property);

            return;
        }

        let kind = AstKind::PropertyDefinition(self.alloc(property));

        self.enter_node(kind);
        self.visit_class_field(
            property.span,
            property.r#static,
            &property.decorators,
            &property.key,
            property.value.as_ref(),
        );
        self.leave_node(kind);
    }

    fn visit_accessor_property(&mut self, property: &AccessorProperty<'a>) {
        if self.class.is_none() {
            walk::walk_accessor_property(self, property);

            return;
        }

        let kind = AstKind::AccessorProperty(self.alloc(property));

        self.enter_node(kind);
        self.visit_class_field(
            property.span,
            property.r#static,
            &property.decorators,
            &property.key,
            property.value.as_ref(),
        );
        self.leave_node(kind);
    }

    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        let Some(&member) = self.members.get(&property.span) else {
            walk::walk_object_property(self, property);

            return;
        };
        let object = self.drafts[member]
            .member
            .as_ref()
            .map_or(member, |membership| membership.owner);
        let saved_owner = self.owner.replace(member);
        let saved_this = self.this_value;
        let saved_member_function = self.member_function;

        if let Expression::FunctionExpression(function) = property.value.get_inner_expression() {
            self.this_value = Some(ThisValue::Object(object));
            self.member_function = Some(function.span);
        }

        walk::walk_object_property(self, property);
        self.owner = saved_owner;
        self.this_value = saved_this;
        self.member_function = saved_member_function;
    }

    fn visit_function(&mut self, function: &Function<'a>, flags: ScopeFlags) {
        let this_value = if self.member_function == Some(function.span) {
            self.this_value
        } else {
            None
        };
        let saved_this = std::mem::replace(&mut self.this_value, this_value);
        let declared = function
            .is_declaration()
            .then(|| function.id.as_ref().and_then(|id| id.symbol_id.get()))
            .flatten();

        self.enter_function(function.span, declared, |collector| {
            walk::walk_function(collector, function, flags);
        });
        self.this_value = saved_this;
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'a>) {
        self.enter_function(arrow.span, None, |collector| {
            walk::walk_arrow_function_expression(collector, arrow);
        });
    }

    fn visit_ts_type(&mut self, _: &TSType<'a>) {}

    fn visit_ts_type_annotation(&mut self, _: &TSTypeAnnotation<'a>) {}

    fn visit_ts_type_parameter_declaration(&mut self, _: &TSTypeParameterDeclaration<'a>) {}

    fn visit_ts_type_parameter_instantiation(&mut self, _: &TSTypeParameterInstantiation<'a>) {}

    fn visit_ts_class_implements(&mut self, _: &TSClassImplements<'a>) {}
}

fn called(symbol_use: Use) -> Use {
    Use::Call(Box::new(symbol_use))
}

fn constructed(symbol_use: Use) -> Use {
    Use::New(Box::new(symbol_use))
}

fn binding_use(binding: Binding, member: Option<String>) -> Use {
    match (binding, member) {
        (Binding::Local(symbol), None) => Use::Local(symbol),
        (Binding::Local(symbol), Some(member)) => Use::Member { symbol, member },
        (Binding::Import(binding), member) => Use::Import { binding, member },
    }
}

fn place(is_static: bool) -> Place {
    if is_static {
        Place::Own
    } else {
        Place::Instance
    }
}

fn member_key(key: &PropertyKey) -> Option<String> {
    key.static_name()
        .map(|name| name.into_owned())
        .or_else(|| key.private_name().map(|name| format!("#{name}")))
}

fn member_name(owner: &str, key: &str, place: Place, in_class: bool) -> String {
    let access = if is_identifier_name(key) {
        format!("{owner}.{key}")
    } else {
        format!("{owner}[{key:?}]")
    };

    match place {
        Place::Own if in_class => format!("static {access}"),
        Place::Own | Place::Instance => access,
    }
}

fn is_identifier_name(key: &str) -> bool {
    let name = key.strip_prefix('#').unwrap_or(key);
    let mut chars = name.chars();

    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        && chars.all(|next| next.is_alphanumeric() || next == '_' || next == '$')
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
            Use::Member { symbol, member } => {
                format!("{}.{member}", module.symbols[*symbol].name)
            }
            Use::This {
                class,
                member,
                place,
            } => format!("this {}", described_member(module, *class, member, *place)),
            Use::Super {
                class,
                member,
                place,
            } => format!("super {}", described_member(module, *class, member, *place)),
            Use::Call(called) => format!("{}()", describe_use(module, called)),
            Use::New(constructed) => format!("new {}()", describe_use(module, constructed)),
        }
    }

    fn described_member(
        module: &ModuleSymbols,
        class: usize,
        member: &str,
        place: Place,
    ) -> String {
        let class = &module.symbols[class].name;

        match place {
            Place::Own => format!("static {class}.{member}"),
            Place::Instance => format!("{class}.{member}"),
        }
    }

    fn described_uses(module: &ModuleSymbols, uses: &[Use]) -> Vec<String> {
        uses.iter()
            .map(|symbol_use| describe_use(module, symbol_use))
            .collect()
    }

    fn described_held(module: &ModuleSymbols, held: &[Held]) -> Vec<(Vec<String>, Vec<String>)> {
        held.iter()
            .map(|held| {
                let kept_by = held
                    .kept_by
                    .iter()
                    .map(|keeping| described_keeping(module, keeping))
                    .collect();

                (kept_by, described_uses(module, &held.uses))
            })
            .collect()
    }

    fn described_keeping(module: &ModuleSymbols, keeping: &Keeping) -> String {
        format!(
            "{} #{}",
            describe_use(module, &keeping.callee),
            keeping.index
        )
    }

    type DescribedRuns = (String, Vec<String>, Vec<(Vec<String>, Vec<String>)>);

    fn runs(source_text: &str) -> Vec<DescribedRuns> {
        let module = module_symbols(source_text);
        let init = (
            MODULE_INIT_NAME.to_owned(),
            described_uses(&module, &module.symbols[ModuleSymbols::INIT].uses),
            described_held(&module, &module.held),
        );
        let others = module.symbols.iter().filter_map(|symbol| {
            let runs = symbol.runs.as_ref()?;

            let name = if symbol.kind == SymbolKind::Class {
                format!("new {}()", symbol.name)
            } else {
                format!("{}()", symbol.name)
            };

            Some((
                name,
                described_uses(&module, &runs.uses),
                described_held(&module, &runs.held),
            ))
        });

        std::iter::once(init).chain(others).collect()
    }

    fn ran(name: &str, uses: &[&str]) -> DescribedRuns {
        (
            name.to_owned(),
            uses.iter()
                .map(|&symbol_use| symbol_use.to_owned())
                .collect(),
            Vec::new(),
        )
    }

    fn ran_holding(name: &str, uses: &[&str], held: &[(&[&str], &[&str])]) -> DescribedRuns {
        let held = held
            .iter()
            .map(|(kept_by, uses)| {
                (
                    kept_by.iter().map(|&keeping| keeping.to_owned()).collect(),
                    uses.iter()
                        .map(|&symbol_use| symbol_use.to_owned())
                        .collect(),
                )
            })
            .collect();

        (
            name.to_owned(),
            uses.iter()
                .map(|&symbol_use| symbol_use.to_owned())
                .collect(),
            held,
        )
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
                ("Store.save", SymbolKind::Member),
                ("Base", SymbolKind::Class),
                ("VALUE", SymbolKind::Variable),
                ("DEFAULTS", SymbolKind::Variable),
                ("DEFAULTS.a", SymbolKind::Member),
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
                used(MODULE_INIT_NAME, &["pair()"]),
                used("load", &["helper"]),
                used("helper", &["VALUE"]),
                used("Store", &["Base"]),
                used("Store.save", &["load", "DEFAULTS"]),
                used("Base", &[]),
                used("VALUE", &[]),
                used("DEFAULTS", &[]),
                used("DEFAULTS.a", &["VALUE"]),
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
    fn module_initialisation_runs_top_level_code_and_calls_at_load_but_not_what_declarations_store()
    {
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
            runs(source_text),
            [
                ran_holding(
                    MODULE_INIT_NAME,
                    &[
                        "log",
                        "decorate",
                        "./registry#register()",
                        "./logger#createLogger()",
                        "new Widget()"
                    ],
                    &[(&["./registry#register #0"], &["handlers"])]
                ),
                ran("new Widget()", &[]),
                ran("new Lazy()", &["new Lazy()"]),
                ran("new Decorated()", &[]),
                ran("makeMap()", &[]),
            ]
        );
        assert_eq!(
            uses(source_text)[1..],
            [
                used("log", &["./logger#createLogger"]),
                used("handlers", &[]),
                used("handlers.start", &["./registry#register"]),
                used("ready", &["log"]),
                used("Widget", &[]),
                used("static Widget.instance", &["Widget"]),
                used("Widget.render", &["log"]),
                used("Lazy", &[]),
                used("Lazy.instance", &["Lazy"]),
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
                            .map(|keeping| described_keeping(&module, keeping))
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
                parameters("unused", &[(true, &[]), (true, &[])]),
            ]
        );
    }

    #[test]
    fn declarations_running_at_load_hand_their_callbacks_to_the_functions_they_call() {
        let source_text = r#"
            import { createLazyRuntimeModule } from "./lazy";
            const loadRuntime = createLazyRuntimeModule(() => import("./runtime"));
            const value = compute(helper, { limit: LIMIT });
            const LIMIT = 3;
            function helper() { return 1; }
            function compute(callback: () => number, options: { limit: number }) { return callback() + options.limit; }
            export { loadRuntime, value };
        "#;

        assert_eq!(
            runs(source_text)[0],
            ran_holding(
                MODULE_INIT_NAME,
                &["compute()", "./lazy#createLazyRuntimeModule()"],
                &[
                    (&["compute #0"], &["helper"]),
                    (&["compute #1"], &["LIMIT"]),
                    (
                        &["./lazy#createLazyRuntimeModule #0"],
                        &["module ./runtime"]
                    ),
                ]
            )
        );
        assert_eq!(
            module_symbols(source_text).symbols[ModuleSymbols::INIT]
                .lines
                .len(),
            2
        );
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

    #[test]
    fn calling_a_function_at_load_runs_its_statements_not_the_functions_it_returns() {
        let source_text = r#"
            import { log } from "./log";
            export function createHandlers(prefix: string) {
                log(prefix);
                const greet = async (name: string) => (await import("./greeting")).format(prefix, name);
                return { greet, shout: (name: string) => import("./shouting").then(({ shout }) => shout(name)) };
            }
            export const handlers = createHandlers("hello");
        "#;

        assert_eq!(
            runs(source_text),
            [
                ran(MODULE_INIT_NAME, &["createHandlers()"]),
                ran("createHandlers()", &["./log#log()"]),
            ]
        );
        assert_eq!(
            uses(source_text)[1..],
            [
                used(
                    "createHandlers",
                    &[
                        "./greeting#import().format",
                        "./shouting#import().shout",
                        "load ./greeting",
                        "load ./shouting",
                        "./log#log()"
                    ]
                ),
                used("handlers", &["createHandlers"]),
            ]
        );
    }

    #[test]
    fn reads_that_decide_what_runs_count_and_dropped_results_need_only_what_the_call_runs() {
        let source_text = r#"
            import { FLAG, LIMIT, register, save, load, render } from "./lib";
            if (FLAG) register();
            export const label = LIMIT > 3 ? "many" : "few";
            export function store(value: string) {
                save(value);
                if (load(value)) return render(value);
                return `${LIMIT}`;
            }
        "#;

        assert_eq!(
            runs(source_text),
            [
                ran(MODULE_INIT_NAME, &["./lib#FLAG", "./lib#register()"]),
                ran(
                    "store()",
                    &["./lib#save()", "./lib#load()", "./lib#render()"]
                ),
            ]
        );
        assert_eq!(
            uses(source_text)[1..],
            [
                used("label", &["./lib#LIMIT"]),
                used(
                    "store",
                    &[
                        "./lib#LIMIT",
                        "./lib#render",
                        "./lib#save()",
                        "./lib#load()"
                    ]
                ),
            ]
        );
    }

    #[test]
    fn classes_split_into_members_and_what_constructing_them_runs() {
        let source_text = r#"
            import { Base, connect, initial, register } from "./base";
            export class Store extends Base {
                static instance = new Store();
                static { register(Store); }
                private items: string[] = initial();
                private handler = () => connect();
                constructor(readonly name: string) { super(name); this.created = Date.now(); }
                add(item: string) { this.items.push(item); return this.count(); }
                count() { return super.size() + this.items.length; }
                static create() { return Store.instance; }
                #secret() { return this.handler(); }
            }
        "#;
        let module = module_symbols(source_text);
        let members: Vec<(&str, Place, Vec<&str>)> = module
            .symbols
            .iter()
            .filter_map(|symbol| {
                let member = symbol.member.as_ref()?;

                Some((
                    symbol.name.as_str(),
                    member.place,
                    member.assigns.iter().map(String::as_str).collect(),
                ))
            })
            .collect();

        assert_eq!(
            members,
            [
                ("static Store.instance", Place::Own, vec![]),
                ("Store.items", Place::Instance, vec![]),
                ("Store.handler", Place::Instance, vec![]),
                ("Store.constructor", Place::Instance, vec!["created"]),
                ("Store.add", Place::Instance, vec![]),
                ("Store.count", Place::Instance, vec![]),
                ("static Store.create", Place::Own, vec![]),
                ("Store.#secret", Place::Instance, vec![]),
            ]
        );
        assert_eq!(
            runs(source_text),
            [
                ran_holding(
                    MODULE_INIT_NAME,
                    &["./base#register()", "new Store()"],
                    &[(&["./base#register #0"], &["Store"])]
                ),
                ran("new Store()", &["./base#initial()", "new ./base#Base()"]),
            ]
        );
        assert_eq!(
            uses(source_text)[1..],
            [
                used("Store", &["./base#Base", "./base#register()"]),
                used("static Store.instance", &["Store"]),
                used("Store.items", &["./base#initial"]),
                used("Store.handler", &["./base#connect"]),
                used("Store.constructor", &[]),
                used("Store.add", &["this Store.count", "this Store.items"]),
                used("Store.count", &["this Store.items", "super Store.size()"]),
                used("static Store.create", &["Store.instance"]),
                used("Store.#secret", &["this Store.handler"]),
            ]
        );
    }

    #[test]
    fn constructing_a_class_whose_base_is_not_followed_runs_the_whole_class() {
        let source_text = r#"
            import { mixin, Base } from "./base";
            export class Store extends mixin(Base) {}
            export class Explicit extends mixin(Base) { constructor() { super(); } }
            export const store = new Store();
            export const explicit = new Explicit();
        "#;

        assert_eq!(
            runs(source_text),
            [
                ran_holding(
                    MODULE_INIT_NAME,
                    &["./base#mixin()", "new Store()", "new Explicit()"],
                    &[(&["./base#mixin #0"], &["./base#Base"])]
                ),
                ran("new Store()", &["Store"]),
                ran("new Explicit()", &["Explicit"]),
            ]
        );
    }

    #[test]
    fn object_literals_split_into_properties_read_by_name() {
        let source_text = r#"
            import { send } from "./send";
            export const handlers = {
                "chat.send": (text: string) => send(text),
                ping() { return this["chat.send"]("ping"); },
                ...extra,
            };
            const extra = { quiet: true };
            export const reply = () => handlers.ping();
        "#;
        let module = module_symbols(source_text);

        assert_eq!(
            uses(source_text)[1..],
            [
                used("handlers", &["extra"]),
                used("handlers[\"chat.send\"]", &["./send#send"]),
                used("handlers.ping", &["handlers.chat.send"]),
                used("extra", &[]),
                used("extra.quiet", &[]),
                used("reply", &["handlers.ping"]),
            ]
        );
        assert_eq!(module.symbols[1].shape, Some(Shape::Object { open: true }));
    }

    #[test]
    fn callbacks_stay_out_of_what_runs_when_handed_on_only_to_be_kept() {
        let source_text = r#"
            export function define(...[method, handler]: [string, () => void]) {
                return Object.assign((request: unknown) => handler(request), { method });
            }
            export function inject({ run, label }: { run: () => void; label: string }) {
                const { name } = { name: label };
                return Object.freeze({ run, name });
            }
            export class Lifecycle {
                constructor(readonly options: { run: () => void }) {}
            }
        "#;

        assert_eq!(
            parameters_of(source_text),
            [
                parameters("define", &[(true, &[]), (true, &[])]),
                parameters("inject", &[(true, &[])]),
                parameters("Lifecycle", &[(true, &[])]),
            ]
        );
    }
}
