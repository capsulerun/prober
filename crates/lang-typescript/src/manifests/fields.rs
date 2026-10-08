//! What a change of each manifest, workspace and tsconfig field can affect.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldEffect {
    NoTest,
    Packages(PackageField),
    Resolution,
    PackageScope,
    Entries,
    Nested(FieldTable),
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageField {
    NameMap,
    NameMaps,
    SelectorMap,
    NameList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldTable {
    PackageJson,
    PnpmSettings,
    Tsconfig,
    CompilerOptions,
}

const PACKAGE_METADATA: &[&str] = &[
    "$schema",
    "author",
    "bugs",
    "commitlint",
    "contributors",
    "cpu",
    "description",
    "devEngines",
    "directories",
    "displayName",
    "engineStrict",
    "engines",
    "eslintConfig",
    "eslintIgnore",
    "files",
    "funding",
    "galleryBanner",
    "gitHead",
    "homepage",
    "husky",
    "icon",
    "keywords",
    "knip",
    "libc",
    "license",
    "licenses",
    "lint-staged",
    "maintainers",
    "man",
    "os",
    "packageManager",
    "prettier",
    "private",
    "publishConfig",
    "readme",
    "readmeFilename",
    "release",
    "renovate",
    "repository",
    "scripts",
    "sideEffects",
    "simple-git-hooks",
    "size-limit",
    "stylelint",
    "types",
    "typesVersions",
    "typings",
    "version",
    "volta",
];

const PNPM_POLICIES: &[&str] = &[
    "allowNonAppliedPatches",
    "allowedDeprecatedVersions",
    "auditConfig",
    "autoInstallPeers",
    "blockExoticSubdeps",
    "catalogMode",
    "cleanupUnusedCatalogs",
    "dedupePeers",
    "ignorePatchFailures",
    "linkWorkspacePackages",
    "minimumReleaseAge",
    "minimumReleaseAgeExclude",
    "minimumReleaseAgeStrict",
    "packageImportMethod",
    "peerDependencyRules",
    "preferWorkspacePackages",
    "requiredScripts",
    "resolutionMode",
    "saveExact",
    "savePrefix",
    "strictPeerDependencies",
    "trustPolicy",
    "trustPolicyExclude",
    "trustPolicyIgnoreAfter",
    "updateConfig",
    "verifyDepsBeforeRun",
];

const TSCONFIG_TOOLING: &[&str] = &[
    "$schema",
    "buildOptions",
    "compileOnSave",
    "display",
    "typeAcquisition",
    "watchOptions",
];

const TYPE_CHECKING_OPTIONS: &[&str] = &[
    "allowArbitraryExtensions",
    "allowImportingTsExtensions",
    "allowJs",
    "allowSyntheticDefaultImports",
    "allowUnreachableCode",
    "allowUnusedLabels",
    "assumeChangesOnlyAffectDirectDependencies",
    "checkJs",
    "composite",
    "declaration",
    "declarationDir",
    "declarationMap",
    "diagnostics",
    "disableReferencedProjectLoad",
    "disableSizeLimit",
    "disableSolutionSearching",
    "disableSourceOfProjectReferenceRedirect",
    "emitDeclarationOnly",
    "erasableSyntaxOnly",
    "exactOptionalPropertyTypes",
    "explainFiles",
    "extendedDiagnostics",
    "forceConsistentCasingInFileNames",
    "generateTrace",
    "incremental",
    "isolatedDeclarations",
    "isolatedModules",
    "lib",
    "libReplacement",
    "listEmittedFiles",
    "listFiles",
    "maxNodeModuleJsDepth",
    "moduleResolution",
    "noCheck",
    "noEmit",
    "noEmitOnError",
    "noErrorTruncation",
    "noFallthroughCasesInSwitch",
    "noImplicitAny",
    "noImplicitOverride",
    "noImplicitReturns",
    "noImplicitThis",
    "noLib",
    "noPropertyAccessFromIndexSignature",
    "noUncheckedIndexedAccess",
    "noUncheckedSideEffectImports",
    "noUnusedLocals",
    "noUnusedParameters",
    "plugins",
    "pretty",
    "resolveJsonModule",
    "resolvePackageJsonExports",
    "resolvePackageJsonImports",
    "skipDefaultLibCheck",
    "skipLibCheck",
    "strict",
    "strictBindCallApply",
    "strictBuiltinIteratorReturn",
    "strictFunctionTypes",
    "strictNullChecks",
    "strictPropertyInitialization",
    "stripInternal",
    "traceResolution",
    "tsBuildInfoFile",
    "typeRoots",
    "types",
    "useUnknownInCatchVariables",
];

impl FieldTable {
    pub fn effect(self, field: &str) -> FieldEffect {
        match self {
            FieldTable::PackageJson => package_json_field(field),
            FieldTable::PnpmSettings => pnpm_setting(field),
            FieldTable::Tsconfig => tsconfig_field(field),
            FieldTable::CompilerOptions => compiler_option(field),
        }
    }
}

fn package_json_field(field: &str) -> FieldEffect {
    match field {
        "dependencies"
        | "devDependencies"
        | "optionalDependencies"
        | "peerDependencies"
        | "peerDependenciesMeta"
        | "dependenciesMeta" => FieldEffect::Packages(PackageField::NameMap),
        "bundleDependencies" | "bundledDependencies" | "trustedDependencies" => {
            FieldEffect::Packages(PackageField::NameList)
        }
        "overrides" | "resolutions" => FieldEffect::Packages(PackageField::SelectorMap),
        "pnpm" => FieldEffect::Nested(FieldTable::PnpmSettings),
        "name" | "main" | "exports" | "imports" | "workspaces" => FieldEffect::Resolution,
        "type" | "module" | "browser" => FieldEffect::PackageScope,
        "bin" => FieldEffect::Entries,
        _ if PACKAGE_METADATA.contains(&field) => FieldEffect::NoTest,
        _ => FieldEffect::Global,
    }
}

fn pnpm_setting(setting: &str) -> FieldEffect {
    match setting {
        "packages" => FieldEffect::Resolution,
        "catalog" => FieldEffect::Packages(PackageField::NameMap),
        "catalogs" => FieldEffect::Packages(PackageField::NameMaps),
        "overrides" | "packageExtensions" | "patchedDependencies" | "allowBuilds" => {
            FieldEffect::Packages(PackageField::SelectorMap)
        }
        "onlyBuiltDependencies"
        | "neverBuiltDependencies"
        | "ignoredBuiltDependencies"
        | "ignoredOptionalDependencies" => FieldEffect::Packages(PackageField::NameList),
        _ if PNPM_POLICIES.contains(&setting) => FieldEffect::NoTest,
        _ => FieldEffect::Global,
    }
}

fn tsconfig_field(field: &str) -> FieldEffect {
    match field {
        "compilerOptions" => FieldEffect::Nested(FieldTable::CompilerOptions),
        "include" | "exclude" | "files" | "references" => FieldEffect::Resolution,
        _ if TSCONFIG_TOOLING.contains(&field) => FieldEffect::NoTest,
        _ => FieldEffect::Global,
    }
}

fn compiler_option(option: &str) -> FieldEffect {
    match option {
        "paths" | "baseUrl" | "rootDirs" | "moduleSuffixes" | "customConditions" => {
            FieldEffect::Resolution
        }
        _ if TYPE_CHECKING_OPTIONS.contains(&option) => FieldEffect::NoTest,
        _ => FieldEffect::Global,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_lists_are_sorted_so_they_stay_easy_to_review() {
        for list in [
            PACKAGE_METADATA,
            PNPM_POLICIES,
            TSCONFIG_TOOLING,
            TYPE_CHECKING_OPTIONS,
        ] {
            assert!(list.is_sorted(), "{list:?}");
        }
    }

    #[test]
    fn package_json_fields_are_metadata_dependencies_resolution_or_global() {
        let package_json = FieldTable::PackageJson;

        assert_eq!(package_json.effect("version"), FieldEffect::NoTest);
        assert_eq!(package_json.effect("scripts"), FieldEffect::NoTest);
        assert_eq!(
            package_json.effect("devDependencies"),
            FieldEffect::Packages(PackageField::NameMap)
        );
        assert_eq!(
            package_json.effect("pnpm"),
            FieldEffect::Nested(FieldTable::PnpmSettings)
        );
        assert_eq!(package_json.effect("exports"), FieldEffect::Resolution);
        assert_eq!(package_json.effect("type"), FieldEffect::PackageScope);
        assert_eq!(package_json.effect("bin"), FieldEffect::Entries);
        assert_eq!(package_json.effect("browserslist"), FieldEffect::Global);
        assert_eq!(package_json.effect("jest"), FieldEffect::Global);
    }

    #[test]
    fn pnpm_settings_are_workspace_dependencies_policies_or_global() {
        let pnpm_settings = FieldTable::PnpmSettings;

        assert_eq!(pnpm_settings.effect("packages"), FieldEffect::Resolution);
        assert_eq!(
            pnpm_settings.effect("catalogs"),
            FieldEffect::Packages(PackageField::NameMaps)
        );
        assert_eq!(
            pnpm_settings.effect("overrides"),
            FieldEffect::Packages(PackageField::SelectorMap)
        );
        assert_eq!(
            pnpm_settings.effect("minimumReleaseAge"),
            FieldEffect::NoTest
        );
        assert_eq!(pnpm_settings.effect("nodeLinker"), FieldEffect::Global);
    }

    #[test]
    fn tsconfig_resolution_options_are_diffed_and_emit_options_stay_global() {
        let compiler_options = FieldTable::CompilerOptions;

        assert_eq!(
            FieldTable::Tsconfig.effect("compilerOptions"),
            FieldEffect::Nested(FieldTable::CompilerOptions)
        );
        assert_eq!(FieldTable::Tsconfig.effect("extends"), FieldEffect::Global);
        assert_eq!(compiler_options.effect("paths"), FieldEffect::Resolution);
        assert_eq!(compiler_options.effect("strict"), FieldEffect::NoTest);
        assert_eq!(compiler_options.effect("target"), FieldEffect::Global);
        assert_eq!(
            compiler_options.effect("verbatimModuleSyntax"),
            FieldEffect::Global
        );
    }
}
