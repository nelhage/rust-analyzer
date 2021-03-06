//! This module implements a reference search.
//! First, the element at the cursor position must be either an `ast::Name`
//! or `ast::NameRef`. If it's a `ast::NameRef`, at the classification step we
//! try to resolve the direct tree parent of this element, otherwise we
//! already have a definition and just need to get its HIR together with
//! some information that is needed for futher steps of searching.
//! After that, we collect files that might contain references and look
//! for text occurrences of the identifier. If there's an `ast::NameRef`
//! at the index that the match starts at and its tree parent is
//! resolved to the search element definition, we get a reference.

mod rename;

use hir::Semantics;
use ra_ide_db::{
    defs::{classify_name, classify_name_ref, Definition},
    search::SearchScope,
    RootDatabase,
};
use ra_prof::profile;
use ra_syntax::{
    algo::find_node_at_offset,
    ast::{self, NameOwner},
    AstNode, SyntaxKind, SyntaxNode, TextRange, TokenAtOffset,
};

use crate::{display::TryToNav, FilePosition, FileRange, NavigationTarget, RangeInfo};

pub(crate) use self::rename::rename;

pub use ra_ide_db::search::{Reference, ReferenceAccess, ReferenceKind};

#[derive(Debug, Clone)]
pub struct ReferenceSearchResult {
    declaration: Declaration,
    references: Vec<Reference>,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub nav: NavigationTarget,
    pub kind: ReferenceKind,
    pub access: Option<ReferenceAccess>,
}

impl ReferenceSearchResult {
    pub fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    pub fn decl_target(&self) -> &NavigationTarget {
        &self.declaration.nav
    }

    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    /// Total number of references
    /// At least 1 since all valid references should
    /// Have a declaration
    pub fn len(&self) -> usize {
        self.references.len() + 1
    }
}

// allow turning ReferenceSearchResult into an iterator
// over References
impl IntoIterator for ReferenceSearchResult {
    type Item = Reference;
    type IntoIter = std::vec::IntoIter<Reference>;

    fn into_iter(mut self) -> Self::IntoIter {
        let mut v = Vec::with_capacity(self.len());
        v.push(Reference {
            file_range: FileRange {
                file_id: self.declaration.nav.file_id(),
                range: self.declaration.nav.range(),
            },
            kind: self.declaration.kind,
            access: self.declaration.access,
        });
        v.append(&mut self.references);
        v.into_iter()
    }
}

pub(crate) fn find_all_refs(
    db: &RootDatabase,
    position: FilePosition,
    search_scope: Option<SearchScope>,
) -> Option<RangeInfo<ReferenceSearchResult>> {
    let _p = profile("find_all_refs");
    let sema = Semantics::new(db);
    let syntax = sema.parse(position.file_id).syntax().clone();

    let (opt_name, search_kind) = if let Some(name) =
        get_struct_def_name_for_struct_literal_search(&sema, &syntax, position)
    {
        (Some(name), ReferenceKind::StructLiteral)
    } else {
        (
            sema.find_node_at_offset_with_descend::<ast::Name>(&syntax, position.offset),
            ReferenceKind::Other,
        )
    };

    let RangeInfo { range, info: def } = find_name(&sema, &syntax, position, opt_name)?;

    let references = def
        .find_usages(db, search_scope)
        .into_iter()
        .filter(|r| search_kind == ReferenceKind::Other || search_kind == r.kind)
        .collect();

    let decl_range = def.try_to_nav(db)?.range();

    let declaration = Declaration {
        nav: def.try_to_nav(db)?,
        kind: ReferenceKind::Other,
        access: decl_access(&def, &syntax, decl_range),
    };

    Some(RangeInfo::new(range, ReferenceSearchResult { declaration, references }))
}

fn find_name(
    sema: &Semantics<RootDatabase>,
    syntax: &SyntaxNode,
    position: FilePosition,
    opt_name: Option<ast::Name>,
) -> Option<RangeInfo<Definition>> {
    if let Some(name) = opt_name {
        let def = classify_name(sema, &name)?.definition();
        let range = name.syntax().text_range();
        return Some(RangeInfo::new(range, def));
    }
    let name_ref =
        sema.find_node_at_offset_with_descend::<ast::NameRef>(&syntax, position.offset)?;
    let def = classify_name_ref(sema, &name_ref)?.definition();
    let range = name_ref.syntax().text_range();
    Some(RangeInfo::new(range, def))
}

fn decl_access(def: &Definition, syntax: &SyntaxNode, range: TextRange) -> Option<ReferenceAccess> {
    match def {
        Definition::Local(_) | Definition::Field(_) => {}
        _ => return None,
    };

    let stmt = find_node_at_offset::<ast::LetStmt>(syntax, range.start())?;
    if stmt.initializer().is_some() {
        let pat = stmt.pat()?;
        if let ast::Pat::BindPat(it) = pat {
            if it.mut_token().is_some() {
                return Some(ReferenceAccess::Write);
            }
        }
    }

    None
}

fn get_struct_def_name_for_struct_literal_search(
    sema: &Semantics<RootDatabase>,
    syntax: &SyntaxNode,
    position: FilePosition,
) -> Option<ast::Name> {
    if let TokenAtOffset::Between(ref left, ref right) = syntax.token_at_offset(position.offset) {
        if right.kind() != SyntaxKind::L_CURLY && right.kind() != SyntaxKind::L_PAREN {
            return None;
        }
        if let Some(name) =
            sema.find_node_at_offset_with_descend::<ast::Name>(&syntax, left.text_range().start())
        {
            return name.syntax().ancestors().find_map(ast::StructDef::cast).and_then(|l| l.name());
        }
        if sema
            .find_node_at_offset_with_descend::<ast::TypeParamList>(
                &syntax,
                left.text_range().start(),
            )
            .is_some()
        {
            return left.ancestors().find_map(ast::StructDef::cast).and_then(|l| l.name());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::{
        mock_analysis::{analysis_and_position, single_file_with_position, MockAnalysis},
        Declaration, Reference, ReferenceSearchResult, SearchScope,
    };

    #[test]
    fn test_struct_literal_after_space() {
        let code = r#"
    struct Foo <|>{
        a: i32,
    }
    impl Foo {
        fn f() -> i32 { 42 }
    }
    fn main() {
        let f: Foo;
        f = Foo {a: Foo::f()};
    }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "Foo STRUCT_DEF FileId(1) 5..39 12..15 Other",
            &["FileId(1) 138..141 StructLiteral"],
        );
    }

    #[test]
    fn test_struct_literal_befor_space() {
        let code = r#"
    struct Foo<|> {}
        fn main() {
        let f: Foo;
        f = Foo {};
    }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "Foo STRUCT_DEF FileId(1) 5..18 12..15 Other",
            &["FileId(1) 54..57 Other", "FileId(1) 71..74 StructLiteral"],
        );
    }

    #[test]
    fn test_struct_literal_with_generic_type() {
        let code = r#"
    struct Foo<T> <|>{}
        fn main() {
        let f: Foo::<i32>;
        f = Foo {};
    }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "Foo STRUCT_DEF FileId(1) 5..21 12..15 Other",
            &["FileId(1) 81..84 StructLiteral"],
        );
    }

    #[test]
    fn test_struct_literal_for_tuple() {
        let code = r#"
    struct Foo<|>(i32);

    fn main() {
        let f: Foo;
        f = Foo(1);
    }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "Foo STRUCT_DEF FileId(1) 5..21 12..15 Other",
            &["FileId(1) 71..74 StructLiteral"],
        );
    }

    #[test]
    fn test_find_all_refs_for_local() {
        let code = r#"
    fn main() {
        let mut i = 1;
        let j = 1;
        i = i<|> + j;

        {
            i = 0;
        }

        i = 5;
    }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "i BIND_PAT FileId(1) 33..34 Other Write",
            &[
                "FileId(1) 67..68 Other Write",
                "FileId(1) 71..72 Other Read",
                "FileId(1) 101..102 Other Write",
                "FileId(1) 127..128 Other Write",
            ],
        );
    }

    #[test]
    fn search_filters_by_range() {
        let code = r#"
            fn foo() {
                let spam<|> = 92;
                spam + spam
            }
            fn bar() {
                let spam = 92;
                spam + spam
            }
        "#;
        let refs = get_all_refs(code);
        check_result(
            refs,
            "spam BIND_PAT FileId(1) 44..48 Other",
            &["FileId(1) 71..75 Other Read", "FileId(1) 78..82 Other Read"],
        );
    }

    #[test]
    fn test_find_all_refs_for_param_inside() {
        let code = r#"
    fn foo(i : u32) -> u32 {
        i<|>
    }"#;

        let refs = get_all_refs(code);
        check_result(refs, "i BIND_PAT FileId(1) 12..13 Other", &["FileId(1) 38..39 Other Read"]);
    }

    #[test]
    fn test_find_all_refs_for_fn_param() {
        let code = r#"
    fn foo(i<|> : u32) -> u32 {
        i
    }"#;

        let refs = get_all_refs(code);
        check_result(refs, "i BIND_PAT FileId(1) 12..13 Other", &["FileId(1) 38..39 Other Read"]);
    }

    #[test]
    fn test_find_all_refs_field_name() {
        let code = r#"
            //- /lib.rs
            struct Foo {
                pub spam<|>: u32,
            }

            fn main(s: Foo) {
                let f = s.spam;
            }
        "#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "spam RECORD_FIELD_DEF FileId(1) 66..79 70..74 Other",
            &["FileId(1) 152..156 Other Read"],
        );
    }

    #[test]
    fn test_find_all_refs_impl_item_name() {
        let code = r#"
            //- /lib.rs
            struct Foo;
            impl Foo {
                fn f<|>(&self) {  }
            }
        "#;

        let refs = get_all_refs(code);
        check_result(refs, "f FN_DEF FileId(1) 88..104 91..92 Other", &[]);
    }

    #[test]
    fn test_find_all_refs_enum_var_name() {
        let code = r#"
            //- /lib.rs
            enum Foo {
                A,
                B<|>,
                C,
            }
        "#;

        let refs = get_all_refs(code);
        check_result(refs, "B ENUM_VARIANT FileId(1) 83..84 83..84 Other", &[]);
    }

    #[test]
    fn test_find_all_refs_two_modules() {
        let code = r#"
            //- /lib.rs
            pub mod foo;
            pub mod bar;

            fn f() {
                let i = foo::Foo { n: 5 };
            }

            //- /foo.rs
            use crate::bar;

            pub struct Foo {
                pub n: u32,
            }

            fn f() {
                let i = bar::Bar { n: 5 };
            }

            //- /bar.rs
            use crate::foo;

            pub struct Bar {
                pub n: u32,
            }

            fn f() {
                let i = foo::Foo<|> { n: 5 };
            }
        "#;

        let (analysis, pos) = analysis_and_position(code);
        let refs = analysis.find_all_refs(pos, None).unwrap().unwrap();
        check_result(
            refs,
            "Foo STRUCT_DEF FileId(2) 16..50 27..30 Other",
            &["FileId(1) 52..55 StructLiteral", "FileId(3) 77..80 StructLiteral"],
        );
    }

    // `mod foo;` is not in the results because `foo` is an `ast::Name`.
    // So, there are two references: the first one is a definition of the `foo` module,
    // which is the whole `foo.rs`, and the second one is in `use foo::Foo`.
    #[test]
    fn test_find_all_refs_decl_module() {
        let code = r#"
            //- /lib.rs
            mod foo<|>;

            use foo::Foo;

            fn f() {
                let i = Foo { n: 5 };
            }

            //- /foo.rs
            pub struct Foo {
                pub n: u32,
            }
        "#;

        let (analysis, pos) = analysis_and_position(code);
        let refs = analysis.find_all_refs(pos, None).unwrap().unwrap();
        check_result(refs, "foo SOURCE_FILE FileId(2) 0..35 Other", &["FileId(1) 13..16 Other"]);
    }

    #[test]
    fn test_find_all_refs_super_mod_vis() {
        let code = r#"
            //- /lib.rs
            mod foo;

            //- /foo.rs
            mod some;
            use some::Foo;

            fn f() {
                let i = Foo { n: 5 };
            }

            //- /foo/some.rs
            pub(super) struct Foo<|> {
                pub n: u32,
            }
        "#;

        let (analysis, pos) = analysis_and_position(code);
        let refs = analysis.find_all_refs(pos, None).unwrap().unwrap();
        check_result(
            refs,
            "Foo STRUCT_DEF FileId(3) 0..41 18..21 Other",
            &["FileId(2) 20..23 Other", "FileId(2) 46..49 StructLiteral"],
        );
    }

    #[test]
    fn test_find_all_refs_with_scope() {
        let code = r#"
            //- /lib.rs
            mod foo;
            mod bar;

            pub fn quux<|>() {}

            //- /foo.rs
            fn f() { super::quux(); }

            //- /bar.rs
            fn f() { super::quux(); }
        "#;

        let (mock, pos) = MockAnalysis::with_files_and_position(code);
        let bar = mock.id_of("/bar.rs");
        let analysis = mock.analysis();

        let refs = analysis.find_all_refs(pos, None).unwrap().unwrap();
        check_result(
            refs,
            "quux FN_DEF FileId(1) 18..34 25..29 Other",
            &["FileId(2) 16..20 StructLiteral", "FileId(3) 16..20 StructLiteral"],
        );

        let refs =
            analysis.find_all_refs(pos, Some(SearchScope::single_file(bar))).unwrap().unwrap();
        check_result(
            refs,
            "quux FN_DEF FileId(1) 18..34 25..29 Other",
            &["FileId(3) 16..20 StructLiteral"],
        );
    }

    #[test]
    fn test_find_all_refs_macro_def() {
        let code = r#"
        #[macro_export]
        macro_rules! m1<|> { () => (()) }

        fn foo() {
            m1();
            m1();
        }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "m1 MACRO_CALL FileId(1) 9..63 46..48 Other",
            &["FileId(1) 96..98 StructLiteral", "FileId(1) 114..116 StructLiteral"],
        );
    }

    #[test]
    fn test_basic_highlight_read_write() {
        let code = r#"
        fn foo() {
            let mut i<|> = 0;
            i = i + 1;
        }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "i BIND_PAT FileId(1) 40..41 Other Write",
            &["FileId(1) 59..60 Other Write", "FileId(1) 63..64 Other Read"],
        );
    }

    #[test]
    fn test_basic_highlight_field_read_write() {
        let code = r#"
        struct S {
            f: u32,
        }

        fn foo() {
            let mut s = S{f: 0};
            s.f<|> = 0;
        }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "f RECORD_FIELD_DEF FileId(1) 32..38 32..33 Other",
            &["FileId(1) 96..97 Other Read", "FileId(1) 117..118 Other Write"],
        );
    }

    #[test]
    fn test_basic_highlight_decl_no_write() {
        let code = r#"
        fn foo() {
            let i<|>;
            i = 1;
        }"#;

        let refs = get_all_refs(code);
        check_result(refs, "i BIND_PAT FileId(1) 36..37 Other", &["FileId(1) 51..52 Other Write"]);
    }

    #[test]
    fn test_find_struct_function_refs_outside_module() {
        let code = r#"
        mod foo {
            pub struct Foo;

            impl Foo {
                pub fn new<|>() -> Foo {
                    Foo
                }
            }
        }

        fn main() {
            let _f = foo::Foo::new();
        }"#;

        let refs = get_all_refs(code);
        check_result(
            refs,
            "new FN_DEF FileId(1) 87..150 94..97 Other",
            &["FileId(1) 227..230 StructLiteral"],
        );
    }

    #[test]
    fn test_find_all_refs_nested_module() {
        let code = r#"
            //- /lib.rs
            mod foo {
                mod bar;
            }

            fn f<|>() {}

            //- /foo/bar.rs
            use crate::f;

            fn g() {
                f();
            }
        "#;

        let (analysis, pos) = analysis_and_position(code);
        let refs = analysis.find_all_refs(pos, None).unwrap().unwrap();
        check_result(
            refs,
            "f FN_DEF FileId(1) 25..34 28..29 Other",
            &["FileId(2) 11..12 Other", "FileId(2) 27..28 StructLiteral"],
        );
    }

    fn get_all_refs(text: &str) -> ReferenceSearchResult {
        let (analysis, position) = single_file_with_position(text);
        analysis.find_all_refs(position, None).unwrap().unwrap()
    }

    fn check_result(res: ReferenceSearchResult, expected_decl: &str, expected_refs: &[&str]) {
        res.declaration().assert_match(expected_decl);
        assert_eq!(res.references.len(), expected_refs.len());
        res.references()
            .iter()
            .enumerate()
            .for_each(|(i, r)| ref_assert_match(r, expected_refs[i]));
    }

    impl Declaration {
        fn debug_render(&self) -> String {
            let mut s = format!("{} {:?}", self.nav.debug_render(), self.kind);
            if let Some(access) = self.access {
                s.push_str(&format!(" {:?}", access));
            }
            s
        }

        fn assert_match(&self, expected: &str) {
            let actual = self.debug_render();
            test_utils::assert_eq_text!(expected.trim(), actual.trim(),);
        }
    }

    fn ref_debug_render(r: &Reference) -> String {
        let mut s = format!("{:?} {:?} {:?}", r.file_range.file_id, r.file_range.range, r.kind);
        if let Some(access) = r.access {
            s.push_str(&format!(" {:?}", access));
        }
        s
    }

    fn ref_assert_match(r: &Reference, expected: &str) {
        let actual = ref_debug_render(r);
        test_utils::assert_eq_text!(expected.trim(), actual.trim(),);
    }
}
