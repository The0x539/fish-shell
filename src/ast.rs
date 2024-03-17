/*!
 * This defines the fish abstract syntax tree.
 * The fish ast is a tree data structure. The nodes of the tree
 * are divided into three categories:
 *
 * - leaf nodes refer to a range of source, and have no child nodes.
 * - branch nodes have ONLY child nodes, and no other fields.
 * - list nodes contain a list of some other node type (branch or leaf).
 *
 * Most clients will be interested in visiting the nodes of an ast.
 */
use crate::common::{unescape_string, UnescapeStringStyle};
use crate::flog::{FLOG, FLOGF};
use crate::parse_constants::{
    token_type_user_presentable_description, ParseError, ParseErrorCode, ParseErrorList,
    ParseKeyword, ParseTokenType, ParseTreeFlags, SourceRange, StatementDecoration,
    ERROR_BAD_COMMAND_ASSIGN_ERR_MSG, INVALID_PIPELINE_CMD_ERR_MSG, SOURCE_OFFSET_INVALID,
};
use crate::parse_tree::ParseToken;
#[cfg(test)]
use crate::tests::prelude::*;
use crate::tokenizer::{
    variable_assignment_equals_pos, TokFlags, TokenType, Tokenizer, TokenizerError,
    TOK_ACCEPT_UNFINISHED, TOK_CONTINUE_AFTER_ERROR, TOK_SHOW_COMMENTS,
};
use crate::wchar::prelude::*;
use enum_dispatch::enum_dispatch;
use std::ops::{ControlFlow, Index, IndexMut};

/**
 * A NodeVisitor is something which can visit an AST node.
 *
 * To visit a node's fields, use the node's accept() function:
 *    let mut v = MyNodeVisitor{};
 *    node.accept(&mut v);
 */
pub trait NodeVisitor<'a> {
    fn visit(&mut self, node: NodeEnumRef<'a>);
}

#[enum_dispatch]
pub trait Acceptor {
    fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool)
    where
        Self: Sized;
}

impl<T: Acceptor> Acceptor for Option<T> {
    fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {
        match self {
            Some(node) => node.accept(visitor, reversed),
            None => (),
        }
    }
}

pub struct MissingEndError {
    allowed_keywords: &'static [ParseKeyword],
    token: ParseToken,
}

pub type VisitResult = ControlFlow<MissingEndError>;

#[enum_dispatch]
trait AcceptorMut {
    fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool);
}

impl<T: AcceptorMut> AcceptorMut for Option<T> {
    fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
        match self {
            Some(node) => node.accept_mut(visitor, reversed),
            None => (),
        }
    }
}

/// Node is the base trait of all AST nodes.
#[enum_dispatch]
pub trait Node: Acceptor + std::fmt::Debug {
    /// The parent node, or null if this is root.
    fn parent(&self) -> Option<&dyn Node>;

    /// The type of this node.
    fn typ(&self) -> Type;

    /// The category of this node.
    fn category(&self) -> Category;

    /// \return a helpful string description of this node.
    fn describe(&self) -> WString {
        match self.as_node() {
            NodeEnumRef::Leaf(LeafRef::Token(n)) => {
                sprintf!(" '%ls'", n.token_type().to_wstr())
            }
            NodeEnumRef::Leaf(LeafRef::Keyword(n)) => {
                sprintf!(" '%ls'", n.keyword().to_wstr())
            }
            _ => ast_type_to_string(self.typ()).to_owned(),
        }
    }

    /// \return the source range for this node, or none if unsourced.
    /// This may return none if the parse was incomplete or had an error.
    fn try_source_range(&self) -> Option<SourceRange>;

    /// \return the source range for this node, or an empty range {0, 0} if unsourced.
    fn source_range(&self) -> SourceRange {
        self.try_source_range().unwrap_or(SourceRange::new(0, 0))
    }

    /// \return the source code for this node, or none if unsourced.
    fn try_source<'s>(&self, orig: &'s wstr) -> Option<&'s wstr> {
        self.try_source_range().map(|r| &orig[r.start()..r.end()])
    }

    /// \return the source code for this node, or an empty string if unsourced.
    fn source<'s>(&self, orig: &'s wstr) -> &'s wstr {
        self.try_source(orig).unwrap_or_default()
    }

    // The address of the object, for comparison.
    fn as_ptr(&self) -> *const ();

    fn pointer_eq(&self, rhs: &dyn Node) -> bool {
        std::ptr::eq(self.as_ptr(), rhs.as_ptr())
    }

    fn as_node(&self) -> NodeEnumRef<'_>;
}

#[allow(dead_code)]
trait EnumVal: 'static {
    type Ref<'n>;
    type Mut<'n>;

    fn to_ref(&self) -> Self::Ref<'_>;
    fn to_mut(&mut self) -> Self::Mut<'_>;
}

#[allow(dead_code)]
trait FromConcrete<T>: EnumVal {
    fn from_val(value: T) -> Self;
    fn from_ref(value: &T) -> Self::Ref<'_>;
    fn from_mut(value: &mut T) -> Self::Mut<'_>;
}

macro_rules! impl_from_concrete {
    (inner: $inner:ident, outer: $outer:ty,) => {
        impl FromConcrete<$inner> for $outer {
            fn from_val(value: $inner) -> Self {
                Self::$inner(value)
            }
            fn from_ref(value: &$inner) -> Self::Ref<'_> {
                Self::Ref::$inner(value)
            }
            fn from_mut(value: &mut $inner) -> Self::Mut<'_> {
                Self::Mut::$inner(value)
            }
        }
    };

    (inner: $inner:ty, outer: $outer:ty, variant: $variant:ident,) => {
        impl FromConcrete<$inner> for $outer {
            fn from_val(value: $inner) -> Self {
                Self::$variant(value)
            }
            fn from_ref(value: &$inner) -> Self::Ref<'_> {
                Self::Ref::$variant(value.to_ref())
            }
            fn from_mut(value: &mut $inner) -> Self::Mut<'_> {
                Self::Mut::$variant(value.to_mut())
            }
        }
    };

    (inner: $inner:ty, outer: $outer:ty, middle: $middle:ty, variant: $variant:ident,) => {
        impl FromConcrete<$inner> for $outer {
            fn from_val(value: $inner) -> Self {
                Self::$variant(<$middle>::from_val(value))
            }
            fn from_ref(value: &$inner) -> Self::Ref<'_> {
                Self::Ref::$variant(<$middle>::from_ref(value))
            }
            fn from_mut(value: &mut $inner) -> Self::Mut<'_> {
                Self::Mut::$variant(<$middle>::from_mut(value))
            }
        }
    };
}

/// NodeMut is a mutable node.
#[enum_dispatch]
trait NodeMut: Node + AcceptorMut {
    fn as_mut_node(&mut self) -> NodeEnumRefMut<'_>;
}

macro_rules! define_node {
    (
        $(#[$node_attr:meta])*
        pub enum $node:ident;

        $(
            $(#[$category_attr:meta])*
            pub enum $category:ident {
                $($variant:ident,)*
                $(@)?
                $(
                    $(#[$subcat_attr:meta])*
                    pub enum $subcat:ident {
                        $($subvariant:ident,)*
                    }
                )*
            }
        )*
    ) => {paste::paste!{
        $(#[$node_attr])*
        pub enum $node {
            $($category([< $category Enum >]),)*
        }

        $(#[$node_attr])*
        #[derive(Copy, Clone)]
        pub enum [< $node Ref >] <'n> {
            $($category([< $category Ref >] <'n> ),)*
        }

        $(#[$node_attr])*
        #[enum_dispatch(AcceptorMut, NodeMut)]
        pub enum [< $node RefMut >] <'n> {
            $($category([< $category RefMut >] <'n> ),)*
        }

        impl EnumVal for $node {
            type Ref<'n> = [< $node Ref >] <'n>;
            type Mut<'n> = [< $node RefMut >] <'n>;

            fn to_ref(&self) -> Self::Ref<'_> {
                match self {
                    $(Self::$category(x) => Self::Ref::$category(x.to_ref()),)*
                }
            }

            fn to_mut(&mut self) -> Self::Mut<'_> {
                match self {
                    $(Self::$category(x) => Self::Mut::$category(x.to_mut()),)*
                }
            }
        }

        $(
            $(#[$category_attr])*
            pub enum [< $category Enum >] {
                $($variant($variant),)*
                $($subcat([< $subcat Enum >]),)*
            }

            $(#[$category_attr])*
            #[derive(Copy, Clone)]
            pub enum [< $category Ref >] <'n> {
                $($variant(&'n $variant),)*
                $($subcat( [< $subcat Ref >] <'n> ),)*
            }

            $(#[$category_attr])*
            #[enum_dispatch(AcceptorMut, NodeMut)]
            pub enum [< $category RefMut >] <'n> {
                $($variant(&'n mut $variant),)*
                $($subcat( [< $subcat RefMut >] <'n> ),)*
            }

            impl EnumVal for [< $category Enum >] {
                type Ref<'n> = [< $category Ref >] <'n>;
                type Mut<'n> = [< $category RefMut >] <'n>;

                fn to_ref(&self) -> Self::Ref<'_> {
                    match self {
                        $(Self::$variant(x) => Self::Ref::$variant(x),)*
                        $(Self::$subcat(x) => Self::Ref::$subcat(x.to_ref()),)*
                    }
                }

                fn to_mut(&mut self) -> Self::Mut<'_> {
                    match self {
                        $(Self::$variant(x) => Self::Mut::$variant(x),)*
                        $(Self::$subcat(x) => Self::Mut::$subcat(x.to_mut()),)*
                    }
                }
            }

            $(
                $(#[$subcat_attr])*
                pub enum [< $subcat Enum >] {
                    $($subvariant($subvariant),)*
                }

                $(#[$subcat_attr])*
                #[derive(Copy, Clone)]
                pub enum [< $subcat Ref >] <'n> {
                    $($subvariant(&'n $subvariant),)*
                }

                $(#[$subcat_attr])*
                #[enum_dispatch(AcceptorMut, NodeMut)]
                pub enum [< $subcat RefMut >] <'n> {
                    $($subvariant(&'n mut $subvariant),)*
                }

                impl EnumVal for [< $subcat Enum >] {
                    type Ref<'n> = [< $subcat Ref >] <'n>;
                    type Mut<'n> = [< $subcat RefMut >] <'n>;

                    fn to_ref(&self) -> Self::Ref<'_> {
                        match self {
                            $(Self::$subvariant(x) => Self::Ref::$subvariant(x),)*
                        }
                    }

                    fn to_mut(&mut self) -> Self::Mut<'_> {
                        match self {
                            $(Self::$subvariant(x) => Self::Mut::$subvariant(x),)*
                        }
                    }
                }

            )*
        )*

        $(
            // impl FromConcrete<LeafEnum> for NodeEnum
            impl_from_concrete!{
                inner: [<$category Enum>],
                outer: $node,
                variant: $category,
            }
            $(
                // impl FromConcrete<SwitchStatement> for BranchEnum
                impl_from_concrete! {
                    inner: $variant,
                    outer: [<$category Enum>],
                }
                // impl FromConcrete<SwitchStatement> for NodeEnum
                impl_from_concrete! {
                    inner: $variant,
                    outer: $node,
                    middle: [<$category Enum>],
                    variant: $category,
                }
            )*
            $(
                // impl FromConcrete<TokenEnum> for LeafEnum
                impl_from_concrete! {
                    inner: [<$subcat Enum>],
                    outer: [<$category Enum>],
                    variant: $subcat,
                }
                // impl FromConcrete<TokenEnum> for NodeEnum
                impl_from_concrete! {
                    inner: [<$subcat Enum>],
                    outer: $node,
                    middle: [<$category Enum>],
                    variant: $category,
                }
                $(
                    // impl FromConcrete<TokenPipe> for TokenEnum
                    impl_from_concrete! {
                        inner: $subvariant,
                        outer: [<$subcat Enum>],
                    }
                    // impl FromConcrete<TokenPipe> for LeafEnum
                    impl_from_concrete! {
                        inner: $subvariant,
                        outer: [<$category Enum>],
                        middle: [<$subcat Enum>],
                        variant: $subcat,
                    }
                    // impl FromConcrete<TokenPipe> for NodeEnum
                    impl_from_concrete! {
                        inner: $subvariant,
                        outer: $node,
                        middle: [<$category Enum>],
                        variant: $category,
                    }
                )*
            )*
        )*

        impl<'a> [< $node Ref >] <'a> {
            // Special case for some annoying lifetime problems. Should go away later.
            pub fn enum_accept(self, visitor: &mut impl NodeVisitor<'a>, reverse: bool) {
                match self {
                    $(Self::$category(x) => match x {
                        $([<$category Ref>]::$variant(y) => y.accept(visitor, reverse),)*
                        $([<$category Ref>]::$subcat(z) => match z {
                            $([<$subcat Ref>]::$subvariant(w) => w.accept(visitor, reverse),)*
                        },)*
                    },)*
                }
            }
        }
    }}
}

define_node! {
    #[enum_dispatch(Acceptor, Node)]
    #[derive(Debug)]
    pub enum NodeEnum;

    #[enum_dispatch(Acceptor, Node)]
    #[derive(Debug)]
    pub enum Branch {
        Redirection,
        ArgumentOrRedirection,
        Statement,
        JobPipeline,
        JobConjunction,
        ForHeader,
        WhileHeader,
        FunctionHeader,
        BeginHeader,
        BlockStatement,
        IfClause,
        ElseifClause,
        ElseClause,
        IfStatement,
        CaseItem,
        SwitchStatement,
        DecoratedStatement,
        NotStatement,
        JobContinuation,
        JobConjunctionContinuation,
        AndorJob,
        FreestandingArgumentList,
    }

    #[enum_dispatch(Acceptor, Node, Leaf)]
    #[derive(Debug)]
    pub enum Leaf {
        VariableAssignment,
        MaybeNewlines,
        Argument,
        @
        #[enum_dispatch(Acceptor, Node, Leaf, Keyword)]
        #[derive(Debug)]
        pub enum Keyword {
            DecoratedStatementDecorator,
            JobConjunctionDecorator,
            KeywordBegin,
            KeywordCase,
            KeywordElse,
            KeywordEnd,
            KeywordFor,
            KeywordFunction,
            KeywordIf,
            KeywordIn,
            KeywordNot,
            KeywordSwitch,
            KeywordTime,
            KeywordWhile,
        }
        #[enum_dispatch(Acceptor, Node, Leaf, Token)]
        #[derive(Debug)]
        pub enum Token {
            SemiNl,
            String_,
            TokenBackground,
            TokenConjunction,
            TokenPipe,
            TokenRedirection,
        }
    }

    #[enum_dispatch(Acceptor, Node)]
    #[derive(Debug)]
    pub enum List {
        VariableAssignmentList,
        ArgumentOrRedirectionList,
        ElseifClauseList,
        JobContinuationList,
        AndorJobList,
        JobConjunctionContinuationList,
        ArgumentList,
        JobList,
        CaseItemList,
    }
}

/// Trait for all "leaf" nodes: nodes with no ast children.
#[enum_dispatch]
pub trait Leaf: Node {
    /// Returns none if this node is "unsourced." This happens if for whatever reason we are
    /// unable to parse the node, either because we had a parse error and recovered, or because
    /// we accepted incomplete and the token stream was exhausted.
    fn range(&self) -> Option<SourceRange>;
    fn range_mut(&mut self) -> &mut Option<SourceRange>;
    fn has_source(&self) -> bool {
        self.range().is_some()
    }
}

// A token node is a node which contains a token, which must be one of a fixed set.
#[enum_dispatch]
pub trait Token: Leaf {
    /// The token type which was parsed.
    fn token_type(&self) -> ParseTokenType;
    fn token_type_mut(&mut self) -> &mut ParseTokenType;
    fn allowed_tokens(&self) -> &'static [ParseTokenType];
    /// \return whether a token type is allowed in this token_t, i.e. is a member of our Toks list.
    fn allows_token(&self, token_type: ParseTokenType) -> bool {
        self.allowed_tokens().contains(&token_type)
    }
}

/// A keyword node is a node which contains a keyword, which must be one of a fixed set.
#[enum_dispatch]
pub trait Keyword: Leaf {
    fn keyword(&self) -> ParseKeyword;
    fn keyword_mut(&mut self) -> &mut ParseKeyword;
    fn allowed_keywords(&self) -> &'static [ParseKeyword];
    fn allows_keyword(&self, kw: ParseKeyword) -> bool {
        self.allowed_keywords().contains(&kw)
    }
}

// A simple variable-sized array, possibly empty.
pub trait List: Node {
    type ContentsNode: Node + Default;
    fn contents(&self) -> &[Box<Self::ContentsNode>];
    fn contents_mut(&mut self) -> &mut Vec<Box<Self::ContentsNode>>;
    /// \return our count.
    fn count(&self) -> usize {
        self.contents().len()
    }
    /// \return whether we are empty.
    fn is_empty(&self) -> bool {
        self.contents().is_empty()
    }
    /// Iteration support.
    fn iter(&self) -> std::slice::Iter<Box<Self::ContentsNode>> {
        self.contents().iter()
    }
    fn get(&self, index: usize) -> Option<&Self::ContentsNode> {
        self.contents().get(index).map(|b| &**b)
    }
}

/// This is for optional values and for lists.
trait CheckParse {
    /// A true return means we should descend into the production, false means stop.
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool;
}

impl<T: Node> Node for &T {
    fn parent(&self) -> Option<&dyn Node> {
        T::parent(self)
    }
    fn typ(&self) -> Type {
        T::typ(self)
    }
    fn category(&self) -> Category {
        T::category(self)
    }
    fn try_source_range(&self) -> Option<SourceRange> {
        T::try_source_range(self)
    }
    fn as_ptr(&self) -> *const () {
        T::as_ptr(self)
    }
    fn as_node(&self) -> NodeEnumRef<'_> {
        T::as_node(self)
    }
}
impl<T: Node> Node for &mut T {
    fn parent(&self) -> Option<&dyn Node> {
        T::parent(self)
    }
    fn typ(&self) -> Type {
        T::typ(self)
    }
    fn category(&self) -> Category {
        T::category(self)
    }
    fn try_source_range(&self) -> Option<SourceRange> {
        T::try_source_range(self)
    }
    fn as_ptr(&self) -> *const () {
        T::as_ptr(self)
    }
    fn as_node(&self) -> NodeEnumRef<'_> {
        T::as_node(self)
    }
}
impl<T: Acceptor> Acceptor for &T {
    fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {
        T::accept(self, visitor, reversed)
    }
}
impl<T: Acceptor> Acceptor for &mut T {
    fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {
        T::accept(self, visitor, reversed)
    }
}
impl<T: Leaf> Leaf for &T {
    fn range(&self) -> Option<SourceRange> {
        T::range(self)
    }
    // TODO: make this unnecessary
    fn range_mut(&mut self) -> &mut Option<SourceRange> {
        unimplemented!()
    }
}
impl<T: Leaf> Leaf for &mut T {
    fn range(&self) -> Option<SourceRange> {
        T::range(self)
    }
    fn range_mut(&mut self) -> &mut Option<SourceRange> {
        T::range_mut(self)
    }
}
impl<T: Keyword> Keyword for &T {
    fn keyword(&self) -> ParseKeyword {
        T::keyword(self)
    }
    // TODO: make this unnecessary
    fn keyword_mut(&mut self) -> &mut ParseKeyword {
        unimplemented!()
    }
    fn allowed_keywords(&self) -> &'static [ParseKeyword] {
        T::allowed_keywords(self)
    }
}
impl<T: Keyword> Keyword for &mut T {
    fn keyword(&self) -> ParseKeyword {
        T::keyword(self)
    }
    fn keyword_mut(&mut self) -> &mut ParseKeyword {
        T::keyword_mut(self)
    }
    fn allowed_keywords(&self) -> &'static [ParseKeyword] {
        T::allowed_keywords(self)
    }
}
impl<T: Token> Token for &T {
    fn token_type(&self) -> ParseTokenType {
        T::token_type(self)
    }
    // TODO: make this unnecessary
    fn token_type_mut(&mut self) -> &mut ParseTokenType {
        unimplemented!()
    }
    fn allowed_tokens(&self) -> &'static [ParseTokenType] {
        T::allowed_tokens(self)
    }
}
impl<T: Token> Token for &mut T {
    fn token_type(&self) -> ParseTokenType {
        T::token_type(self)
    }
    fn token_type_mut(&mut self) -> &mut ParseTokenType {
        T::token_type_mut(self)
    }
    fn allowed_tokens(&self) -> &'static [ParseTokenType] {
        T::allowed_tokens(self)
    }
}
impl<T: AcceptorMut> AcceptorMut for &mut T {
    fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
        T::accept_mut(self, visitor, reversed)
    }
}
impl<T: NodeMut> NodeMut for &mut T {
    fn as_mut_node(&mut self) -> NodeEnumRefMut<'_> {
        T::as_mut_node(self)
    }
}

/// Implement the node trait.
macro_rules! implement_node {
    (
        $name:ident,
        $category:ident,
        $type:ident $(,)?
    ) => {
        impl Node for $name {
            fn typ(&self) -> Type {
                Type::$type
            }
            fn parent(&self) -> Option<&dyn Node> {
                self.parent.map(|p| unsafe { &*p })
            }
            fn category(&self) -> Category {
                Category::$category
            }
            fn try_source_range(&self) -> Option<SourceRange> {
                let mut visitor = SourceRangeVisitor {
                    total: SourceRange::new(0, 0),
                    any_unsourced: false,
                };
                visitor.visit(self.as_node());
                if visitor.any_unsourced {
                    None
                } else {
                    Some(visitor.total)
                }
            }
            fn as_ptr(&self) -> *const () {
                (self as *const $name).cast()
            }
            fn as_node(&self) -> NodeEnumRef<'_> {
                NodeEnum::from_ref(self)
            }
        }
        impl NodeMut for $name {
            fn as_mut_node(&mut self) -> NodeEnumRefMut<'_> {
                NodeEnum::from_mut(self)
            }
        }
    };
}

/// Implement the leaf trait.
macro_rules! implement_leaf {
    ( $name:ident ) => {
        impl Leaf for $name {
            fn range(&self) -> Option<SourceRange> {
                self.range
            }
            fn range_mut(&mut self) -> &mut Option<SourceRange> {
                &mut self.range
            }
        }
        impl Acceptor for $name {
            #[allow(unused_variables)]
            fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {}
        }
        impl AcceptorMut for $name {
            #[allow(unused_variables)]
            fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
                visitor.will_visit_fields_of(self);
                visitor.did_visit_fields_of(self, VisitResult::Continue(()));
            }
        }
        impl $name {
            /// Set the parent fields of all nodes in the tree rooted at \p self.
            fn set_parents(&mut self) {}
        }
    };
}

/// Define a node that implements the keyword trait.
macro_rules! define_keyword_node {
    ( $name:ident, $($allowed:ident),* $(,)? ) => {
        #[derive(Default, Debug)]
        pub struct $name {
            parent: Option<*const dyn Node>,
            range: Option<SourceRange>,
            keyword: ParseKeyword,
        }
        implement_node!($name, leaf, keyword_base);
        implement_leaf!($name);
        impl Keyword for $name {
            fn keyword(&self) -> ParseKeyword {
                self.keyword
            }
            fn keyword_mut(&mut self) -> &mut ParseKeyword {
                &mut self.keyword
            }
            fn allowed_keywords(&self) -> &'static [ParseKeyword] {
                &[$(ParseKeyword::$allowed),*]
            }
        }
    }
}

/// Define a node that implements the token trait.
macro_rules! define_token_node {
    ( $name:ident, $($allowed:ident),* $(,)? ) => {
        #[derive(Default, Debug)]
        pub struct $name {
            parent: Option<*const dyn Node>,
            range: Option<SourceRange>,
            parse_token_type: ParseTokenType,
        }
        implement_node!($name, leaf, token_base);
        implement_leaf!($name);
        impl Token for $name {
            fn token_type(&self) -> ParseTokenType {
                self.parse_token_type
            }
            fn token_type_mut(&mut self) -> &mut ParseTokenType {
                &mut self.parse_token_type
            }
            fn allowed_tokens(&self) -> &'static [ParseTokenType] {
                Self::ALLOWED_TOKENS
            }
        }
        impl CheckParse for $name {
            fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
                let typ = pop.peek_type(0);
                Self::ALLOWED_TOKENS.contains(&typ)
            }
        }
        impl $name {
            const ALLOWED_TOKENS: &'static [ParseTokenType] = &[$(ParseTokenType::$allowed),*];
        }
    }
}

/// Define a node that implements the list trait.
macro_rules! define_list_node {
    (
        $name:ident,
        $type:tt,
        $contents:ident
    ) => {
        #[derive(Default, Debug)]
        pub struct $name {
            parent: Option<*const dyn Node>,
            list_contents: Vec<Box<$contents>>,
        }
        implement_node!($name, list, $type);
        impl List for $name {
            type ContentsNode = $contents;
            fn contents(&self) -> &[Box<Self::ContentsNode>] {
                &self.list_contents
            }
            fn contents_mut(&mut self) -> &mut Vec<Box<Self::ContentsNode>> {
                &mut self.list_contents
            }
        }
        impl<'a> IntoIterator for &'a $name {
            type Item = &'a Box<$contents>;
            type IntoIter = std::slice::Iter<'a, Box<$contents>>;
            fn into_iter(self) -> Self::IntoIter {
                self.contents().into_iter()
            }
        }
        impl Index<usize> for $name {
            type Output = <$name as List>::ContentsNode;
            fn index(&self, index: usize) -> &Self::Output {
                &*self.contents()[index]
            }
        }
        impl IndexMut<usize> for $name {
            fn index_mut(&mut self, index: usize) -> &mut Self::Output {
                &mut *self.contents_mut()[index]
            }
        }
        impl Acceptor for $name {
            #[allow(unused_variables)]
            fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {
                accept_list_visitor!(Self, accept, visit, self, visitor, reversed, $contents);
            }
        }
        impl AcceptorMut for $name {
            #[allow(unused_variables)]
            fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
                visitor.will_visit_fields_of(self);
                let flow = accept_list_visitor!(
                    Self, accept_mut, visit_mut, self, visitor, reversed, $contents
                );
                visitor.did_visit_fields_of(self, flow);
            }
        }
        impl $name {
            /// Set the parent fields of all nodes in the tree rooted at \p self.
            fn set_parents(&mut self) {
                for i in 0..self.count() {
                    self[i].parent = Some(self);
                    self[i].set_parents();
                }
            }
        }
    };
}

macro_rules! accept_list_visitor {
    (
        $Self:ident,
        $accept:ident,
        $visit:ident,
        $self:ident,
        $visitor:ident,
        $reversed:ident,
        $list_element:ident
    ) => {
        loop {
            let mut result = VisitResult::Continue(());
            // list types pretend their child nodes are direct embeddings.
            // This isn't used during AST construction because we need to construct the list.
            if $reversed {
                for i in (0..$self.count()).rev() {
                    result = accept_list_visitor_impl!($self, $visitor, $visit, $self[i]);
                    if result.is_break() {
                        break;
                    }
                }
            } else {
                for i in 0..$self.count() {
                    result = accept_list_visitor_impl!($self, $visitor, $visit, $self[i]);
                    if result.is_break() {
                        break;
                    }
                }
            }
            break result;
        }
    };
}

macro_rules! accept_list_visitor_impl {
    (
        $self:ident,
        $visitor:ident,
        visit,
        $child:expr) => {{
        $visitor.visit($child.as_node());
        VisitResult::Continue(())
    }};
    (
        $self:ident,
        $visitor:ident,
        visit_mut,
        $child:expr) => {
        $visitor.visit_mut(&mut $child)
    };
}

/// Implement the acceptor trait for the given branch node.
macro_rules! implement_acceptor_for_branch {
    (
        $name:ident
        $(, ($field_name:ident: $field_type:tt) )*
        $(,)?
    ) => {
        impl Acceptor for $name {
            #[allow(unused_variables)]
            fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool){
                visitor_accept_field!(
                    Self,
                    accept,
                    visit,
                    self,
                    visitor,
                    reversed,
                    ( $( $field_name: $field_type, )* ) );
            }
        }
        impl AcceptorMut for $name {
            #[allow(unused_variables)]
            fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
                visitor.will_visit_fields_of(self);
                let flow = visitor_accept_field!(
                                Self,
                                accept_mut,
                                visit_mut,
                                self,
                                visitor,
                                reversed,
                                ( $( $field_name: $field_type, )* ));
                visitor.did_visit_fields_of(self, flow);
            }
        }
        impl $name {
            /// Set the parent fields of all nodes in the tree rooted at \p self.
            fn set_parents(&mut self) {
                $(
                    set_parent_of_field!(self, $field_name, $field_type);
                )*
            }
        }
    }
}

/// Visit the given fields in order, returning whether the visitation succeeded.
macro_rules! visitor_accept_field {
    (
        $Self:ident,
        $accept:ident,
        $visit:ident,
        $self:ident,
        $visitor:ident,
        $reversed:ident,
        $fields:tt
    ) => {
        loop {
            visitor_accept_field_impl!($visit, $self, $visitor, $reversed, $fields);
            break VisitResult::Continue(());
        }
    };
}

/// Visit the given fields in order, breaking if a visitation fails.
macro_rules! visitor_accept_field_impl {
    // Base case: no fields left to visit.
    (
        $visit:ident,
        $self:ident,
        $visitor:ident,
        $reversed:ident,
        ()
    ) => {};
    // Visit the first or last field and then the rest.
    (
        $visit:ident,
        $self:ident,
        $visitor:ident,
        $reversed:ident,
        (
            $field_name:ident: $field_type:tt,
            $( $field_names:ident: $field_types:tt, )*
        )
    ) => {
        if !$reversed {
            visit_1_field!($visit, ($self.$field_name), $field_type, $visitor);
        }
        visitor_accept_field_impl!(
            $visit, $self, $visitor, $reversed,
            ( $( $field_names: $field_types, )* ));
        if $reversed {
            visit_1_field!($visit, ($self.$field_name), $field_type, $visitor);
        }
    }
}

/// Visit the given field, breaking on failure.
macro_rules! visit_1_field {
    (
        visit,
        $field:expr,
        $field_type:tt,
        $visitor:ident
    ) => {
        visit_1_field_impl!(visit, $field, $field_type, $visitor);
    };
    (
        visit_mut,
        $field:expr,
        $field_type:tt,
        $visitor:ident
    ) => {
        let result = visit_1_field_impl!(visit_mut, $field, $field_type, $visitor);
        if result.is_break() {
            break result;
        }
    };
}

/// Visit the given field.
macro_rules! visit_1_field_impl {
    (
        $visit:ident,
        $field:expr,
        (Box<$field_type:ident>),
        $visitor:ident
    ) => {
        visit_union_field!($visit, $field_type, $field, $visitor)
    };
    (
        $visit:ident,
        $field:expr,
        (Option<$field_type:ident>),
        $visitor:ident
    ) => {
        visit_optional_field!($visit, $field_type, $field, $visitor)
    };
    (
        $visit:ident,
        $field:expr,
        $field_type:tt,
        $visitor:ident
    ) => {
        $visitor.$visit(apply_borrow!($visit, $field))
    };
}

macro_rules! apply_borrow {
    ( visit, $expr:expr ) => {
        $expr.as_node()
    };
    ( visit_mut, $expr:expr ) => {
        &mut $expr
    };
}

macro_rules! visit_union_field {
    (
        visit,
        $field_type:ident,
        $field:expr,
        $visitor:ident
    ) => {
        $visitor.visit($field.embedded_node())
    };
    (
        visit_mut,
        $field_type:ident,
        $field:expr,
        $visitor:ident
    ) => {
        visit_union_field_mut!($field_type, $visitor, $field)
    };
}

macro_rules! visit_union_field_mut {
    (ArgumentOrRedirectionVariant, $visitor:ident, $field:expr) => {
        $visitor.visit_argument_or_redirection(&mut $field)
    };
    (BlockStatementHeaderVariant, $visitor:ident, $field:expr) => {
        $visitor.visit_block_statement_header(&mut $field)
    };
    (StatementVariant, $visitor:ident, $field:expr) => {
        $visitor.visit_statement(&mut $field)
    };
}

macro_rules! visit_optional_field {
    (
        visit,
        $field_type:ident,
        $field:expr,
        $visitor:ident
    ) => {
        match &$field {
            Some(value) => $visitor.visit(value.as_node()),
            None => visit_result!(visit),
        }
    };
    (
        visit_mut,
        $field_type:ident,
        $field:expr,
        $visitor:ident
    ) => {{
        visit_optional_field_mut!($field_type, $field, $visitor);
        VisitResult::Continue(())
    }};
}

macro_rules! visit_optional_field_mut {
    (DecoratedStatementDecorator, $field:expr, $visitor:ident) => {
        $visitor.visit_decorated_statement_decorator(&mut $field);
    };
    (JobConjunctionDecorator, $field:expr, $visitor:ident) => {
        $visitor.visit_job_conjunction_decorator(&mut $field);
    };
    (ElseClause, $field:expr, $visitor:ident) => {
        $visitor.visit_else_clause(&mut $field);
    };
    (SemiNl, $field:expr, $visitor:ident) => {
        $visitor.visit_semi_nl(&mut $field);
    };
    (KeywordTime, $field:expr, $visitor:ident) => {
        $visitor.visit_time(&mut $field);
    };
    (TokenBackground, $field:expr, $visitor:ident) => {
        $visitor.visit_token_background(&mut $field);
    };
}

macro_rules! visit_result {
    ( visit) => {
        ()
    };
    ( visit_mut ) => {
        VisitResult::Continue(())
    };
}

macro_rules! set_parent_of_field {
    (
        $self:ident,
        $field_name:ident,
        (Box<$field_type:ident>)
    ) => {
        set_parent_of_union_field!($self, $field_name, $field_type);
    };
    (
        $self:ident,
        $field_name:ident,
        (Option<$field_type:ident>)
    ) => {
        if $self.$field_name.is_some() {
            $self.$field_name.as_mut().unwrap().parent = Some($self);
            $self.$field_name.as_mut().unwrap().set_parents();
        }
    };
    (
        $self:ident,
        $field_name:ident,
        $field_type:tt
    ) => {
        $self.$field_name.parent = Some($self);
        $self.$field_name.set_parents();
    };
}

macro_rules! set_parent_of_union_field {
    (
        $self:ident,
        $field_name:ident,
        ArgumentOrRedirectionVariant
    ) => {
        if matches!(
            *$self.$field_name,
            ArgumentOrRedirectionVariant::Argument(_)
        ) {
            $self.$field_name.as_mut_argument().parent = Some($self);
            $self.$field_name.as_mut_argument().set_parents();
        } else {
            $self.$field_name.as_mut_redirection().parent = Some($self);
            $self.$field_name.as_mut_redirection().set_parents();
        }
    };
    (
        $self:ident,
        $field_name:ident,
        StatementVariant
    ) => {
        if matches!(*$self.$field_name, StatementVariant::NotStatement(_)) {
            $self.$field_name.as_mut_not_statement().parent = Some($self);
            $self.$field_name.as_mut_not_statement().set_parents();
        } else if matches!(*$self.$field_name, StatementVariant::BlockStatement(_)) {
            $self.$field_name.as_mut_block_statement().parent = Some($self);
            $self.$field_name.as_mut_block_statement().set_parents();
        } else if matches!(*$self.$field_name, StatementVariant::IfStatement(_)) {
            $self.$field_name.as_mut_if_statement().parent = Some($self);
            $self.$field_name.as_mut_if_statement().set_parents();
        } else if matches!(*$self.$field_name, StatementVariant::SwitchStatement(_)) {
            $self.$field_name.as_mut_switch_statement().parent = Some($self);
            $self.$field_name.as_mut_switch_statement().set_parents();
        } else if matches!(*$self.$field_name, StatementVariant::DecoratedStatement(_)) {
            $self.$field_name.as_mut_decorated_statement().parent = Some($self);
            $self.$field_name.as_mut_decorated_statement().set_parents();
        }
    };
    (
        $self:ident,
        $field_name:ident,
        BlockStatementHeaderVariant
    ) => {
        if matches!(
            *$self.$field_name,
            BlockStatementHeaderVariant::ForHeader(_)
        ) {
            $self.$field_name.as_mut_for_header().parent = Some($self);
            $self.$field_name.as_mut_for_header().set_parents();
        } else if matches!(
            *$self.$field_name,
            BlockStatementHeaderVariant::WhileHeader(_)
        ) {
            $self.$field_name.as_mut_while_header().parent = Some($self);
            $self.$field_name.as_mut_while_header().set_parents();
        } else if matches!(
            *$self.$field_name,
            BlockStatementHeaderVariant::FunctionHeader(_)
        ) {
            $self.$field_name.as_mut_function_header().parent = Some($self);
            $self.$field_name.as_mut_function_header().set_parents();
        } else if matches!(
            *$self.$field_name,
            BlockStatementHeaderVariant::BeginHeader(_)
        ) {
            $self.$field_name.as_mut_begin_header().parent = Some($self);
            $self.$field_name.as_mut_begin_header().set_parents();
        }
    };
}

/// A redirection has an operator like > or 2>, and a target like /dev/null or &1.
/// Note that pipes are not redirections.
#[derive(Default, Debug)]
pub struct Redirection {
    parent: Option<*const dyn Node>,
    pub oper: TokenRedirection,
    pub target: String_,
}
implement_node!(Redirection, branch, redirection);
implement_acceptor_for_branch!(Redirection, (oper: TokenRedirection), (target: String_));
impl CheckParse for Redirection {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        pop.peek_type(0) == ParseTokenType::redirection
    }
}

define_list_node!(
    VariableAssignmentList,
    variable_assignment_list,
    VariableAssignment
);

/// An argument or redirection holds either an argument or redirection.
#[derive(Default, Debug)]
pub struct ArgumentOrRedirection {
    parent: Option<*const dyn Node>,
    pub contents: Box<ArgumentOrRedirectionVariant>,
}
implement_node!(ArgumentOrRedirection, branch, argument_or_redirection);
implement_acceptor_for_branch!(
    ArgumentOrRedirection,
    (contents: (Box<ArgumentOrRedirectionVariant>))
);
impl CheckParse for ArgumentOrRedirection {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        let typ = pop.peek_type(0);
        matches!(typ, ParseTokenType::string | ParseTokenType::redirection)
    }
}

define_list_node!(
    ArgumentOrRedirectionList,
    argument_or_redirection_list,
    ArgumentOrRedirection
);

/// A statement is a normal command, or an if / while / etc
#[derive(Default, Debug)]
pub struct Statement {
    parent: Option<*const dyn Node>,
    pub contents: Box<StatementVariant>,
}
implement_node!(Statement, branch, statement);
implement_acceptor_for_branch!(Statement, (contents: (Box<StatementVariant>)));

/// A job is a non-empty list of statements, separated by pipes. (Non-empty is useful for cases
/// like if statements, where we require a command).
#[derive(Default, Debug)]
pub struct JobPipeline {
    parent: Option<*const dyn Node>,
    /// Maybe the time keyword.
    pub time: Option<KeywordTime>,
    /// A (possibly empty) list of variable assignments.
    pub variables: VariableAssignmentList,
    /// The statement.
    pub statement: Statement,
    /// Piped remainder.
    pub continuation: JobContinuationList,
    /// Maybe backgrounded.
    pub bg: Option<TokenBackground>,
}
implement_node!(JobPipeline, branch, job_pipeline);
implement_acceptor_for_branch!(
    JobPipeline,
    (time: (Option<KeywordTime>)),
    (variables: (VariableAssignmentList)),
    (statement: (Statement)),
    (continuation: (JobContinuationList)),
    (bg: (Option<TokenBackground>)),
);

/// A job_conjunction is a job followed by a && or || continuations.
#[derive(Default, Debug)]
pub struct JobConjunction {
    parent: Option<*const dyn Node>,
    /// The job conjunction decorator.
    pub decorator: Option<JobConjunctionDecorator>,
    /// The job itself.
    pub job: JobPipeline,
    /// The rest of the job conjunction, with && or ||s.
    pub continuations: JobConjunctionContinuationList,
    /// A terminating semicolon or newline.  This is marked optional because it may not be
    /// present, for example the command `echo foo` may not have a terminating newline. It will
    /// only fail to be present if we ran out of tokens.
    pub semi_nl: Option<SemiNl>,
}
implement_node!(JobConjunction, branch, job_conjunction);
implement_acceptor_for_branch!(
    JobConjunction,
    (decorator: (Option<JobConjunctionDecorator>)),
    (job: (JobPipeline)),
    (continuations: (JobConjunctionContinuationList)),
    (semi_nl: (Option<SemiNl>)),
);
impl CheckParse for JobConjunction {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        let token = pop.peek_token(0);
        // These keywords end a job list.
        token.typ == ParseTokenType::string
            && !matches!(
                token.keyword,
                ParseKeyword::kw_end | ParseKeyword::kw_else | ParseKeyword::kw_case
            )
    }
}

#[derive(Default, Debug)]
pub struct ForHeader {
    parent: Option<*const dyn Node>,
    /// 'for'
    pub kw_for: KeywordFor,
    /// var_name
    pub var_name: String_,
    /// 'in'
    pub kw_in: KeywordIn,
    /// list of arguments
    pub args: ArgumentList,
    /// newline or semicolon
    pub semi_nl: SemiNl,
}
implement_node!(ForHeader, branch, for_header);
implement_acceptor_for_branch!(
    ForHeader,
    (kw_for: (KeywordFor)),
    (var_name: (String_)),
    (kw_in: (KeywordIn)),
    (args: (ArgumentList)),
    (semi_nl: (SemiNl)),
);

#[derive(Default, Debug)]
pub struct WhileHeader {
    parent: Option<*const dyn Node>,
    /// 'while'
    pub kw_while: KeywordWhile,
    pub condition: JobConjunction,
    pub andor_tail: AndorJobList,
}
implement_node!(WhileHeader, branch, while_header);
implement_acceptor_for_branch!(
    WhileHeader,
    (kw_while: (KeywordWhile)),
    (condition: (JobConjunction)),
    (andor_tail: (AndorJobList)),
);

#[derive(Default, Debug)]
pub struct FunctionHeader {
    parent: Option<*const dyn Node>,
    pub kw_function: KeywordFunction,
    /// functions require at least one argument.
    pub first_arg: Argument,
    pub args: ArgumentList,
    pub semi_nl: SemiNl,
}
implement_node!(FunctionHeader, branch, function_header);
implement_acceptor_for_branch!(
    FunctionHeader,
    (kw_function: (KeywordFunction)),
    (first_arg: (Argument)),
    (args: (ArgumentList)),
    (semi_nl: (SemiNl)),
);

#[derive(Default, Debug)]
pub struct BeginHeader {
    parent: Option<*const dyn Node>,
    pub kw_begin: KeywordBegin,
    /// Note that 'begin' does NOT require a semi or nl afterwards.
    /// This is valid: begin echo hi; end
    pub semi_nl: Option<SemiNl>,
}
implement_node!(BeginHeader, branch, begin_header);
implement_acceptor_for_branch!(
    BeginHeader,
    (kw_begin: (KeywordBegin)),
    (semi_nl: (Option<SemiNl>))
);

#[derive(Default, Debug)]
pub struct BlockStatement {
    parent: Option<*const dyn Node>,
    /// A header like for, while, etc.
    pub header: Box<BlockStatementHeaderVariant>,
    /// List of jobs in this block.
    pub jobs: JobList,
    /// The 'end' node.
    pub end: KeywordEnd,
    /// Arguments and redirections associated with the block.
    pub args_or_redirs: ArgumentOrRedirectionList,
}
implement_node!(BlockStatement, branch, block_statement);
implement_acceptor_for_branch!(
    BlockStatement,
    (header: (Box<BlockStatementHeaderVariant>)),
    (jobs: (JobList)),
    (end: (KeywordEnd)),
    (args_or_redirs: (ArgumentOrRedirectionList)),
);

#[derive(Default, Debug)]
pub struct IfClause {
    parent: Option<*const dyn Node>,
    /// The 'if' keyword.
    pub kw_if: KeywordIf,
    /// The 'if' condition.
    pub condition: JobConjunction,
    /// 'and/or' tail.
    pub andor_tail: AndorJobList,
    /// The body to execute if the condition is true.
    pub body: JobList,
}
implement_node!(IfClause, branch, if_clause);
implement_acceptor_for_branch!(
    IfClause,
    (kw_if: (KeywordIf)),
    (condition: (JobConjunction)),
    (andor_tail: (AndorJobList)),
    (body: (JobList)),
);

#[derive(Default, Debug)]
pub struct ElseifClause {
    parent: Option<*const dyn Node>,
    /// The 'else' keyword.
    pub kw_else: KeywordElse,
    /// The 'if' clause following it.
    pub if_clause: IfClause,
}
implement_node!(ElseifClause, branch, elseif_clause);
implement_acceptor_for_branch!(
    ElseifClause,
    (kw_else: (KeywordElse)),
    (if_clause: (IfClause)),
);
impl CheckParse for ElseifClause {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        pop.peek_token(0).keyword == ParseKeyword::kw_else
            && pop.peek_token(1).keyword == ParseKeyword::kw_if
    }
}

define_list_node!(ElseifClauseList, elseif_clause_list, ElseifClause);

#[derive(Default, Debug)]
pub struct ElseClause {
    parent: Option<*const dyn Node>,
    /// else ; body
    pub kw_else: KeywordElse,
    pub semi_nl: SemiNl,
    pub body: JobList,
}
implement_node!(ElseClause, branch, else_clause);
implement_acceptor_for_branch!(
    ElseClause,
    (kw_else: (KeywordElse)),
    (semi_nl: (SemiNl)),
    (body: (JobList)),
);
impl CheckParse for ElseClause {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        pop.peek_token(0).keyword == ParseKeyword::kw_else
    }
}

#[derive(Default, Debug)]
pub struct IfStatement {
    parent: Option<*const dyn Node>,
    /// if part
    pub if_clause: IfClause,
    /// else if list
    pub elseif_clauses: ElseifClauseList,
    /// else part
    pub else_clause: Option<ElseClause>,
    /// literal end
    pub end: KeywordEnd,
    /// block args / redirs
    pub args_or_redirs: ArgumentOrRedirectionList,
}
implement_node!(IfStatement, branch, if_statement);
implement_acceptor_for_branch!(
    IfStatement,
    (if_clause: (IfClause)),
    (elseif_clauses: (ElseifClauseList)),
    (else_clause: (Option<ElseClause>)),
    (end: (KeywordEnd)),
    (args_or_redirs: (ArgumentOrRedirectionList)),
);

#[derive(Default, Debug)]
pub struct CaseItem {
    parent: Option<*const dyn Node>,
    /// case <arguments> ; body
    pub kw_case: KeywordCase,
    pub arguments: ArgumentList,
    pub semi_nl: SemiNl,
    pub body: JobList,
}
implement_node!(CaseItem, branch, case_item);
implement_acceptor_for_branch!(
    CaseItem,
    (kw_case: (KeywordCase)),
    (arguments: (ArgumentList)),
    (semi_nl: (SemiNl)),
    (body: (JobList)),
);
impl CheckParse for CaseItem {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        pop.peek_token(0).keyword == ParseKeyword::kw_case
    }
}

#[derive(Default, Debug)]
pub struct SwitchStatement {
    parent: Option<*const dyn Node>,
    /// switch <argument> ; body ; end args_redirs
    pub kw_switch: KeywordSwitch,
    pub argument: Argument,
    pub semi_nl: SemiNl,
    pub cases: CaseItemList,
    pub end: KeywordEnd,
    pub args_or_redirs: ArgumentOrRedirectionList,
}
implement_node!(SwitchStatement, branch, switch_statement);
implement_acceptor_for_branch!(
    SwitchStatement,
    (kw_switch: (KeywordSwitch)),
    (argument: (Argument)),
    (semi_nl: (SemiNl)),
    (cases: (CaseItemList)),
    (end: (KeywordEnd)),
    (args_or_redirs: (ArgumentOrRedirectionList)),
);

/// A decorated_statement is a command with a list of arguments_or_redirections, possibly with
/// "builtin" or "command" or "exec"
#[derive(Default, Debug)]
pub struct DecoratedStatement {
    parent: Option<*const dyn Node>,
    /// An optional decoration (command, builtin, exec, etc).
    pub opt_decoration: Option<DecoratedStatementDecorator>,
    /// Command to run.
    pub command: String_,
    /// Args and redirs
    pub args_or_redirs: ArgumentOrRedirectionList,
}
implement_node!(DecoratedStatement, branch, decorated_statement);
implement_acceptor_for_branch!(
    DecoratedStatement,
    (opt_decoration: (Option<DecoratedStatementDecorator>)),
    (command: (String_)),
    (args_or_redirs: (ArgumentOrRedirectionList)),
);

/// A not statement like `not true` or `! true`
#[derive(Default, Debug)]
pub struct NotStatement {
    parent: Option<*const dyn Node>,
    /// Keyword, either not or exclam.
    pub kw: KeywordNot,
    pub variables: VariableAssignmentList,
    pub time: Option<KeywordTime>,
    pub contents: Statement,
}
implement_node!(NotStatement, branch, not_statement);
implement_acceptor_for_branch!(
    NotStatement,
    (kw: (KeywordNot)),
    (variables: (VariableAssignmentList)),
    (time: (Option<KeywordTime>)),
    (contents: (Statement)),
);

#[derive(Default, Debug)]
pub struct JobContinuation {
    parent: Option<*const dyn Node>,
    pub pipe: TokenPipe,
    pub newlines: MaybeNewlines,
    pub variables: VariableAssignmentList,
    pub statement: Statement,
}
implement_node!(JobContinuation, branch, job_continuation);
implement_acceptor_for_branch!(
    JobContinuation,
    (pipe: (TokenPipe)),
    (newlines: (MaybeNewlines)),
    (variables: (VariableAssignmentList)),
    (statement: (Statement)),
);
impl CheckParse for JobContinuation {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        pop.peek_type(0) == ParseTokenType::pipe
    }
}

define_list_node!(JobContinuationList, job_continuation_list, JobContinuation);

#[derive(Default, Debug)]
pub struct JobConjunctionContinuation {
    parent: Option<*const dyn Node>,
    /// The && or || token.
    pub conjunction: TokenConjunction,
    pub newlines: MaybeNewlines,
    /// The job itself.
    pub job: JobPipeline,
}
implement_node!(
    JobConjunctionContinuation,
    branch,
    job_conjunction_continuation
);
implement_acceptor_for_branch!(
    JobConjunctionContinuation,
    (conjunction: (TokenConjunction)),
    (newlines: (MaybeNewlines)),
    (job: (JobPipeline)),
);
impl CheckParse for JobConjunctionContinuation {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        let typ = pop.peek_type(0);
        matches!(typ, ParseTokenType::andand | ParseTokenType::oror)
    }
}

/// An andor_job just wraps a job, but requires that the job have an 'and' or 'or' job_decorator.
/// Note this is only used for andor_job_list; jobs that are not part of an andor_job_list are not
/// instances of this.
#[derive(Default, Debug)]
pub struct AndorJob {
    parent: Option<*const dyn Node>,
    pub job: JobConjunction,
}
implement_node!(AndorJob, branch, andor_job);
implement_acceptor_for_branch!(AndorJob, (job: (JobConjunction)));
impl CheckParse for AndorJob {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        let keyword = pop.peek_token(0).keyword;
        if !matches!(keyword, ParseKeyword::kw_and | ParseKeyword::kw_or) {
            return false;
        }
        // Check that the argument to and/or is a string that's not help. Otherwise
        // it's either 'and --help' or a naked 'and', and not part of this list.
        let next_token = pop.peek_token(1);
        next_token.typ == ParseTokenType::string && !next_token.is_help_argument
    }
}

define_list_node!(AndorJobList, andor_job_list, AndorJob);

/// A freestanding_argument_list is equivalent to a normal argument list, except it may contain
/// TOK_END (newlines, and even semicolons, for historical reasons).
/// In practice the tok_ends are ignored by fish code so we do not bother to store them.
#[derive(Default, Debug)]
pub struct FreestandingArgumentList {
    parent: Option<*const dyn Node>,
    pub arguments: ArgumentList,
}
implement_node!(FreestandingArgumentList, branch, freestanding_argument_list);
implement_acceptor_for_branch!(FreestandingArgumentList, (arguments: (ArgumentList)));

define_list_node!(
    JobConjunctionContinuationList,
    job_conjunction_continuation_list,
    JobConjunctionContinuation
);

define_list_node!(ArgumentList, argument_list, Argument);

// For historical reasons, a job list is a list of job *conjunctions*. This should be fixed.
define_list_node!(JobList, job_list, JobConjunction);

define_list_node!(CaseItemList, case_item_list, CaseItem);

/// A variable_assignment contains a source range like FOO=bar.
#[derive(Default, Debug)]
pub struct VariableAssignment {
    parent: Option<*const dyn Node>,
    range: Option<SourceRange>,
}
implement_node!(VariableAssignment, leaf, variable_assignment);
implement_leaf!(VariableAssignment);
impl CheckParse for VariableAssignment {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        // Do we have a variable assignment at all?
        if !pop.peek_token(0).may_be_variable_assignment {
            return false;
        }
        // What is the token after it?
        match pop.peek_type(1) {
            // We have `a= cmd` and should treat it as a variable assignment.
            ParseTokenType::string => true,
            // We have `a=` which is OK if we are allowing incomplete, an error otherwise.
            ParseTokenType::terminate => pop.allow_incomplete(),
            // We have e.g. `a= >` which is an error.
            // Note that we do not produce an error here. Instead we return false
            // so this the token will be seen by allocate_populate_statement_contents.
            _ => false,
        }
    }
}

/// Zero or more newlines.
#[derive(Default, Debug)]
pub struct MaybeNewlines {
    parent: Option<*const dyn Node>,
    range: Option<SourceRange>,
}
implement_node!(MaybeNewlines, leaf, maybe_newlines);
implement_leaf!(MaybeNewlines);

/// An argument is just a node whose source range determines its contents.
/// This is a separate type because it is sometimes useful to find all arguments.
#[derive(Default, Debug)]
pub struct Argument {
    parent: Option<*const dyn Node>,
    range: Option<SourceRange>,
}
implement_node!(Argument, leaf, argument);
implement_leaf!(Argument);
impl CheckParse for Argument {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        pop.peek_type(0) == ParseTokenType::string
    }
}

define_token_node!(SemiNl, end);
define_token_node!(String_, string);
define_token_node!(TokenBackground, background);
define_token_node!(TokenConjunction, andand, oror);
define_token_node!(TokenPipe, pipe);
define_token_node!(TokenRedirection, redirection);

define_keyword_node!(DecoratedStatementDecorator, kw_command, kw_builtin, kw_exec);
define_keyword_node!(JobConjunctionDecorator, kw_and, kw_or);
define_keyword_node!(KeywordBegin, kw_begin);
define_keyword_node!(KeywordCase, kw_case);
define_keyword_node!(KeywordElse, kw_else);
define_keyword_node!(KeywordEnd, kw_end);
define_keyword_node!(KeywordFor, kw_for);
define_keyword_node!(KeywordFunction, kw_function);
define_keyword_node!(KeywordIf, kw_if);
define_keyword_node!(KeywordIn, kw_in);
define_keyword_node!(KeywordNot, kw_not, kw_builtin, kw_exclam);
define_keyword_node!(KeywordSwitch, kw_switch);
define_keyword_node!(KeywordTime, kw_time);
define_keyword_node!(KeywordWhile, kw_while);

impl CheckParse for JobConjunctionDecorator {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        // This is for a job conjunction like `and stuff`
        // But if it's `and --help` then we treat it as an ordinary command.
        let keyword = pop.peek_token(0).keyword;
        if !matches!(keyword, ParseKeyword::kw_and | ParseKeyword::kw_or) {
            return false;
        }
        !pop.peek_token(1).is_help_argument
    }
}

impl CheckParse for DecoratedStatementDecorator {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        // Here the keyword is 'command' or 'builtin' or 'exec'.
        // `command stuff` executes a command called stuff.
        // `command -n` passes the -n argument to the 'command' builtin.
        // `command` by itself is a command.
        let keyword = pop.peek_token(0).keyword;
        if !matches!(
            keyword,
            ParseKeyword::kw_command | ParseKeyword::kw_builtin | ParseKeyword::kw_exec
        ) {
            return false;
        }
        let next_token = pop.peek_token(1);
        next_token.typ == ParseTokenType::string && !next_token.is_dash_prefix_string()
    }
}

impl CheckParse for KeywordTime {
    fn can_be_parsed(pop: &mut Populator<'_>) -> bool {
        // Time keyword is only the time builtin if the next argument doesn't have a dash.
        let keyword = pop.peek_token(0).keyword;
        if !matches!(keyword, ParseKeyword::kw_time) {
            return false;
        }
        !pop.peek_token(1).is_dash_prefix_string()
    }
}

impl DecoratedStatement {
    /// \return the decoration for this statement.
    pub fn decoration(&self) -> StatementDecoration {
        let Some(decorator) = &self.opt_decoration else {
            return StatementDecoration::none;
        };
        match decorator.keyword() {
            ParseKeyword::kw_command => StatementDecoration::command,
            ParseKeyword::kw_builtin => StatementDecoration::builtin,
            ParseKeyword::kw_exec => StatementDecoration::exec,
            _ => panic!("Unexpected keyword in statement decoration"),
        }
    }
}

#[derive(Debug)]
pub enum ArgumentOrRedirectionVariant {
    Argument(Argument),
    Redirection(Redirection),
}

impl Default for ArgumentOrRedirectionVariant {
    fn default() -> Self {
        ArgumentOrRedirectionVariant::Argument(Argument::default())
    }
}

impl Acceptor for ArgumentOrRedirectionVariant {
    fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {
        match self {
            ArgumentOrRedirectionVariant::Argument(child) => child.accept(visitor, reversed),
            ArgumentOrRedirectionVariant::Redirection(child) => child.accept(visitor, reversed),
        }
    }
}
impl AcceptorMut for ArgumentOrRedirectionVariant {
    fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
        match self {
            ArgumentOrRedirectionVariant::Argument(child) => child.accept_mut(visitor, reversed),
            ArgumentOrRedirectionVariant::Redirection(child) => child.accept_mut(visitor, reversed),
        }
    }
}

impl ArgumentOrRedirectionVariant {
    pub fn typ(&self) -> Type {
        self.embedded_node().typ()
    }
    pub fn try_source_range(&self) -> Option<SourceRange> {
        self.embedded_node().try_source_range()
    }

    fn embedded_node(&self) -> NodeEnumRef<'_> {
        match self {
            ArgumentOrRedirectionVariant::Argument(node) => node.as_node(),
            ArgumentOrRedirectionVariant::Redirection(node) => node.as_node(),
        }
    }
    fn as_mut_argument(&mut self) -> &mut Argument {
        match self {
            ArgumentOrRedirectionVariant::Argument(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_redirection(&mut self) -> &mut Redirection {
        match self {
            ArgumentOrRedirectionVariant::Redirection(redirection) => redirection,
            _ => panic!(),
        }
    }
}

impl ArgumentOrRedirection {
    /// \return whether this represents an argument.
    pub fn is_argument(&self) -> bool {
        matches!(*self.contents, ArgumentOrRedirectionVariant::Argument(_))
    }

    /// \return whether this represents a redirection
    pub fn is_redirection(&self) -> bool {
        matches!(*self.contents, ArgumentOrRedirectionVariant::Redirection(_))
    }

    /// \return this as an argument, assuming it wraps one.
    pub fn argument(&self) -> &Argument {
        match *self.contents {
            ArgumentOrRedirectionVariant::Argument(ref arg) => arg,
            _ => panic!("Is not an argument"),
        }
    }

    /// \return this as an argument, assuming it wraps one.
    pub fn redirection(&self) -> &Redirection {
        match *self.contents {
            ArgumentOrRedirectionVariant::Redirection(ref arg) => arg,
            _ => panic!("Is not a redirection"),
        }
    }
}

#[derive(Debug)]
pub enum StatementVariant {
    None,
    NotStatement(NotStatement),
    BlockStatement(BlockStatement),
    // IfStatement is much larger than the rest, so we box it.
    IfStatement(Box<IfStatement>),
    SwitchStatement(SwitchStatement),
    DecoratedStatement(DecoratedStatement),
}

impl Default for StatementVariant {
    fn default() -> Self {
        StatementVariant::None
    }
}

impl Acceptor for StatementVariant {
    fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {
        match self {
            StatementVariant::None => panic!("cannot visit null statement"),
            StatementVariant::NotStatement(node) => node.accept(visitor, reversed),
            StatementVariant::BlockStatement(node) => node.accept(visitor, reversed),
            StatementVariant::IfStatement(node) => node.accept(visitor, reversed),
            StatementVariant::SwitchStatement(node) => node.accept(visitor, reversed),
            StatementVariant::DecoratedStatement(node) => node.accept(visitor, reversed),
        }
    }
}
impl AcceptorMut for StatementVariant {
    fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
        match self {
            StatementVariant::None => panic!("cannot visit null statement"),
            StatementVariant::NotStatement(node) => node.accept_mut(visitor, reversed),
            StatementVariant::BlockStatement(node) => node.accept_mut(visitor, reversed),
            StatementVariant::IfStatement(node) => node.accept_mut(visitor, reversed),
            StatementVariant::SwitchStatement(node) => node.accept_mut(visitor, reversed),
            StatementVariant::DecoratedStatement(node) => node.accept_mut(visitor, reversed),
        }
    }
}

impl StatementVariant {
    pub fn typ(&self) -> Type {
        self.embedded_node().typ()
    }
    pub fn try_source_range(&self) -> Option<SourceRange> {
        self.embedded_node().try_source_range()
    }

    pub fn as_not_statement(&self) -> Option<&NotStatement> {
        match self {
            StatementVariant::NotStatement(node) => Some(node),
            _ => None,
        }
    }
    pub fn as_block_statement(&self) -> Option<&BlockStatement> {
        match self {
            StatementVariant::BlockStatement(node) => Some(node),
            _ => None,
        }
    }
    pub fn as_if_statement(&self) -> Option<&IfStatement> {
        match self {
            StatementVariant::IfStatement(node) => Some(node),
            _ => None,
        }
    }
    pub fn as_switch_statement(&self) -> Option<&SwitchStatement> {
        match self {
            StatementVariant::SwitchStatement(node) => Some(node),
            _ => None,
        }
    }
    pub fn as_decorated_statement(&self) -> Option<&DecoratedStatement> {
        match self {
            StatementVariant::DecoratedStatement(node) => Some(node),
            _ => None,
        }
    }

    fn embedded_node(&self) -> NodeEnumRef {
        match self {
            StatementVariant::None => panic!("cannot visit null statement"),
            StatementVariant::NotStatement(node) => node.as_node(),
            StatementVariant::BlockStatement(node) => node.as_node(),
            StatementVariant::IfStatement(node) => node.as_node(),
            StatementVariant::SwitchStatement(node) => node.as_node(),
            StatementVariant::DecoratedStatement(node) => node.as_node(),
        }
    }
    fn as_mut_not_statement(&mut self) -> &mut NotStatement {
        match self {
            StatementVariant::NotStatement(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_block_statement(&mut self) -> &mut BlockStatement {
        match self {
            StatementVariant::BlockStatement(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_if_statement(&mut self) -> &mut IfStatement {
        match self {
            StatementVariant::IfStatement(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_switch_statement(&mut self) -> &mut SwitchStatement {
        match self {
            StatementVariant::SwitchStatement(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_decorated_statement(&mut self) -> &mut DecoratedStatement {
        match self {
            StatementVariant::DecoratedStatement(node) => node,
            _ => panic!(),
        }
    }
}

#[derive(Debug)]
pub enum BlockStatementHeaderVariant {
    None,
    ForHeader(ForHeader),
    WhileHeader(WhileHeader),
    FunctionHeader(FunctionHeader),
    BeginHeader(BeginHeader),
}

impl Default for BlockStatementHeaderVariant {
    fn default() -> Self {
        BlockStatementHeaderVariant::None
    }
}

impl Acceptor for BlockStatementHeaderVariant {
    fn accept<'a>(&'a self, visitor: &mut impl NodeVisitor<'a>, reversed: bool) {
        match self {
            BlockStatementHeaderVariant::None => panic!("cannot visit null block header"),
            BlockStatementHeaderVariant::ForHeader(node) => node.accept(visitor, reversed),
            BlockStatementHeaderVariant::WhileHeader(node) => node.accept(visitor, reversed),
            BlockStatementHeaderVariant::FunctionHeader(node) => node.accept(visitor, reversed),
            BlockStatementHeaderVariant::BeginHeader(node) => node.accept(visitor, reversed),
        }
    }
}
impl AcceptorMut for BlockStatementHeaderVariant {
    fn accept_mut(&mut self, visitor: &mut Populator<'_>, reversed: bool) {
        match self {
            BlockStatementHeaderVariant::None => panic!("cannot visit null block header"),
            BlockStatementHeaderVariant::ForHeader(node) => node.accept_mut(visitor, reversed),
            BlockStatementHeaderVariant::WhileHeader(node) => node.accept_mut(visitor, reversed),
            BlockStatementHeaderVariant::FunctionHeader(node) => node.accept_mut(visitor, reversed),
            BlockStatementHeaderVariant::BeginHeader(node) => node.accept_mut(visitor, reversed),
        }
    }
}

impl BlockStatementHeaderVariant {
    pub fn typ(&self) -> Type {
        self.embedded_node().typ()
    }
    pub fn try_source_range(&self) -> Option<SourceRange> {
        self.embedded_node().try_source_range()
    }

    pub fn as_for_header(&self) -> Option<&ForHeader> {
        match self {
            BlockStatementHeaderVariant::ForHeader(node) => Some(node),
            _ => None,
        }
    }
    pub fn as_while_header(&self) -> Option<&WhileHeader> {
        match self {
            BlockStatementHeaderVariant::WhileHeader(node) => Some(node),
            _ => None,
        }
    }
    pub fn as_function_header(&self) -> Option<&FunctionHeader> {
        match self {
            BlockStatementHeaderVariant::FunctionHeader(node) => Some(node),
            _ => None,
        }
    }
    pub fn as_begin_header(&self) -> Option<&BeginHeader> {
        match self {
            BlockStatementHeaderVariant::BeginHeader(node) => Some(node),
            _ => None,
        }
    }

    fn embedded_node(&self) -> NodeEnumRef<'_> {
        match self {
            BlockStatementHeaderVariant::None => panic!("cannot visit null block header"),
            BlockStatementHeaderVariant::ForHeader(node) => node.as_node(),
            BlockStatementHeaderVariant::WhileHeader(node) => node.as_node(),
            BlockStatementHeaderVariant::FunctionHeader(node) => node.as_node(),
            BlockStatementHeaderVariant::BeginHeader(node) => node.as_node(),
        }
    }
    fn as_mut_for_header(&mut self) -> &mut ForHeader {
        match self {
            BlockStatementHeaderVariant::ForHeader(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_while_header(&mut self) -> &mut WhileHeader {
        match self {
            BlockStatementHeaderVariant::WhileHeader(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_function_header(&mut self) -> &mut FunctionHeader {
        match self {
            BlockStatementHeaderVariant::FunctionHeader(node) => node,
            _ => panic!(),
        }
    }
    fn as_mut_begin_header(&mut self) -> &mut BeginHeader {
        match self {
            BlockStatementHeaderVariant::BeginHeader(node) => node,
            _ => panic!(),
        }
    }
}

/// \return a string literal name for an ast type.
pub fn ast_type_to_string(t: Type) -> &'static wstr {
    match t {
        Type::token_base => L!("token_base"),
        Type::keyword_base => L!("keyword_base"),
        Type::redirection => L!("redirection"),
        Type::variable_assignment => L!("variable_assignment"),
        Type::variable_assignment_list => L!("variable_assignment_list"),
        Type::argument_or_redirection => L!("argument_or_redirection"),
        Type::argument_or_redirection_list => L!("argument_or_redirection_list"),
        Type::statement => L!("statement"),
        Type::job_pipeline => L!("job_pipeline"),
        Type::job_conjunction => L!("job_conjunction"),
        Type::for_header => L!("for_header"),
        Type::while_header => L!("while_header"),
        Type::function_header => L!("function_header"),
        Type::begin_header => L!("begin_header"),
        Type::block_statement => L!("block_statement"),
        Type::if_clause => L!("if_clause"),
        Type::elseif_clause => L!("elseif_clause"),
        Type::elseif_clause_list => L!("elseif_clause_list"),
        Type::else_clause => L!("else_clause"),
        Type::if_statement => L!("if_statement"),
        Type::case_item => L!("case_item"),
        Type::switch_statement => L!("switch_statement"),
        Type::decorated_statement => L!("decorated_statement"),
        Type::not_statement => L!("not_statement"),
        Type::job_continuation => L!("job_continuation"),
        Type::job_continuation_list => L!("job_continuation_list"),
        Type::job_conjunction_continuation => L!("job_conjunction_continuation"),
        Type::andor_job => L!("andor_job"),
        Type::andor_job_list => L!("andor_job_list"),
        Type::freestanding_argument_list => L!("freestanding_argument_list"),
        Type::token_conjunction => L!("token_conjunction"),
        Type::job_conjunction_continuation_list => L!("job_conjunction_continuation_list"),
        Type::maybe_newlines => L!("maybe_newlines"),
        Type::token_pipe => L!("token_pipe"),
        Type::case_item_list => L!("case_item_list"),
        Type::argument => L!("argument"),
        Type::argument_list => L!("argument_list"),
        Type::job_list => L!("job_list"),
    }
}

// A way to visit nodes iteratively.
// This is pre-order. Each node is visited before its children.
// Example:
//    let tv = Traversal::new(start);
//    while let Some(node) = tv.next() {...}
pub struct Traversal<'a> {
    stack: Vec<NodeEnumRef<'a>>,
}

impl<'a> Traversal<'a> {
    // Construct starting with a node
    pub fn new(n: NodeEnumRef<'a>) -> Self {
        Self { stack: vec![n] }
    }
}

impl<'a> Iterator for Traversal<'a> {
    type Item = NodeEnumRef<'a>;
    fn next(&mut self) -> Option<NodeEnumRef<'a>> {
        let node = self.stack.pop()?;
        // We want to visit in reverse order so the first child ends up on top of the stack.
        node.enum_accept(self, true /* reverse */);
        Some(node)
    }
}

impl<'a, 'v: 'a> NodeVisitor<'v> for Traversal<'a> {
    fn visit(&mut self, node: NodeEnumRef<'v>) {
        self.stack.push(node)
    }
}

pub type SourceRangeList = Vec<SourceRange>;

/// Extra source ranges.
/// These are only generated if the corresponding flags are set.
#[derive(Default)]
pub struct Extras {
    /// Set of comments, sorted by offset.
    pub comments: SourceRangeList,

    /// Set of semicolons, sorted by offset.
    pub semis: SourceRangeList,

    /// Set of error ranges, sorted by offset.
    pub errors: SourceRangeList,
}

/// The ast type itself.
pub struct Ast {
    // The top node.
    // Its type depends on what was requested to parse.
    top: Box<dyn NodeMut>,
    /// Whether any errors were encountered during parsing.
    pub any_error: bool,
    /// Extra fields.
    pub extras: Extras,
}

#[allow(clippy::derivable_impls)] // false positive
impl Default for Ast {
    fn default() -> Ast {
        Self {
            top: Box::<String_>::default(),
            any_error: false,
            extras: Extras::default(),
        }
    }
}

impl Ast {
    /// Construct an ast by parsing \p src as a job list.
    /// The ast attempts to produce \p type as the result.
    /// \p type may only be JobList or FreestandingArgumentList.
    pub fn parse(
        src: &wstr,
        flags: ParseTreeFlags,
        out_errors: Option<&mut ParseErrorList>,
    ) -> Self {
        parse_from_top(src, flags, out_errors, Type::job_list)
    }
    /// Like parse(), but constructs a freestanding_argument_list.
    pub fn parse_argument_list(
        src: &wstr,
        flags: ParseTreeFlags,
        out_errors: Option<&mut ParseErrorList>,
    ) -> Self {
        parse_from_top(src, flags, out_errors, Type::freestanding_argument_list)
    }
    /// \return a traversal, allowing iteration over the nodes.
    pub fn walk(&'_ self) -> Traversal<'_> {
        Traversal::new(self.top())
    }
    /// \return the top node. This has the type requested in the 'parse' method.
    pub fn top(&self) -> NodeEnumRef<'_> {
        self.top.as_node()
    }
    fn top_mut(&mut self) -> NodeEnumRefMut<'_> {
        self.top.as_mut_node()
    }
    /// \return whether any errors were encountered during parsing.
    pub fn errored(&self) -> bool {
        self.any_error
    }

    /// \return a textual representation of the tree.
    /// Pass the original source as \p orig.
    pub fn dump(&self, orig: &wstr) -> WString {
        let mut result = WString::new();

        for node in self.walk() {
            let depth = get_depth(&node);
            // dot-| padding
            result += &str::repeat("! ", depth)[..];

            if let NodeEnumRef::Leaf(LeafRef::Argument(n)) = node {
                result += "argument";
                if let Some(argsrc) = n.try_source(orig) {
                    sprintf!(=> &mut result, ": '%ls'", argsrc);
                }
            } else if let NodeEnumRef::Leaf(LeafRef::Keyword(n)) = node {
                sprintf!(=> &mut result, "keyword: %ls", n.keyword().to_wstr());
            } else if let NodeEnumRef::Leaf(LeafRef::Token(n)) = node {
                let desc = match n.token_type() {
                    ParseTokenType::string => {
                        let mut desc = WString::from_str("string");
                        if let Some(strsource) = n.try_source(orig) {
                            sprintf!(=> &mut desc, ": '%ls'", strsource);
                        }
                        desc
                    }
                    ParseTokenType::redirection => {
                        let mut desc = WString::from_str("redirection");
                        if let Some(strsource) = n.try_source(orig) {
                            sprintf!(=> &mut desc, ": '%ls'", strsource);
                        }
                        desc
                    }
                    ParseTokenType::end => WString::from_str("<;>"),
                    ParseTokenType::invalid => {
                        // This may occur with errors, e.g. we expected to see a string but saw a
                        // redirection.
                        WString::from_str("<error>")
                    }
                    _ => {
                        token_type_user_presentable_description(n.token_type(), ParseKeyword::none)
                    }
                };
                result += &desc[..];
            } else {
                result += &node.describe()[..];
            }
            result += "\n";
        }
        result
    }
}

// \return the depth of a node, i.e. number of parent links.
fn get_depth(node: &dyn Node) -> usize {
    let mut result = 0;
    let mut cursor = node;
    while let Some(parent) = cursor.parent() {
        result += 1;
        cursor = parent;
    }
    result
}

struct SourceRangeVisitor {
    /// Total range we have encountered.
    total: SourceRange,
    /// Whether any node was found to be unsourced.
    any_unsourced: bool,
}

impl<'a> NodeVisitor<'a> for SourceRangeVisitor {
    fn visit(&mut self, node: NodeEnumRef<'a>) {
        match node {
            NodeEnumRef::Leaf(leaf) => match leaf.range() {
                None => self.any_unsourced = true,
                // Union with our range.
                Some(range) if range.length > 0 => {
                    if self.total.length == 0 {
                        self.total = range;
                    } else {
                        let end =
                            (self.total.start + self.total.length).max(range.start + range.length);
                        self.total.start = self.total.start.min(range.start);
                        self.total.length = end - self.total.start;
                    }
                }
                _ => (),
            },
            _ => {
                // Other node types recurse.
                node.accept(self, false);
            }
        }
    }
}

/// A token stream generates a sequence of parser tokens, permitting arbitrary lookahead.
struct TokenStream<'a> {
    // We implement a queue with a simple circular buffer.
    // Note that peek() returns an address, so we must not move elements which are peek'd.
    // This prevents using vector (which may reallocate).
    // Deque would work but is too heavyweight for just 2 items.
    lookahead: [ParseToken; TokenStream::MAX_LOOKAHEAD],

    // Starting index in our lookahead.
    // The "first" token is at this index.
    start: usize,

    // Number of items in our lookahead.
    count: usize,

    // A reference to the original source.
    src: &'a wstr,

    // The tokenizer to generate new tokens.
    tok: Tokenizer,

    /// Any comment nodes are collected here.
    /// These are only collected if parse_flag_include_comments is set.
    comment_ranges: SourceRangeList,
}

impl<'a> TokenStream<'a> {
    // The maximum number of lookahead supported.
    const MAX_LOOKAHEAD: usize = 2;

    fn new(src: &'a wstr, flags: ParseTreeFlags) -> Self {
        Self {
            lookahead: [ParseToken::new(ParseTokenType::invalid); Self::MAX_LOOKAHEAD],
            start: 0,
            count: 0,
            src,
            tok: Tokenizer::new(src, TokFlags::from(flags)),
            comment_ranges: vec![],
        }
    }

    /// \return the token at the given index, without popping it. If the token stream is exhausted,
    /// it will have parse_token_type_t::terminate. idx = 0 means the next token, idx = 1 means the
    /// next-next token, and so forth.
    /// We must have that idx < kMaxLookahead.
    fn peek(&mut self, idx: usize) -> &ParseToken {
        assert!(idx < Self::MAX_LOOKAHEAD, "Trying to look too far ahead");
        while idx >= self.count {
            self.lookahead[Self::mask(self.start + self.count)] = self.next_from_tok();
            self.count += 1
        }
        &self.lookahead[Self::mask(self.start + idx)]
    }

    /// Pop the next token.
    fn pop(&mut self) -> ParseToken {
        if self.count == 0 {
            return self.next_from_tok();
        }
        let result = self.lookahead[self.start];
        self.start = Self::mask(self.start + 1);
        self.count -= 1;
        result
    }

    // Helper to mask our circular buffer.
    fn mask(idx: usize) -> usize {
        idx % Self::MAX_LOOKAHEAD
    }

    /// \return the next parse token from the tokenizer.
    /// This consumes and stores comments.
    fn next_from_tok(&mut self) -> ParseToken {
        loop {
            let res = self.advance_1();
            if res.typ == ParseTokenType::comment {
                self.comment_ranges.push(res.range());
                continue;
            }
            return res;
        }
    }

    /// \return a new parse token, advancing the tokenizer.
    /// This returns comments.
    fn advance_1(&mut self) -> ParseToken {
        let Some(token) = self.tok.next() else {
            return ParseToken::new(ParseTokenType::terminate);
        };
        // Set the type, keyword, and whether there's a dash prefix. Note that this is quite
        // sketchy, because it ignores quotes. This is the historical behavior. For example,
        // `builtin --names` lists builtins, but `builtin "--names"` attempts to run --names as a
        // command. Amazingly as of this writing (10/12/13) nobody seems to have noticed this.
        // Squint at it really hard and it even starts to look like a feature.
        let mut result = ParseToken::new(ParseTokenType::from(token.type_));
        let text = self.tok.text_of(&token);
        result.keyword = keyword_for_token(token.type_, text);
        result.has_dash_prefix = text.starts_with('-');
        result.is_help_argument = [L!("-h"), L!("--help")].contains(&text);
        result.is_newline = result.typ == ParseTokenType::end && text == "\n";
        result.may_be_variable_assignment = variable_assignment_equals_pos(text).is_some();
        result.tok_error = token.error;

        assert!(token.offset() < SOURCE_OFFSET_INVALID);
        result.set_source_start(token.offset());
        result.set_source_length(token.length());

        if token.error != TokenizerError::none {
            let subtoken_offset = token.error_offset_within_token();
            // Skip invalid tokens that have a zero length, especially if they are at EOF.
            if subtoken_offset < result.source_length() {
                result.set_source_start(result.source_start() + subtoken_offset);
                result.set_source_length(token.error_length());
            }
        }

        result
    }
}

/// This indicates a bug in fish code.
macro_rules! internal_error {
    (
        $self:ident,
        $func:ident,
        $fmt:expr
        $(, $args:expr)*
        $(,)?
    ) => {
        FLOG!(
            debug,
            concat!(
                "Internal parse error from {$func} - this indicates a bug in fish.",
                $fmt,
            )
            $(, $args)*
        );
        FLOGF!(debug, "Encountered while parsing:<<<<\n{}\n>>>", $self.tokens.src);
        panic!();
    };
}

/// Report an error based on \p fmt for the tokens' range
macro_rules! parse_error {
    (
        $self:ident,
        $token:expr,
        $code:expr,
        $fmt:expr
        $(, $args:expr)*
        $(,)?
    ) => {
        let range = $token.range();
        parse_error_range!($self, range, $code, $fmt $(, $args)*);
    }
}

/// Report an error based on \p fmt for the source range \p range.
macro_rules! parse_error_range {
    (
        $self:ident,
        $range:expr,
        $code:expr,
        $fmt:expr
        $(, $args:expr)*
        $(,)?
    ) => {
        let text = if $self.out_errors.is_some() && !$self.unwinding {
            Some(wgettext_maybe_fmt!($fmt $(, $args)*))
        } else {
            None
        };
        $self.any_error = true;

        // Ignore additional parse errors while unwinding.
        // These may come about e.g. from `true | and`.
        if !$self.unwinding {
            $self.unwinding = true;

            FLOGF!(ast_construction, "%*sparse error - begin unwinding", $self.spaces(), "");
            // TODO: can store this conditionally dependent on flags.
            if $range.start() != SOURCE_OFFSET_INVALID {
                $self.errors.push($range);
            }

            if let Some(errors) = &mut $self.out_errors {
                let mut err = ParseError::default();
                err.text = text.unwrap();
                err.code = $code;
                err.source_start = $range.start();
                err.source_length = $range.length();
                errors.push(err);
            }
        }
    }
}

struct Populator<'a> {
    /// Flags controlling parsing.
    flags: ParseTreeFlags,

    /// Set of semicolons, sorted by offset.
    semis: SourceRangeList,

    /// Set of error ranges, sorted by offset.
    errors: SourceRangeList,

    /// Stream of tokens which we consume.
    tokens: TokenStream<'a>,

    /** The type which we are attempting to parse, typically job_list but may be
    freestanding_argument_list. */
    top_type: Type,

    /// If set, we are unwinding due to error recovery.
    unwinding: bool,

    /// If set, we have encountered an error.
    any_error: bool,

    /// The number of parent links of the node we are visiting
    depth: usize,

    // If non-null, populate with errors.
    out_errors: Option<&'a mut ParseErrorList>,
}

impl Populator<'_> {
    /// will_visit (did_visit) is called before (after) a node's fields are visited.

    fn visit_mut(&mut self, node: &mut dyn NodeMut) -> VisitResult {
        match node.as_mut_node() {
            NodeEnumRefMut::Leaf(leaf) => match leaf {
                LeafRefMut::Argument(argument) => self.visit_argument(argument),
                LeafRefMut::VariableAssignment(assign) => self.visit_variable_assignment(assign),
                LeafRefMut::Token(token) => self.visit_token(token),
                LeafRefMut::Keyword(keyword) => return self.visit_keyword(keyword),
                LeafRefMut::MaybeNewlines(newlines) => self.visit_maybe_newlines(newlines),
            },
            NodeEnumRefMut::Branch(BranchRefMut::JobContinuation(n)) => {
                self.visit_job_continuation(n);
            }
            // Visit branch nodes by just calling accept() to visit their fields.
            NodeEnumRefMut::Branch(_branch) => {
                // This field is a direct embedding of an AST value.
                node.accept_mut(self, false);
            }
            // This field is an embedding of an array of (pointers to) ContentsNode.
            // Parse as many as we can.
            NodeEnumRefMut::List(list) => match list {
                ListRefMut::AndorJobList(l) => self.populate_list(l, false),
                ListRefMut::ArgumentList(l) => self.populate_list(l, false),
                ListRefMut::ArgumentOrRedirectionList(l) => self.populate_list(l, false),
                ListRefMut::CaseItemList(l) => self.populate_list(l, false),
                ListRefMut::ElseifClauseList(l) => self.populate_list(l, false),
                ListRefMut::JobConjunctionContinuationList(l) => self.populate_list(l, false),
                ListRefMut::JobContinuationList(l) => self.populate_list(l, false),
                ListRefMut::JobList(l) => self.populate_list(l, false),
                ListRefMut::VariableAssignmentList(l) => self.populate_list(l, false),
            },
        }
        VisitResult::Continue(())
    }

    fn will_visit_fields_of(&mut self, node: &mut dyn NodeMut) {
        FLOGF!(
            ast_construction,
            "%*swill_visit %ls",
            self.spaces(),
            "",
            node.describe()
        );
        self.depth += 1
    }

    fn did_visit_fields_of<'a>(&'a mut self, node: &impl Node, flow: VisitResult) {
        self.depth -= 1;

        if self.unwinding {
            return;
        }
        let VisitResult::Break(error) = flow else {
            return;
        };

        // We believe the node is some sort of block statement. Attempt to find a source range
        // for the block's keyword (for, if, etc) and a user-presentable description. This
        // is used to provide better error messages. Note at this point the parse tree is
        // incomplete; in particular parent nodes are not set.
        let mut cursor = node.as_node();
        let header = loop {
            let NodeEnumRef::Branch(branch) = cursor else {
                break None;
            };

            if let BranchRef::BlockStatement(n) = branch {
                cursor = n.header.embedded_node();
                continue;
            }

            let (range, stmt) = match branch {
                BranchRef::ForHeader(n) => (n.kw_for.range, L!("for loop")),
                BranchRef::WhileHeader(n) => (n.kw_while.range, L!("while loop")),
                BranchRef::FunctionHeader(n) => (n.kw_function.range, L!("function definition")),
                BranchRef::BeginHeader(n) => (n.kw_begin.range, L!("begin")),
                BranchRef::IfStatement(n) => (n.if_clause.kw_if.range, L!("if statement")),
                BranchRef::SwitchStatement(n) => (n.kw_switch.range, L!("switch statement")),
                _ => break None,
            };

            break Some((range.unwrap(), stmt));
        };

        if let Some((header_kw_range, enclosing_stmt)) = header {
            parse_error_range!(
                self,
                header_kw_range,
                ParseErrorCode::generic,
                "Missing end to balance this %ls",
                enclosing_stmt
            );
        } else {
            parse_error!(
                self,
                error.token,
                ParseErrorCode::generic,
                "Expected %ls, but found %ls",
                keywords_user_presentable_description(error.allowed_keywords),
                error.token.user_presentable_description(),
            );
        }
    }

    // We currently only have a handful of union pointer types.
    // Handle them directly.
    fn visit_argument_or_redirection(
        &mut self,
        node: &mut Box<ArgumentOrRedirectionVariant>,
    ) -> VisitResult {
        if let Some(arg) = self.try_parse::<Argument>() {
            **node = ArgumentOrRedirectionVariant::Argument(*arg);
        } else if let Some(redir) = self.try_parse::<Redirection>() {
            **node = ArgumentOrRedirectionVariant::Redirection(*redir);
        } else {
            internal_error!(
                self,
                visit_argument_or_redirection,
                "Unable to parse argument or redirection"
            );
        }
        VisitResult::Continue(())
    }
    fn visit_block_statement_header(
        &mut self,
        node: &mut Box<BlockStatementHeaderVariant>,
    ) -> VisitResult {
        *node = self.allocate_populate_block_header();
        VisitResult::Continue(())
    }
    fn visit_statement(&mut self, node: &mut Box<StatementVariant>) -> VisitResult {
        *node = self.allocate_populate_statement_contents();
        VisitResult::Continue(())
    }

    fn visit_decorated_statement_decorator(
        &mut self,
        node: &mut Option<DecoratedStatementDecorator>,
    ) {
        *node = self.try_parse::<DecoratedStatementDecorator>().map(|b| *b);
    }
    fn visit_job_conjunction_decorator(&mut self, node: &mut Option<JobConjunctionDecorator>) {
        *node = self.try_parse::<JobConjunctionDecorator>().map(|b| *b);
    }
    fn visit_else_clause(&mut self, node: &mut Option<ElseClause>) {
        *node = self.try_parse::<ElseClause>().map(|b| *b);
    }
    fn visit_semi_nl(&mut self, node: &mut Option<SemiNl>) {
        *node = self.try_parse::<SemiNl>().map(|b| *b);
    }
    fn visit_time(&mut self, node: &mut Option<KeywordTime>) {
        *node = self.try_parse::<KeywordTime>().map(|b| *b);
    }
    fn visit_token_background(&mut self, node: &mut Option<TokenBackground>) {
        *node = self.try_parse::<TokenBackground>().map(|b| *b);
    }
}

/// Helper to describe a list of keywords.
/// TODO: these need to be localized properly.
fn keywords_user_presentable_description(kws: &'static [ParseKeyword]) -> WString {
    assert!(!kws.is_empty(), "Should not be empty list");
    if kws.len() == 1 {
        return sprintf!("keyword '%ls'", kws[0]);
    }
    let mut res = L!("keywords ").to_owned();
    for (i, kw) in kws.iter().enumerate() {
        if i != 0 {
            res += L!(" or ");
        }
        res += &sprintf!("'%ls'", *kw)[..];
    }
    res
}

/// Helper to describe a list of token types.
/// TODO: these need to be localized properly.
fn token_types_user_presentable_description(types: &'static [ParseTokenType]) -> WString {
    assert!(!types.is_empty(), "Should not be empty list");
    let mut res = WString::new();
    for typ in types {
        if !res.is_empty() {
            res += L!(" or ");
        }
        res += &token_type_user_presentable_description(*typ, ParseKeyword::none)[..];
    }
    res
}

impl<'s> Populator<'s> {
    /// Construct from a source, flags, top type, and out_errors, which may be null.
    fn new(
        src: &'s wstr,
        flags: ParseTreeFlags,
        top_type: Type,
        out_errors: Option<&'s mut ParseErrorList>,
    ) -> Self {
        Self {
            flags,
            semis: vec![],
            errors: vec![],
            tokens: TokenStream::new(src, flags),
            top_type,
            unwinding: false,
            any_error: false,
            depth: 0,
            out_errors,
        }
    }

    /// Helper for FLOGF. This returns a number of spaces appropriate for a '%*c' format.
    fn spaces(&self) -> usize {
        self.depth * 2
    }

    /// \return the parser's status.
    fn status(&mut self) -> ParserStatus {
        if self.unwinding {
            ParserStatus::unwinding
        } else if self.flags.contains(ParseTreeFlags::LEAVE_UNTERMINATED)
            && self.peek_type(0) == ParseTokenType::terminate
        {
            ParserStatus::unsourcing
        } else {
            ParserStatus::ok
        }
    }

    /// \return whether any leaf nodes we visit should be marked as unsourced.
    fn unsource_leaves(&mut self) -> bool {
        matches!(
            self.status(),
            ParserStatus::unsourcing | ParserStatus::unwinding
        )
    }

    /// \return whether we permit an incomplete parse tree.
    fn allow_incomplete(&self) -> bool {
        self.flags.contains(ParseTreeFlags::LEAVE_UNTERMINATED)
    }

    /// \return whether a list type \p type allows arbitrary newlines in it.
    fn list_type_chomps_newlines(&self, typ: Type) -> bool {
        match typ {
            Type::argument_list => {
                // Hackish. If we are producing a freestanding argument list, then it allows
                // semicolons, for hysterical raisins.
                self.top_type == Type::freestanding_argument_list
            }
            Type::argument_or_redirection_list => {
                // No newlines inside arguments.
                false
            }
            Type::variable_assignment_list => {
                // No newlines inside variable assignment lists.
                false
            }
            Type::job_list => {
                // Like echo a \n \n echo b
                true
            }
            Type::case_item_list => {
                // Like switch foo \n \n \n case a \n end
                true
            }
            Type::andor_job_list => {
                // Like while true ; \n \n and true ; end
                true
            }
            Type::elseif_clause_list => {
                // Like if true ; \n \n else if false; end
                true
            }
            Type::job_conjunction_continuation_list => {
                // This would be like echo a && echo b \n && echo c
                // We could conceivably support this but do not now.
                false
            }
            Type::job_continuation_list => {
                // This would be like echo a \n | echo b
                // We could conceivably support this but do not now.
                false
            }
            _ => {
                internal_error!(
                    self,
                    list_type_chomps_newlines,
                    "Type %ls not handled",
                    ast_type_to_string(typ)
                );
            }
        }
    }

    /// \return whether a list type \p type allows arbitrary semicolons in it.
    fn list_type_chomps_semis(&self, typ: Type) -> bool {
        match typ {
            Type::argument_list => {
                // Hackish. If we are producing a freestanding argument list, then it allows
                // semicolons, for hysterical raisins.
                // That is, this is OK: complete -c foo -a 'x ; y ; z'
                // But this is not: foo x ; y ; z
                self.top_type == Type::freestanding_argument_list
            }

            Type::argument_or_redirection_list | Type::variable_assignment_list => false,
            Type::job_list => {
                // Like echo a ; ;  echo b
                true
            }
            Type::case_item_list => {
                // Like switch foo ; ; ;  case a \n end
                // This is historically allowed.
                true
            }
            Type::andor_job_list => {
                // Like while true ; ; ;  and true ; end
                true
            }
            Type::elseif_clause_list => {
                // Like if true ; ; ;  else if false; end
                false
            }
            Type::job_conjunction_continuation_list => {
                // Like echo a ; ; && echo b. Not supported.
                false
            }
            Type::job_continuation_list => {
                // This would be like echo a ; | echo b
                // Not supported.
                // We could conceivably support this but do not now.
                false
            }
            _ => {
                internal_error!(
                    self,
                    list_type_chomps_semis,
                    "Type %ls not handled",
                    ast_type_to_string(typ)
                );
            }
        }
    }

    /// Chomp extra comments, semicolons, etc. for a given list type.
    fn chomp_extras(&mut self, typ: Type) {
        let chomp_semis = self.list_type_chomps_semis(typ);
        let chomp_newlines = self.list_type_chomps_newlines(typ);
        loop {
            let peek = self.tokens.peek(0);
            if chomp_newlines && peek.typ == ParseTokenType::end && peek.is_newline {
                // Just skip this newline, no need to save it.
                self.tokens.pop();
            } else if chomp_semis && peek.typ == ParseTokenType::end && !peek.is_newline {
                let tok = self.tokens.pop();
                // Perhaps save this extra semi.
                if self.flags.contains(ParseTreeFlags::SHOW_EXTRA_SEMIS) {
                    self.semis.push(tok.range());
                }
            } else {
                break;
            }
        }
    }

    /// \return whether a list type should recover from errors.s
    /// That is, whether we should stop unwinding when we encounter this type.
    fn list_type_stops_unwind(&self, typ: Type) -> bool {
        typ == Type::job_list && self.flags.contains(ParseTreeFlags::CONTINUE_AFTER_ERROR)
    }

    /// \return a reference to a non-comment token at index \p idx.
    fn peek_token(&mut self, idx: usize) -> &ParseToken {
        self.tokens.peek(idx)
    }

    /// \return the type of a non-comment token.
    fn peek_type(&mut self, idx: usize) -> ParseTokenType {
        self.peek_token(idx).typ
    }

    /// Consume the next token, chomping any comments.
    /// It is an error to call this unless we know there is a non-terminate token available.
    /// \return the token.
    fn consume_any_token(&mut self) -> ParseToken {
        let tok = self.tokens.pop();
        assert!(
            tok.typ != ParseTokenType::comment,
            "Should not be a comment"
        );
        assert!(
            tok.typ != ParseTokenType::terminate,
            "Cannot consume terminate token, caller should check status first"
        );
        tok
    }

    /// Consume the next token which is expected to be of the given type.
    fn consume_token_type(&mut self, typ: ParseTokenType) -> SourceRange {
        assert!(
            typ != ParseTokenType::terminate,
            "Should not attempt to consume terminate token"
        );
        let tok = self.consume_any_token();
        if tok.typ != typ {
            parse_error!(
                self,
                tok,
                ParseErrorCode::generic,
                "Expected %ls, but found %ls",
                token_type_user_presentable_description(typ, ParseKeyword::none),
                tok.user_presentable_description()
            );
            return SourceRange::new(0, 0);
        }
        tok.range()
    }

    /// The next token could not be parsed at the top level.
    /// For example a trailing end like `begin ; end ; end`
    /// Or an unexpected redirection like `>`
    /// Consume it and add an error.
    fn consume_excess_token_generating_error(&mut self) {
        let tok = self.consume_any_token();

        // In the rare case that we are parsing a freestanding argument list and not a job list,
        // generate a generic error.
        // TODO: this is a crummy message if we get a tokenizer error, for example:
        //   complete -c foo -a "'abc"
        if self.top_type == Type::freestanding_argument_list {
            parse_error!(
                self,
                tok,
                ParseErrorCode::generic,
                "Expected %ls, but found %ls",
                token_type_user_presentable_description(ParseTokenType::string, ParseKeyword::none),
                tok.user_presentable_description()
            );
            return;
        }

        assert!(self.top_type == Type::job_list);
        match tok.typ {
            ParseTokenType::string => {
                // There are three keywords which end a job list.
                match tok.keyword {
                    ParseKeyword::kw_end => {
                        parse_error!(
                            self,
                            tok,
                            ParseErrorCode::unbalancing_end,
                            "'end' outside of a block"
                        );
                    }
                    ParseKeyword::kw_else => {
                        parse_error!(
                            self,
                            tok,
                            ParseErrorCode::unbalancing_else,
                            "'else' builtin not inside of if block"
                        );
                    }
                    ParseKeyword::kw_case => {
                        parse_error!(
                            self,
                            tok,
                            ParseErrorCode::unbalancing_case,
                            "'case' builtin not inside of switch block"
                        );
                    }
                    _ => {
                        internal_error!(
                            self,
                            consume_excess_token_generating_error,
                            "Token %ls should not have prevented parsing a job list",
                            tok.user_presentable_description()
                        );
                    }
                }
            }
            ParseTokenType::redirection if self.peek_type(0) == ParseTokenType::string => {
                let next = self.tokens.pop();
                parse_error_range!(
                    self,
                    next.range().combine(tok.range()),
                    ParseErrorCode::generic,
                    "Expected a string, but found a redirection"
                );
            }
            ParseTokenType::pipe
            | ParseTokenType::redirection
            | ParseTokenType::background
            | ParseTokenType::andand
            | ParseTokenType::oror => {
                parse_error!(
                    self,
                    tok,
                    ParseErrorCode::generic,
                    "Expected a string, but found %ls",
                    tok.user_presentable_description()
                );
            }
            ParseTokenType::tokenizer_error => {
                parse_error!(
                    self,
                    tok,
                    ParseErrorCode::from(tok.tok_error),
                    "%ls",
                    tok.tok_error
                );
            }
            ParseTokenType::end => {
                internal_error!(
                    self,
                    consume_excess_token_generating_error,
                    "End token should never be excess"
                );
            }
            ParseTokenType::terminate => {
                internal_error!(
                    self,
                    consume_excess_token_generating_error,
                    "Terminate token should never be excess"
                );
            }
            _ => {
                internal_error!(
                    self,
                    consume_excess_token_generating_error,
                    "Unexpected excess token type: %ls",
                    tok.user_presentable_description()
                );
            }
        }
    }

    /// Given that we are a list of type ListNodeType, whose contents type is ContentsNode,
    /// populate as many elements as we can.
    /// If exhaust_stream is set, then keep going until we get parse_token_type_t::terminate.
    fn populate_list<ListType: List>(&mut self, list: &mut ListType, exhaust_stream: bool)
    where
        <ListType as List>::ContentsNode: NodeMut + CheckParse,
    {
        assert!(list.contents().is_empty(), "List is not initially empty");

        // Do not attempt to parse a list if we are unwinding.
        if self.unwinding {
            assert!(
                !exhaust_stream,
                "exhaust_stream should only be set at top level, and so we should not be unwinding"
            );
            // Mark in the list that it was unwound.
            FLOGF!(
                ast_construction,
                "%*sunwinding %ls",
                self.spaces(),
                "",
                ast_type_to_string(list.typ())
            );
            assert!(list.contents().is_empty(), "Should be an empty list");
            return;
        }

        // We're going to populate a vector with our nodes.
        // Later on we will copy this to the heap with a single allocation.
        let mut contents = vec![];

        loop {
            // If we are unwinding, then either we recover or we break the loop, dependent on the
            // loop type.
            if self.unwinding {
                if !self.list_type_stops_unwind(list.typ()) {
                    break;
                }
                // We are going to stop unwinding.
                // Rather hackish. Just chomp until we get to a string or end node.
                loop {
                    let typ = self.peek_type(0);
                    if [
                        ParseTokenType::string,
                        ParseTokenType::terminate,
                        ParseTokenType::end,
                    ]
                    .contains(&typ)
                    {
                        break;
                    }
                    let tok = self.tokens.pop();
                    self.errors.push(tok.range());
                    FLOGF!(
                        ast_construction,
                        "%*schomping range %u-%u",
                        self.spaces(),
                        "",
                        tok.source_start(),
                        tok.source_length()
                    );
                }
                FLOGF!(ast_construction, "%*sdone unwinding", self.spaces(), "");
                self.unwinding = false;
            }

            // Chomp semis and newlines.
            self.chomp_extras(list.typ());

            // Now try parsing a node.
            if let Some(node) = self.try_parse::<ListType::ContentsNode>() {
                // #7201: Minimize reallocations of contents vector
                // Empirically, 99.97% of cases are 16 elements or fewer,
                // with 75% being empty, so this works out best.
                if contents.is_empty() {
                    contents.reserve(16);
                }
                contents.push(node);
            } else if exhaust_stream && self.peek_type(0) != ParseTokenType::terminate {
                // We aren't allowed to stop. Produce an error and keep going.
                self.consume_excess_token_generating_error()
            } else {
                // We either stop once we can't parse any more of this contents node, or we
                // exhausted the stream as requested.
                break;
            }
        }

        // Populate our list from our contents.
        if !contents.is_empty() {
            assert!(
                contents.len() <= u32::MAX.try_into().unwrap(),
                "Contents size out of bounds"
            );
            assert!(list.contents().is_empty(), "List should still be empty");
            *list.contents_mut() = contents;
        }

        FLOGF!(
            ast_construction,
            "%*s%ls size: %lu",
            self.spaces(),
            "",
            ast_type_to_string(list.typ()),
            list.count()
        );
    }

    /// Allocate and populate a statement contents pointer.
    /// This must never return null.
    fn allocate_populate_statement_contents(&mut self) -> Box<StatementVariant> {
        // In case we get a parse error, we still need to return something non-null. Use a
        // decorated statement; all of its leaf nodes will end up unsourced.
        fn got_error(slf: &mut Populator<'_>) -> Box<StatementVariant> {
            assert!(slf.unwinding, "Should have produced an error");
            new_decorated_statement(slf)
        }

        fn new_decorated_statement(slf: &mut Populator<'_>) -> Box<StatementVariant> {
            let embedded = slf.allocate_visit::<DecoratedStatement>();
            Box::new(StatementVariant::DecoratedStatement(*embedded))
        }

        if self.peek_token(0).typ == ParseTokenType::terminate && self.allow_incomplete() {
            // This may happen if we just have a 'time' prefix.
            // Construct a decorated statement, which will be unsourced.
            self.allocate_visit::<DecoratedStatement>();
        } else if self.peek_token(0).typ != ParseTokenType::string {
            // We may be unwinding already; do not produce another error.
            // For example in `true | and`.
            parse_error!(
                self,
                self.peek_token(0),
                ParseErrorCode::generic,
                "Expected a command, but found %ls",
                self.peek_token(0).user_presentable_description()
            );
            return got_error(self);
        } else if self.peek_token(0).may_be_variable_assignment {
            // Here we have a variable assignment which we chose to not parse as a variable
            // assignment because there was no string after it.
            // Ensure we consume the token, so we don't get back here again at the same place.
            let token = &self.consume_any_token();
            let text = &self.tokens.src
                [token.source_start()..token.source_start() + token.source_length()];
            let equals_pos = variable_assignment_equals_pos(text).unwrap();
            let variable = &text[..equals_pos];
            let value = &text[equals_pos + 1..];
            parse_error!(
                self,
                token,
                ParseErrorCode::bare_variable_assignment,
                ERROR_BAD_COMMAND_ASSIGN_ERR_MSG,
                variable,
                value
            );
            return got_error(self);
        }

        // In some cases a block starter is a decorated statement instead, mostly if invoked with "--help".
        // The logic here is subtle:
        //
        // If we are 'begin', it's only really a block if it has no arguments.
        // If we are 'function' or another block starter, then we are a non-block if we are invoked with -h or --help
        // If we are anything else, we require an argument, so do the same thing if the subsequent
        // token is a statement terminator.
        if self.peek_token(0).typ == ParseTokenType::string {
            // If we are one of these, then look for specifically help arguments. Otherwise, if the next token
            // looks like an option (starts with a dash), then parse it as a decorated statement.
            let help_only_kws = [
                ParseKeyword::kw_begin,
                ParseKeyword::kw_function,
                ParseKeyword::kw_if,
                ParseKeyword::kw_switch,
                ParseKeyword::kw_while,
            ];
            if (help_only_kws.contains(&self.peek_token(0).keyword)
                && self.peek_token(1).is_help_argument)
                || (!help_only_kws.contains(&self.peek_token(0).keyword)
                    && self.peek_token(1).is_dash_prefix_string())
            {
                return new_decorated_statement(self);
            }

            // Likewise if the next token doesn't look like an argument at all. This corresponds to
            // e.g. a "naked if".
            let naked_invocation_invokes_help = ![ParseKeyword::kw_begin, ParseKeyword::kw_end]
                .contains(&self.peek_token(0).keyword);
            if naked_invocation_invokes_help
                && [ParseTokenType::end, ParseTokenType::terminate]
                    .contains(&self.peek_token(1).typ)
            {
                return new_decorated_statement(self);
            }
        }

        match self.peek_token(0).keyword {
            ParseKeyword::kw_not | ParseKeyword::kw_exclam => {
                let embedded = self.allocate_visit::<NotStatement>();
                Box::new(StatementVariant::NotStatement(*embedded))
            }
            ParseKeyword::kw_for
            | ParseKeyword::kw_while
            | ParseKeyword::kw_function
            | ParseKeyword::kw_begin => {
                let embedded = self.allocate_visit::<BlockStatement>();
                Box::new(StatementVariant::BlockStatement(*embedded))
            }
            ParseKeyword::kw_if => {
                let embedded = self.allocate_visit::<IfStatement>();
                Box::new(StatementVariant::IfStatement(embedded))
            }
            ParseKeyword::kw_switch => {
                let embedded = self.allocate_visit::<SwitchStatement>();
                Box::new(StatementVariant::SwitchStatement(*embedded))
            }
            ParseKeyword::kw_end => {
                // 'end' is forbidden as a command.
                // For example, `if end` or `while end` will produce this error.
                // We still have to descend into the decorated statement because
                // we can't leave our pointer as null.
                parse_error!(
                    self,
                    self.peek_token(0),
                    ParseErrorCode::generic,
                    "Expected a command, but found %ls",
                    self.peek_token(0).user_presentable_description()
                );
                return got_error(self);
            }
            _ => new_decorated_statement(self),
        }
    }

    /// Allocate and populate a block statement header.
    /// This must never return null.
    fn allocate_populate_block_header(&mut self) -> Box<BlockStatementHeaderVariant> {
        Box::new(match self.peek_token(0).keyword {
            ParseKeyword::kw_for => {
                let embedded = self.allocate_visit::<ForHeader>();
                BlockStatementHeaderVariant::ForHeader(*embedded)
            }
            ParseKeyword::kw_while => {
                let embedded = self.allocate_visit::<WhileHeader>();
                BlockStatementHeaderVariant::WhileHeader(*embedded)
            }
            ParseKeyword::kw_function => {
                let embedded = self.allocate_visit::<FunctionHeader>();
                BlockStatementHeaderVariant::FunctionHeader(*embedded)
            }
            ParseKeyword::kw_begin => {
                let embedded = self.allocate_visit::<BeginHeader>();
                BlockStatementHeaderVariant::BeginHeader(*embedded)
            }
            _ => {
                internal_error!(
                    self,
                    allocate_populate_block_header,
                    "should not have descended into block_header"
                );
            }
        })
    }

    fn try_parse<T: NodeMut + Default + CheckParse>(&mut self) -> Option<Box<T>> {
        if !T::can_be_parsed(self) {
            return None;
        }
        Some(self.allocate_visit())
    }

    /// Given a node type, allocate it and invoke its default constructor.
    /// \return the resulting Node
    fn allocate<T: NodeMut + Default>(&self) -> Box<T> {
        let result = Box::<T>::default();
        FLOGF!(
            ast_construction,
            "%*smake %ls %ls",
            self.spaces(),
            "",
            ast_type_to_string(result.typ()),
            format!("{result:p}")
        );
        result
    }

    // Given a node type, allocate it, invoke its default constructor,
    // and then visit it as a field.
    // \return the resulting Node pointer. It is never null.
    fn allocate_visit<T: NodeMut + Default>(&mut self) -> Box<T> {
        let mut result = Box::<T>::default();
        self.visit_mut(&mut *result);
        result
    }

    fn visit_argument(&mut self, arg: &mut Argument) {
        if self.unsource_leaves() {
            arg.range = None;
            return;
        }
        arg.range = Some(self.consume_token_type(ParseTokenType::string));
    }

    fn visit_variable_assignment(&mut self, varas: &mut VariableAssignment) {
        if self.unsource_leaves() {
            varas.range = None;
            return;
        }
        if !self.peek_token(0).may_be_variable_assignment {
            internal_error!(
                self,
                visit_variable_assignment,
                "Should not have created variable_assignment_t from this token"
            );
        }
        varas.range = Some(self.consume_token_type(ParseTokenType::string));
    }

    fn visit_job_continuation(&mut self, node: &mut JobContinuation) {
        // Special error handling to catch 'and' and 'or' in pipelines, like `true | and false`.
        if [ParseKeyword::kw_and, ParseKeyword::kw_or].contains(&self.peek_token(1).keyword) {
            parse_error!(
                self,
                self.peek_token(1),
                ParseErrorCode::andor_in_pipeline,
                INVALID_PIPELINE_CMD_ERR_MSG,
                self.peek_token(1).keyword
            );
        }
        node.accept_mut(self, false);
    }

    // Overload for token fields.
    fn visit_token(&mut self, mut token: TokenRefMut<'_>) {
        if self.unsource_leaves() {
            *token.range_mut() = None;
            return;
        }

        if !token.allows_token(self.peek_token(0).typ) {
            if self.flags.contains(ParseTreeFlags::LEAVE_UNTERMINATED)
                && [
                    TokenizerError::unterminated_quote,
                    TokenizerError::unterminated_subshell,
                ]
                .contains(&self.peek_token(0).tok_error)
            {
                return;
            }

            parse_error!(
                self,
                self.peek_token(0),
                ParseErrorCode::generic,
                "Expected %ls, but found %ls",
                token_types_user_presentable_description(token.allowed_tokens()),
                self.peek_token(0).user_presentable_description()
            );
            *token.range_mut() = None;
            return;
        }
        let tok = self.consume_any_token();
        *token.token_type_mut() = tok.typ;
        *token.range_mut() = Some(tok.range());
    }

    // Overload for keyword fields.
    fn visit_keyword(&mut self, mut keyword: KeywordRefMut<'_>) -> VisitResult {
        if self.unsource_leaves() {
            *keyword.range_mut() = None;
            return VisitResult::Continue(());
        }

        if !keyword.allows_keyword(self.peek_token(0).keyword) {
            *keyword.range_mut() = None;

            if self.flags.contains(ParseTreeFlags::LEAVE_UNTERMINATED)
                && [
                    TokenizerError::unterminated_quote,
                    TokenizerError::unterminated_subshell,
                ]
                .contains(&self.peek_token(0).tok_error)
            {
                return VisitResult::Continue(());
            }

            // Special error reporting for keyword_t<kw_end>.
            let allowed_keywords = keyword.allowed_keywords();
            if keyword.allowed_keywords() == [ParseKeyword::kw_end] {
                return VisitResult::Break(MissingEndError {
                    allowed_keywords,
                    token: *self.peek_token(0),
                });
            } else {
                parse_error!(
                    self,
                    self.peek_token(0),
                    ParseErrorCode::generic,
                    "Expected %ls, but found %ls",
                    keywords_user_presentable_description(allowed_keywords),
                    self.peek_token(0).user_presentable_description(),
                );
                return VisitResult::Continue(());
            }
        }
        let tok = self.consume_any_token();
        *keyword.keyword_mut() = tok.keyword;
        *keyword.range_mut() = Some(tok.range());
        VisitResult::Continue(())
    }

    fn visit_maybe_newlines(&mut self, nls: &mut MaybeNewlines) {
        if self.unsource_leaves() {
            nls.range = None;
            return;
        }
        let mut range = SourceRange::new(0, 0);
        // TODO: it would be nice to have the start offset be the current position in the token
        // stream, even if there are no newlines.
        while self.peek_token(0).is_newline {
            let r = self.consume_token_type(ParseTokenType::end);
            if range.length == 0 {
                range = r;
            } else {
                range.length = r.start + r.length - range.start
            }
        }
        nls.range = Some(range);
    }
}

/// The status of our parser.
enum ParserStatus {
    /// Parsing is going just fine, thanks for asking.
    ok,

    /// We have exhausted the token stream, but the caller was OK with an incomplete parse tree.
    /// All further leaf nodes should have the unsourced flag set.
    unsourcing,

    /// We encountered an parse error and are "unwinding."
    /// Do not consume any tokens until we get back to a list type which stops unwinding.
    unwinding,
}

fn parse_from_top(
    src: &wstr,
    flags: ParseTreeFlags,
    out_errors: Option<&mut ParseErrorList>,
    top_type: Type,
) -> Ast {
    assert!(
        [Type::job_list, Type::freestanding_argument_list].contains(&top_type),
        "Invalid top type"
    );

    let mut ast = Ast::default();

    let mut pops = Populator::new(src, flags, top_type, out_errors);
    if top_type == Type::job_list {
        let mut list = pops.allocate::<JobList>();
        pops.populate_list(&mut *list, true /* exhaust_stream */);
        ast.top = list;
    } else {
        let mut list = pops.allocate::<FreestandingArgumentList>();
        pops.populate_list(&mut list.arguments, true /* exhaust_stream */);
        ast.top = list;
    }

    // Chomp trailing extras, etc.
    pops.chomp_extras(Type::job_list);

    ast.any_error = pops.any_error;
    ast.extras = Extras {
        comments: pops.tokens.comment_ranges,
        semis: pops.semis,
        errors: pops.errors,
    };

    // Set all parent nodes.
    // It turns out to be more convenient to do this after the parse phase.
    match ast.top_mut() {
        NodeEnumRefMut::List(ListRefMut::JobList(list)) => list.set_parents(),
        NodeEnumRefMut::Branch(BranchRefMut::FreestandingArgumentList(list)) => list.set_parents(),
        _ => unreachable!(),
    }

    ast
}

/// \return tokenizer flags corresponding to parse tree flags.
impl From<ParseTreeFlags> for TokFlags {
    fn from(flags: ParseTreeFlags) -> Self {
        let mut tok_flags = TokFlags(0);
        // Note we do not need to respect parse_flag_show_blank_lines, no clients are interested
        // in them.
        if flags.contains(ParseTreeFlags::INCLUDE_COMMENTS) {
            tok_flags |= TOK_SHOW_COMMENTS;
        }
        if flags.contains(ParseTreeFlags::ACCEPT_INCOMPLETE_TOKENS) {
            tok_flags |= TOK_ACCEPT_UNFINISHED;
        }
        if flags.contains(ParseTreeFlags::CONTINUE_AFTER_ERROR) {
            tok_flags |= TOK_CONTINUE_AFTER_ERROR
        }
        tok_flags
    }
}

/// Convert from Tokenizer's token type to a parse_token_t type.
impl From<TokenType> for ParseTokenType {
    fn from(token_type: TokenType) -> Self {
        match token_type {
            TokenType::string => ParseTokenType::string,
            TokenType::pipe => ParseTokenType::pipe,
            TokenType::andand => ParseTokenType::andand,
            TokenType::oror => ParseTokenType::oror,
            TokenType::end => ParseTokenType::end,
            TokenType::background => ParseTokenType::background,
            TokenType::redirect => ParseTokenType::redirection,
            TokenType::error => ParseTokenType::tokenizer_error,
            TokenType::comment => ParseTokenType::comment,
        }
    }
}

fn is_keyword_char(c: char) -> bool {
    ('a'..='z').contains(&c)
        || ('A'..='Z').contains(&c)
        || ('0'..='9').contains(&c)
        || c == '\''
        || c == '"'
        || c == '\\'
        || c == '\n'
        || c == '!'
}

/// Given a token, returns the keyword it matches, or ParseKeyword::none.
fn keyword_for_token(tok: TokenType, token: &wstr) -> ParseKeyword {
    /* Only strings can be keywords */
    if tok != TokenType::string {
        return ParseKeyword::none;
    }

    // If token is clean (which most are), we can compare it directly. Otherwise we have to expand
    // it. We only expand quotes, and we don't want to do expensive expansions like tilde
    // expansions. So we do our own "cleanliness" check; if we find a character not in our allowed
    // set we know it's not a keyword, and if we never find a quote we don't have to expand! Note
    // that this lowercase set could be shrunk to be just the characters that are in keywords.
    let mut result = ParseKeyword::none;
    let mut needs_expand = false;
    let mut all_chars_valid = true;
    for c in token.chars() {
        if !is_keyword_char(c) {
            all_chars_valid = false;
            break;
        }
        // If we encounter a quote, we need expansion.
        needs_expand = needs_expand || c == '"' || c == '\'' || c == '\\'
    }

    if all_chars_valid {
        // Expand if necessary.
        if !needs_expand {
            result = ParseKeyword::from(token);
        } else if let Some(unescaped) = unescape_string(token, UnescapeStringStyle::default()) {
            result = ParseKeyword::from(&unescaped[..]);
        }
    }
    result
}

#[test]
#[serial]
fn test_ast_parse() {
    test_init();
    let src = L!("echo");
    let ast = Ast::parse(src, ParseTreeFlags::empty(), None);
    assert!(!ast.any_error);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Category {
    branch,
    leaf,
    list,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Type {
    token_base,
    keyword_base,
    redirection,
    variable_assignment,
    variable_assignment_list,
    argument_or_redirection,
    argument_or_redirection_list,
    statement,
    job_pipeline,
    job_conjunction,
    for_header,
    while_header,
    function_header,
    begin_header,
    block_statement,
    if_clause,
    elseif_clause,
    elseif_clause_list,
    else_clause,
    if_statement,
    case_item,
    switch_statement,
    decorated_statement,
    not_statement,
    job_continuation,
    job_continuation_list,
    job_conjunction_continuation,
    andor_job,
    andor_job_list,
    freestanding_argument_list,
    token_conjunction,
    job_conjunction_continuation_list,
    maybe_newlines,
    token_pipe,
    case_item_list,
    argument,
    argument_list,
    job_list,
}
