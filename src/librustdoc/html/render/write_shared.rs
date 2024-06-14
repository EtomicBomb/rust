#![allow(dead_code)]
//! Rustdoc writes out two kinds of shared files:
//!  - Static files, which are embedded in the rustdoc binary and are written with a
//!    filename that includes a hash of their contents. These will always have a new
//!    URL if the contents change, so they are safe to cache with the
//!    `Cache-Control: immutable` directive. They are written under the static.files/
//!    directory and are written when --emit-type is empty (default) or contains
//!    "toolchain-specific". If using the --static-root-path flag, it should point
//!    to a URL path prefix where each of these filenames can be fetched.
//!  - Invocation specific files. These are generated based on the crate(s) being
//!    documented. Their filenames need to be predictable without knowing their
//!    contents, so they do not include a hash in their filename and are not safe to
//!    cache with `Cache-Control: immutable`. They include the contents of the
//!    --resource-suffix flag and are emitted when --emit-type is empty (default)
//!    or contains "invocation-specific".

use std:: marker::PhantomData;
use std::cell::RefCell;
use std::fs::{self, read_dir};
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::fmt::{self, Display};
use std::ffi::OsString;

use indexmap::IndexMap;
use itertools::Itertools;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_middle::ty::fast_reject::{DeepRejectCtxt, TreatParams};
use rustc_span::def_id::DefId;
use rustc_span::Symbol;
use serde::ser::SerializeSeq;
use serde::{Serialize, Deserialize, DeserializeOwned, Serializer};

use super::{collect_paths_for_type, ensure_trailing_slash, Context, SharedContext, RenderMode};
use crate::html::render::sorted_json::SortedJson;
use crate::clean::{Crate, Item, ItemId, ItemKind};
use crate::config::{EmitType, RenderOptions};
use crate::docfs::PathError;
use crate::error::Error;
use crate::formats::cache::Cache;
use crate::formats::item_type::ItemType;
use crate::formats::Impl;
use crate::html::format::Buffer;
use crate::html::render::search_index::SerializedSearchIndex;
use crate::html::render::{AssocItemLink, ImplRenderingParameters};
use crate::html::layout;
use crate::html::static_files::{self, suffix_path};
use crate::visit::DocVisitor;
use crate::{try_err, try_none};

/// Writes the static files, the style files, and the css extensions
pub(crate) fn write_static_files(
    cx: &mut Context<'_>,
    options: &RenderOptions,
) -> Result<(), Error> {
    let static_dir = cx.dst.join("static.files");

    cx.shared
        .fs
        .create_dir_all(&static_dir)
        .map_err(|e| PathError::new(e, "static.files"))?;

    // Handle added third-party themes
    for entry in &cx.shared.style_files {
        let theme = entry.basename()?;
        let extension =
            try_none!(try_none!(entry.path.extension(), &entry.path).to_str(), &entry.path);

        // Skip the official themes. They are written below as part of STATIC_FILES_LIST.
        if matches!(theme.as_str(), "light" | "dark" | "ayu") {
            continue;
        }

        let bytes = try_err!(fs::read(&entry.path), &entry.path);
        let filename = format!("{theme}{suffix}.{extension}", suffix = cx.shared.resource_suffix);
        cx.shared.fs.write(cx.dst.join(filename), bytes)?;
    }

    // When the user adds their own CSS files with --extend-css, we write that as an
    // invocation-specific file (that is, with a resource suffix).
    if let Some(ref css) = cx.shared.layout.css_file_extension {
        let buffer = try_err!(fs::read_to_string(css), css);
        let path = static_files::suffix_path("theme.css", &cx.shared.resource_suffix);
        cx.shared.fs.write(cx.dst.join(path), buffer)?;
    }

    if options.emit.is_empty() || options.emit.contains(&EmitType::Toolchain) {
        static_files::for_each(|f: &static_files::StaticFile| {
            let filename = static_dir.join(f.output_filename());
            cx.shared.fs.write(filename, f.minified())
        })?;
    }

    Ok(())
}

/// None if more than or fewer than one element in `items`
fn only_element<T>(mut items: Vec<T>) -> Option<T> {
    let ret = items.pop()?;
    items.is_empty().then_some(ret)
}

/// A crate name.
///
/// If we have an identification identifier for a specific crate,
/// then we assume we have a `doc/.parts/` folder for it
pub(crate) struct InvocationIdentifier(String);

/// TODO: remove this, and just get the crates that have been --externed
/// Gets the complete list that have been documented by inspecting `doc/.parts`.
/// Users may prefer to provide their own list by documenting the crates separately, and linking
/// them with `rustdoc link`.
pub(crate) fn all_documented_crates(doc_root: &Path) -> Result<Vec<InvocationIdentifier>, Error> {
    let parts_path = PathBuf::from_iter([doc_root, Path::new(".parts")]);
    try_err!(read_dir(&parts_path), &parts_path)
        .map(|invocation| {
            let path = try_err!(invocation, &parts_path);
            let path = path.path().into_os_string().into_string().map_err(|_| "osstring pathname conversion failed");
            let path = try_err!(path, &parts_path);
            Ok(InvocationIdentifier(path))
        })
        .collect()
}

/// Paths (relative to `doc/`) and their pre-merge contents
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct PartsAndLocations<P> {
    parts: Vec<(PathBuf, P)>,
}

impl<P> Default for PartsAndLocations<P> {
    fn default() -> Self {
        Self { parts: Vec::default() }
    }
}

impl<T, U> PartsAndLocations<Part<T, U>> {
    fn push(
        &mut self,
        path: PathBuf,
        part: Part<T, U>,
    ) {
        self.parts.push((path, part));
    }

    fn with(path: PathBuf, part: Part<T, U>) -> Self {
        let mut ret = Self::default();
        ret.push(path, part);
        ret
    }
}

impl<T: NamedCrossCrateInformation, U: Serialize> PartsAndLocations<Part<T, U>> {
    fn write(self, cx: &mut Context<'_>, invocation: &InvocationIdentifier) -> Result<(), Error> {
        let path = PathBuf::from_iter([&cx.dst, Path::new(".parts"), Path::new(&invocation.0), Path::new(T::NAME)]);
        write_create_parents(cx, path, serde_json::to_string(&self).unwrap())?;
        Ok(())
    }
}

/// A piece of one of the shared artifacts for documentation (search index, sources, alias list, etc.)
///
/// Merged at a user specified time and written to the `doc/` directory
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(transparent)]
struct Part<T, U> {
    #[serde(skip)]
    _artifact: PhantomData<T>,
    item: U,
}

impl<T: NamedCrossCrateInformation + DeserializeOwned + Default, U: DeserializeOwned + Default> Part<T, U> {
    fn one(item: U) -> Self {
        Part { item, _artifact: PhantomData }
    }

    fn read_merged_parts(doc_root: &Path, invocations: &[InvocationIdentifier]) -> Result<FxHashMap<PathBuf, Vec<U>>, Error> {
        let mut ret: FxHashMap<PathBuf, Vec<U>> = Default::default();
        for invocation in invocations {
            let path = PathBuf::from_iter([doc_root, Path::new(".parts"), Path::new(&invocation.0), Path::new(T::NAME)]);
            let parts = try_err!(fs::read(&path), &path);
            let parts: PartsAndLocations::<Self> = try_err!(serde_json::from_slice(&parts), &path);
            for (path, mut part) in parts.parts {
                ret.entry(path).or_default().items.push(part.item);
            }
        }
        Ok(ret)
    }
}

trait NamedCrossCrateInformation {
    /// Identifies the kind of cross crate information.
    ///
    /// The filename in `doc/.parts/`
    const NAME: &'static str;
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct Sources;
type SourcesPart = Part<Sources, SortedJson>;
impl NamedCrossCrateInformation for Sources {
    const NAME: &'static str = "src-files-js";
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct SearchIndex;
type SearchIndexPart = Part<SearchIndex, SortedJson>;
impl NamedCrossCrateInformation for SearchIndex {
    const NAME: &'static str = "search-index-js";
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct SearchDesc;
type SearchDescPart = Part<SearchIndex, String>;
impl NamedCrossCrateInformation for SearchDesc {
    const NAME: &'static str = "search-desc";
}
impl SearchDesc {
    const PATH: &'static str = "search.desc";
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct AllCrates;
type AllCratesPart = Part<AllCrates, SortedJson>;
impl NamedCrossCrateInformation for AllCrates {
    const NAME: &'static str = "crates-js";
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct CratesIndex;
type CratesIndexPart = Part<CratesIndex, String>;
impl NamedCrossCrateInformation for CratesIndex {
    const NAME: &'static str = "index-html";
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct TypeAlias;
type TypeAliasPart = Part<TypeAlias, (SortedJson, SortedJson)>;
impl NamedCrossCrateInformation for TypeAlias {
    const NAME: &'static str = "type-impl";
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct TraitAlias;
type TraitAliasPart = Part<TraitAlias, (SortedJson, SortedJson)>;
impl NamedCrossCrateInformation for TraitAlias {
    const NAME: &'static str = "trait-impl";
}

fn write_create_parents(cx: &mut Context<'_>, path: PathBuf, content: String) -> Result<(), Error> {
    let parent = path.parent().expect("trying to write to an empty path");
    try_err!(cx.shared.fs.create_dir_all(parent), parent);
    cx.shared.fs.write(path, content)?;
    Ok(())
}

/// Writes the cross crate information to the filesystem
pub(crate) fn write_merged(
    cx: &mut Context<'_>,
    options: &RenderOptions,
    invocations: &[InvocationIdentifier],
) -> Result<(), Error> {

    let emit_invocation_specific = options.emit.is_empty() || options.emit.contains(&EmitType::InvocationSpecific);

    if cx.include_sources && emit_invocation_specific {
        for (path, part) in SourcesPart::read_merged_parts(&cx.dst, invocations)? {
            let sources = SortedJson::array(part.items.into_iter());
            // This needs to be `var`, not `const`.
            // This variable needs declared in the current global scope so that if
            // src-script.js loads first, it can pick it up.
            let content = format!("var srcIndex = new Map({sources}); createSrcSidebar();");
            write_create_parents(cx, path, content)?;
        }
    }

    if emit_invocation_specific {
        for (path, part) in SearchIndexPart::read_merged_parts(&cx.dst, invocations)? {
            let all_indexes = SortedJson::array(part.items.into_iter());
            write_create_parents(cx, path, format!(r"#
var searchIndex = new Map({all_indexes});
if (typeof exports !== 'undefined') exports.searchIndex = searchIndex;
else if (window.initSearch) window.initSearch(searchIndex);
#"))?;
        }
    }

    let search_desc = PathBuf::from_iter([&cx.dst, Path::new(SearchDesc::PATH)]);
    if Path::new(&search_desc).exists() {
        try_err!(fs::remove_dir_all(&search_desc), &search_desc);
    }
    for (path, part) in SearchDescPart::read_merged_parts(&cx.dst, invocations)? {
        let part = try_err!(only_element(part.items).ok_or("not one shard in part"), &path);
        write_create_parents(cx, path, part)?;
    }

    if emit_invocation_specific {
        for (path, part) in AllCratesPart::read_merged_parts(&cx.dst, invocations)? {
            let crates = SortedJson::array(part.items.into_iter());
            write_create_parents(cx, path, format!("window.ALL_CRATES = {crates};"))?;
        }
    }

    if options.enable_index_page {
        for (path, part) in CratesIndexPart::read_merged_parts(&cx.dst, invocations)? {
            if let Some(index_page) = options.index_page.clone() {
                let mut md_opts = options.clone();
                md_opts.output = cx.dst.clone();
                md_opts.external_html = (*cx.shared).layout.external_html.clone();
                crate::markdown::render(&index_page, md_opts, cx.shared.edition())
                    .map_err(|e| Error::new(e, &index_page))?;
            } else {
                let page = layout::Page {
                    title: "Index of crates",
                    css_class: "mod sys",
                    root_path: "./",
                    static_root_path: shared.static_root_path.as_deref(),
                    description: "List of crates",
                    resource_suffix: &shared.resource_suffix,
                    rust_logo: true,
                };
                let layout = &shared.layout;
                let style_files = &shared.style_files;
                let content = format!(
                    "<h1>List of all crates</h1><ul class=\"all-items\">{}</ul>",
                    part.item.into_iter().format_with("", |k, f| {
                        f(&format_args!(
                            "<li><a href=\"{trailing_slash}index.html\">{k}</a></li>",
                            trailing_slash = ensure_trailing_slash(&k),
                        ))
                    })
                );
                layout::render(layout, &page, "", content, &style_files)
                write_create_parents(cx, path, part)?;
            }
        }
    }

    let implementors_iife = |impls, register, pending, json| {
            format!(r"#(function() {{
var {impls} = {json};
if (window.{register}) {{
    window.{register}({impls});
}} else {{
    window.{pending} = {impls};
}}
}})();
#")
    };

    for (path, part) in TypeAliasPart::read_merged_parts(&cx.dst, invocations)? {
        let part = SortedJson::object(part.items.into_iter());
        write_create_parents(cx, path, implementors_iife("type_impls", "register_type_impls", "pending_type_impls", part))?;
    }

    for (path, part) in TraitAliasPart::read_merged_parts(&cx.dst, invocations)? {
        let part = SortedJson::object(part.items.into_iter());
        write_create_parents(cx, path, implementors_iife("implementors", "register_implementors", "pending_implementors", part))?;
    }

    Ok(())
}

struct Implementor {
    text: String,
    synthetic: bool,
    types: Vec<String>,
}

impl Serialize for Implementor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(None)?;
        seq.serialize_element(&self.text)?;
        if self.synthetic {
            seq.serialize_element(&1)?;
            seq.serialize_element(&self.types)?;
        }
        seq.end()
    }
}

/// Collect the list of aliased types and their aliases.
/// <https://github.com/search?q=repo%3Arust-lang%2Frust+[RUSTDOCIMPL]+type.impl&type=code>
///
/// The clean AST has type aliases that point at their types, but
/// this visitor works to reverse that: `aliased_types` is a map
/// from target to the aliases that reference it, and each one
/// will generate one file.
struct TypeImplCollector<'cx, 'cache> {
    /// Map from DefId-of-aliased-type to its data.
    aliased_types: IndexMap<DefId, AliasedType<'cache>>,
    visited_aliases: FxHashSet<DefId>,
    cache: &'cache Cache,
    cx: &'cache mut Context<'cx>,
}

/// Data for an aliased type.
///
/// In the final file, the format will be roughly:
///
/// ```json
/// // type.impl/CRATE/TYPENAME.js
/// JSONP(
/// "CRATE": [
///   ["IMPL1 HTML", "ALIAS1", "ALIAS2", ...],
///   ["IMPL2 HTML", "ALIAS3", "ALIAS4", ...],
///    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ struct AliasedType
///   ...
/// ]
/// )
/// ```
struct AliasedType<'cache> {
    /// This is used to generate the actual filename of this aliased type.
    target_fqp: &'cache [Symbol],
    target_type: ItemType,
    /// This is the data stored inside the file.
    /// ItemId is used to deduplicate impls.
    impl_: IndexMap<ItemId, AliasedTypeImpl<'cache>>,
}

/// The `impl_` contains data that's used to figure out if an alias will work,
/// and to generate the HTML at the end.
///
/// The `type_aliases` list is built up with each type alias that matches.
struct AliasedTypeImpl<'cache> {
    impl_: &'cache Impl,
    type_aliases: Vec<(&'cache [Symbol], Item)>,
}

impl<'cx, 'cache> DocVisitor for TypeImplCollector<'cx, 'cache> {
    fn visit_item(&mut self, it: &Item) {
        self.visit_item_recur(it);
        let cache = self.cache;
        let ItemKind::TypeAliasItem(ref t) = *it.kind else { return };
        let Some(self_did) = it.item_id.as_def_id() else { return };
        if !self.visited_aliases.insert(self_did) {
            return;
        }
        let Some(target_did) = t.type_.def_id(cache) else { return };
        let get_extern = { || cache.external_paths.get(&target_did) };
        let Some(&(ref target_fqp, target_type)) =
            cache.paths.get(&target_did).or_else(get_extern)
        else {
            return;
        };
        let aliased_type = self.aliased_types.entry(target_did).or_insert_with(|| {
            let impl_ = cache
                .impls
                .get(&target_did)
                .map(|v| &v[..])
                .unwrap_or_default()
                .iter()
                .map(|impl_| {
                    (
                        impl_.impl_item.item_id,
                        AliasedTypeImpl { impl_, type_aliases: Vec::new() },
                    )
                })
                .collect();
            AliasedType { target_fqp: &target_fqp[..], target_type, impl_ }
        });
        let get_local = { || cache.paths.get(&self_did).map(|(p, _)| p) };
        let Some(self_fqp) = cache.exact_paths.get(&self_did).or_else(get_local) else {
            return;
        };
        let aliased_ty = self.cx.tcx().type_of(self_did).skip_binder();
        // Exclude impls that are directly on this type. They're already in the HTML.
        // Some inlining scenarios can cause there to be two versions of the same
        // impl: one on the type alias and one on the underlying target type.
        let mut seen_impls: FxHashSet<ItemId> = cache
            .impls
            .get(&self_did)
            .map(|s| &s[..])
            .unwrap_or_default()
            .iter()
            .map(|i| i.impl_item.item_id)
            .collect();
        for (impl_item_id, aliased_type_impl) in &mut aliased_type.impl_ {
            // Only include this impl if it actually unifies with this alias.
            // Synthetic impls are not included; those are also included in the HTML.
            //
            // FIXME(lazy_type_alias): Once the feature is complete or stable, rewrite this
            // to use type unification.
            // Be aware of `tests/rustdoc/type-alias/deeply-nested-112515.rs` which might regress.
            let Some(impl_did) = impl_item_id.as_def_id() else { continue };
            let for_ty = self.cx.tcx().type_of(impl_did).skip_binder();
            let reject_cx =
                DeepRejectCtxt { treat_obligation_params: TreatParams::AsCandidateKey };
            if !reject_cx.types_may_unify(aliased_ty, for_ty) {
                continue;
            }
            // Avoid duplicates
            if !seen_impls.insert(*impl_item_id) {
                continue;
            }
            // This impl was not found in the set of rejected impls
            aliased_type_impl.type_aliases.push((&self_fqp[..], it.clone()));
        }
    }
}

/// Final serialized form of the alias impl
struct AliasSerializableImpl {
    text: String,
    trait_: Option<String>,
    aliases: Vec<String>,
}

impl Serialize for AliasSerializableImpl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(None)?;
        seq.serialize_element(&self.text)?;
        if let Some(trait_) = &self.trait_ {
            seq.serialize_element(trait_)?;
        } else {
            seq.serialize_element(&0)?;
        }
        for type_ in &self.aliases {
            seq.serialize_element(type_)?;
        }
        seq.end()
    }
}

/// Source files directory tree
#[derive(Debug, Default)]
struct Hierarchy {
    parent: Weak<Self>,
    elem: OsString,
    children: RefCell<FxHashMap<OsString, Rc<Self>>>,
    elems: RefCell<FxHashSet<OsString>>,
}

impl Hierarchy {
    fn with_parent(elem: OsString, parent: &Rc<Self>) -> Self {
        Self { elem, parent: Rc::downgrade(parent), ..Self::default() }
    }

    fn to_json_string(&self) -> SortedJson {
        let subs = self.children.borrow();
        let files = self.elems.borrow();
        let name = SortedJson::serialize(self.elem.to_str().expect("invalid osstring conversion"));
        let mut out = Vec::from([name]);
        if !subs.is_empty() || !files.is_empty() {
            let subs = subs.iter().map(|(_, s)| s.to_json_string());
            out.push(SortedJson::array(subs));
        }
        if !files.is_empty() {
            let files = files.iter().map(|s| SortedJson::serialize(s.to_str().expect("invalid osstring")));
            out.push(SortedJson::array(files));
        }
        SortedJson::array_unsorted(out)
    }

    fn add_path(self: &Rc<Self>, path: &Path) {
        let mut h = Rc::clone(&self);
        let mut elems = path
            .components()
            .filter_map(|s| match s {
                Component::Normal(s) => Some(s.to_owned()),
                Component::ParentDir => Some(OsString::from("..")),
                _ => None,
            })
            .peekable();
        loop {
            let cur_elem = elems.next().expect("empty file path");
            if cur_elem == ".." {
                if let Some(parent) = h.parent.upgrade() {
                    h = parent;
                }
                continue;
            }
            if elems.peek().is_none() {
                h.elems.borrow_mut().insert(cur_elem);
                break;
            } else {
                let entry = Rc::clone(
                    h.children
                        .borrow_mut()
                        .entry(cur_elem.clone())
                        .or_insert_with(|| Rc::new(Self::with_parent(cur_elem, &h))),
                );
                h = entry;
            }
        }
    }
}

/// Documents the shared artifacts from `krate` to the `doc/.parts` directory
pub(crate) fn write_parts(
    cx: &mut Context<'_>,
    krate: &Crate,
    search_index: SerializedSearchIndex,
) -> Result<(), Error> {
    // Write out the shared files. Note that these are shared among all rustdoc
    // docs placed in the output directory, so this needs to be a synchronized
    // operation with respect to all other rustdocs running around.

    let crate_name = krate.name(cx.tcx()).to_string();
    let encoded_crate_name = SortedJson::serialize(&crate_name);
    let invocation = InvocationIdentifier(crate_name.clone());

    let path = PathBuf::from("index.html");
    let part = CratesIndexPart::one(crate_name.clone());
    PartsAndLocations::<CratesIndexPart>::with(path, part).write(cx, &invocation)?;

    let path = PathBuf::from("crates.js");
    let part = AllCratesPart::one(encoded_crate_name.clone());
    PartsAndLocations::<AllCratesPart>::with(path, part).write(cx, &invocation)?;

    let hierarchy = Rc::new(Hierarchy::default());
    for source in cx
        .shared
        .local_sources
        .iter()
        .filter_map(|p| p.0.strip_prefix(&cx.shared.src_root).ok())
    {
        hierarchy.add_path(source);
    }
    let path = suffix_path("src-files.js", &cx.shared.resource_suffix);
    let part = SourcesPart::one(SortedJson::serialize(&crate_name));
    PartsAndLocations::<SourcesPart>::with(path, part).write(cx, &invocation)?;

    // Update the search index and crate list.
    let SerializedSearchIndex { index, desc } = search_index;

    let path = suffix_path("search-index.js", &cx.shared.resource_suffix);
    let part = SearchIndexPart::one(SortedJson::serialize(&index));
    PartsAndLocations::<SearchIndexPart>::with(path, part).write(cx, &invocation)?;

    let mut parts = PartsAndLocations::<SearchDescPart>::default();
    for (i, (_, part)) in desc.into_iter().enumerate() {
        let path = PathBuf::from(static_files::suffix_path(
            &format!("{}/{crate_name}/{crate_name}-desc-{i}-.js", SearchDesc::PATH),
            &cx.shared.resource_suffix,
        ));
        let part = SortedJson::serialize(&part);
        let part = format!("searchState.loadedDescShard({encoded_crate_name}, {i}, {part})");
        let part = SearchDescPart::one(part);
        parts.push(path, part);
    }
    parts.write(cx, &invocation)?;

    let cloned_shared = Rc::clone(&cx.shared);
    let cache = &cloned_shared.cache;

    let mut type_impl_collector = TypeImplCollector {
        aliased_types: IndexMap::default(),
        visited_aliases: FxHashSet::default(),
        cache,
        cx,
    };
    let mut path_parts = PartsAndLocations::<TypeAliasPart>::default();
    DocVisitor::visit_crate(&mut type_impl_collector, &krate);
    let cx = type_impl_collector.cx;
    let aliased_types = type_impl_collector.aliased_types;
    for aliased_type in aliased_types.values() {
        let impls = aliased_type
            .impl_
            .values()
            .flat_map(|AliasedTypeImpl { impl_, type_aliases }| {
                let mut ret = Vec::new();
                let trait_ = impl_
                    .inner_impl()
                    .trait_
                    .as_ref()
                    .map(|trait_| format!("{:#}", trait_.print(cx)));
                // render_impl will filter out "impossible-to-call" methods
                // to make that functionality work here, it needs to be called with
                // each type alias, and if it gives a different result, split the impl
                for &(type_alias_fqp, ref type_alias_item) in type_aliases {
                    let mut buf = Buffer::html();
                    cx.id_map = Default::default();
                    cx.deref_id_map = Default::default();
                    let target_did = impl_
                        .inner_impl()
                        .trait_
                        .as_ref()
                        .map(|trait_| trait_.def_id())
                        .or_else(|| impl_.inner_impl().for_.def_id(cache));
                    let provided_methods;
                    let assoc_link = if let Some(target_did) = target_did {
                        provided_methods = impl_.inner_impl().provided_trait_methods(cx.tcx());
                        AssocItemLink::GotoSource(ItemId::DefId(target_did), &provided_methods)
                    } else {
                        AssocItemLink::Anchor(None)
                    };
                    super::render_impl(
                        &mut buf,
                        cx,
                        *impl_,
                        &type_alias_item,
                        assoc_link,
                        RenderMode::Normal,
                        None,
                        &[],
                        ImplRenderingParameters {
                            show_def_docs: true,
                            show_default_items: true,
                            show_non_assoc_items: true,
                            toggle_open_by_default: true,
                        },
                    );
                    let text = buf.into_inner();
                    let type_alias_fqp = (*type_alias_fqp).iter().join("::");
                    if Some(&text) == ret.last().map(|s: &AliasSerializableImpl| &s.text) {
                        ret.last_mut()
                            .expect("already established that ret.last() is Some()")
                            .aliases
                            .push(type_alias_fqp);
                    } else {
                        ret.push(AliasSerializableImpl {
                            text,
                            trait_: trait_.clone(),
                            aliases: vec![type_alias_fqp],
                        })
                    }
                }
                ret
            })
            .collect::<Vec<_>>();

        let mut path = PathBuf::from("type.impl");
        for part in &aliased_type.target_fqp[..aliased_type.target_fqp.len() - 1] {
            path.push(part.to_string());
        }
        let aliased_item_type = aliased_type.target_type;
        path.push(&format!(
            "{aliased_item_type}.{}.js",
            aliased_type.target_fqp[aliased_type.target_fqp.len() - 1]
        ));

        let part = SortedJson::array(impls.iter().map(SortedJson::serialize).collect::<Vec<_>>());
        let part = TypeAliasPart::one((encoded_crate_name.clone(), part));
        path_parts.push(path, part);
    }
    path_parts.write(cx, &invocation)?;

    let mut path_parts = PartsAndLocations::<TraitAliasPart>::default();
    // Update the list of all implementors for traits
    // <https://github.com/search?q=repo%3Arust-lang%2Frust+[RUSTDOCIMPL]+trait.impl&type=code>
    for (&did, imps) in &cache.implementors {
        // Private modules can leak through to this phase of rustdoc, which
        // could contain implementations for otherwise private types. In some
        // rare cases we could find an implementation for an item which wasn't
        // indexed, so we just skip this step in that case.
        //
        // FIXME: this is a vague explanation for why this can't be a `get`, in
        //        theory it should be...
        let (remote_path, remote_item_type) = match cache.exact_paths.get(&did) {
            Some(p) => match cache.paths.get(&did).or_else(|| cache.external_paths.get(&did)) {
                Some((_, t)) => (p, t),
                None => continue,
            },
            None => match cache.external_paths.get(&did) {
                Some((p, t)) => (p, t),
                None => continue,
            },
        };

        let implementors = imps
            .iter()
            .filter_map(|imp| {
                // If the trait and implementation are in the same crate, then
                // there's no need to emit information about it (there's inlining
                // going on). If they're in different crates then the crate defining
                // the trait will be interested in our implementation.
                //
                // If the implementation is from another crate then that crate
                // should add it.
                if imp.impl_item.item_id.krate() == did.krate || !imp.impl_item.item_id.is_local() {
                    None
                } else {
                    Some(Implementor {
                        text: imp.inner_impl().print(false, cx).to_string(),
                        synthetic: imp.inner_impl().kind.is_auto(),
                        types: collect_paths_for_type(imp.inner_impl().for_.clone(), cache),
                    })
                }
            })
            .collect::<Vec<_>>();

        // Only create a js file if we have impls to add to it. If the trait is
        // documented locally though we always create the file to avoid dead
        // links.
        if implementors.is_empty() && !cache.paths.contains_key(&did) {
            continue;
        }

        let mut path = PathBuf::from("trait.impl");
        for part in &remote_path[..remote_path.len() - 1] {
            path.push(part.to_string());
        }
        path.push(&format!("{remote_item_type}.{}.js", remote_path[remote_path.len() - 1]));

        let part = SortedJson::array(implementors.iter().map(SortedJson::serialize).collect::<Vec<_>>());
        let part = TraitAliasPart::one((encoded_crate_name.clone(), part));
        path_parts.push(path, part);
    }
    path_parts.write(cx, &invocation)?;

    Ok(())
}
